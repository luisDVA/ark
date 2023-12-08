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

#[derive(Clone, Debug, Serialize)]
pub enum JsonRpcResponse {
    Result(JsonRpcResult),
    Error(JsonRpcError),
}

// FIXME
impl crate::wire::jupyter_message::MessageType for JsonRpcResponse {
    fn message_type() -> String {
        String::from("rpc_reply")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcRequest {
    pub method: String,
    pub params: Vec<Value>, // Should we use Value::Object() instead?
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcResult {
    pub id: String,
    pub result: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "msg_type", rename_all = "snake_case")]
pub struct JsonRpcError {
    pub id: String,
    pub error: JsonRpcErrorData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JsonRpcErrorData {
    pub message: String,
    pub code: JsonRpcErrorCode,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[repr(i64)]
pub enum JsonRpcErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,
}
