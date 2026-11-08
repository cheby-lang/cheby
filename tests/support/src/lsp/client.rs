//! [`Client`]: one `cheby lsp` process spoken to over its standard input
//! and output, as an editor would.
//!
//! - One thread reads frames from the server's standard output
//!   ([`super::frame::read`]) and another collects its standard error,
//!   which goes into every failure message, so that a panic report is
//!   visible. The pipes are separate: standard output must hold frames
//!   only (A §3.7), and [`Ended::frames`] says whether it did.
//! - Every wait has a deadline, the client's timeout from now, so a server
//!   that never answers fails the trial instead of hanging it.
//! - Requests from the server get a default answer at once
//!   (`workspace/configuration`, `client/registerCapability`,
//!   `window/workDoneProgress/create` and the rest), or a request of the
//!   client would deadlock behind them.
//! - [`Client::diagnostics`] waits for a `publishDiagnostics` of one URI
//!   with one version, which the server always sends for an open document
//!   after its analysis, an empty list included (D-1373). So waits need no
//!   sleep.
//! - Dropping a client kills the process.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::text::Encoding;
use super::{frame, uri};
use crate::cache::HarnessCache;
use crate::milestone::POISON_ENV;

/// The method of a diagnostics notification.
pub const PUBLISH_DIAGNOSTICS: &str = "textDocument/publishDiagnostics";

/// How many bytes of the server's standard error a failure message shows.
const STDERR_TAIL: usize = 8 * 1024;

/// What [`Client::start`] needs.
#[derive(Debug, Clone, Copy)]
pub struct StartOptions<'a> {
    /// The `cheby` binary.
    pub cheby: &'a Path,
    /// The directory the server runs in, and the root that
    /// [`Client::display`] makes paths relative to.
    pub root: &'a Path,
    /// The harness's fresh cache (D-959, D-1128).
    pub cache: &'a HarnessCache,
    /// The deadline of every wait, already scaled (D-722).
    pub timeout: Duration,
    /// Arguments after `lsp`, such as `--stdio`.
    pub args: &'a [&'a str],
}

/// A JSON-RPC error answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// The error code, such as -32601 for an unknown method.
    pub code: i64,
    /// The server's message.
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error {}: {}", self.code, self.message)
    }
}

/// The protocol's error codes that the harness checks.
pub mod codes {
    /// The request is not valid JSON-RPC, or comes after `shutdown`.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The params do not fit the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The server failed.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// A request before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// The client cancelled the request.
    pub const REQUEST_CANCELLED: i64 = -32800;
    /// The text moved on before the answer.
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// The request failed, a wait past `requestWaitMs` included.
    pub const REQUEST_FAILED: i64 = -32803;
}

/// The client capabilities a session announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientKind {
    /// `{}`: a client that announces nothing, so the server must fall back
    /// to plain behaviour (A §12.1).
    Empty,
    /// Every capability of [`ClientKind::Full`] but the position
    /// encodings, so the session runs on UTF-16 (D-1368). The default.
    #[default]
    Standard,
    /// Every capability the server reads, every position encoding
    /// included.
    Full,
}

impl ClientKind {
    /// The kind a header names: `empty`, `standard` or `full`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "empty" => Some(Self::Empty),
            "standard" => Some(Self::Standard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The `capabilities` of `initialize`. `encoding`, if given, is the
    /// only position encoding the client offers, which overrides the
    /// kind's own list.
    #[must_use]
    pub fn capabilities(self, encoding: Option<Encoding>) -> Value {
        let mut caps = match self {
            Self::Empty => json!({}),
            Self::Standard | Self::Full => full_capabilities(),
        };
        let encodings = match (self, encoding) {
            (_, Some(encoding)) => Some(vec![encoding.name()]),
            (Self::Full, None) => Some(vec!["utf-8", "utf-32", "utf-16"]),
            _ => None,
        };
        if let (Some(encodings), Some(map)) = (encodings, caps.as_object_mut()) {
            map.insert(
                "general".to_owned(),
                json!({ "positionEncodings": encodings }),
            );
        }
        caps
    }
}

/// Every client capability of A §12.1, set.
fn full_capabilities() -> Value {
    json!({
        "workspace": workspace_capabilities(),
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": true } },
            "showDocument": { "support": true },
        },
        "textDocument": text_document_capabilities(),
        "experimental": { "chebyIdle": true },
    })
}

