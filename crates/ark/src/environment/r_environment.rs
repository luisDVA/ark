//
// r_environment.rs
//
// Copyright (C) 2023 by Posit Software, PBC
//
//
use std::thread;

use amalthea::comm::comm_channel::CommChannelMsg;
use crossbeam::channel::Select;
use crossbeam::channel::unbounded;
use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use harp::object::RObject;
use harp::r_lock;
use harp::r_symbol;
use harp::utils::r_assert_type;
use libR_sys::*;
use log::debug;
use log::error;
use log::warn;

use crate::environment::message::EnvironmentMessage;
use crate::environment::message::EnvironmentMessageError;
use crate::environment::message::EnvironmentMessageList;
use crate::environment::variable::EnvironmentVariable;
use crate::lsp::signals::SIGNALS;

struct Binding {
    name: SEXP,
    binding: SEXP
}

unsafe impl Send for Binding {}

/**
 * The R Environment handler provides the server side of Positron's Environment
 * panel, and is responsible for creating and updating the list of variables in
 * the R environment.
 */
pub struct REnvironment {
    pub channel_msg_rx: Receiver<CommChannelMsg>,

    pub frontend_msg_sender: Sender<CommChannelMsg>,

    pub env: RObject,

    current_bindings: Vec<Binding>

    // TODO:
    // - a version count
    // - some data to maintain state, e.g. a Map<string, SEXP>
}

impl REnvironment {
    /**
     * Creates a new REnvironment instance.
     *
     * - `env`: An R environment to scan for variables, typically R_GlobalEnv
     * - `frontend_msg_sender`: A channel used to send messages to the front end
     */
    pub fn new(env: RObject, frontend_msg_sender: Sender<CommChannelMsg>) -> Sender<CommChannelMsg> {
        let (channel_msg_tx, channel_msg_rx) = unbounded::<CommChannelMsg>();

        // Validate that the RObject we were passed is actually an environment
        unsafe {
            if let Err(err) = r_assert_type(env.sexp, &[ENVSXP]) {
                warn!(
                    "Environment: Attempt to monitor or list non-environment object {:?} ({:?})",
                    env, err
                );
            }
        };

        let environment = Self {
            channel_msg_rx,
            frontend_msg_sender,
            env,
            current_bindings: vec![]
        };

        // Start the execution thread and wait for requests from the front end
        thread::spawn(move || Self::execution_thread(environment));

        channel_msg_tx
    }

    pub fn execution_thread(
        mut self
    ) {
        let (prompt_signal_tx, prompt_signal_rx) = unbounded::<()>();

        // Register a handler for console prompt events
        let listen_id = SIGNALS.console_prompt.listen({
            move |_| {
                log::info!("Got console prompt signal.");
                prompt_signal_tx.send(()).unwrap();
            }
        });

        // Perform the initial environment scan and deliver to the front end
        self.refresh();

        // Flag initially set to false, but set to true if the user closes the
        // channel (i.e. the front end is closed)
        let mut user_initiated_close = false;

        // Main message processing loop; we wait here for messages from the
        // front end and loop as long as the channel is open
        loop {
            let mut sel = Select::new();

            // Listen to the comm
            sel.recv(&self.channel_msg_rx);

            // Listen to R events
            sel.recv(&prompt_signal_rx);

            // Wait until a message is received (blocking call)
            let oper = sel.select();

            if oper.index() == 1 {
                if let Ok(()) = oper.recv(&prompt_signal_rx) {
                    self.update();
                }

                continue;
            }

            let msg = match oper.recv(&self.channel_msg_rx) {
                Ok(msg) => msg,
                Err(e) => {
                    // We failed to receive a message from the front end. This
                    // is usually not a transient issue and indicates that the
                    // channel is closed, so allowing the thread to exit is
                    // appropriate. Retrying is likely to just lead to a busy
                    // loop.
                    error!(
                        "Environment: Error receiving message from front end: {:?}",
                        e
                    );

                    break;
                },
            };
            debug!("Environment: Received message from front end: {:?}", msg);

            // Break out of the loop if the front end has closed the channel
            if msg == CommChannelMsg::Close {
                debug!("Environment: Closing down after receiving comm_close from front end.");

                // Remember that the user initiated the close so that we can
                // avoid sending a duplicate close message from the back end
                user_initiated_close = true;
                break;
            }

            // Process ordinary data messages
            if let CommChannelMsg::Data(data) = msg {
                let message = match serde_json::from_value::<EnvironmentMessage>(data) {
                    Ok(m) => m,
                    Err(err) => {
                        error!(
                            "Environment: Received invalid message from front end. {}",
                            err
                        );
                        continue;
                    },
                };

                // Match on the type of data received.
                match message {
                    // This is a request to refresh the environment list, so
                    // perform a full environment scan and deliver to the
                    // front end
                    EnvironmentMessage::Refresh => {
                        self.refresh();
                    },

                    _ => {
                        error!(
                            "Environment: Don't know how to handle message type '{:?}'",
                            message
                        );
                    },
                }
            }
        }

        SIGNALS.console_prompt.remove(listen_id);

        if !user_initiated_close {
            // Send a close message to the front end if the front end didn't
            // initiate the close
            self.frontend_msg_sender.send(CommChannelMsg::Close).unwrap();
        }
    }

