/*
 * comm_event.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use serde_json::Value;

use super::frontend_comm::JsonRpcResponse;
use crate::comm::comm_channel::CommMsg;
use crate::socket::comm::CommSocket;
use crate::wire::header::JupyterHeader;

/**
 * Enumeration of events that can be received by the comm manager.
 */
pub enum CommManagerEvent {
    /// A new Comm was opened
    Opened(CommSocket, Value),

    /// A message was received on a Comm; the first value is the comm ID, and the
    /// second value is the message.
    Message(String, CommMsg),

    /// An RPC request was received from the front end
    PendingRpc(JupyterHeader),

    /// An RPC response was received from the front end
    RpcResponse(JsonRpcResponse),

    /// A Comm was closed
    Closed(String),
}

/**
 * Enumeration of events that can be sent by the comm manager. These notify
 * other parts of the application that a comm was opened or closed, so that they
 * can update their state.
 */
pub enum CommShellEvent {
    /// A new comm was opened. The first value is the comm ID, and the second
    /// value is the comm name.
    Added(String, String),

    /// A comm was removed. The value is the comm ID.
    Removed(String),
}