fn workspace_capabilities() -> Value {
    json!({
        "applyEdit": true,
        "workspaceEdit": {
            "documentChanges": true,
            "resourceOperations": ["create", "rename", "delete"],
            "changeAnnotationSupport": { "groupsOnLabel": true },
        },
        "didChangeConfiguration": { "dynamicRegistration": true },
        "didChangeWatchedFiles": {
            "dynamicRegistration": true,
            "relativePatternSupport": true,
        },
        "configuration": true,
        "workspaceFolders": true,
        "symbol": { "dynamicRegistration": false },
        "executeCommand": { "dynamicRegistration": false },
        "inlayHint": { "refreshSupport": true },
        "codeLens": { "refreshSupport": true },
        "semanticTokens": { "refreshSupport": true },
        "fileOperations": {
            "didCreate": true,
            "didRename": true,
            "didDelete": true,
            "willRename": true,
        },
    })
}

fn text_document_capabilities() -> Value {
    json!({
        "synchronization": { "didSave": true, "willSave": false },
        "publishDiagnostics": {
            "relatedInformation": true,
            "versionSupport": true,
            "tagSupport": { "valueSet": [1, 2] },
            "codeDescriptionSupport": true,
            "dataSupport": true,
        },
        "hover": { "contentFormat": ["markdown", "plaintext"] },
        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
        "completion": {
            "contextSupport": true,
            "completionItem": {
                "snippetSupport": true,
                "labelDetailsSupport": true,
                "resolveSupport": {
                    "properties": ["documentation", "detail", "additionalTextEdits"],
                },
            },
        },
        "signatureHelp": {
            "signatureInformation": {
                "documentationFormat": ["markdown", "plaintext"],
                "parameterInformation": { "labelOffsetSupport": true },
                "activeParameterSupport": true,
            },
        },
        "definition": { "linkSupport": true },
        "declaration": { "linkSupport": true },
        "typeDefinition": { "linkSupport": true },
        "implementation": { "linkSupport": true },
        "codeAction": {
            "codeActionLiteralSupport": {
                "codeActionKind": {
                    "valueSet": [
                        "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                        "refactor.rewrite", "source", "source.organizeImports",
                    ],
                },
            },
            "resolveSupport": { "properties": ["edit"] },
            "dataSupport": true,
            "isPreferredSupport": true,
        },
        "codeLens": {},
        "inlayHint": {
            "resolveSupport": {
                "properties": ["tooltip", "textEdits", "label.tooltip", "label.location"],
            },
        },
        "semanticTokens": {
            "requests": { "full": true, "range": true },
            "tokenTypes": [
                "namespace", "type", "class", "enum", "interface", "struct",
                "typeParameter", "parameter", "variable", "property", "enumMember",
                "event", "function", "method", "macro", "keyword", "modifier", "comment",
                "string", "number", "regexp", "operator", "decorator",
            ],
            "tokenModifiers": [
                "declaration", "definition", "readonly", "static", "deprecated",
                "abstract", "async", "modification", "documentation", "defaultLibrary",
            ],
            "formats": ["relative"],
            "multilineTokenSupport": true,
        },
        "foldingRange": { "lineFoldingOnly": false },
        "selectionRange": {},
        "documentLink": {},
        "documentHighlight": {},
        "references": {},
        "formatting": {},
        "rangeFormatting": {},
        "rename": { "prepareSupport": true },
        "callHierarchy": {},
    })
}

/// What the reader thread passes on.
#[derive(Debug)]
enum Incoming {
    Message(Value),
    /// The output ended between frames.
    End,
    /// The output held something that is not a frame.
    Broken(String),
}

