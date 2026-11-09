//! The main loop (A §3.2, D-1377).
//!
//! The loop owns all server state and never parses or checks. It reads the
//! client's messages, the analysis thread's events and a debounce timer,
//! and after each one starts a job if the analysis thread is free and a
//! package is due.
//!
//! The handshake and shutdown are this module's own, not `lsp-server`'s
//! helpers, which fail on legal messages or unwrap a send (D-1377): before
//! `initialize`, a request is answered `ServerNotInitialized` and a
//! notification other than `exit` is dropped. After `shutdown`, every
//! request is answered `InvalidRequest`. `exit` ends the loop with status
//! 0 after `shutdown` and 1 before it; the end of input ends it the same
//! way. So does a frame whose body is not a JSON-RPC message, since it
//! ends `lsp-server`'s reader (D-1430).
//!
//! A request the loop does not answer itself has a route in `features`:
//! a text request runs at once on the query pool, and a snapshot request
//! when its package's newest snapshot holds the text it names (D-1367,
//! `schedule`, `pool`). Handlers never write to the connection: the loop
//! applies their effects, then sends their response.
//!
//! Nothing below this module holds a generation handle: only `features`
//! may (D-1366). The crate's `clippy.toml` enforces it.

mod caps;
mod docs;
mod effects;
mod pool;
mod publish;
#[cfg(test)]
mod request_tests;
mod routes;
mod schedule;
#[cfg(test)]
mod session_tests;
mod sessions;
mod settings;
#[cfg(test)]
mod single_file_tests;
mod state;
#[cfg(test)]
mod tests;
mod watch;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use cheby_driver::analysis::Globals;
use crossbeam_channel::{Receiver, Sender, never, select};
use lsp_server::{Connection, Message, Notification, ReqQueue, Request, RequestId, Response};
use lsp_types::{InitializeResult, MessageType, ServerInfo, Uri};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(crate) use self::caps::ClientCaps;
use self::caps::server_capabilities;
pub(crate) use self::effects::Effect;
use self::pool::{QueryDone, QueryPool};
use self::schedule::{JobTexts, Parked, Served};
pub(crate) use self::settings::Settings;
use self::state::{Doc, Once, Package};
use self::watch::Watch;
use crate::ServerOptions;
use crate::analysis_thread::{self, Event, Work};
use crate::features::stdlib::StdService;
use crate::features::{CancelToken, LiveText};
use crate::text::lines::PositionEncoding;
use crate::text::uri::{UriMap, normalize, uri_to_path};

/// JSON-RPC and LSP error codes, as numbers: `lsp_server::ErrorCode` is
/// an enum whose numbers only an `as` gives.
pub(crate) mod codes {
    /// `InvalidRequest`.
    pub(crate) const INVALID_REQUEST: i32 = -32600;
    /// `MethodNotFound`.
    pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
    /// `InvalidParams`.
    pub(crate) const INVALID_PARAMS: i32 = -32602;
    /// `InternalError`: a handler panicked (A §3.7).
    pub(crate) const INTERNAL_ERROR: i32 = -32603;
    /// `ServerNotInitialized`.
    pub(crate) const SERVER_NOT_INITIALIZED: i32 = -32002;
    /// `RequestCancelled` (A §3.5).
    pub(crate) const REQUEST_CANCELLED: i32 = -32800;
    /// `ContentModified` (D-1367).
    pub(crate) const CONTENT_MODIFIED: i32 = -32801;
    /// `RequestFailed`: also a wait past `analysis.requestWaitMs` (D-1367).
    pub(crate) const REQUEST_FAILED: i32 = -32803;
}

/// The manifest's file name.
const MANIFEST: &str = "cheby.toml";

/// What an answer to one of the server's own requests is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outgoing {
    /// `workspace/configuration`: the `cheby` settings.
    Configuration,
    /// An answer nothing reads, such as to `client/registerCapability`.
    Ignore,
}

/// How the loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ended {
    /// The process's status: 0 after `shutdown`, else 1.
    pub code: u8,
    /// The client sent `exit` or closed its input, so the reader thread
    /// has ended and the I/O threads can be joined. False when the client
    /// stopped reading, since the reader may still block on input.
    pub joinable: bool,
}

