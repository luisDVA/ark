/*
 * stdin.rs
 *
 * Copyright (C) 2022 Posit Software, PBC. All rights reserved.
 *
 */

use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use crossbeam::select;
use log::error;
use log::trace;
use log::warn;
use serde_json::Value;

use crate::comm::frontend_comm::JsonRpcResponse;
use crate::session::Session;
use crate::wire::input_reply::InputReply;
use crate::wire::input_request::ShellInputRequest;
use crate::wire::jupyter_message::JupyterMessage;
use crate::wire::jupyter_message::Message;
use crate::wire::jupyter_message::OutboundMessage;

pub enum StdInRequest {
    InputRequest(ShellInputRequest),
    CommRequest(Sender<JsonRpcResponse>, Value),
}

pub struct Stdin {
    /// Receiver connected to the StdIn's ZeroMQ socket
    inbound_rx: Receiver<Message>,

    /// Sender connected to the StdIn's ZeroMQ socket
    outbound_tx: Sender<OutboundMessage>,

    // 0MQ session, needed to create `JupyterMessage` objects
    session: Session,
}

impl Stdin {
    /// Create a new Stdin socket
    ///
    /// * `inbound_rx` - Channel relaying replies from frontend
    /// * `outbound_tx` - Channel relaying requests to frontend
    /// * `session` - Juptyer session
    pub fn new(
        inbound_rx: Receiver<Message>,
        outbound_tx: Sender<OutboundMessage>,
        session: Session,
    ) -> Self {
        Self {
            inbound_rx,
            outbound_tx,
            session,
        }
    }

    /// Listens for messages on the stdin socket. This follows a simple loop:
    ///
    /// 1. Wait for
    pub fn listen(
        &self,
        stdin_request_rx: Receiver<StdInRequest>,
        input_reply_tx: Sender<InputReply>,
        interrupt_rx: Receiver<bool>,
    ) {
        loop {
            // Listen for input requests from the backend. We ignore
            // interrupt notifications here and loop infinitely over them.
            //
            // This could be simplified by having a mechanism for
            // subscribing and unsubscribing to a broadcasting channel. We
            // don't need to listen to interrupts at this stage so we'd
            // only subscribe after receiving an input request, and the
            // loop/select below could be removed.
            let req: StdInRequest;
            loop {
                select! {
                    recv(stdin_request_rx) -> msg => {
                        match msg {
                            Ok(m) => {
                                req = m;
                                break;
                            },
                            Err(err) => {
                                error!("Could not read input request: {}", err);
                                continue;
                            }
                        }
                    },
                    recv(interrupt_rx) -> _ => {
                        continue;
                    }
                };
            }

            let msg = match req {
                StdInRequest::InputRequest(req) => {
                    Message::InputRequest(JupyterMessage::create_with_identity(
                        req.originator,
                        req.request,
                        &self.session,
                    ))
                },
                StdInRequest::CommRequest(_response_tx, _value) => {
                    todo!()
                },
            };

            // Deliver the message to the front end
            if let Err(err) = self.outbound_tx.send(OutboundMessage::StdIn(msg)) {
                error!("Failed to send message to front end: {}", err);
            }
            trace!("Sent input request to front end, waiting for input reply...");

            // Wait for the front end's reply message from the ZeroMQ socket.
            let message = select! {
                recv(self.inbound_rx) -> msg => match msg {
                    Ok(m) => m,
                    Err(err) => {
                        error!("Could not read message from stdin socket: {}", err);
                        continue;
                    }
                },
                // Cancel current iteration if an interrupt is
                // signaled. We're no longer waiting for an `input_reply`
                // but for an `input_request`.
                recv(interrupt_rx) -> msg => {
                    if let Err(err) = msg {
                        error!("Could not read interrupt message: {}", err);
                    }
                    continue;
                }
            };

            // Only input replies are expected on this socket
            let reply = match message {
                Message::InputReply(reply) => reply,
                _ => {
                    warn!("Received unexpected message on stdin socket: {:?}", message);
                    continue;
                },
            };
            trace!("Received input reply from front-end: {:?}", reply);

            // Send it to the kernel implementation
            input_reply_tx.send(reply.content).unwrap();
        }
    }
}