/// How a session ended.
#[derive(Debug)]
pub struct Ended {
    /// The exit status, `None` for a signal.
    pub status: Option<i32>,
    /// `Ok` if standard output held only frames of JSON-RPC 2.0 messages
    /// and ended between two of them.
    pub frames: Result<(), String>,
    /// The latest `publishDiagnostics` params of each URI the session saw.
    pub published: BTreeMap<String, Value>,
    /// Every message the server sent, in order.
    pub received: Vec<Value>,
    /// The id of every request the client sent, as JSON text.
    pub sent: BTreeSet<String>,
    /// The server's standard error.
    pub stderr: String,
}

impl Ended {
    /// `Ok` if the server exited with `status` and standard output held
    /// only frames.
    ///
    /// # Errors
    ///
    /// Returns a message naming what differed, with the server's standard
    /// error.
    pub fn expect(&self, status: i32) -> Result<(), String> {
        if let Err(e) = &self.frames {
            return Err(format!(
                "the server's standard output held more than frames: {e}{}",
                tail(&self.stderr)
            ));
        }
        if self.status != Some(status) {
            return Err(format!(
                "the server exited with {:?}, expected {status}{}",
                self.status,
                tail(&self.stderr)
            ));
        }
        Ok(())
    }
}

/// `stderr`'s end, as a suffix of a failure message.
fn tail(stderr: &str) -> String {
    if stderr.is_empty() {
        return "\n(the server's standard error is empty)".to_owned();
    }
    let mut start = stderr.len().saturating_sub(STDERR_TAIL);
    while !stderr.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    format!(
        "\nthe server's standard error:\n{}",
        stderr.get(start..).unwrap_or_default()
    )
}

/// How a session shows URIs: paths below its root relative to it.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
    /// `root` canonicalized, which a server may report instead.
    canonical: PathBuf,
}

