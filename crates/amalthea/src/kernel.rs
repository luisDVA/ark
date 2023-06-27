/*
 * kernel.rs
 *
 * Copyright (C) 2022 Posit Software, PBC. All rights reserved.
 *
 */

use std::sync::Arc;
use std::sync::Mutex;

use crossbeam::channel::bounded;
use crossbeam::channel::unbounded;
use crossbeam::channel::Receiver;
use crossbeam::channel::Select;
use crossbeam::channel::Sender;
use log::error;
use log::info;
use stdext::spawn;
use stdext::unwrap;

use crate::comm::comm_manager::CommManager;
use crate::comm::event::CommChanged;
use crate::comm::event::CommEvent;
use crate::connection_file::ConnectionFile;
use crate::error::Error;
use crate::language::control_handler::ControlHandler;
use crate::language::lsp_handler::LspHandler;
use crate::language::shell_handler::ShellHandler;
use crate::session::Session;
use crate::socket::control::Control;
use crate::socket::heartbeat::Heartbeat;
use crate::socket::iopub::IOPub;
use crate::socket::iopub::IOPubMessage;
use crate::socket::shell::Shell;
use crate::socket::socket::Socket;
use crate::socket::stdin::Stdin;
use crate::stream_capture::StreamCapture;
use crate::wire::header::JupyterHeader;
use crate::wire::input_request::ShellInputRequest;
use crate::wire::jupyter_message::Message;

/// A Kernel represents a unique Jupyter kernel session and is the host for all
/// execution and messaging threads.
pub struct Kernel {
    /// The name of the kernel.
    name: String,

    /// The connection metadata.
    connection: ConnectionFile,

    /// The unique session information for this kernel session.
    session: Session,

    /// Sends messages to the IOPub socket. This field is used throughout the
    /// kernel codebase to send events to the front end; use `create_iopub_tx`
    /// to access it.
    iopub_tx: Sender<IOPubMessage>,

    /// Receives message sent to the IOPub socket
    iopub_rx: Option<Receiver<IOPubMessage>>,

    /// The current message context; attached to outgoing messages to pair
    /// outputs with the message that caused them. Normally set and accessed
    /// by IOPub but can also be set by other threads such as StdIn.
    msg_context: Arc<Mutex<Option<JupyterHeader>>>,

    /// Sends notifications about comm changes and events to the comm manager.
    /// Use `create_comm_manager_tx` to access it.
    comm_manager_tx: Sender<CommEvent>,

    /// Receives notifications about comm changes and events
    comm_manager_rx: Receiver<CommEvent>,
}

/// Possible behaviors for the stream capture thread. When set to `Capture`,
/// the stream capture thread will capture all output to stdout and stderr.
/// When set to `None`, no stream output is captured.
#[derive(PartialEq)]
pub enum StreamBehavior {
    Capture,
    None,
}

impl Kernel {
    /// Create a new Kernel, given a connection file from a front end.
    pub fn new(name: &str, file: ConnectionFile) -> Result<Kernel, Error> {
        let key = file.key.clone();

        let (iopub_tx, iopub_rx) = bounded::<IOPubMessage>(10);

        // Create the pair of channels that will be used to relay messages from
        // the open comms
        let (comm_manager_tx, comm_manager_rx) = bounded::<CommEvent>(10);

        Ok(Self {
            name: name.to_string(),
            connection: file,
            session: Session::create(key)?,
            iopub_tx,
            iopub_rx: Some(iopub_rx),
            msg_context: Arc::new(Mutex::new(None)),
            comm_manager_tx,
            comm_manager_rx,
        })
    }

