/*
 * execute_reply.rs
 *
 * Copyright (C) 2022 by RStudio, PBC
 *
 */

use crate::wire::jupyter_message::MessageType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Represents a reply from an execute_request message
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteReply {
    /// The status of the request
    pub status: String,

    /// Monotonically increasing execution counter
    pub execution_count: u32,

    /// Results for user expressions
    user_expressions: Value,
}

impl MessageType for ExecuteReply {
    fn message_type() -> String {
        String::from("execute_request")
    }
}
