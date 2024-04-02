use std::collections::HashMap;

use ethers::abi::{Address, Hash, Uint};
use ethers::core::types::Log;
use ethers::types::transaction::eip2930::AccessList;
use ethers::types::Bytes;
use foundry_config::Chain;
use foundry_evm::executor::backend::DatabaseExt;
use foundry_evm::executor::{fork::CreateFork, Executor};
use foundry_evm::executor::{opts::EvmOpts, Backend, ExecutorBuilder};
use foundry_evm::trace::identifier::{EtherscanIdentifier, SignaturesIdentifier};
use foundry_evm::trace::node::CallTraceNode;
use foundry_evm::trace::{CallTraceArena, CallTraceDecoder, CallTraceDecoderBuilder};
use foundry_evm::utils::{h160_to_b160, u256_to_ru256};
use revm::db::DatabaseRef;
use revm::interpreter::InstructionResult;
use revm::primitives::{Account, Bytecode, Env, StorageSlot};
use revm::DatabaseCommit;

use crate::errors::{EvmError, OverrideError};
use crate::simulation::CallTrace;

#[derive(Debug, Clone)]
pub struct CallRawRequest {
    pub from: Address,
    pub to: Address,
    pub value: Option<Uint>,
    pub data: Option<Bytes>,
    pub access_list: Option<AccessList>,
    pub format_trace: bool,
}

#[derive(Debug, Clone)]
pub struct CallRawResult {
    pub gas_used: u64,
    pub block_number: u64,
    pub success: bool,
    pub trace: Option<CallTraceArena>,
    pub logs: Vec<Log>,
    pub exit_reason: InstructionResult,
    pub return_data: Bytes,
    pub formatted_trace: Option<String>,
}