    /**
     * Perform a full environment scan and deliver the results to the front end.
     */
    fn refresh(&mut self) {
        self.current_bindings = self.bindings();

        let env_list = list_environment(&self.env);
        let data = serde_json::to_value(env_list);
        match data {
            Ok(data) => self.frontend_msg_sender
                .send(CommChannelMsg::Data(data))
                .unwrap(),
            Err(err) => {
                error!("Environment: Failed to serialize environment data: {}", err);
            },
        }
    }

    fn update(&mut self) {

        self.refresh();
    }

    fn bindings(&self) -> Vec<Binding> {
        unsafe {
            let mut bindings : Vec<Binding> = vec![];

            // 1: traverse the envinronment
            let env = *self.env;
            let hash  = HASHTAB(env);
            if hash == R_NilValue {
                Self::frame_bindings(FRAME(hash), &mut bindings);
            } else {
                let n = XLENGTH(hash);
                for i in 0..n {
                    Self::frame_bindings(VECTOR_ELT(hash, i), &mut bindings);
                }
            }

            // 2: sort by .name

            bindings.sort_by(|a, b| a.name.partial_cmp(&b.name).unwrap());
            bindings
        }
    }

    unsafe fn frame_bindings(mut frame: SEXP, bindings: &mut Vec<Binding> ) {
        while frame != R_NilValue {

            bindings.push(Binding{
                name: TAG(frame),

                // TODO: handle active bindings and promises ?
                binding: CAR(frame)
            });

            frame = CDR(frame);
        }
    }

    fn has_changed(&mut self) -> bool {
        let _new_bindings = self.bindings();
        let _old_bindings = &self.current_bindings;

        false
    }

}

/**
 * List the variables in the given R environment; returns a message that can be
 * sent to the front end, either containing the list of variables or an error
 * message.
 */
fn list_environment(env: &RObject) -> EnvironmentMessage {
    // Acquire the R lock to ensure we have exclusive access to the R global
    // environment while we're scanning it below.
    r_lock! {

        // List symbols in the environment.
        let symbols = R_lsInternal(env.sexp, 1);

        // Convert to a vector of strings.
        let strings = match RObject::new(symbols).to::<Vec<String>>() {
            Ok(v) => v,
            Err(e) => {
                return EnvironmentMessage::Error(EnvironmentMessageError {
                    message: format!("Error listing environment: {}", e),
                });
            },
        };

        // Convert each string to an EnvironmentVariable by looking up the value in
        // the global environment. (It would be more efficient, of course, to use
        // symbol vector directly, but this code is a placeholder.)
        let variables: Vec<EnvironmentVariable> = strings
            .iter()
            .map(|s| {
                let symbol = r_symbol!(s);
                let obj = RObject::view(Rf_findVarInFrame(env.sexp, symbol));
                EnvironmentVariable::new(s, obj)
            })
            .collect();

        // Form the response message.
        EnvironmentMessage::List(EnvironmentMessageList { variables })
    }
}