impl Ended {
    /// The status as an [`ExitCode`].
    pub(crate) fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

/// The settings layers of plan §1.9, in order after the defaults.
#[derive(Debug, Default)]
struct Layers {
    /// `initializationOptions`.
    init: Value,
    /// The latest configuration that was not `null`.
    config: Value,
}

/// The server: its connection, its state and its analysis thread.
pub(crate) struct Server {
    connection: Connection,
    queue: ReqQueue<(), Outgoing>,
    jobs: Sender<Work>,
    events: Receiver<Event>,
    options: ServerOptions,
    /// The package whose job is on the analysis thread.
    running: Option<PathBuf>,
    /// `shutdown` was answered.
    shutting_down: bool,
    /// The client stopped reading.
    closed: bool,
    caps: ClientCaps,
    encoding: PositionEncoding,
    settings: Arc<Settings>,
    layers: Layers,
    /// Settings warnings already logged.
    warned: Once,
    /// The workspace folders, normalized paths. They only seed sessions:
    /// a document routes by its own path (A §6.5).
    folders: Vec<PathBuf>,
    /// Where std's sources may be materialized, for routing (A §6.3).
    std_dirs: Vec<PathBuf>,
    /// The std base and std's sources, for requests.
    std: Arc<StdService>,
    /// File watchers and sweeps (A §8).
    watch: Watch,
    /// Open documents by normalized path.
    docs: BTreeMap<PathBuf, Doc>,
    /// The client's spelling of each open document's URI.
    uris: UriMap,
    /// Packages by root.
    packages: BTreeMap<PathBuf, Package>,
    /// The order of the latest edit.
    stamp: u64,
    /// The query threads.
    pool: QueryPool,
    /// Their finished requests.
    query_done: Receiver<QueryDone>,
    /// Requests waiting for an analysis of their text (A §4).
    parked: Vec<Parked>,
    /// Requests on the query pool, with their cancel tokens.
    in_pool: BTreeMap<RequestId, CancelToken>,
    /// Each package's newest snapshot as requests read it.
    served: BTreeMap<PathBuf, Served>,
    /// The texts of the job on the analysis thread.
    in_flight: Option<JobTexts>,
    /// The live text of open documents that text requests read, with the
    /// stamp it was made at.
    texts: BTreeMap<PathBuf, (u64, Arc<LiveText>)>,
    /// What tests switch on through `cheby/testHold`, `cheby/testPanicRun`
    /// and `cheby/testNoDebounce`.
    #[cfg(test)]
    test: TestSwitches,
}

/// The server's switches that only tests set.
#[cfg(test)]
#[derive(Debug, Default)]
struct TestSwitches {
    /// The analysis thread is held: no job starts.
    hold: bool,
    /// The next job's Q run panics after it ran (R-14).
    panic_next: bool,
    /// An edit's debounce never ends, so only a flush starts a run.
    no_debounce: bool,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("options", &self.options)
            .field("running", &self.running)
            .field("shutting_down", &self.shutting_down)
            .field("settings", &self.settings)
            .field("docs", &self.docs.keys().collect::<Vec<_>>())
            .field("packages", &self.packages.keys().collect::<Vec<_>>())
            .field("parked", &self.parked.len())
            .field("in_pool", &self.in_pool.len())
            .finish_non_exhaustive()
    }
}

