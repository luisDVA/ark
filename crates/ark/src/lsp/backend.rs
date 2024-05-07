//
// backend.rs
//
// Copyright (C) 2022-2024 Posit Software, PBC. All rights reserved.
//
//

#![allow(deprecated)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use crossbeam::channel::Sender;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde_json::Value;
use stdext::result::ResultOrLog;
use stdext::*;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::channel as tokio_channel;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::GotoImplementationParams;
use tower_lsp::lsp_types::request::GotoImplementationResponse;
use tower_lsp::lsp_types::SelectionRange;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::LspService;
use tower_lsp::Server;
use tree_sitter::Point;

use crate::interface::RMain;
use crate::lsp::completions::provide_completions;
use crate::lsp::completions::resolve_completion;
use crate::lsp::definitions::goto_definition;
use crate::lsp::diagnostics;
use crate::lsp::document_context::DocumentContext;
use crate::lsp::documents::Document;
use crate::lsp::encoding::convert_position_to_point;
use crate::lsp::encoding::get_position_encoding_kind;
use crate::lsp::help_topic;
use crate::lsp::hover::hover;
use crate::lsp::indexer;
use crate::lsp::indexer::IndexerStateManager;
use crate::lsp::selection_range::convert_selection_range_from_tree_sitter_to_lsp;
use crate::lsp::selection_range::selection_range;
use crate::lsp::signature_help::signature_help;
use crate::lsp::statement_range;
use crate::lsp::symbols;
use crate::r_task;

type TokioReceiver<T> = tokio::sync::mpsc::Receiver<T>;
type TokioSender<T> = tokio::sync::mpsc::Sender<T>;

#[macro_export]
macro_rules! backend_trace {
    ($self: expr, $($rest: expr),*) => {{
        let message = format!($($rest, )*);
        $self.client.log_message(tower_lsp::lsp_types::MessageType::INFO, message).await
    }};
}

// The following synchronisation macros should be used at entry in all LSP
// methods. The LSP handlers are run in message order by tower-lsp but they run
// concurrently. This means that a single `.await`, for instance to log a
// message, may cause out of order handling. This is problematic for
// state-changing methods as these should only run after all other methods have
// finished or were cancelled (the latter would be preferred, see
// `ContentModified` error and this thread:
// https://github.com/microsoft/language-server-protocol/issues/584).
//
// To fix this, we now request an `RwLock` at entry in each handler.
// World-changing handlers require an exclusive lock whereas world-observing
// handlers require a non-exclusive shared lock. This should prevent handlers
// from operating on outdated documents with stale positions or ranges.

#[macro_export]
macro_rules! backend_read_method {
    ($self:expr, $($arg:tt)*) => {{
        let _guard = $self.lock.read().await;
        backend_trace!($self, $($arg)*);
    }};
}

#[macro_export]
macro_rules! backend_write_method {
    ($self:expr, $($arg:tt)*) => {{
        let _guard = $self.lock.write().await;
        backend_trace!($self, $($arg)*);
    }};
}

