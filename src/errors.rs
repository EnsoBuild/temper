use eyre::Report;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, error::Error};

use warp::{body::BodyDeserializeError, hyper::StatusCode, reject::Reject, Rejection, Reply};

#[derive(Serialize, Deserialize, Debug)]
pub struct ErrorMessage {
    pub code: u16,
    pub message: String,
}

#[derive(Debug)]
pub struct NoURLForChainIdError;

impl Reject for NoURLForChainIdError {}

#[derive(Debug)]
pub struct IncorrectChainIdError();

impl Reject for IncorrectChainIdError {}

#[derive(Debug)]
pub struct MultipleChainIdsError();

impl Reject for MultipleChainIdsError {}

#[derive(Debug)]
pub struct MultipleBlockNumbersError();

impl Reject for MultipleBlockNumbersError {}

#[derive(Debug)]
pub struct InvalidBlockNumbersError();

impl Reject for InvalidBlockNumbersError {}

#[derive(Debug)]
pub struct StateNotFound();

impl Reject for StateNotFound {}

#[derive(Debug)]
pub struct OverrideError;

impl Reject for OverrideError {}

#[derive(Debug)]
pub struct EvmError(pub Report);

impl Reject for EvmError {}

#[derive(Debug)]
pub struct InvalidRpcError();

impl Reject for InvalidRpcError {}

#[derive(Debug)]
pub struct RpcError();

impl Reject for RpcError {}

#[derive(Debug)]
pub struct InvalidIndexError();

impl Reject for InvalidIndexError {}

pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
    let code;
    let message: String;
    println!("Handling rejection: {:?}", err);
    if err.is_not_found() {
        code = StatusCode::NOT_FOUND;
        message = "NOT_FOUND".to_string();
    } else if let Some(_e) = err.find::<StateNotFound>() {
        code = StatusCode::NOT_FOUND;
        message = "STATE_NOT_FOUND".to_string();
    } else if let Some(NoURLForChainIdError) = err.find() {
        code = StatusCode::BAD_REQUEST;
        message = "CHAIN_ID_NOT_SUPPORTED".to_string();
    } else if let Some(_e) = err.find::<IncorrectChainIdError>() {
        code = StatusCode::BAD_REQUEST;
        message = "INCORRECT_CHAIN_ID".to_string();
    } else if let Some(_e) = err.find::<MultipleChainIdsError>() {
        code = StatusCode::BAD_REQUEST;
        message = "MULTIPLE_CHAIN_IDS".to_string();
    } else if let Some(_e) = err.find::<MultipleBlockNumbersError>() {
        code = StatusCode::BAD_REQUEST;
        message = "MULTIPLE_BLOCK_NUMBERS".to_string();
    } else if let Some(_e) = err.find::<InvalidBlockNumbersError>() {
        code = StatusCode::BAD_REQUEST;
        message = "INVALID_BLOCK_NUMBERS".to_string();
    } else if let Some(_e) = err.find::<OverrideError>() {
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = "OVERRIDE_ERROR".to_string();
    } else if let Some(_e) = err.find::<InvalidIndexError>() {
        code = StatusCode::BAD_REQUEST;
        message = "INVALID_TRANSACTION_INDEX".to_string();
    } else if let Some(_e) = err.find::<EvmError>() {
        if _e.0.to_string().contains("CallGasCostMoreThanGasLimit") {
            code = StatusCode::BAD_REQUEST;
            message = "OUT_OF_GAS".to_string();
        } else {
            code = StatusCode::INTERNAL_SERVER_ERROR;
            message = "EVM_ERROR".to_string();
        }
    } else if let Some(e) = err.find::<BodyDeserializeError>() {
        // This error happens if the body could not be deserialized correctly
        // We can use the cause to analyze the error and customize the error message
        dbg!(e);
        message = match e.source() {
            Some(cause) => format!("BAD REQUEST: {cause}"),
            None => "BAD_REQUEST".to_string(),
        };
        code = StatusCode::BAD_REQUEST;
    } else if let Some(_e) = err.find::<InvalidRpcError>() {
        code = StatusCode::BAD_REQUEST;
        message = "INVALID_RPC".to_string();
    } else if let Some(_e) = err.find::<RpcError>() {
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = "RPC_ERROR".to_string();
    } else if err.find::<warp::reject::MethodNotAllowed>().is_some() {
        // We can handle a specific error, here METHOD_NOT_ALLOWED,
        // and render it however we want
        code = StatusCode::METHOD_NOT_ALLOWED;
        message = "METHOD_NOT_ALLOWED".to_string();
    } else if err.find::<warp::reject::MissingHeader>().is_some() {
        code = StatusCode::UNAUTHORIZED;
        message = "UNAUTHORIZED".to_string();
    } else {
        // We should have expected this... Just log and say its a 500
        eprintln!("unhandled rejection: {err:?}");
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = "UNHANDLED_REJECTION".to_string();
    }

    let json = warp::reply::json(&ErrorMessage {
        code: code.as_u16(),
        message,
    });

    Ok(warp::reply::with_status(json, code))
}
