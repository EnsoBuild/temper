use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use dashmap::mapref::one::RefMut;
use ethers::abi::{Address, Hash, Uint};
use ethers::core::types::Log;
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::transaction::eip2930::AccessList;
use ethers::types::Bytes;
use eyre::anyhow;
use foundry_evm::CallKind;
use revm::interpreter::InstructionResult;
use revm::primitives::bitvec::macros::internal::funty::Fundamental;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;
use warp::reply::Json;
use warp::Rejection;

use crate::errors::{
    IncorrectChainIdError, InvalidBlockNumbersError, MultipleChainIdsError, NoURLForChainIdError,
    StateNotFound, RpcError
};
use crate::evm::StorageOverride;
use crate::SharedSimulationState;

use super::config::Config;
use super::evm::{CallRawRequest, Evm};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationRequest {
    pub chain_id: u64,
    pub from: Address,
    pub to: Address,
    pub data: Option<Bytes>,
    pub gas_limit: u64,
    pub value: Option<PermissiveUint>,
    pub access_list: Option<AccessList>,
    pub block_number: Option<u64>,
    pub block_timestamp: Option<u64>,
    pub state_overrides: Option<HashMap<Address, StateOverride>>,
    pub format_trace: Option<bool>,
    #[serde(rename = "transactionBlockIndex")]
    pub transaction_block_index: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SimulationResponse {
    pub simulation_id: u64,
    pub gas_used: u64,
    pub block_number: u64,
    pub success: bool,
    pub trace: Vec<CallTrace>,
    pub formatted_trace: Option<String>,
    pub logs: Vec<Log>,
    pub exit_reason: InstructionResult,
    pub return_data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSimulationRequest {
    pub chain_id: u64,
    pub gas_limit: u64,
    pub block_number: Option<u64>,
    pub block_timestamp: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSimulationResponse {
    pub stateful_simulation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatefulSimulationEndResponse {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateOverride {
    pub balance: Option<PermissiveUint>,
    pub nonce: Option<u64>,
    pub code: Option<Bytes>,
    #[serde(flatten)]
    pub state: Option<State>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum State {
    Full {
        state: HashMap<Hash, PermissiveUint>,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        state_diff: HashMap<Hash, PermissiveUint>,
    },
}

impl From<State> for StorageOverride {
    fn from(value: State) -> Self {
        let (slots, diff) = match value {
            State::Full { state } => (state, false),
            State::Diff { state_diff } => (state_diff, true),
        };

        StorageOverride {
            slots: slots
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
            diff,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CallTrace {
    pub call_type: CallKind,
    pub from: Address,
    pub to: Address,
    pub value: Uint,
}

#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq)]
#[serde(transparent)]
pub struct PermissiveUint(pub Uint);

impl From<PermissiveUint> for Uint {
    fn from(value: PermissiveUint) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for PermissiveUint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept value in hex or decimal formats
        let value = String::deserialize(deserializer)?;
        let parsed = if value.starts_with("0x") {
            Uint::from_str(&value).map_err(serde::de::Error::custom)?
        } else {
            Uint::from_dec_str(&value).map_err(serde::de::Error::custom)?
        };
        Ok(Self(parsed))
    }
}

fn chain_id_to_fork_url(chain_id: u64) -> Result<String, Rejection> {
    match chain_id {
        // ethereum
        1 => Ok("https://eth.llamarpc.com".to_string()),
        5 => Ok("https://eth-goerli.g.alchemy.com/v2/demo".to_string()),
        11155111 => Ok("https://eth-sepolia.g.alchemy.com/v2/demo".to_string()),
        // polygon
        137 => Ok("https://polygon-mainnet.g.alchemy.com/v2/demo".to_string()),
        80001 => Ok("https://polygon-mumbai.g.alchemy.com/v2/demo".to_string()),
        // avalanche
        43114 => Ok("https://api.avax.network/ext/bc/C/rpc".to_string()),
        43113 => Ok("https://api.avax-test.network/ext/bc/C/rpc".to_string()),
        // fantom
        250 => Ok("https://rpcapi.fantom.network/".to_string()),
        4002 => Ok("https://rpc.testnet.fantom.network/".to_string()),
        // xdai
        100 => Ok("https://rpc.xdaichain.com/".to_string()),
        // bsc
        56 => Ok("https://bsc-dataseed.binance.org/".to_string()),
        97 => Ok("https://data-seed-prebsc-1-s1.binance.org:8545/".to_string()),
        // arbitrum
        42161 => Ok("https://arb1.arbitrum.io/rpc".to_string()),
        421613 => Ok("https://goerli-rollup.arbitrum.io/rpc".to_string()),
        // optimism
        10 => Ok("https://mainnet.optimism.io/".to_string()),
        420 => Ok("https://goerli.optimism.io/".to_string()),
        _ => Err(NoURLForChainIdError.into()),
    }
}

async fn run(
    evm: &mut Evm,
    transaction: SimulationRequest,
    commit: bool,
) -> Result<SimulationResponse, Rejection> {
    for (address, state_override) in transaction.state_overrides.into_iter().flatten() {
        evm.override_account(
            address,
            state_override.balance.map(Uint::from),
            state_override.nonce,
            state_override.code,
            state_override.state.map(StorageOverride::from),
        )?;
    }

    let call = CallRawRequest {
        from: transaction.from,
        to: transaction.to,
        value: transaction.value.map(Uint::from),
        data: transaction.data,
        access_list: transaction.access_list,
        format_trace: transaction.format_trace.unwrap_or_default(),
    };
    let result = if commit {
        evm.call_raw_committing(call, transaction.gas_limit).await?
    } else {
        evm.call_raw(call).await?
    };

    Ok(SimulationResponse {
        simulation_id: 1,
        gas_used: result.gas_used,
        block_number: result.block_number,
        success: result.success,
        trace: result
            .trace
            .unwrap_or_default()
            .arena
            .into_iter()
            .map(CallTrace::from)
            .collect(),
        logs: result.logs,
        exit_reason: result.exit_reason,
        formatted_trace: result.formatted_trace,
        return_data: result.return_data,
    })
}

pub async fn simulate(transaction: SimulationRequest, config: Config) -> Result<Json, Rejection> {
    let fork_url = config
        .fork_url
        .unwrap_or(chain_id_to_fork_url(transaction.chain_id)?);
    let mut evm = Evm::new(
        None,
        fork_url.clone(),
        transaction.block_number,
        transaction.gas_limit,
        true,
        config.etherscan_key,
    );

    if evm.get_chain_id() != Uint::from(transaction.chain_id) {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    if let Some(timestamp) = transaction.block_timestamp {
        evm.set_block_timestamp(timestamp)
            .await
            .expect("failed to set block timestamp");
    }

    let response: SimulationResponse = if transaction.transaction_block_index.is_some() {
        let mut arr_resp = Vec::with_capacity(1);
        apply_block_transactions(&fork_url, &transaction, &mut evm, &mut arr_resp).await?;
        arr_resp
            .pop()
            .ok_or_else(|| anyhow!("No simulated transaction"))
            .unwrap()
    } else {
        run(&mut evm, transaction, false).await?
    };

    Ok(warp::reply::json(&response))
}

pub async fn simulate_bundle(
    transactions: Vec<SimulationRequest>,
    config: Config,
) -> Result<Json, Rejection> {
    let first_chain_id = transactions[0].chain_id;
    let first_block_number = transactions[0].block_number;
    let first_block_timestamp = transactions[0].block_timestamp;

    let fork_url = config
        .fork_url
        .unwrap_or(chain_id_to_fork_url(first_chain_id)?);
    let mut evm = Evm::new(
        None,
        fork_url.clone(),
        first_block_number,
        transactions[0].gas_limit,
        true,
        config.etherscan_key,
    );

    if evm.get_chain_id() != Uint::from(first_chain_id) {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    if let Some(timestamp) = first_block_timestamp {
        evm.set_block_timestamp(timestamp)
            .await
            .expect("failed to set block timestamp");
    }

    let mut response = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        if transaction.chain_id != first_chain_id {
            return Err(warp::reject::custom(MultipleChainIdsError()));
        }
        if transaction.block_number != first_block_number {
            let tx_block = transaction
                .block_number
                .expect("Transaction has no block number");
            if transaction.block_number < first_block_number || tx_block < evm.get_block().as_u64()
            {
                return Err(warp::reject::custom(InvalidBlockNumbersError()));
            }
            evm.set_block(tx_block)
                .await
                .expect("Failed to set block number");
            evm.set_block_timestamp(evm.get_block_timestamp().as_u64() + 12)
                .await
                .expect("Failed to set block timestamp");
        }

        if transaction.clone().transaction_block_index.is_some() {
            apply_block_transactions(&fork_url, &transaction, &mut evm, &mut response).await?;
        } else {
            response.push(run(&mut evm, transaction, true).await?);
        }
    }

    Ok(warp::reply::json(&response))
}

async fn apply_block_transactions(
    fork_url: &String,
    transaction: &SimulationRequest,
    evm: &mut Evm,
    response: &mut Vec<SimulationResponse>,
) -> Result<(), Rejection> {
    let provider = Provider::<Http>::try_from(fork_url);

    if provider.is_err() {
        return Err(warp::reject::custom(NoURLForChainIdError));
    }

    let block_transactions = provider
        .unwrap()
        .get_block_with_txs(
            transaction
                .clone()
                .block_number
                .expect("Transaction has no block number"),
        )
        .await;
    if block_transactions.is_err() {
        return Err(warp::reject::custom(RpcError()));
    }
    let block_transactions = block_transactions.unwrap();

    if block_transactions.is_none() {
        response.push(run(evm, transaction.clone(), true).await?);
        return Ok(());
    }

    let block_transactions = block_transactions.unwrap();

    let simulation_transactions: Vec<_> = block_transactions
        .transactions
        .iter()
        .map(|x| SimulationRequest {
            chain_id: x.chain_id.unwrap().as_u64(),
            from: x.from,
            to: x.to.unwrap(),
            value: Some(PermissiveUint(x.value)),
            data: Some(x.input.clone()),
            gas_limit: x.gas.as_u64(),
            block_number: None,
            format_trace: None,
            transaction_block_index: None,
            access_list: None,
            block_timestamp: None,
            state_overrides: None,
        })
        .collect();
    let transaction_block_index = transaction.transaction_block_index.unwrap();
    let transactions_before_index = simulation_transactions
        .iter()
        .take(transaction_block_index.as_usize())
        .collect::<Vec<_>>();
    let transactions_after_index = simulation_transactions
        .iter()
        .skip(transaction_block_index.as_usize());
    for before_tx in transactions_before_index {
        let result = run(evm, before_tx.clone(), true).await;
        result.expect("Failed to run transactions in block prior to transaction index");
    }
    response.push(run(evm, transaction.clone(), true).await?);
    for after_tx in transactions_after_index {
        let result = run(evm, after_tx.clone(), true).await;
        result.expect("Failed to run transactions in block after transaction index");
    }
    Ok(())
}

pub async fn simulate_stateful_new(
    stateful_simulation_request: StatefulSimulationRequest,
    config: Config,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    let fork_url = config
        .fork_url
        .unwrap_or(chain_id_to_fork_url(stateful_simulation_request.chain_id)?);
    let mut evm = Evm::new(
        None,
        fork_url,
        stateful_simulation_request.block_number,
        stateful_simulation_request.gas_limit,
        true,
        config.etherscan_key,
    );

    if let Some(timestamp) = stateful_simulation_request.block_timestamp {
        evm.set_block_timestamp(timestamp).await?;
    }

    let new_id = Uuid::new_v4();
    state.evms.insert(new_id, Arc::new(Mutex::new(evm)));

    let response = StatefulSimulationResponse {
        stateful_simulation_id: new_id,
    };

    Ok(warp::reply::json(&response))
}

pub async fn simulate_stateful_end(
    param: Uuid,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    if state.evms.contains_key(&param) {
        state.evms.remove(&param);
        let response = StatefulSimulationEndResponse { success: true };
        Ok(warp::reply::json(&response))
    } else {
        Err(warp::reject::custom(StateNotFound()))
    }
}

pub async fn simulate_stateful(
    param: Uuid,
    transactions: Vec<SimulationRequest>,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    let first_chain_id = transactions[0].chain_id;
    let first_block_number = transactions[0].block_number;

    let mut response = Vec::with_capacity(transactions.len());

    // Get a mutable reference to the EVM here.
    let evm_ref_mut: RefMut<'_, Uuid, Arc<Mutex<Evm>>> = state
        .evms
        .get_mut(&param)
        .ok_or_else(warp::reject::not_found)?;

    // Dereference to obtain the EVM.
    let evm = evm_ref_mut.value();
    let mut evm = evm.lock().await;

    if evm.get_chain_id() != Uint::from(first_chain_id) {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    for transaction in transactions {
        if transaction.chain_id != first_chain_id {
            return Err(warp::reject::custom(MultipleChainIdsError()));
        }
        if transaction.block_number != first_block_number
            || transaction.block_number.unwrap() != evm.get_block().as_u64()
        {
            let tx_block = transaction
                .block_number
                .expect("Transaction has no block number");
            if transaction.block_number < first_block_number || tx_block < evm.get_block().as_u64()
            {
                return Err(warp::reject::custom(InvalidBlockNumbersError()));
            }
            evm.set_block(tx_block)
                .await
                .expect("Failed to set block number");
            let block_timestamp = evm.get_block_timestamp().as_u64();
            evm.set_block_timestamp(block_timestamp + 12)
                .await
                .expect("Failed to set block timestamp");
        }
        if transaction.clone().transaction_block_index.is_some() {
            apply_block_transactions(
                &evm.get_fork_url().expect("No fork URL"),
                &transaction,
                &mut evm,
                &mut response,
            )
            .await?;
        } else {
            response.push(run(&mut evm, transaction, true).await?);
        }
    }

    Ok(warp::reply::json(&response))
}