#[derive(Debug)]
pub struct Workspace {
    pub folders: Vec<Url>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            folders: Default::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Backend {
    pub lock: Arc<RwLock<()>>,
    sync_tx: TokioSender<HandlerSync>,
    pub client: Client,
    pub documents: Arc<DashMap<Url, Document>>,
    pub workspace: Arc<Mutex<Workspace>>,
    pub indexer_state_manager: IndexerStateManager,
}

impl Backend {
    pub fn with_document<T, F>(&self, path: &Path, mut callback: F) -> anyhow::Result<T>
    where
        F: FnMut(&Document) -> anyhow::Result<T>,
    {
        let mut fallback = || {
            let contents = std::fs::read_to_string(path)?;
            let document = Document::new(contents.as_str(), None);
            return callback(&document);
        };

        // If we have a cached copy of the document (because we're monitoring it)
        // then use that; otherwise, try to read the document from the provided
        // path and use that instead.
        let uri = unwrap!(Url::from_file_path(path), Err(_) => {
            log::info!("couldn't construct uri from {}; reading from disk instead", path.display());
            return fallback();
        });

        let document = unwrap!(self.documents.get(&uri), None => {
            log::info!("no document for uri {}; reading from disk instead", uri);
            return fallback();
        });

        return callback(document.value());
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        backend_write_method!(self, "initialize({:#?})", params);

        // initialize the set of known workspaces
        let mut workspace = self.workspace.lock();

        // initialize the workspace folders
        let mut folders: Vec<String> = Vec::new();
        if let Some(workspace_folders) = params.workspace_folders {
            for folder in workspace_folders.iter() {
                workspace.folders.push(folder.uri.clone());
                if let Ok(path) = folder.uri.to_file_path() {
                    if let Some(path) = path.to_str() {
                        folders.push(path.to_string());
                    }
                }
            }
        }

        // start indexing
        indexer::start(folders, self.indexer_state_manager.clone());

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "Amalthea R Kernel (ARK)".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                position_encoding: Some(get_position_encoding_kind()),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::from(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        "@".to_string(),
                        ":".to_string(),
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![
                        "(".to_string(),
                        ",".to_string(),
                        "=".to_string(),
                    ]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: None,
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![],
                    work_done_progress_options: Default::default(),
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, params: InitializedParams) {
        backend_read_method!(self, "initialized({:?})", params);
    }

    async fn shutdown(&self) -> Result<()> {
        backend_read_method!(self, "shutdown()");
        Ok(())
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        backend_write_method!(self, "did_change_workspace_folders({:?})", params);

        // TODO: Re-start indexer with new folders.
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        backend_write_method!(self, "did_change_configuration({:?})", params);
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        backend_write_method!(self, "did_change_watched_files({:?})", params);

        // TODO: Re-index the changed files.
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        backend_read_method!(self, "symbol({:?})", params);

        let response = unwrap!(symbols::symbols(self, &params), Err(error) => {
            log::error!("{:?}", error);
            return Ok(None);
        });

        Ok(Some(response))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        backend_read_method!(self, "document_symbols({})", params.text_document.uri);

        let response = unwrap!(symbols::document_symbols(self, &params), Err(error) => {
            log::error!("{:?}", error);
            return Ok(None);
        });

        Ok(Some(DocumentSymbolResponse::Nested(response)))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        backend_read_method!(self, "execute_command({:?})", params);

        match self.client.apply_edit(WorkspaceEdit::default()).await {
            Ok(res) if res.applied => self.client.log_message(MessageType::INFO, "applied").await,
            Ok(_) => self.client.log_message(MessageType::INFO, "rejected").await,
            Err(err) => self.client.log_message(MessageType::ERROR, err).await,
        }

        Ok(None)
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        backend_read_method!(self, "did_open({}", params.text_document.uri);

        let contents = params.text_document.text.as_str();
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        self.documents
            .insert(uri.clone(), Document::new(contents, Some(version)));

        diagnostics::refresh_diagnostics(self.clone(), uri.clone(), Some(version));
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        backend_write_method!(self, "did_change({:?})", params);

        // get reference to document
        let uri = &params.text_document.uri;
        let mut doc = unwrap!(self.documents.get_mut(uri), None => {
            backend_trace!(self, "did_change(): unexpected document uri '{}'", uri);
            return;
        });

        // respond to document updates
        let version = unwrap!(doc.on_did_change(&params), Err(error) => {
            backend_trace!(
                self,
                "did_change(): unexpected error applying updates {}",
                error
            );
            return;
        });

        // update index
        if let Ok(path) = uri.to_file_path() {
            let path = Path::new(&path);
            if let Err(error) = indexer::update(&doc, &path) {
                log::error!("{:?}", error);
            }
        }

        // publish diagnostics - but only publish them if the version of
        // the document now matches the version of the change after applying
        // it in `on_did_change()` (i.e. no changes left in the out of order queue)
        if params.text_document.version == version {
            diagnostics::refresh_diagnostics(self.clone(), uri.clone(), Some(version));
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        backend_read_method!(self, "did_save({:?}", params);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        backend_read_method!(self, "did_close({:?}", params);

        let uri = params.text_document.uri;

        diagnostics::clear_diagnostics(self.clone(), uri.clone(), None);

        match self.documents.remove(&uri) {
            Some(_) => {
                backend_trace!(self, "did_close(): closed document with URI: '{uri}'.");
            },
            None => {
                backend_trace!(
                    self,
                    "did_close(): failed to remove document with unknown URI: '{uri}'."
                );
            },
        };
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        backend_read_method!(self, "completion({:?})", params);

        // Get reference to document.
        let uri = &params.text_document_position.text_document.uri;
        let document = unwrap!(self.documents.get(uri), None => {
            backend_trace!(self, "completion(): No document associated with URI {}", uri);
            return Ok(None);
        });

        let position = params.text_document_position.position;
        let point = convert_position_to_point(&document.contents, position);

        let trigger = params.context.and_then(|ctxt| ctxt.trigger_character);

        // Build the document context.
        let context = DocumentContext::new(&document, point, trigger);
        log::info!("Completion context: {:#?}", context);

        let completions = r_task(|| provide_completions(&self, &context));

        let completions = unwrap!(completions, Err(err) => {
            backend_trace!(self, "completion(): Failed to provide completions: {err:?}.");
            return Ok(None)
        });

        if !completions.is_empty() {
            Ok(Some(CompletionResponse::Array(completions)))
        } else {
            Ok(None)
        }
    }

    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        backend_read_method!(self, "completion_resolve({:?})", item);

        // Try resolving the completion item
        let result = r_task(|| unsafe { resolve_completion(&mut item) });

        // Handle error case
        if let Err(err) = result {
            log::error!("Failed to resolve completion item due to: {err:?}.");
        }

        Ok(item)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        backend_read_method!(self, "hover({:?})", params);

        // get document reference
        let uri = &params.text_document_position_params.text_document.uri;
        let document = unwrap!(self.documents.get(uri), None => {
            backend_trace!(self, "hover(): No document associated with URI {}", uri);
            return Ok(None);
        });

        let position = params.text_document_position_params.position;
        let point = convert_position_to_point(&document.contents, position);

        // build document context
        let context = DocumentContext::new(&document, point, None);

        // request hover information
        let result = r_task(|| unsafe { hover(&context) });

        // unwrap errors
        let result = unwrap!(result, Err(error) => {
            log::error!("{:?}", error);
            return Ok(None);
        });

        // unwrap empty options
        let result = unwrap!(result, None => {
            return Ok(None);
        });

        // we got a result; use it
        Ok(Some(Hover {
            contents: HoverContents::Markup(result),
            range: None,
        }))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        backend_read_method!(self, "signature_help({params:?})");

        // get document reference
        let uri = &params.text_document_position_params.text_document.uri;
        let document = unwrap!(self.documents.get(uri), None => {
            backend_trace!(self, "signature_help(): No document associated with URI {}", uri);
            return Ok(None);
        });

        let position = params.text_document_position_params.position;
        let point = convert_position_to_point(&document.contents, position);

        let context = DocumentContext::new(&document, point, None);

        // request signature help
        let result = r_task(|| unsafe { signature_help(&context) });

        // unwrap errors
        let result = unwrap!(result, Err(error) => {
            log::error!("{:?}", error);
            return Ok(None);
        });

        // unwrap empty options
        let result = unwrap!(result, None => {
            return Ok(None);
        });

        Ok(Some(result))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        backend_read_method!(self, "goto_definition({params:?})");

        // get reference to document
        let uri = &params.text_document_position_params.text_document.uri;
        let document = unwrap!(self.documents.get(uri), None => {
            backend_trace!(self, "completion(): No document associated with URI {}", uri);
            return Ok(None);
        });

        // build goto definition context
        let result = unwrap!(unsafe { goto_definition(&document, params) }, Err(error) => {
            log::error!("{}", error);
            return Ok(None);
        });

        Ok(result)
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        backend_read_method!(self, "goto_implementation({params:?})");
        let _ = params;
        log::error!("Got a textDocument/implementation request, but it is not implemented");
        return Ok(None);
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        backend_read_method!(self, "selection_range({params:?})");

        // Get reference to document
        let uri = &params.text_document.uri;
        let document = unwrap!(self.documents.get(uri), None => {
            backend_trace!(self, "completion(): No document associated with URI {}", uri);
            return Ok(None);
        });

        let tree = &document.ast;

        // Get tree-sitter points to return selection ranges for
        let points: Vec<Point> = params
            .positions
            .into_iter()
            .map(|position| convert_position_to_point(&document.contents, position))
            .collect();

        let Some(selections) = selection_range(tree, points) else {
            return Ok(None);
        };

        // Convert tree-sitter points to LSP positions everywhere
        let selections = selections
            .into_iter()
            .map(|selection| convert_selection_range_from_tree_sitter_to_lsp(selection, &document))
            .collect();

        Ok(Some(selections))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        backend_read_method!(self, "references({params:?})");

        let locations = match self.find_references(params) {
            Ok(locations) => locations,
            Err(_error) => {
                return Ok(None);
            },
        };

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }
}

// Custom methods for the backend.
//
// NOTE: Request / notification methods _must_ accept a params object,
// even for notifications that don't include any auxiliary data.
//
// I'm not positive, but I think this is related to the way VSCode
// serializes parameters for notifications / requests when no data
// is supplied. Instead of supplying "nothing", it supplies something
// like `[null]` which tower_lsp seems to quietly reject when attempting
// to invoke the registered method.
//
// See also:
//
// https://github.com/Microsoft/vscode-languageserver-node/blob/18fad46b0e8085bb72e1b76f9ea23a379569231a/client/src/common/client.ts#L802-L838
// https://github.com/Microsoft/vscode-languageserver-node/blob/18fad46b0e8085bb72e1b76f9ea23a379569231a/client/src/common/client.ts#L701-L752
impl Backend {
    async fn notification(&self, params: Option<Value>) {
        backend_read_method!(self, "notification({params:?})");
        log::info!("Received Positron notification: {:?}", params);
    }
}

struct HandlerSync {
    exclusive: bool,
    status_tx: TokioSender<()>,
}

// enum HandlerStatus {
//     /// Handler blocks on entry until it is sent this status
//     Started,
//     /// Handler blocks on exit until it is sent this status
//     Finished,
// }

pub fn start_lsp(runtime: Arc<Runtime>, address: String, conn_init_tx: Sender<bool>) {
    runtime.block_on(async {
        #[cfg(feature = "runtime-agnostic")]
        use tokio_util::compat::TokioAsyncReadCompatExt;
        #[cfg(feature = "runtime-agnostic")]
        use tokio_util::compat::TokioAsyncWriteCompatExt;

        log::trace!("Connecting to LSP at '{}'", &address);
        let listener = TcpListener::bind(&address).await.unwrap();

        // Notify frontend that we are ready to accept connections
        conn_init_tx
            .send(true)
            .or_log_warning("Couldn't send LSP server init notification");

        let (stream, _) = listener.accept().await.unwrap();
        log::trace!("Connected to LSP at '{}'", address);
        let (read, write) = tokio::io::split(stream);

        #[cfg(feature = "runtime-agnostic")]
        let (read, write) = (read.compat(), write.compat_write());

        let init = |client: Client| {
            // Create task with a channel. Handlers send a receiver channel to
            // it, that will block until all previous handlers have finished
            // running or the handling is cancelled.
            //
            // The channel blocks while a mut handler is running.
            let (sync_tx, mut sync_rx) = tokio_channel::<HandlerSync>(1);

            tokio::spawn(async move {
                let mut pending: VecDeque<TokioSender<()>> = VecDeque::new();

                loop {
                    let maybe_finish_current = || async {
                        if let Some(status_tx) = pending.front() {
                            let _res = status_tx.send(()).await;
                        } else {
                            // Wait for a handler to arrive
                            std::future::pending::<()>().await;
                        }
                    };

                    tokio::select! {
                        _ = maybe_finish_current() => {
                            pending.pop_front();
                        },
                        handler = sync_rx.recv() => {
                            let handler = handler.unwrap();

                            // If this handler requires exclusive access to the
                            // LSP, typically because it's handling a
                            // notification that changes the state of the world,
                            // we first flush all pending handlers before moving on.
                            if handler.exclusive {
                                while let Some(status_tx) = pending.pop_front() {
                                    // We could send a cancellation notification
                                    // at this point to speed things up
                                    let _res = status_tx.send(()).await;
                                }

                                // Now wait until the exclusive handler is finished and unblock it
                                let _res = handler.status_tx.send(()).await;
                                continue;
                            }

                            // The handler has now started running, queue it up for completion
                            pending.push_back(handler.status_tx)
                        },
                    }
                }
            });

            // Create backend.
            // Note that DashMap uses synchronization primitives internally, so we
            // don't guard access to the map via a mutex.
            let backend = Backend {
                lock: Arc::new(RwLock::new(())),
                client,
                documents: Arc::new(DashMap::new()),
                workspace: Arc::new(Mutex::new(Workspace::default())),
                indexer_state_manager: IndexerStateManager::new(),
                sync_tx,
            };

            // Forward `backend` along to `RMain`.
            // This also updates an outdated `backend` after a reconnect.
            // `RMain` should be initialized by now, since the caller of this
            // function waits to receive the init notification sent on
            // `kernel_init_rx`. Even if it isn't, this should be okay because
            // `r_task()` defensively blocks until its sender is initialized.
            r_task({
                let backend = backend.clone();
                move || {
                    let main = RMain::get_mut();
                    main.set_lsp_backend(backend);
                }
            });

            backend
        };

        let (service, socket) = LspService::build(init)
            .custom_method(
                statement_range::POSITRON_STATEMENT_RANGE_REQUEST,
                Backend::statement_range,
            )
            .custom_method(help_topic::POSITRON_HELP_TOPIC_REQUEST, Backend::help_topic)
            .custom_method("positron/notification", Backend::notification)
            .finish();

        let server = Server::new(read, write, socket);
        server.serve(service).await;

        log::trace!(
            "LSP thread exiting gracefully after connection closed ({:?}).",
            address
        );
    })
}
