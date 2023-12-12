/*
 * comm_manager.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use std::collections::HashMap;

use crossbeam::channel::Receiver;
use crossbeam::channel::Select;
use crossbeam::channel::Sender;
use log::info;
use log::warn;
use stdext::spawn;

use crate::comm::comm_channel::CommMsg;
use crate::comm::event::CommManagerEvent;
use crate::comm::event::CommShellEvent;
use crate::socket::comm::CommInitiator;
use crate::socket::comm::CommSocket;
use crate::socket::iopub::IOPubMessage;
use crate::wire::comm_msg::CommWireMsg;
use crate::wire::comm_open::CommOpen;
use crate::wire::header::JupyterHeader;

pub struct CommManager {
    open_comms: Vec<CommSocket>,
    iopub_tx: Sender<IOPubMessage>,
    comm_event_rx: Receiver<CommManagerEvent>,
    comm_shell_tx: Sender<CommShellEvent>,
    pending_rpcs: HashMap<String, JupyterHeader>,
}

impl CommManager {
    /**
     * The comm manager is responsible for listening for messages on all of the
     * open comms, attaching appropriate metadata, and relaying them to the front
     * end. It is meant to be called on a dedicated thread, and it does not return.
     *
     * - `iopub_tx`: The channel to send messages to the front end.
     * - `comm_event_rx`: The channel to receive messages about changes to the set
     *   (or state) of open comms.
     */
    pub fn start(
        iopub_tx: Sender<IOPubMessage>,
        comm_event_rx: Receiver<CommManagerEvent>,
    ) -> Receiver<CommShellEvent> {
        let (comm_changed_tx, comm_changed_rx) = crossbeam::channel::unbounded();
        spawn!("comm-manager", move || {
            let mut comm_manager = CommManager::new(iopub_tx, comm_event_rx, comm_changed_tx);
            loop {
                comm_manager.execution_thread();
            }
        });
        return comm_changed_rx;
    }

    /**
     * Create a new CommManager.
     */
    pub fn new(
        iopub_tx: Sender<IOPubMessage>,
        comm_event_rx: Receiver<CommManagerEvent>,
        comm_shell_tx: Sender<CommShellEvent>,
    ) -> Self {
        Self {
            iopub_tx,
            comm_event_rx,
            comm_shell_tx,
            open_comms: Vec::<CommSocket>::new(),
            pending_rpcs: HashMap::<String, JupyterHeader>::new(),
        }
    }

    /**
     * The main execution thread for the comm manager; listens for comm events
     * and dispatches them accordingly. Blocks until a message is received;
     * intended to be called in a loop.
     */
    pub fn execution_thread(&mut self) {
        let mut sel = Select::new();

        // Listen for messages from each of the open comms that are destined for
        // the front end
        for comm_socket in &self.open_comms {
            sel.recv(&comm_socket.outgoing_rx);
        }

        // Add a receiver for the comm_event channel; this is used to
        // unblock the select when a comm is added or removed so we can
        // start a new `Select` with the updated set of open comms.
        sel.recv(&self.comm_event_rx);

        // Wait until a message is received (blocking call)
        let oper = sel.select();

        // Look up the index in the set of open comms
        let index = oper.index();
        if index >= self.open_comms.len() {
            // If the index is greater than the number of open comms,
            // then the message was received on the comm_event channel.
            let comm_event = oper.recv(&self.comm_event_rx);
            if let Err(err) = comm_event {
                warn!("Error receiving comm_event message: {}", err);
                return;
            }
            match comm_event.unwrap() {
                // A Comm was opened; notify everyone
                CommManagerEvent::Opened(comm_socket, val) => {
                    // Notify the shell handler; it maintains a list of open
                    // comms so that the frontend can query for comm state
                    self.comm_shell_tx
                        .send(CommShellEvent::Added(
                            comm_socket.comm_id.clone(),
                            comm_socket.comm_name.clone(),
                        ))
                        .unwrap();

                    // Notify the front end, if this request originated from the back end
                    if comm_socket.initiator == CommInitiator::BackEnd {
                        self.iopub_tx
                            .send(IOPubMessage::CommOpen(CommOpen {
                                comm_id: comm_socket.comm_id.clone(),
                                target_name: comm_socket.comm_name.clone(),
                                data: val,
                            }))
                            .unwrap();
                    }

                    // Add to our own list of open comms
                    self.open_comms.push(comm_socket);

                    info!(
                        "Comm channel opened; there are now {} open comms",
                        self.open_comms.len()
                    );
                },

                // An RPC was received; add it to the map of pending RPCs
                CommManagerEvent::PendingRpc(header) => {
                    self.pending_rpcs.insert(header.msg_id.clone(), header);
                },

                // A message was received from the front end
                CommManagerEvent::Message(comm_id, msg) => {
                    // Find the index of the comm in the vector
                    let index = self
                        .open_comms
                        .iter()
                        .position(|comm_socket| comm_socket.comm_id == comm_id);

                    // If we found it, send the message to the comm. TODO: Fewer unwraps
                    if let Some(index) = index {
                        self.open_comms
                            .get(index)
                            .unwrap()
                            .incoming_tx
                            .send(msg)
                            .unwrap();
                    } else {
                        warn!(
                            "Received message for unknown comm channel {}: {:?}",
                            comm_id, msg
                        );
                    }
                },

                // A Comm was closed; attempt to remove it from the set of open comms
                CommManagerEvent::Closed(comm_id) => {
                    // Find the index of the comm in the vector
                    let index = self
                        .open_comms
                        .iter()
                        .position(|comm_socket| comm_socket.comm_id == comm_id);

                    // If we found it, remove it.
                    if let Some(index) = index {
                        self.open_comms.remove(index);
                        self.comm_shell_tx
                            .send(CommShellEvent::Removed(comm_id))
                            .unwrap();
                        info!(
                            "Comm channel closed; there are now {} open comms",
                            self.open_comms.len()
                        );
                    } else {
                        warn!(
                            "Received close message for unknown comm channel {}",
                            comm_id
                        );
                    }
                },
            }
        } else {
            // Otherwise, the message was received on one of the open comms.
            let comm_socket = &self.open_comms[index];
            let comm_msg = match oper.recv(&comm_socket.outgoing_rx) {
                Ok(msg) => msg,
                Err(err) => {
                    warn!("Error receiving comm message: {}", err);
                    return;
                },
            };

            // Amend the message with the comm's ID, convert it to an
            // IOPub message, and send it to the front end
            let msg = match comm_msg {
                // The comm is emitting data to the front end without being
                // asked; this is treated like an event.
                CommMsg::Data(data) => IOPubMessage::CommMsgEvent(CommWireMsg {
                    comm_id: comm_socket.comm_id.clone(),
                    data,
                }),

                // The comm is replying to a message from the front end; the
                // first parameter names the ID of the message to which this is
                // a reply.
                CommMsg::Rpc(string, data) => {
                    // Create the payload to send to the front end
                    let payload = CommWireMsg {
                        comm_id: comm_socket.comm_id.clone(),
                        data,
                    };

                    // Try to find the message ID in the map of pending RPCs.
                    match self.pending_rpcs.remove(&string) {
                        Some(header) => {
                            // Found it; consume the pending RPC and convert the
                            // message to a reply.
                            IOPubMessage::CommMsgReply(header, payload)
                        },
                        None => {
                            // If this is not a known request from the frontend this means this is a
                            // new request _to_ the frontend
                            IOPubMessage::CommMsgRequest(payload)
                        },
                    }
                },
                CommMsg::Close => IOPubMessage::CommClose(comm_socket.comm_id.clone()),
            };

            // Deliver the message to the front end
            self.iopub_tx.send(msg).unwrap();
        }
    }
}