impl From<CallTraceNode> for CallTrace {
    fn from(item: CallTraceNode) -> Self {
        CallTrace {
            call_type: item.trace.kind,
            from: item.trace.caller,
            to: item.trace.address,
            value: item.trace.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageOverride {
    pub slots: HashMap<Hash, Uint>,
    pub diff: bool,
}

pub struct Evm {
    executor: Executor,
    decoder: CallTraceDecoder,
    etherscan_identifier: Option<EtherscanIdentifier>,
}

impl Evm {
    pub fn new(
        env: Option<Env>,
        fork_url: String,
        fork_block_number: Option<u64>,
        gas_limit: u64,
        tracing: bool,
        etherscan_key: Option<String>,
    ) -> Self {
        let evm_opts = EvmOpts {
            fork_url: Some(fork_url.clone()),
            fork_block_number,
            env: foundry_evm::executor::opts::Env {
                chain_id: None,
                code_size_limit: None,
                gas_price: Some(0),
                gas_limit: u64::MAX,
                ..Default::default()
            },
            memory_limit: foundry_config::Config::default().memory_limit,
            ..Default::default()
        };

        let fork_opts = CreateFork {
            url: fork_url,
            enable_caching: true,
            env: evm_opts.evm_env_blocking().unwrap(),
            evm_opts,
        };

        let db = Backend::spawn(Some(fork_opts.clone()));

        let mut builder = ExecutorBuilder::default()
            .with_gas_limit(gas_limit.into())
            .set_tracing(tracing);

        if let Some(env) = env {
            builder = builder.with_config(env);
        } else {
            builder = builder.with_config(fork_opts.env.clone());
        }

        let executor = builder.build(db);

        let foundry_config = foundry_config::Config {
            etherscan_api_key: etherscan_key,
            ..Default::default()
        };

        let chain: Chain = fork_opts.env.cfg.chain_id.to::<u64>().into();
        let etherscan_identifier = EtherscanIdentifier::new(&foundry_config, Some(chain)).ok();
        let mut decoder = CallTraceDecoderBuilder::new().with_verbosity(5).build();

        if let Ok(identifier) =
            SignaturesIdentifier::new(foundry_config::Config::foundry_cache_dir(), false)
        {
            decoder.add_signature_identifier(identifier);
        }

        Evm {
            executor,
            decoder,
            etherscan_identifier,
        }
    }

    pub async fn call_raw(&mut self, call: CallRawRequest) -> Result<CallRawResult, EvmError> {
        self.set_access_list(call.access_list);
        let res = self
            .executor
            .call_raw(
                call.from,
                call.to,
                call.data.unwrap_or_default().0,
                call.value.unwrap_or_default(),
            )
            .map_err(|err| {
                dbg!(&err);
                EvmError(err)
            })?;

        let formatted_trace = if call.format_trace {
            let mut output = String::new();
            for trace in &mut res.traces.clone() {
                if let Some(identifier) = &mut self.etherscan_identifier {
                    self.decoder.identify(trace, identifier);
                }
                self.decoder.decode(trace).await;
                output.push_str(format!("{trace}").as_str());
            }
            Some(output)
        } else {
            None
        };

        Ok(CallRawResult {
            gas_used: res.gas_used,
            block_number: res.env.block.number.to(),
            success: !res.reverted,
            trace: res.traces,
            logs: res.logs,
            exit_reason: res.exit_reason,
            return_data: Bytes(res.result),
            formatted_trace,
        })
    }

    pub fn override_account(
        &mut self,
        address: Address,
        balance: Option<Uint>,
        nonce: Option<u64>,
        code: Option<Bytes>,
        storage: Option<StorageOverride>,
    ) -> Result<(), OverrideError> {
        let address = h160_to_b160(address);
        let mut account = Account {
            info: self
                .executor
                .backend()
                .basic(address)
                .map_err(|_| OverrideError)?
                .unwrap_or_default(),
            ..Account::new_not_existing()
        };

        if let Some(balance) = balance {
            account.info.balance = u256_to_ru256(balance);
        }
        if let Some(nonce) = nonce {
            account.info.nonce = nonce;
        }
        if let Some(code) = code {
            account.info.code = Some(Bytecode::new_raw(code.to_vec().into()));
        }
        if let Some(storage) = storage {
            // If we do a "full storage override", make sure to set this flag so
            // that existing storage slots are cleared, and unknown ones aren't
            // fetched from the forked node.
            account.storage_cleared = !storage.diff;
            account
                .storage
                .extend(storage.slots.into_iter().map(|(key, value)| {
                    (
                        u256_to_ru256(Uint::from_big_endian(key.as_bytes())),
                        StorageSlot::new(u256_to_ru256(value)),
                    )
                }));
        }

        self.executor
            .backend_mut()
            .commit([(address, account)].into_iter().collect());

        Ok(())
    }

    pub async fn call_raw_committing(
        &mut self,
        call: CallRawRequest,
        gas_limit: u64,
    ) -> Result<CallRawResult, EvmError> {
        self.executor.set_gas_limit(gas_limit.into());
        self.set_access_list(call.access_list);
        let res = self
            .executor
            .call_raw_committing(
                call.from,
                call.to,
                call.data.unwrap_or_default().0,
                call.value.unwrap_or_default(),
            )
            .map_err(|err| {
                dbg!(&err);
                EvmError(err)
            })?;

        let formatted_trace = if call.format_trace {
            let mut output = String::new();
            for trace in &mut res.traces.clone() {
                if let Some(identifier) = &mut self.etherscan_identifier {
                    self.decoder.identify(trace, identifier);
                }
                self.decoder.decode(trace).await;
                output.push_str(format!("{trace}").as_str());
            }
            Some(output)
        } else {
            None
        };

        Ok(CallRawResult {
            gas_used: res.gas_used,
            block_number: res.env.block.number.to(),
            success: !res.reverted,
            trace: res.traces,
            logs: res.logs,
            exit_reason: res.exit_reason,
            return_data: Bytes(res.result),
            formatted_trace,
        })
    }

    pub async fn set_block(&mut self, number: u64) -> Result<(), EvmError> {
        self.executor.env_mut().block.number = Uint::from(number).into();
        Ok(())
    }

    pub fn get_block(&self) -> Uint {
        self.executor.env().block.number.into()
    }

    pub async fn set_block_timestamp(&mut self, timestamp: u64) -> Result<(), EvmError> {
        self.executor.env_mut().block.timestamp = Uint::from(timestamp).into();
        Ok(())
    }

    pub fn get_block_timestamp(&self) -> Uint {
        self.executor.env().block.timestamp.into()
    }

    pub fn get_chain_id(&self) -> Uint {
        self.executor.env().cfg.chain_id.into()
    }

    pub fn get_fork_url(&self) -> Option<String> {
        self.executor.backend().active_fork_url()
    }
    
    fn set_access_list(&mut self, access_list: Option<AccessList>) {
        self.executor.env_mut().tx.access_list = access_list
            .unwrap_or_default()
            .0
            .into_iter()
            .map(|item| {
                (
                    h160_to_b160(item.address),
                    item.storage_keys
                        .into_iter()
                        .map(|key| u256_to_ru256(Uint::from_big_endian(key.as_bytes())))
                        .collect(),
                )
            })
            .collect();
    }
}