    /// Connects the Kernel to the front end
    pub fn connect(
        &mut self,
        shell_handler: Arc<Mutex<dyn ShellHandler>>,
        control_handler: Arc<Mutex<dyn ControlHandler>>,
        lsp_handler: Option<Arc<Mutex<dyn LspHandler>>>,
        stream_behavior: StreamBehavior,
        // Receiver channel for the stdin socket; when input is needed, the
        // language runtime can request it by sending an InputRequest to
        // this channel. The front end will prompt the user for input and
        // deliver it via the `handle_input_reply` method.
        // https://jupyter-client.readthedocs.io/en/stable/messaging.html#messages-on-the-stdin-router-dealer-channel
        input_request_rx: Receiver<ShellInputRequest>,
        conn_init_tx: Option<Sender<bool>>,
    ) -> Result<(), Error> {
        let ctx = zmq::Context::new();

        // Create the comm manager thread
        let iopub_tx = self.create_iopub_tx();
        let comm_manager_rx = self.comm_manager_rx.clone();
        let comm_changed_rx = CommManager::start(iopub_tx, comm_manager_rx);

        // Create the Shell ROUTER/DEALER socket and start a thread to listen
        // for client messages.
        let shell_socket = Socket::new(
            self.session.clone(),
            ctx.clone(),
            String::from("Shell"),
            zmq::ROUTER,
            None,
            self.connection.endpoint(self.connection.shell_port),
        )?;

        let shell_clone = shell_handler.clone();
        let iopub_tx_clone = self.create_iopub_tx();
        let comm_manager_tx_clone = self.comm_manager_tx.clone();
        let lsp_handler_clone = lsp_handler.clone();
        spawn!(format!("{}-shell", self.name), move || {
            Self::shell_thread(
                shell_socket,
                iopub_tx_clone,
                comm_manager_tx_clone,
                comm_changed_rx,
                shell_clone,
                lsp_handler_clone,
            )
        });

        // Create the IOPub PUB/SUB socket and start a thread to broadcast to
        // the client. IOPub only broadcasts messages, so it listens to other
        // threads on a Receiver<Message> instead of to the client.
        let iopub_socket = Socket::new(
            self.session.clone(),
            ctx.clone(),
            String::from("IOPub"),
            zmq::PUB,
            None,
            self.connection.endpoint(self.connection.iopub_port),
        )?;
        let iopub_rx = self.iopub_rx.take().unwrap();
        let msg_context = self.msg_context.clone();
        spawn!(format!("{}-iopub", self.name), move || {
            Self::iopub_thread(iopub_socket, iopub_rx, msg_context)
        });

        // Create the heartbeat socket and start a thread to listen for
        // heartbeat messages.
        let heartbeat_socket = Socket::new(
            self.session.clone(),
            ctx.clone(),
            String::from("Heartbeat"),
            zmq::REP,
            None,
            self.connection.endpoint(self.connection.hb_port),
        )?;
        spawn!(format!("{}-heartbeat", self.name), move || {
            Self::heartbeat_thread(heartbeat_socket)
        });

        // Create the stdin socket and start a thread to listen for stdin
        // messages. These are used by the kernel to request input from the
        // user, and so flow in the opposite direction to the other sockets.
        let stdin_socket = Socket::new(
            self.session.clone(),
            ctx.clone(),
            String::from("Stdin"),
            zmq::ROUTER,
            None,
            self.connection.endpoint(self.connection.stdin_port),
        )?;
        let shell_clone = shell_handler.clone();
        let msg_context = self.msg_context.clone();

        let (stdin_inbound_tx, stdin_inbound_rx) = unbounded();
        let (stdin_outbound_tx, stdin_outbound_rx) = unbounded();
        let stdin_session = stdin_socket.session.clone();

        spawn!(format!("{}-stdin", self.name), move || {
            Self::stdin_thread(
                stdin_inbound_rx,
                stdin_outbound_tx,
                shell_clone,
                msg_context,
                input_request_rx,
                stdin_session,
            )
        });

        // Create the thread that handles stdout and stderr, if requested
        if stream_behavior == StreamBehavior::Capture {
            let iopub_tx = self.create_iopub_tx();
            spawn!(format!("{}-output-capture", self.name), move || {
                Self::output_capture_thread(iopub_tx)
            });
        }

        // Create the Control ROUTER/DEALER socket
        let control_socket = Socket::new(
            self.session.clone(),
            ctx.clone(),
            String::from("Control"),
            zmq::ROUTER,
            None,
            self.connection.endpoint(self.connection.control_port),
        )?;

        // Internal sockets for notifying the 0MQ forwarding
        // thread that new outbound messages are available
        let outbound_notif_socket_tx = Socket::new_pair(
            self.session.clone(),
            ctx.clone(),
            String::from("OutboundNotifierTx"),
            None,
            String::from("inproc://outbound_notif"),
            true,
        )?;
        let outbound_notif_socket_rx = Socket::new_pair(
            self.session.clone(),
            ctx.clone(),
            String::from("OutboundNotifierRx"),
            None,
            String::from("inproc://outbound_notif"),
            false,
        )?;

        let stdin_outbound_rx_clone = stdin_outbound_rx.clone();

        // Forwarding thread that bridges 0MQ sockets and Amalthea
        // channels. Currently only used by StdIn.
        spawn!(format!("{}-zmq-forwarding", self.name), move || {
            Self::zmq_forwarding_thread(
                outbound_notif_socket_rx,
                stdin_socket,
                stdin_inbound_tx,
                stdin_outbound_rx_clone,
            )
        });

        // The notifier thread watches Amalthea channels of outgoing
        // messages for readiness. When a channel is hot, it notifies the
        // forwarding thread through a 0MQ socket.
        spawn!(format!("{}-zmq-notifier", self.name), move || {
            Self::zmq_notifier_thread(outbound_notif_socket_tx, vec![stdin_outbound_rx])
        });

        // 0MQ sockets are now initialised. We can start the kernel runtime
        // with relative multithreading safety. See
        // https://github.com/rstudio/positron/issues/720
        if let Some(tx) = conn_init_tx {
            tx.send(true).unwrap();
            drop(tx);
        }

        // TODO: thread/join thread? Exiting this thread will cause the whole
        // kernel to exit.
        Self::control_thread(control_socket, control_handler);
        info!("Control thread exited, exiting kernel");
        Ok(())
    }

    /// Returns a copy of the IOPub sending channel.
    pub fn create_iopub_tx(&self) -> Sender<IOPubMessage> {
        self.iopub_tx.clone()
    }

    /// Returns a copy of the comm manager sending channel.
    pub fn create_comm_manager_tx(&self) -> Sender<CommEvent> {
        self.comm_manager_tx.clone()
    }