/// Serves `connection` until `exit`, the end of input, or a client that
/// stops reading.
///
/// # Errors
///
/// The analysis thread could not be started.
pub(crate) fn serve(connection: Connection, options: ServerOptions) -> Result<Ended, String> {
    let Some((id, params)) = handshake_start(&connection) else {
        return Ok(Ended {
            code: 1,
            joinable: true,
        });
    };
    let (jobs, events) = analysis_thread::spawn(Arc::new(Globals::new()))
        .map_err(|e| format!("cannot start the analysis thread: {e}"))?;
    let (pool, query_done) =
        QueryPool::spawn().map_err(|e| format!("cannot start the query threads: {e}"))?;
    let watch = Watch::new(&params).map_err(|e| format!("cannot start the sweep thread: {e}"))?;
    let std_dirs = sessions::std_dirs(options.build);
    let std = Arc::new(StdService::new(options.build));
    let caps = ClientCaps::from_params(&params);
    let encoding = caps.encoding();
    let layers = Layers {
        init: params
            .get("initializationOptions")
            .cloned()
            .unwrap_or_default(),
        config: Value::Null,
    };
    let mut server = Server {
        connection,
        queue: ReqQueue::default(),
        jobs,
        events,
        options,
        running: None,
        shutting_down: false,
        closed: false,
        caps,
        encoding,
        settings: Arc::new(Settings::default()),
        layers,
        warned: Once::default(),
        folders: folders(&params),
        std_dirs,
        std,
        watch,
        docs: BTreeMap::new(),
        uris: UriMap::new(),
        packages: BTreeMap::new(),
        stamp: 0,
        pool,
        query_done,
        parked: Vec::new(),
        in_pool: BTreeMap::new(),
        served: BTreeMap::new(),
        in_flight: None,
        texts: BTreeMap::new(),
        #[cfg(test)]
        test: TestSwitches::default(),
    };
    server.initialize(id);
    Ok(server.main_loop())
}

/// Waits for `initialize`. A request before it is answered
/// `ServerNotInitialized`, and a notification before it is dropped.
/// `None` when the client sends `exit` or ends its input first.
fn handshake_start(connection: &Connection) -> Option<(RequestId, Value)> {
    loop {
        match connection.receiver.recv().ok()? {
            Message::Request(request) if request.method == "initialize" => {
                return Some((request.id, request.params));
            }
            Message::Request(request) => {
                let response = Response::new_err(
                    request.id,
                    codes::SERVER_NOT_INITIALIZED,
                    format!("`{}` before `initialize`", request.method),
                );
                connection.sender.send(response.into()).ok()?;
            }
            Message::Notification(notification) if notification.method == "exit" => return None,
            Message::Notification(_) | Message::Response(_) => {}
        }
    }
}

/// The workspace folders of `initialize`: `workspaceFolders`, else
/// `rootUri`, else `rootPath`.
fn folders(params: &Value) -> Vec<PathBuf> {
    let uri_path = |value: &Value| {
        let uri: Uri = value.as_str()?.parse().ok()?;
        Some(normalize(&uri_to_path(&uri)?))
    };
    if let Some(list) = params.get("workspaceFolders").and_then(Value::as_array) {
        return list
            .iter()
            .filter_map(|folder| uri_path(folder.get("uri")?))
            .collect();
    }
    if let Some(path) = params.get("rootUri").and_then(uri_path) {
        return vec![path];
    }
    params
        .get("rootPath")
        .and_then(Value::as_str)
        .map(|path| vec![normalize(std::path::Path::new(path))])
        .unwrap_or_default()
}

