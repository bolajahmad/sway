//! The context or environment in which the language server functions.

use crate::{
    config::{Config, GarbageCollectionConfig, Warnings},
    core::{
        document::Documents,
        session::{self, Session},
    },
    error::{DirectoryError, DocumentError, LanguageServerError},
    utils::{debug, keyword_docs::KeywordDocs},
};
use crossbeam_channel::{Receiver, Sender};
use dashmap::{mapref::multiple::RefMulti, DashMap};
use forc_pkg::manifest::GenericManifestFile;
use forc_pkg::PackageManifestFile;
use lsp_types::{Diagnostic, Url};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{BTreeMap, VecDeque},
    process::Command,
};
use std::{
    mem,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use sway_core::LspConfig;
use tokio::sync::Notify;
use tower_lsp::{jsonrpc, Client};

const DEFAULT_SESSION_CACHE_CAPACITY: usize = 4;

/// `ServerState` is the primary mutable state of the language server
pub struct ServerState {
    pub(crate) client: Option<Client>,
    pub config: Arc<RwLock<Config>>,
    pub(crate) keyword_docs: Arc<KeywordDocs>,
    /// A Least Recently Used (LRU) cache of [Session]s, each representing a project opened in the user's workspace.
    /// This cache limits memory usage by maintaining a fixed number of active sessions, automatically
    /// evicting the least recently used sessions when the capacity is reached.
    pub(crate) sessions: LruSessionCache,
    pub documents: Documents,
    // Compilation thread related fields
    pub(crate) retrigger_compilation: Arc<AtomicBool>,
    pub is_compiling: Arc<AtomicBool>,
    pub(crate) cb_tx: Sender<TaskMessage>,
    pub(crate) cb_rx: Arc<Receiver<TaskMessage>>,
    pub(crate) finished_compilation: Arc<Notify>,
    last_compilation_state: Arc<RwLock<LastCompilationState>>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (cb_tx, cb_rx) = crossbeam_channel::bounded(1);
        let state = ServerState {
            client: None,
            config: Arc::new(RwLock::new(Config::default())),
            keyword_docs: Arc::new(KeywordDocs::new()),
            sessions: LruSessionCache::new(DEFAULT_SESSION_CACHE_CAPACITY),
            documents: Documents::new(),
            retrigger_compilation: Arc::new(AtomicBool::new(false)),
            is_compiling: Arc::new(AtomicBool::new(false)),
            cb_tx,
            cb_rx: Arc::new(cb_rx),
            finished_compilation: Arc::new(Notify::new()),
            last_compilation_state: Arc::new(RwLock::new(LastCompilationState::Uninitialized)),
        };
        // Spawn a new thread dedicated to handling compilation tasks
        state.spawn_compilation_thread();
        state
    }
}

/// `LastCompilationState` represents the state of the last compilation process.
/// It is primarily used for debugging purposes.
#[derive(Debug, PartialEq)]
enum LastCompilationState {
    Success,
    Failed,
    Uninitialized,
}

/// `TaskMessage` represents the set of messages or commands that can be sent to and processed by a worker thread in the compilation environment.
#[derive(Debug)]
pub enum TaskMessage {
    CompilationContext(CompilationContext),
    // A signal to the receiving thread to gracefully terminate its operation.
    Terminate,
}

/// `CompilationContext` encapsulates all the necessary details required by the compilation thread to execute a compilation process.
/// It acts as a container for shared resources and state information relevant to a specific compilation task.
#[derive(Debug, Default)]
pub struct CompilationContext {
    pub session: Option<Arc<Session>>,
    pub uri: Option<Url>,
    pub version: Option<i32>,
    pub optimized_build: bool,
    pub gc_options: GarbageCollectionConfig,
    pub file_versions: BTreeMap<PathBuf, Option<u64>>,
}

impl ServerState {
    pub fn new(client: Client) -> ServerState {
        ServerState {
            client: Some(client),
            ..Default::default()
        }
    }