    /// Starts the control thread
    fn control_thread(socket: Socket, handler: Arc<Mutex<dyn ControlHandler>>) {
        let control = Control::new(socket, handler);
        control.listen();
    }

    /// Starts the shell thread.
    fn shell_thread(
        socket: Socket,
        iopub_tx: Sender<IOPubMessage>,
        comm_manager_tx: Sender<CommEvent>,
        comm_changed_rx: Receiver<CommChanged>,
        shell_handler: Arc<Mutex<dyn ShellHandler>>,
        lsp_handler: Option<Arc<Mutex<dyn LspHandler>>>,
    ) -> Result<(), Error> {
        let mut shell = Shell::new(
            socket,
            iopub_tx.clone(),
            comm_manager_tx,
            comm_changed_rx,
            shell_handler,
            lsp_handler,
        );
        shell.listen();
        Ok(())
    }

    /// Starts the IOPub thread.
    fn iopub_thread(
        socket: Socket,
        receiver: Receiver<IOPubMessage>,
        msg_context: Arc<Mutex<Option<JupyterHeader>>>,
    ) -> Result<(), Error> {
        let mut iopub = IOPub::new(socket, receiver, msg_context);
        iopub.listen();
        Ok(())
    }

    /// Starts the heartbeat thread.
    fn heartbeat_thread(socket: Socket) -> Result<(), Error> {
        let heartbeat = Heartbeat::new(socket);
        heartbeat.listen();
        Ok(())
    }

    /// Starts the stdin thread.
    fn stdin_thread(
        stdin_inbound_rx: Receiver<Message>,
        stdin_outbound_tx: Sender<Message>,
        shell_handler: Arc<Mutex<dyn ShellHandler>>,
        msg_context: Arc<Mutex<Option<JupyterHeader>>>,
        input_request_rx: Receiver<ShellInputRequest>,
        session: Session,
    ) -> Result<(), Error> {
        let stdin = Stdin::new(
            stdin_inbound_rx,
            stdin_outbound_tx,
            shell_handler,
            msg_context,
            session,
        );
        stdin.listen(input_request_rx);
        Ok(())
    }

    /// Starts the thread that forwards 0MQ messages to Amalthea channels
    /// and vice versa.
    fn zmq_forwarding_thread(
        outbound_notif_socket: Socket,
        stdin_socket: Socket,
        stdin_inbound_tx: Sender<Message>,
        stdin_outbound_rx: Receiver<Message>,
    ) {
        let outbound_notif_poll_item = outbound_notif_socket.socket.as_poll_item(zmq::POLLIN);
        let stdin_poll_item = stdin_socket.socket.as_poll_item(zmq::POLLIN);

        let mut poll_items = vec![
            outbound_notif_socket.socket.as_poll_item(zmq::POLLIN),
            stdin_socket.socket.as_poll_item(zmq::POLLIN),
        ];

        let has_outbound = || -> bool {
            if outbound_notif_poll_item.is_readable() {
                // Consume notification
                let mut msg = zmq::Message::new();
                unwrap!(outbound_notif_socket.recv(&mut msg), Err(err) => {
                    log::warn!("Could not consume outbound notification socket: {}", err)
                });

                true
            } else {
                false
            }
        };

        let forward_outbound = || -> anyhow::Result<()> {
            let msg = stdin_outbound_rx.recv()?;
            msg.send(&stdin_socket)?;
            Ok(())
        };

        let forward_inbound = || -> anyhow::Result<()> {
            let msg = Message::read_from_socket(&stdin_socket)?;
            stdin_inbound_tx.send(msg)?;
            Ok(())
        };

        loop {
            let n = unwrap!(
                zmq::poll(&mut poll_items, -1),
                Err(err) => {
                    error!("While polling 0MQ items: {}", err);
                    0
                }
            );

            while n > 0 {
                if has_outbound() {
                    unwrap!(
                        forward_outbound(),
                        Err(err) => error!("While forwarding outbound message: {}", err)
                    );

                    let _ = --n;
                    continue;
                }

                if stdin_poll_item.is_readable() {
                    unwrap!(
                        forward_inbound(),
                        Err(err) => error!("While forwarding inbound message: {}", err)
                    );

                    let _ = --n;
                    continue;
                }
            }
        }
    }

    /// Starts the thread that notifies the forwarding thread that new
    /// outgoing messages have arrived from Amalthea.
    fn zmq_notifier_thread(notif_socket: Socket, watch_list: Vec<Receiver<Message>>) {
        let mut sel = Select::new();
        for rx in watch_list.iter() {
            sel.recv(rx);
        }

        loop {
            sel.ready();
            unwrap!(
                notif_socket.send(zmq::Message::new()),
                Err(err) => error!("Couldn't notify 0MQ thread: {}", err)
            );
        }
    }

    /// Starts the output capture thread.
    fn output_capture_thread(iopub_tx: Sender<IOPubMessage>) -> Result<(), Error> {
        let output_capture = StreamCapture::new(iopub_tx);
        output_capture.listen();
        Ok(())
    }
}
