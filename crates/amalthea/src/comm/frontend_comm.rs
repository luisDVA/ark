/*
 * frontend_comm.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::wire::client_event::ClientEvent;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg_type", rename_all = "snake_case")]
pub enum FrontendMessage {
    Event(ClientEvent),
    RpcRequest(JsonRpcRequest),
    RpcResultResponse(JsonRpcResult),
    RpcResultError(JsonRpcError),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcRequest {
    pub method: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcResult {
    pub id: String,
    pub result: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg_type", rename_all = "snake_case")]
pub struct JsonRpcError {
    pub id: String,
    pub error: JsonRpcErrorData,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcErrorData {
    pub message: String,
    pub code: JsonRpcErrorCode,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum JsonRpcErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,
}