impl Paths {
    /// The paths of a session rooted at `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        // On Windows `canonicalize` gives a verbatim path, `\\?\C:\…`,
        // which no server reports and `strip_prefix` cannot match.
        let canonical = match canonical.to_str() {
            Some(text) if cfg!(windows) => PathBuf::from(uri::strip_verbatim(text)),
            _ => canonical,
        };
        Self {
            root: root.to_path_buf(),
            canonical,
        }
    }

    /// The root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path below the root, with `/` separators, that `uri` names, or
    /// `None` for a URI outside the root.
    #[must_use]
    pub fn relative(&self, uri: &str) -> Option<String> {
        let path = uri::uri_to_path(uri)?;
        let relative = path
            .strip_prefix(&self.root)
            .or_else(|_| path.strip_prefix(&self.canonical))
            .ok()?;
        Some(
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// `uri` as goldens show it (D-264, D-421): a path below the root
    /// relative to it, a materialized std file as `<std>/…` (A §6.2), any
    /// other URI as it is.
    #[must_use]
    pub fn display(&self, uri: &str) -> String {
        if let Some(relative) = self.relative(uri) {
            return relative;
        }
        if let Some(path) = uri::uri_to_path(uri) {
            let text = path.to_string_lossy().replace('\\', "/");
            if let Some((_, rest)) = text.split_once("/lsp/std-src/")
                && let Some((_, below)) = rest.split_once('/')
            {
                return format!("<std>/{below}");
            }
        }
        uri.to_owned()
    }

    /// `value` with every `file:` URI, as a string or as an object key,
    /// shown by [`Paths::display`].
    #[must_use]
    pub fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(s) if s.starts_with("file:") => Value::String(self.display(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.normalize(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let key = if k.starts_with("file:") {
                            self.display(k)
                        } else {
                            k.clone()
                        };
                        (key, self.normalize(v))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// One `cheby lsp` process.
#[derive(Debug)]
pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
    reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_reader: Option<JoinHandle<()>>,
    timeout: Duration,
    paths: Paths,
    next_id: i64,
    /// Notifications not yet taken by a wait.
    pending: VecDeque<Value>,
    /// Answers that arrived while another was awaited.
    responses: BTreeMap<i64, Value>,
    /// Every message received, in order.
    received: Vec<Value>,
    /// The id of every request sent, as JSON text.
    sent: BTreeSet<String>,
    published: BTreeMap<String, Value>,
    /// `Ok` until the output held something that is not a frame.
    frames: Result<(), String>,
    ended: bool,
    /// The answer to `workspace/configuration` for the `cheby` section.
    configuration: Value,
    versions: BTreeMap<String, i32>,
    encoding: Encoding,
}

impl Client {
    /// Starts `cheby --color=never lsp`, followed by `options.args`, in
    /// `options.root`, with the harness's environment: no color,
    /// `CHEBY_RT_POISON=1`, the fresh cache, and none of the variables a
    /// developer sets for themselves (D-1043).
    ///
    /// # Errors
    ///
    /// Returns a message if the process cannot be started.
    pub fn start(options: StartOptions<'_>) -> Result<Self, String> {
        let mut cmd = Command::new(options.cheby);
        cmd.args(["--color=never", "lsp"])
            .args(options.args)
            .current_dir(options.root)
            .env("NO_COLOR", "1")
            .env(POISON_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        options.cache.apply(&mut cmd);
        crate::process::clear_inherited(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {cmd:?}: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("stdout is not piped")?;
        let mut stderr_pipe = child.stderr.take().ok_or("stderr is not piped")?;
        let (sender, incoming) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("lsp-client-reader".to_owned())
            .spawn(move || {
                let mut input = BufReader::new(stdout);
                loop {
                    let message = match frame::read(&mut input) {
                        Ok(Some(value)) => Incoming::Message(value),
                        Ok(None) => Incoming::End,
                        Err(e) => {
                            // Report it at once, with what is already
                            // read after it, then drain the rest, so the
                            // server never blocks on a full pipe. Waiting
                            // for the end of output first would block
                            // until the server exits.
                            let rest = input.buffer();
                            let shown = rest.get(..rest.len().min(512)).unwrap_or_default();
                            let broken = Incoming::Broken(format!(
                                "{e}; then {:?}",
                                String::from_utf8_lossy(shown)
                            ));
                            let _ = sender.send(broken);
                            let _ = std::io::copy(&mut input, &mut std::io::sink());
                            return;
                        }
                    };
                    let last = !matches!(message, Incoming::Message(_));
                    if sender.send(message).is_err() || last {
                        return;
                    }
                }
            })
            .map_err(|e| format!("cannot start a reader thread: {e}"))?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&stderr);
        let stderr_reader = std::thread::Builder::new()
            .name("lsp-client-stderr".to_owned())
            .spawn(move || {
                let mut buf = [0_u8; 4096];
                while let Ok(n) = stderr_pipe.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    sink.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(buf.get(..n).unwrap_or_default());
                }
            })
            .map_err(|e| format!("cannot start a reader thread: {e}"))?;
        Ok(Self {
            child,
            stdin,
            incoming,
            reader: Some(reader),
            stderr,
            stderr_reader: Some(stderr_reader),
            timeout: options.timeout,
            paths: Paths::new(options.root),
            next_id: 1,
            pending: VecDeque::new(),
            responses: BTreeMap::new(),
            received: Vec::new(),
            sent: BTreeSet::new(),
            published: BTreeMap::new(),
            frames: Ok(()),
            ended: false,
            configuration: Value::Null,
            versions: BTreeMap::new(),
            encoding: Encoding::Utf16,
        })
    }

    /// Sets the answer the client gives to `workspace/configuration` for
    /// the `cheby` section; `null` until then, which means no opinion
    /// (D-1376).
    pub fn set_configuration(&mut self, settings: Value) {
        self.configuration = settings;
    }

    /// The params of `initialize` for a session whose only workspace
    /// folder is the root, with `capabilities` and, if given,
    /// `initializationOptions`.
    ///
    /// # Errors
    ///
    /// Returns a message if the root has no URI.
    pub fn initialize_params(
        &self,
        capabilities: &Value,
        options: Option<Value>,
    ) -> Result<Value, String> {
        let root = uri::path_to_uri(&self.paths.root)?;
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "cheby-lsp-harness" },
            "rootUri": root,
            "capabilities": capabilities,
            "workspaceFolders": [{ "uri": root, "name": "root" }],
        });
        if let (Some(options), Some(map)) = (options, params.as_object_mut()) {
            map.insert("initializationOptions".to_owned(), options);
        }
        Ok(params)
    }

    /// Sends `initialize` with `params`, then `initialized`, and returns
    /// the result. The negotiated `positionEncoding` becomes the client's
    /// [`Client::encoding`]; its absence means UTF-16.
    ///
    /// # Errors
    ///
    /// Returns a message if the server answers with an error, names an
    /// unknown encoding, or does not answer in time.
    pub fn initialize(&mut self, params: Value) -> Result<Value, String> {
        let result = self.request("initialize", params)?;
        let encoding = result
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
            .unwrap_or("utf-16");
        self.encoding = Encoding::parse(encoding)
            .ok_or_else(|| format!("the server chose an unknown encoding `{encoding}`"))?;
        self.notify("initialized", json!({}))?;
        Ok(result)
    }

    /// The negotiated position encoding.
    #[must_use]
    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The deadline of each wait.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends a request and waits for its answer.
    ///
    /// # Errors
    ///
    /// Returns a message for an error answer, a broken stream, or no
    /// answer in time.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.call(method, params)?
            .map_err(|e| self.fail(&format!("`{method}` answered {e}")))
    }

    /// Sends a request and waits for its answer, which may be an error.
    ///
    /// # Errors
    ///
    /// Returns a message for a broken stream or no answer in time.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Result<Value, RpcError>, String> {
        let id = self.send_request(method, params)?;
        self.response(id)
    }

    /// Sends a request and returns its id, without waiting.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn send_request(&mut self, method: &str, params: Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.send(&message(Some(id), method, params))?;
        Ok(id)
    }

    /// Sends several requests, then waits for all their answers, in the
    /// order given.
    ///
    /// # Errors
    ///
    /// Returns a message for a broken stream or an answer not in time.
    pub fn batch(
        &mut self,
        requests: Vec<(&str, Value)>,
    ) -> Result<Vec<Result<Value, RpcError>>, String> {
        let mut ids = Vec::with_capacity(requests.len());
        for (method, params) in requests {
            ids.push(self.send_request(method, params)?);
        }
        ids.into_iter().map(|id| self.response(id)).collect()
    }

    /// Waits for the answer to request `id`.
    ///
    /// # Errors
    ///
    /// Returns a message for a broken stream, an answer that is neither a
    /// result nor an error, or no answer in time.
    pub fn response(&mut self, id: i64) -> Result<Result<Value, RpcError>, String> {
        let deadline = self.deadline();
        let message = loop {
            if let Some(message) = self.responses.remove(&id) {
                break message;
            }
            let message = self.next(deadline, &format!("the answer to request {id}"))?;
            if message.get("method").is_some() {
                self.pending.push_back(message);
                continue;
            }
            match message.get("id").and_then(Value::as_i64) {
                Some(got) if got == id => break message,
                Some(other) => {
                    self.responses.insert(other, message);
                }
                None => return Err(self.fail(&format!("an answer without an id: {message}"))),
            }
        };
        if let Some(error) = message.get("error") {
            return Ok(Err(RpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }));
        }
        match message.get("result") {
            Some(result) => Ok(Ok(result.clone())),
            None => Err(self.fail(&format!(
                "an answer with neither result nor error: {message}"
            ))),
        }
    }

    /// Sends a notification.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&message(None, method, params))
    }

    /// Writes one raw message, for tests of malformed input.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn send(&mut self, message: &Value) -> Result<(), String> {
        if message.get("method").is_some()
            && let Some(id) = message.get("id")
        {
            self.sent.insert(id.to_string());
        }
        let stdin = self.stdin.as_mut().ok_or("the server's input is closed")?;
        frame::write(stdin, message)
            .map_err(|e| format!("cannot write to the server: {e}"))
            .map_err(|e| self.fail(&e))
    }

    /// Waits for the first notification, taken or not yet seen, for which
    /// `matches` holds, and takes it. `what` names it in a timeout.
    ///
    /// # Errors
    ///
    /// Returns a message for a broken stream or no such notification in
    /// time.
    pub fn notification(
        &mut self,
        what: &str,
        mut matches: impl FnMut(&Value) -> bool,
    ) -> Result<Value, String> {
        if let Some(at) = self.pending.iter().position(&mut matches)
            && let Some(found) = self.pending.remove(at)
        {
            return Ok(found);
        }
        let deadline = self.deadline();
        loop {
            let message = self.next(deadline, what)?;
            if message.get("method").is_none() {
                if let Some(id) = message.get("id").and_then(Value::as_i64) {
                    self.responses.insert(id, message);
                }
                continue;
            }
            if matches(&message) {
                return Ok(message);
            }
            self.pending.push_back(message);
        }
    }

    /// Waits for a `publishDiagnostics` of `uri` with `version`, `None`
    /// for a closed file's publish, which has no version, and returns its
    /// diagnostics.
    ///
    /// # Errors
    ///
    /// As [`Client::notification`].
    pub fn diagnostics(&mut self, uri: &str, version: Option<i32>) -> Result<Vec<Value>, String> {
        let what = format!(
            "diagnostics of {} at version {version:?}",
            self.display(uri)
        );
        let found = self.notification(&what, |m| {
            is_publish_of(m, uri)
                && params(m).get("version").and_then(Value::as_i64) == version.map(i64::from)
        })?;
        Ok(diagnostics_of(&found))
    }

    /// Takes every notification received and not yet taken.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        self.pending.drain(..).collect()
    }

    /// The latest `publishDiagnostics` params of each URI so far.
    #[must_use]
    pub const fn published(&self) -> &BTreeMap<String, Value> {
        &self.published
    }

    /// Every message received so far, in order.
    #[must_use]
    pub fn received(&self) -> &[Value] {
        &self.received
    }

    /// Opens the file at `path`, relative paths taken from the root, with
    /// its disk text, as version 1, and returns its URI.
    ///
    /// # Errors
    ///
    /// Returns a message if the file cannot be read or the server's input
    /// is closed.
    pub fn open(&mut self, path: &Path) -> Result<String, String> {
        let path = self.paths.root.join(path);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let uri = self.uri(&path)?;
        self.open_text(&uri, &text)?;
        Ok(uri)
    }

    /// Opens `uri` with `text` as version 1.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn open_text(&mut self, uri: &str, text: &str) -> Result<(), String> {
        let toml = Path::new(uri)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
        let language = if toml { "toml" } else { "cheby" };
        self.versions.insert(uri.to_owned(), 1);
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": language, "version": 1, "text": text,
            } }),
        )
    }

    /// Replaces the whole text of the open `uri` and returns its new
    /// version.
    ///
    /// # Errors
    ///
    /// Returns a message if `uri` is not open or the server's input is
    /// closed.
    pub fn change(&mut self, uri: &str, text: &str) -> Result<i32, String> {
        self.change_with(uri, &[json!({ "text": text })])
    }

    /// Sends `changes`, the protocol's `contentChanges`, for the open
    /// `uri`, and returns its new version.
    ///
    /// # Errors
    ///
    /// Returns a message if `uri` is not open or the server's input is
    /// closed.
    pub fn change_with(&mut self, uri: &str, changes: &[Value]) -> Result<i32, String> {
        let version = self
            .versions
            .get_mut(uri)
            .ok_or_else(|| format!("{uri} is not open"))?;
        *version = version.saturating_add(1);
        let version = *version;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": changes,
            }),
        )?;
        Ok(version)
    }

    /// Sends `didSave` for `uri`.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn save(&mut self, uri: &str) -> Result<(), String> {
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        )
    }

    /// Closes `uri`.
    ///
    /// # Errors
    ///
    /// Returns a message if the server's input is closed.
    pub fn close(&mut self, uri: &str) -> Result<(), String> {
        self.versions.remove(uri);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
    }

    /// The version of the open `uri`.
    #[must_use]
    pub fn version(&self, uri: &str) -> Option<i32> {
        self.versions.get(uri).copied()
    }

    /// The URI of `path`, relative paths taken from the root.
    ///
    /// # Errors
    ///
    /// Returns a message if the path has no URI.
    pub fn uri(&self, path: &Path) -> Result<String, String> {
        uri::path_to_uri(&self.paths.root.join(path))
    }

    /// How this session shows URIs.
    #[must_use]
    pub const fn paths(&self) -> &Paths {
        &self.paths
    }

    /// [`Paths::display`] of this session.
    #[must_use]
    pub fn display(&self, uri: &str) -> String {
        self.paths.display(uri)
    }

    /// [`Paths::normalize`] of this session.
    #[must_use]
    pub fn normalize(&self, value: &Value) -> Value {
        self.paths.normalize(value)
    }

    /// `shutdown`, which must answer `null`, then `exit`, then the end of
    /// the session.
    ///
    /// # Errors
    ///
    /// Returns a message if `shutdown` fails or the process does not end
    /// in time.
    pub fn shutdown(mut self) -> Result<Ended, String> {
        let answer = self.request("shutdown", Value::Null)?;
        if !answer.is_null() {
            return Err(self.fail(&format!("`shutdown` answered {answer}, not null")));
        }
        self.exit()
    }

    /// `exit`, then the end of the session.
    ///
    /// # Errors
    ///
    /// Returns a message if the process does not end in time.
    pub fn exit(mut self) -> Result<Ended, String> {
        self.notify("exit", Value::Null)?;
        self.end_input()
    }

    /// Closes the server's input and waits for the session to end.
    ///
    /// # Errors
    ///
    /// Returns a message if the process does not end in time.
    pub fn end_input(mut self) -> Result<Ended, String> {
        drop(self.stdin.take());
        let deadline = self.deadline();
        // Read the rest of the output, so `published` and `frames` are
        // complete.
        while !self.ended {
            match self.next(deadline, "the end of the server's output") {
                Ok(message) => {
                    if message.get("method").is_some() {
                        self.pending.push_back(message);
                    }
                }
                Err(_) if self.ended => break,
                Err(e) => return Err(e),
            }
        }
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if Instant::now() > deadline => {
                    return Err(self.fail("the server did not exit in time"));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(self.fail(&format!("cannot wait for the server: {e}"))),
            }
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        Ok(Ended {
            status,
            frames: self.frames.clone(),
            published: std::mem::take(&mut self.published),
            received: std::mem::take(&mut self.received),
            sent: std::mem::take(&mut self.sent),
            stderr: self.stderr_text(),
        })
    }

    /// The server's standard error so far.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        let bytes = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        crate::process::normalize(&bytes)
    }

    /// `message` with the end of the server's standard error.
    #[must_use]
    pub fn fail(&self, message: &str) -> String {
        format!("{message}{}", tail(&self.stderr_text()))
    }

    fn deadline(&self) -> Instant {
        Instant::now()
            .checked_add(self.timeout)
            .unwrap_or_else(Instant::now)
    }

    /// The next message that is not a request of the server, which is
    /// answered here.
    fn next(&mut self, deadline: Instant, what: &str) -> Result<Value, String> {
        loop {
            if self.ended {
                return Err(self.fail(&format!(
                    "the server's output ended while waiting for {what}"
                )));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.incoming.recv_timeout(left) {
                Ok(Incoming::Message(message)) => message,
                Ok(Incoming::End) | Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    continue;
                }
                Ok(Incoming::Broken(e)) => {
                    self.ended = true;
                    self.frames = Err(e.clone());
                    return Err(self.fail(&format!("the server's output is not frames only: {e}")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(self.fail(&format!(
                        "timed out after {:?} waiting for {what}",
                        self.timeout
                    )));
                }
            };
            self.received.push(message.clone());
            if message.get("method").and_then(Value::as_str) == Some(PUBLISH_DIAGNOSTICS)
                && let Some(uri) = params(&message).get("uri").and_then(Value::as_str)
            {
                self.published
                    .insert(uri.to_owned(), params(&message).clone());
            }
            if message.get("method").is_some() && message.get("id").is_some() {
                self.answer(&message);
                continue;
            }
            return Ok(message);
        }
    }

    /// The default answer to a request of the server.
    fn answer(&mut self, request: &Value) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "workspace/configuration" => {
                let items = params(request)
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let section = |i: usize| {
                    params(request)
                        .get("items")
                        .and_then(|items| items.get(i))
                        .and_then(|item| item.get("section"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                Ok(Value::Array(
                    (0..items)
                        .map(|i| match section(i).as_deref() {
                            Some("cheby") => self.configuration.clone(),
                            _ => Value::Null,
                        })
                        .collect(),
                ))
            }
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/diagnostic/refresh" => Ok(Value::Null),
            "workspace/applyEdit" => Ok(json!({ "applied": true })),
            "window/showDocument" => Ok(json!({ "success": true })),
            other => Err(json!({
                "code": codes::METHOD_NOT_FOUND,
                "message": format!("the harness's client has no method `{other}`"),
            })),
        };
        let response = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        // The server may already be gone; its output then tells why.
        if self.stdin.is_some() {
            let _ = self.send(&response);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A request, with `id`, or a notification.
fn message(id: Option<i64>, method: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("jsonrpc".to_owned(), Value::from("2.0"));
    if let Some(id) = id {
        map.insert("id".to_owned(), Value::from(id));
    }
    map.insert("method".to_owned(), Value::from(method));
    map.insert("params".to_owned(), params);
    Value::Object(map)
}

/// The `params` of a message, `null` if it has none.
#[must_use]
pub fn params(message: &Value) -> &Value {
    message.get("params").unwrap_or(&Value::Null)
}

/// Whether `message` is a `publishDiagnostics` of `uri`.
#[must_use]
pub fn is_publish_of(message: &Value, uri: &str) -> bool {
    message.get("method").and_then(Value::as_str) == Some(PUBLISH_DIAGNOSTICS)
        && params(message).get("uri").and_then(Value::as_str) == Some(uri)
}

/// The diagnostics of a `publishDiagnostics` message or of its params.
#[must_use]
pub fn diagnostics_of(message: &Value) -> Vec<Value> {
    let params = message.get("params").unwrap_or(message);
    params
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_by_kind() {
        assert_eq!(ClientKind::Empty.capabilities(None), json!({}));
        let standard = ClientKind::Standard.capabilities(None);
        assert!(standard.get("general").is_none());
        assert_eq!(
            standard["textDocument"]["publishDiagnostics"]["versionSupport"],
            json!(true)
        );
        let full = ClientKind::Full.capabilities(None);
        assert_eq!(
            full["general"]["positionEncodings"],
            json!(["utf-8", "utf-32", "utf-16"])
        );
        let one = ClientKind::Empty.capabilities(Some(Encoding::Utf32));
        assert_eq!(one, json!({"general": {"positionEncodings": ["utf-32"]}}));
        assert_eq!(ClientKind::parse("full"), Some(ClientKind::Full));
        assert_eq!(ClientKind::parse("other"), None);
    }

    #[test]
    fn publish_helpers() {
        let m = json!({"jsonrpc": "2.0", "method": PUBLISH_DIAGNOSTICS,
            "params": {"uri": "file:///a", "diagnostics": [{"message": "x"}]}});
        assert!(is_publish_of(&m, "file:///a"));
        assert!(!is_publish_of(&m, "file:///b"));
        assert_eq!(diagnostics_of(&m).len(), 1);
        assert_eq!(diagnostics_of(params(&m)).len(), 1);
    }

    #[test]
    fn stderr_tail_keeps_the_end() {
        assert!(tail("").contains("empty"));
        let long = "é".repeat(STDERR_TAIL);
        assert!(tail(&long).len() <= STDERR_TAIL.saturating_add(64));
    }
}
