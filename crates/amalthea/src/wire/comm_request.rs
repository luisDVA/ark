/*
 * comm_request.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use serde::Deserialize;
use serde::Serialize;

use crate::wire::jupyter_message::MessageType;

/// Represents a request from the front end to show open comms
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommRequest {
    pub target_name: String,
}

impl MessageType for CommRequest {
    fn message_type() -> String {
        String::from("comm_request")
    }
}
