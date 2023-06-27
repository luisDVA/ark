/*
 * stdin.rs
 *
 * Copyright (C) 2022 Posit Software, PBC. All rights reserved.
 *
 */

use std::sync::Arc;
use std::sync::Mutex;

use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use futures::executor::block_on;
use log::trace;
use log::warn;

use crate::language::shell_handler::ShellHandler;
use crate::session::Session;
use crate::wire::header::JupyterHeader;
use crate::wire::input_request::ShellInputRequest;
use crate::wire::jupyter_message::JupyterMessage;
use crate::wire::jupyter_message::Message;
use crate::wire::originator::Originator;

pub struct Stdin {
    /// Receiver connected to the StdIn's ZeroMQ socket
    stdin_inbound_rx: Receiver<Message>,

    /// Sender connected to the StdIn's ZeroMQ socket
    stdin_outbound_tx: Sender<Message>,

    /// Language-provided shell handler object
    handler: Arc<Mutex<dyn ShellHandler>>,

    // IOPub message context. Updated from StdIn on input replies so that new
    // output gets attached to the correct input element in the console.
    msg_context: Arc<Mutex<Option<JupyterHeader>>>,

    // 0MQ session, needed to create `JupyterMessage` objects
    session: Session,
}

impl Stdin {
    /// Create a new Stdin socket
    ///
    /// * `socket` - The underlying ZeroMQ socket
    /// * `handler` - The language's shell handler
    /// * `msg_context` - The IOPub message context
    pub fn new(
        stdin_inbound_rx: Receiver<Message>,
        stdin_outbound_tx: Sender<Message>,
        handler: Arc<Mutex<dyn ShellHandler>>,
        msg_context: Arc<Mutex<Option<JupyterHeader>>>,
        session: Session,
    ) -> Self {
        Self {
            stdin_inbound_rx,
            stdin_outbound_tx,
            handler,
            msg_context,
            session,
        }
    }

    /// Listens for messages on the stdin socket. This follows a simple loop:
    ///
    /// 1. Wait for
    pub fn listen(&self, input_request_rx: Receiver<ShellInputRequest>) {
        // Listen for input requests from the back end
        loop {
            // Wait for a message (input request) from the back end
            let req = input_request_rx.recv().unwrap();

            if let None = req.originator {
                warn!("No originator for stdin request");
            }

            // Deliver the message to the front end
            let msg = Message::InputRequest(JupyterMessage::create_with_identity(
                req.originator,
                req.request,
                &self.session,
            ));

            if let Err(_) = self.stdin_outbound_tx.send(msg) {
                warn!("Failed to send message to front end");
            }
            trace!("Sent input request to front end, waiting for input reply...");

            // Wait for the front end's reply message from the ZeroMQ socket.
            // TODO: Wait for interrupts via another channel.
            let message = match self.stdin_inbound_rx.recv() {
                Ok(m) => m,
                Err(err) => {
                    warn!("Could not read message from stdin socket: {}", err);
                    continue;
                },
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

            // Update IOPub message context
            {
                let mut ctxt = self.msg_context.lock().unwrap();
                *ctxt = Some(reply.header.clone());
            }

            // Send the reply to the shell handler
            let handler = self.handler.lock().unwrap();
            let orig = Originator::from(&reply);
            if let Err(err) = block_on(handler.handle_input_reply(&reply.content, orig)) {
                warn!("Error handling input reply: {:?}", err);
            }
        }
    }
}
