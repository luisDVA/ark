/*
 * frontend_comm.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::comm::base_comm::JsonRpcErrorCode;
use crate::wire::client_event::ClientEvent;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg_type", rename_all = "snake_case")]
pub enum FrontendMessage {
    Event(ClientEvent),
    RpcRequest(FrontendRpcRequest),
    RpcResultResponse(FrontendRpcResult),
    RpcResultError(FrontendRpcError),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FrontendRpcRequest {
    pub method: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FrontendRpcResult {
    pub id: String,
    pub result: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FrontendRpcErrorData {
    pub message: String,
    pub code: JsonRpcErrorCode,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg_type", rename_all = "snake_case")]
pub struct FrontendRpcError {
    pub id: String,
    pub error: FrontendRpcErrorData,
}