    /// Spawns a new thread dedicated to handling compilation tasks. This thread listens for
    /// `TaskMessage` instances sent over a channel and processes them accordingly.
    ///
    /// This approach allows for asynchronous compilation tasks to be handled in parallel to
    /// the main application flow, improving efficiency and responsiveness.
    pub fn spawn_compilation_thread(&self) {
        let is_compiling = self.is_compiling.clone();
        let retrigger_compilation = self.retrigger_compilation.clone();
        let finished_compilation = self.finished_compilation.clone();
        let rx = self.cb_rx.clone();
        let last_compilation_state = self.last_compilation_state.clone();
        let experimental = sway_core::ExperimentalFlags {
            new_encoding: false,
        };
        std::thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    TaskMessage::CompilationContext(ctx) => {
                        let uri = ctx.uri.as_ref().unwrap().clone();
                        let session = ctx.session.as_ref().unwrap().clone();
                        let mut engines_clone = session.engines.read().clone();

                        if let Some(version) = ctx.version {
                            // Perform garbage collection at configured intervals if enabled to manage memory usage.
                            if ctx.gc_options.gc_enabled
                                && version % ctx.gc_options.gc_frequency == 0
                            {
                                // Call this on the engines clone so we don't clear types that are still in use
                                // and might be needed in the case cancel compilation was triggered.
                                if let Err(err) = session.garbage_collect(&mut engines_clone) {
                                    tracing::error!(
                                        "Unable to perform garbage collection: {}",
                                        err.to_string()
                                    );
                                }
                            }
                        }

                        let lsp_mode = Some(LspConfig {
                            optimized_build: ctx.optimized_build,
                            file_versions: ctx.file_versions,
                        });

                        // Set the is_compiling flag to true so that the wait_for_parsing function knows that we are compiling
                        is_compiling.store(true, Ordering::SeqCst);
                        match session::parse_project(
                            &uri,
                            &engines_clone,
                            Some(retrigger_compilation.clone()),
                            lsp_mode,
                            session.clone(),
                            experimental,
                        ) {
                            Ok(()) => {
                                let path = uri.to_file_path().unwrap();
                                // Find the module id from the path
                                match session::program_id_from_path(&path, &engines_clone) {
                                    Ok(program_id) => {
                                        // Use the module id to get the metrics for the module
                                        if let Some(metrics) = session.metrics.get(&program_id) {
                                            // It's very important to check if the workspace AST was reused to determine if we need to overwrite the engines.
                                            // Because the engines_clone has garbage collection applied. If the workspace AST was reused, we need to keep the old engines
                                            // as the engines_clone might have cleared some types that are still in use.
                                            if metrics.reused_programs == 0 {
                                                // The compiler did not reuse the workspace AST.
                                                // We need to overwrite the old engines with the engines clone.
                                                mem::swap(
                                                    &mut *session.engines.write(),
                                                    &mut engines_clone,
                                                );
                                            }
                                        }
                                        *last_compilation_state.write() =
                                            LastCompilationState::Success;
                                    }
                                    Err(err) => {
                                        tracing::error!("{}", err.to_string());
                                        *last_compilation_state.write() =
                                            LastCompilationState::Failed;
                                    }
                                }
                            }
                            Err(_err) => {
                                *last_compilation_state.write() = LastCompilationState::Failed;
                            }
                        }

                        // Reset the flags to false
                        is_compiling.store(false, Ordering::SeqCst);
                        retrigger_compilation.store(false, Ordering::SeqCst);

                        // Make sure there isn't any pending compilation work
                        if rx.is_empty() {
                            // finished compilation, notify waiters
                            finished_compilation.notify_waiters();
                        }
                    }
                    TaskMessage::Terminate => {
                        // If we receive a terminate message, we need to exit the thread
                        return;
                    }
                }
            }
        });
    }

    /// Spawns a new thread dedicated to checking if the client process is still active,
    /// and if not, shutting down the server.
    pub fn spawn_client_heartbeat(&self, client_pid: usize) {
        tokio::spawn(async move {
            loop {
                // Not using sysinfo here because it has compatibility issues with fuel.nix
                // https://github.com/FuelLabs/fuel.nix/issues/64
                let output = Command::new("ps")
                    .arg("-p")
                    .arg(client_pid.to_string())
                    .output()
                    .expect("Failed to execute ps command");

                if String::from_utf8_lossy(&output.stdout).contains(&format!("{client_pid} ")) {
                    tracing::trace!("Client Heartbeat: still running ({client_pid})");
                } else {
                    std::process::exit(0);
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    /// Waits asynchronously for the `is_compiling` flag to become false.
    ///
    /// This function checks the state of `is_compiling`, and if it's true,
    /// it awaits on a notification. Once notified, it checks again, repeating
    /// this process until `is_compiling` becomes false.
    pub async fn wait_for_parsing(&self) {
        loop {
            // Check both the is_compiling flag and the last_compilation_state.
            // Wait if is_compiling is true or if the last_compilation_state is Uninitialized.
            if !self.is_compiling.load(Ordering::SeqCst)
                && *self.last_compilation_state.read() != LastCompilationState::Uninitialized
            {
                // compilation is finished, lets check if there are pending compilation requests.
                if self.cb_rx.is_empty() {
                    // no pending compilation work, safe to break.
                    break;
                }
            }
            // We are still compiling, lets wait to be notified.
            self.finished_compilation.notified().await;
        }
    }

    pub fn shutdown_server(&self) -> jsonrpc::Result<()> {
        let _p = tracing::trace_span!("shutdown_server").entered();
        tracing::info!("Shutting Down the Sway Language Server");

        // Drain pending compilation requests
        while self.cb_rx.try_recv().is_ok() {}

        // Set the retrigger_compilation flag to true so that the compilation exits early
        self.retrigger_compilation.store(true, Ordering::SeqCst);

        // Send a terminate message to the compilation thread
        self.cb_tx
            .send(TaskMessage::Terminate)
            .expect("failed to send terminate message");

        let _ = self.sessions.iter().map(|item| {
            let session = item.value();
            session.shutdown();
        });
        Ok(())
    }

    pub(crate) async fn publish_diagnostics(
        &self,
        uri: Url,
        workspace_uri: Url,
        session: Arc<Session>,
    ) {
        let diagnostics = self.diagnostics(&uri, session.clone());
        // Note: Even if the computed diagnostics vec is empty, we still have to push the empty Vec
        // in order to clear former diagnostics. Newly pushed diagnostics always replace previously pushed diagnostics.
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(workspace_uri.clone(), diagnostics, None)
                .await;
        }
    }

    fn diagnostics(&self, uri: &Url, session: Arc<Session>) -> Vec<Diagnostic> {
        let mut diagnostics_to_publish = vec![];
        let config = &self.config.read();
        let tokens = session.token_map().tokens_for_file(uri);
        match config.debug.show_collected_tokens_as_warnings {
            // If collected_tokens_as_warnings is Parsed or Typed,
            // take over the normal error and warning display behavior
            // and instead show the either the parsed or typed tokens as warnings.
            // This is useful for debugging the lsp parser.
            Warnings::Parsed => {
                diagnostics_to_publish = debug::generate_warnings_for_parsed_tokens(tokens);
            }
            Warnings::Typed => {
                diagnostics_to_publish = debug::generate_warnings_for_typed_tokens(tokens);
            }
            Warnings::Default => {
                if let Some(diagnostics) =
                    session.diagnostics.read().get(&PathBuf::from(uri.path()))
                {
                    if config.diagnostic.show_warnings {
                        diagnostics_to_publish.extend(diagnostics.warnings.clone());
                    }
                    if config.diagnostic.show_errors {
                        diagnostics_to_publish.extend(diagnostics.errors.clone());
                    }
                }
            }
        }
        diagnostics_to_publish
    }

    async fn init_session(&self, uri: &Url) -> Result<(), LanguageServerError> {
        let session = Arc::new(Session::new());
        let project_name = session.init(uri, &self.documents).await?;
        self.sessions.insert(project_name, session);
        Ok(())
    }

    /// Constructs and returns a tuple of `(Url, Arc<Session>)` from a given workspace URI.
    /// The returned URL represents the temp directory workspace.
    pub(crate) async fn uri_and_session_from_workspace(
        &self,
        workspace_uri: &Url,
    ) -> Result<(Url, Arc<Session>), LanguageServerError> {
        let session = self.url_to_session(workspace_uri).await?;
        let uri = session.sync.workspace_to_temp_url(workspace_uri)?;
        Ok((uri, session))
    }

    async fn url_to_session(&self, uri: &Url) -> Result<Arc<Session>, LanguageServerError> {
        let path = PathBuf::from(uri.path());
        let manifest = PackageManifestFile::from_dir(&path).map_err(|_| {
            DocumentError::ManifestFileNotFound {
                dir: path.to_string_lossy().to_string(),
            }
        })?;

        // strip Forc.toml from the path to get the manifest directory
        let manifest_dir = manifest
            .path()
            .parent()
            .ok_or(DirectoryError::ManifestDirNotFound)?
            .to_path_buf();

        let session = self.sessions.get(&manifest_dir).unwrap_or({
            // If no session can be found, then we need to call init and insert a new session into the map
            self.init_session(uri).await?;
            self.sessions
                .get(&manifest_dir)
                .expect("no session found even though it was just inserted into the map")
        });
        Ok(session)
    }
}

/// A Least Recently Used (LRU) cache for storing and managing `Session` objects.
/// This cache helps limit memory usage by maintaining a fixed number of active sessions.
pub(crate) struct LruSessionCache {
    /// Stores the actual `Session` objects, keyed by their file paths.
    sessions: Arc<DashMap<PathBuf, Arc<Session>>>,
    /// Keeps track of the order in which sessions were accessed, with most recent at the front.
    usage_order: Arc<Mutex<VecDeque<PathBuf>>>,
    /// The maximum number of sessions that can be stored in the cache.
    capacity: usize,
}

impl LruSessionCache {
    /// Creates a new `LruSessionCache` with the specified capacity.
    pub(crate) fn new(capacity: usize) -> Self {
        LruSessionCache {
            sessions: Arc::new(DashMap::new()),
            usage_order: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = RefMulti<'_, PathBuf, Arc<Session>>> {
        self.sessions.iter()
    }

    /// Retrieves a session from the cache and updates its position to the front of the usage order.
    pub(crate) fn get(&self, path: &PathBuf) -> Option<Arc<Session>> {
        if let Some(session) = self.sessions.try_get(path).try_unwrap() {
            if self.sessions.len() >= self.capacity {
                self.move_to_front(path);
            }
            Some(session.clone())
        } else {
            None
        }
    }

    /// Inserts or updates a session in the cache.
    /// If at capacity and inserting a new session, evicts the least recently used one.
    /// For existing sessions, updates their position in the usage order if at capacity.
    pub(crate) fn insert(&self, path: PathBuf, session: Arc<Session>) {
        if self.sessions.get(&path).is_some() {
            tracing::trace!("Updating existing session for path: {:?}", path);
            // Session already exists, just update its position in the usage order if at capacity
            if self.sessions.len() >= self.capacity {
                self.move_to_front(&path);
            }
        } else {
            // New session
            tracing::trace!("Inserting new session for path: {:?}", path);
            if self.sessions.len() >= self.capacity {
                self.evict_least_used();
            }
            self.sessions.insert(path.clone(), session);
            let mut order = self.usage_order.lock();
            order.push_front(path);
        }
    }

    /// Moves the specified path to the front of the usage order, marking it as most recently used.
    fn move_to_front(&self, path: &PathBuf) {
        tracing::trace!("Moving path to front of usage order: {:?}", path);
        let mut order = self.usage_order.lock();
        if let Some(index) = order.iter().position(|p| p == path) {
            order.remove(index);
        }
        order.push_front(path.clone());
    }

    /// Removes the least recently used session from the cache when the capacity is reached.
    fn evict_least_used(&self) {
        let mut order = self.usage_order.lock();
        if let Some(old_path) = order.pop_back() {
            tracing::trace!(
                "Cache at capacity. Evicting least used session: {:?}",
                old_path
            );
            self.sessions.remove(&old_path);
        }
    }
}
