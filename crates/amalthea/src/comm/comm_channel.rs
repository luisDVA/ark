/*
 * comm_channel.rs
 *
 * Copyright (C) 2022 Posit Software, PBC. All rights reserved.
 *
 */

use crossbeam::channel::Sender;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use strum_macros::EnumString;
use uuid::Uuid;

use super::frontend_comm::JsonRpcResponse;
use crate::wire::jupyter_message::MessageType;

#[derive(EnumString, PartialEq)]
#[strum(serialize_all = "camelCase")]
pub enum Comm {
    /// A variables pane.
    Variables,

    /// A wrapper for a Language Server Protocol server.
    Lsp,

    /// A wrapper for a Debug Adapter Protocol server.
    Dap,

    /// A dynamic (resizable) plot.
    Plot,

    /// A data viewer.
    DataViewer,

    /// The Positron help pane.
    Help,

    /// The Positron front end.
    FrontEnd,

    /// Some other comm with a custom name.
    Other(String),
}

// TODO: Rename to Request and Reply?
#[derive(Debug)]
pub enum CommMsg {
    /// A message that is part of a Remote Procedure Call (RPC). The first value
    /// is the unique ID of the RPC invocation (i.e. the Jupyter message ID),
    /// and the second value is the data associated with the RPC (the request or
    /// response).
    Rpc(String, Value),

    // DOCME
    ReverseRpc(Sender<JsonRpcResponse>, Value),

    /// A message representing any other data sent on the comm channel; usually
    /// used for events.
    Data(Value),

    // A message indicating that the comm channel should be closed.
    Close,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct RpcRequest {
    msg_type: String,
    id: String,
    jsonrpc: String,
    method: String,
    params: Value,
}

impl RpcRequest {
    pub fn new<T>(method: String, params: T) -> anyhow::Result<Self>
    where
        T: Serialize,
    {
        let request = Self {
            msg_type: String::from("rpc_request"),
            id: Uuid::new_v4().to_string(),
            jsonrpc: String::from("2.0"),
            method,
            params: serde_json::to_value(params)?,
        };
        Ok(request)
    }
}

impl MessageType for RpcRequest {
    fn message_type() -> String {
        String::from("rpc_request")
    }
}