impl Server {
    /// Answers `initialize` and reads the first settings layer.
    fn initialize(&mut self, id: RequestId) {
        let result = InitializeResult {
            capabilities: server_capabilities(self.encoding),
            server_info: Some(ServerInfo {
                name: "cheby".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        };
        self.send(Response::new_ok(id, result).into());
        let child = self
            .options
            .child
            .as_ref()
            .map_or_else(|| "none".to_owned(), |path| path.display().to_string());
        self.log(
            MessageType::INFO,
            format!(
                "cheby lsp {} (build {}), child: {child}",
                env!("CARGO_PKG_VERSION"),
                self.options.build.to_hex()
            ),
        );
        self.refresh_settings();
    }

    /// The loop of A §3.2.
    fn main_loop(mut self) -> Ended {
        let mut events_open = true;
        loop {
            if self.closed {
                return self.ended(false);
            }
            let timer = self
                .next_deadline()
                .into_iter()
                .chain(self.session_deadline())
                .min()
                .map_or_else(never, crossbeam_channel::at);
            let swept = self.watch.swept();
            let events = if events_open {
                self.events.clone()
            } else {
                never()
            };
            select! {
                recv(self.connection.receiver) -> message => match message {
                    // The end of input, or a malformed frame, which ends
                    // `lsp-server`'s reader: status 1 before `shutdown`
                    // (D-1430).
                    Err(_) => return self.ended(true),
                    Ok(message) => {
                        if self.on_message(message) {
                            return self.ended(true);
                        }
                    }
                },
                recv(events) -> event => {
                    if let Ok(event) = event {
                        self.on_event(event);
                    } else {
                        events_open = false;
                        self.running = None;
                        self.log_error(
                            "the analysis thread has stopped; diagnostics will not refresh",
                        );
                    }
                }
                recv(self.query_done) -> done => {
                    // The pool holds a sender while any thread lives.
                    if let Ok(done) = done {
                        self.on_query_done(done);
                    }
                }
                recv(swept) -> found => {
                    if let Ok(found) = found {
                        self.on_swept(found);
                    }
                }
                recv(timer) -> _ => {}
            }
            self.expire_parked();
            self.schedule();
            self.end_idle_sessions();
            self.sync_sweeps();
        }
    }

    /// The loop's end: the status follows `shutdown`.
    const fn ended(&self, joinable: bool) -> Ended {
        Ended {
            code: if self.shutting_down { 0 } else { 1 },
            joinable,
        }
    }

    /// One message from the client. True for `exit`.
    fn on_message(&mut self, message: Message) -> bool {
        match message {
            Message::Request(request) => {
                self.on_request(request);
                false
            }
            Message::Notification(notification) => self.on_notification(notification),
            Message::Response(response) => {
                self.on_response(response);
                false
            }
        }
    }

    /// One request: answered on the main loop when its route is the loop's
    /// own, else sent off it by its class (A §3.3).
    fn on_request(&mut self, request: Request) {
        let Request { id, method, params } = request;
        let response = if self.shutting_down {
            Response::new_err(
                id,
                codes::INVALID_REQUEST,
                format!("`{method}` after `shutdown`"),
            )
        } else if let Some(handler) = routes::request(&method) {
            match handler(self, params) {
                Ok(value) => Response::new_ok(id, value),
                Err(error) => Response {
                    id,
                    response_result: Err(error),
                },
            }
        } else if let Some(route) = routes::query(&method) {
            self.dispatch_query(id, route, params);
            return;
        } else if let Some(result) = self.test_request(&method, &params) {
            Response {
                id,
                response_result: result,
            }
        } else {
            Response::new_err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown method `{method}`"),
            )
        };
        self.send(response.into());
    }

    /// One notification. True for `exit`.
    fn on_notification(&mut self, notification: Notification) -> bool {
        let Notification { method, params } = notification;
        if method == "exit" {
            return true;
        }
        if self.shutting_down {
            return false;
        }
        if method == "$/cancelRequest" {
            self.on_cancel_request(&params);
            return false;
        }
        if let Some(handler) = routes::notification(&method) {
            handler(self, params);
        }
        false
    }

    /// An answer to one of the server's requests.
    fn on_response(&mut self, response: Response) {
        match self.queue.outgoing.complete(response.id.clone()) {
            Some(Outgoing::Configuration) => self.on_configuration(response),
            Some(Outgoing::Ignore) | None => {}
        }
    }

    /// One event of the analysis thread.
    fn on_event(&mut self, event: Event) {
        match event {
            Event::Live {
                root,
                result,
                strict_follows,
            } => {
                let snapshot = result.as_ref().ok().and_then(|live| live.snapshot.clone());
                let texts = self.in_flight.take();
                if let (Some(texts), Some(package)) = (&texts, self.packages.get_mut(&root)) {
                    package.job_stamps.clone_from(&texts.stamps);
                }
                self.on_live(&root, result, !strict_follows);
                self.landed(&root, snapshot, texts);
            }
            Event::Strict { root, result } => {
                self.on_strict(&root, result);
                self.wake(&root);
            }
        }
    }

    /// `initialized`: register for settings changes and file changes, ask
    /// for settings, and start the session of each workspace folder that is
    /// a package root, so that closed files are published before anything
    /// opens (D-1375).
    fn on_initialized(&mut self, _params: Value) {
        if self.caps.config_dynamic {
            self.apply(Effect::Request {
                method: "client/registerCapability",
                params: serde_json::json!({
                    "registrations": [{
                        "id": "cheby-configuration",
                        "method": "workspace/didChangeConfiguration",
                    }],
                }),
            });
        }
        self.watch_folders();
        self.pull_configuration();
        for folder in self.folders.clone() {
            self.start_folder(&folder);
        }
    }

    /// `workspace/didChangeConfiguration`: the push's `settings.cheby`
    /// when it carries the key (D-1376), else the client is asked, when it
    /// answers `workspace/configuration`. Zed pushes the key to every
    /// server, also when nothing changed, so asking after each push would
    /// only double the traffic (cp-notes, CP1).
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a notification handler's signature"
    )]
    fn on_did_change_configuration(&mut self, params: Value) {
        match settings::pushed(&params) {
            Some(settings) => self.set_configuration(settings.clone()),
            None => self.pull_configuration(),
        }
    }

    /// Asks the client for the `cheby` section, when it can answer.
    fn pull_configuration(&mut self) {
        if !self.caps.configuration {
            return;
        }
        self.request(
            "workspace/configuration",
            serde_json::json!({ "items": [{ "section": "cheby" }] }),
            Outgoing::Configuration,
        );
    }

    /// The answer to `workspace/configuration`.
    fn on_configuration(&mut self, response: Response) {
        match response.response_result {
            Err(error) => self.log(
                MessageType::WARNING,
                format!("workspace/configuration failed: {}", error.message),
            ),
            Ok(Value::Array(items)) => {
                let first = items.into_iter().next().unwrap_or_default();
                self.set_configuration(first);
            }
            Ok(Value::Null) => {}
            Ok(_) => self.log(
                MessageType::WARNING,
                "workspace/configuration: the answer is not a list".to_owned(),
            ),
        }
    }

    /// A new configuration layer. `null` is no opinion and changes nothing
    /// (D-1376).
    fn set_configuration(&mut self, settings: Value) {
        if settings.is_null() {
            return;
        }
        self.layers.config = settings;
        self.refresh_settings();
    }

    /// Rebuilds the settings from their layers, logs each new warning once,
    /// and applies what changed (A §12.3).
    fn refresh_settings(&mut self) {
        let (settings, warnings) = Settings::from_layers([&self.layers.init, &self.layers.config]);
        for warning in warnings {
            if self.warned.first(&warning) {
                self.log(MessageType::WARNING, warning);
            }
        }
        let target_changed = settings.analysis.target != self.settings.analysis.target;
        self.settings = Arc::new(settings);
        if target_changed {
            self.mark_all_dirty();
        }
    }

    /// A second `initialize`: the protocol allows one, so it is refused.
    #[expect(clippy::unused_self, reason = "a request handler's signature")]
    fn on_initialize_again(&mut self, _params: Value) -> Result<Value, lsp_server::ResponseError> {
        Err(lsp_server::ResponseError {
            code: codes::INVALID_REQUEST,
            message: "`initialize` was already received".to_owned(),
            data: None,
        })
    }

    /// `shutdown`: answered `null`, after each parked request is answered
    /// `RequestCancelled` (A §3.2). Every later request is refused.
    #[expect(clippy::unnecessary_wraps, reason = "a request handler's signature")]
    fn on_shutdown(&mut self, _params: Value) -> Result<Value, lsp_server::ResponseError> {
        self.shutting_down = true;
        self.cancel_parked();
        Ok(Value::Null)
    }

    /// The parameters of a notification as `T`, or a logged warning.
    fn params<T: DeserializeOwned>(&mut self, what: &str, params: Value) -> Option<T> {
        match serde_json::from_value(params) {
            Ok(params) => Some(params),
            Err(e) => {
                self.log(
                    MessageType::WARNING,
                    format!("{what}: invalid parameters: {e}"),
                );
                None
            }
        }
    }

    /// A `window/logMessage`.
    fn log(&mut self, level: MessageType, message: String) {
        self.apply(Effect::log(level, message));
    }

    /// An error for `window/logMessage`.
    fn log_error(&mut self, message: &str) {
        self.apply(Effect::log(MessageType::ERROR, message));
    }
}
