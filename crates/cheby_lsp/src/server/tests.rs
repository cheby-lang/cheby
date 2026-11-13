//! In-process tests of the main loop through `run_on` (A §3.8): the
//! handshake's capabilities, the settings of plan §1.9 and the publishing
//! rules of A §8.4. `analysis.debounceMs` is 0 where runs matter, so the
//! published versions give the order and no clock is needed.
//!
//! The client and the packages here serve `session_tests` too. Since a
//! publish that changes nothing is skipped (D-1442), a test that needs a
//! job to have finished waits for a publish the job changes, such as the
//! next version's after an edit, since a package's jobs run in order.
//! `cheby/testSettings`, answered on the main loop, only orders it: the
//! answer comes after every notification the main loop sent for the
//! messages before it, and waits for no job.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cheby_driver::cache::BuildId;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};

use crate::text::uri::path_to_uri;
use crate::{ServerOptions, run_on};

/// How long a test waits for one message: the first run checks std.
pub(super) const WAIT: Duration = Duration::from_secs(120);

/// A client talking to a server on its own thread.
pub(super) struct Client {
    connection: Connection,
    server: Option<JoinHandle<Result<ExitCode, String>>>,
    next_id: i32,
    /// Answers to `workspace/configuration`, in order; `[null]` after.
    config_answers: VecDeque<Value>,
    /// Every notification received, in order.
    pub(super) notifications: Vec<Notification>,
    /// Every request of the server received, in order.
    pub(super) requests: Vec<Request>,
}

impl Client {
    /// Starts a server and sends `initialize` with `params`, and
    /// `initialized`. Returns the client and the `initialize` result.
    pub(super) fn start(params: Value, config_answers: Vec<Value>) -> (Self, Value) {
        Self::start_with(params, config_answers, None)
    }

    /// [`Client::start`] with `child` as the executable of child jobs.
    pub(super) fn start_with(
        params: Value,
        config_answers: Vec<Value>,
        child: Option<PathBuf>,
    ) -> (Self, Value) {
        let (server_side, client_side) = Connection::memory();
        let options = ServerOptions {
            build: BuildId([0; 16]),
            child,
        };
        let server = std::thread::Builder::new()
            .name("cheby-test-server".to_owned())
            .stack_size(256 << 20)
            .spawn(move || run_on(server_side, options))
            .unwrap();
        let mut client = Self {
            connection: client_side,
            server: Some(server),
            next_id: 0,
            config_answers: config_answers.into(),
            notifications: Vec::new(),
            requests: Vec::new(),
        };
        let result = client.request("initialize", params).unwrap();
        client.notify("initialized", json!({}));
        (client, result)
    }

    pub(super) fn notify(&self, method: &str, params: Value) {
        let message = Notification::new(method.to_owned(), params);
        self.connection.sender.send(message.into()).unwrap();
    }

    /// Sends a request and waits for its answer, handling everything that
    /// arrives before it.
    pub(super) fn request(&mut self, method: &str, params: Value) -> Result<Value, i32> {
        self.next_id = self.next_id.checked_add(1).unwrap();
        let id = RequestId::from(self.next_id);
        let request = Request::new(id.clone(), method.to_owned(), params);
        self.connection.sender.send(request.into()).unwrap();
        loop {
            if let Message::Response(response) = self.next()
                && response.id == id
            {
                return response.response_result.map_err(|e| e.code);
            }
        }
    }

    /// The next message, after answering a request of the server.
    pub(super) fn next(&mut self) -> Message {
        let message = self
            .connection
            .receiver
            .recv_timeout(WAIT)
            .expect("the server sent nothing in time");
        match &message {
            Message::Request(request) => {
                let result = match request.method.as_str() {
                    "workspace/configuration" => {
                        let answer = self.config_answers.pop_front().unwrap_or(Value::Null);
                        json!([answer])
                    }
                    _ => Value::Null,
                };
                let response = Response::new_ok(request.id.clone(), result);
                self.connection.sender.send(response.into()).unwrap();
                self.requests.push(request.clone());
            }
            Message::Notification(notification) => self.notifications.push(notification.clone()),
            Message::Response(_) => {}
        }
        message
    }

    /// Waits for a server request of `method`, answering it.
    pub(super) fn wait_request(&mut self, method: &str) -> Request {
        self.wait_request_that(method, |_| true)
    }

    /// Waits for a server request of `method` whose parameters `accept`
    /// takes, answering every request on the way.
    pub(super) fn wait_request_that(
        &mut self,
        method: &str,
        accept: impl Fn(&Value) -> bool,
    ) -> Request {
        loop {
            if let Message::Request(request) = self.next()
                && request.method == method
                && accept(&request.params)
            {
                return request;
            }
        }
    }

    /// The first `publishDiagnostics` of `path` that `accept` takes, among
    /// those received so far and then those to come.
    pub(super) fn wait_publish(&mut self, path: &Path, accept: impl Fn(&Value) -> bool) -> Value {
        let uri = uri(path);
        let deadline = Instant::now().checked_add(WAIT).unwrap();
        let mut seen = 0;
        loop {
            while seen < self.notifications.len() {
                let n = &self.notifications[seen];
                seen = seen.checked_add(1).unwrap();
                if n.method == "textDocument/publishDiagnostics"
                    && n.params["uri"] == uri
                    && accept(&n.params)
                {
                    return n.params.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "no publish of {}",
                path.display()
            );
            self.next();
        }
    }

    /// Every publish of `path` received so far.
    pub(super) fn publishes(&self, path: &Path) -> Vec<Value> {
        let uri = uri(path);
        self.notifications
            .iter()
            .filter(|n| n.method == "textDocument/publishDiagnostics" && n.params["uri"] == uri)
            .map(|n| n.params.clone())
            .collect()
    }

    /// The settings in force.
    pub(super) fn settings(&mut self) -> Value {
        self.request("cheby/testSettings", Value::Null).unwrap()
    }

    /// The `window/logMessage` texts received so far.
    pub(super) fn logs(&self) -> Vec<String> {
        self.notifications
            .iter()
            .filter(|n| n.method == "window/logMessage")
            .filter_map(|n| n.params["message"].as_str().map(str::to_owned))
            .collect()
    }

    /// `shutdown`, then `exit`: the server ends with status 0.
    pub(super) fn stop(mut self) {
        assert_eq!(self.request("shutdown", Value::Null), Ok(Value::Null));
        self.notify("exit", Value::Null);
        let code = self.server.take().unwrap().join().unwrap().unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }
}

/// The URI the server uses for `path`.
pub(super) fn uri(path: &Path) -> Value {
    json!(path_to_uri(path).unwrap().as_str())
}

/// A package in a fresh temporary directory, removed on drop.
pub(super) struct Temp(pub(super) PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// A package `name` with a manifest and these files.
    pub(super) fn package(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("cheby.toml"), MANIFEST).unwrap();
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self(crate::text::uri::normalize(
            &std::path::absolute(dir).unwrap(),
        ))
    }

    pub(super) fn at(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }

    /// `initialize` parameters with this package as the root.
    pub(super) fn params(&self, extra: &Value) -> Value {
        let mut params = json!({
            "capabilities": {},
            "rootUri": uri(&self.0),
            "initializationOptions": { "analysis": { "debounceMs": 0 } },
        });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                params.insert(key.clone(), value.clone());
            }
        }
        params
    }
}

pub(super) const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
pub(super) const MAIN: &str = "//! Root.\n\nfn main() {\n  let _ = 1\n}\n";
pub(super) const MAIN_BAD: &str = "//! Root.\n\nfn main() {\n  let x: Int = \"no\"\n}\n";
/// A clean module.
pub(super) const B: &str = "//! B.\n\npub fn g() -> Int {\n  2\n}\n";
/// A module with a type error.
pub(super) const BAD: &str = "//! Bad.\n\npub fn f() -> Int {\n  \"x\"\n}\n";

/// The codes of a publish's diagnostics.
pub(super) fn codes(publish: &Value) -> Vec<String> {
    publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect()
}

pub(super) fn has_error(publish: &Value) -> bool {
    publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["severity"] == 1)
}

pub(super) fn is_empty(publish: &Value) -> bool {
    publish["diagnostics"].as_array().is_some_and(Vec::is_empty)
}

pub(super) fn open(client: &Client, path: &Path, text: &str, version: i32) {
    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri(path), "languageId": "cheby", "version": version, "text": text,
        }}),
    );
}

/// Replaces the whole text of an open document.
pub(super) fn change(client: &Client, path: &Path, text: &str, version: i32) {
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri(path), "version": version },
            "contentChanges": [{ "text": text }],
        }),
    );
}

/// The capabilities the server advertises with `encoding`, the same for
/// every client (plan §1.10).
fn advertised(encoding: &str) -> Value {
    json!({
        "positionEncoding": encoding,
        "textDocumentSync": {
            "openClose": true,
            "change": 2,
            "save": { "includeText": false },
        },
        "selectionRangeProvider": true,
        "documentSymbolProvider": true,
        "documentFormattingProvider": true,
        "documentRangeFormattingProvider": true,
        "foldingRangeProvider": true,
        "hoverProvider": true,
        "completionProvider": {
            "triggerCharacters": [".", ":", "@", ">"],
            "resolveProvider": true,
        },
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
        "inlayHintProvider": { "resolveProvider": false },
        "definitionProvider": true,
        "declarationProvider": true,
        "typeDefinitionProvider": true,
        "referencesProvider": true,
        "documentHighlightProvider": true,
        "callHierarchyProvider": true,
        "documentLinkProvider": { "resolveProvider": false },
        "workspaceSymbolProvider": true,
        "semanticTokensProvider": {
            "legend": {
                "tokenTypes": [
                    "namespace", "type", "enum", "struct", "interface", "typeParameter",
                    "parameter", "variable", "property", "enumMember", "function", "method",
                ],
                "tokenModifiers": [
                    "declaration", "deprecated", "defaultLibrary", "async", "implicitDyn",
                ],
            },
            "full": true,
            "range": true,
        },
        "workspace": {
            "workspaceFolders": { "supported": true, "changeNotifications": true },
        },
    })
}

#[test]
fn capabilities_with_empty_client() {
    let (client, result) = Client::start(json!({ "capabilities": {} }), Vec::new());
    assert_eq!(
        result,
        json!({
            "capabilities": advertised("utf-16"),
            "serverInfo": { "name": "cheby", "version": env!("CARGO_PKG_VERSION") },
        })
    );
    client.stop();
}

#[test]
fn capabilities_with_full_client() {
    let full = |encodings: Value| {
        json!({
            "processId": null,
            "capabilities": {
                "general": { "positionEncodings": encodings },
                "workspace": {
                    "configuration": true,
                    "didChangeConfiguration": { "dynamicRegistration": true },
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                    "workspaceFolders": true,
                },
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true, "versionSupport": true },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                },
                "window": { "workDoneProgress": true },
                "experimental": { "chebyIdle": true },
            },
            "workspaceFolders": null,
        })
    };
    for (offered, chosen) in [
        (json!(["utf-16", "utf-32", "utf-8"]), "utf-8"),
        (json!(["utf-16", "utf-32"]), "utf-32"),
        (json!(["utf-16"]), "utf-16"),
    ] {
        let (mut client, result) = Client::start(full(offered), Vec::new());
        assert_eq!(result["capabilities"], advertised(chosen));
        // A client that can register is asked for its settings changes.
        client.wait_request("client/registerCapability");
        client.stop();
    }
}

/// D-1376, D-1514: a value of the wrong type keeps what the earlier layers
/// gave the key: the default under `initializationOptions`, and the
/// `initializationOptions` value under a configuration push.
#[test]
fn bad_setting_keeps_earlier_layers() {
    let options = json!({
        "analysis": { "debounceMs": "fast", "requestWaitMs": 5000 },
        "inlayHints": { "enable": 3 },
        "ownership": { "lens": "sometimes" },
        "lenses": 7,
    });
    let (mut client, _) = Client::start(
        json!({ "capabilities": {}, "initializationOptions": options }),
        Vec::new(),
    );
    let settings = client.settings();
    assert_eq!(settings["analysis"]["debounceMs"], 40);
    assert_eq!(settings["analysis"]["requestWaitMs"], 5000);
    assert_eq!(settings["inlayHints"]["enable"], true);
    assert_eq!(settings["ownership"]["lens"], "hot");
    assert_eq!(settings["lenses"]["enable"], true);
    let logs = client.logs();
    for key in [
        "analysis.debounceMs",
        "inlayHints.enable",
        "ownership.lens",
        "`lenses`",
    ] {
        assert!(
            logs.iter().any(|l| l.contains(key)),
            "no warning for {key}: {logs:?}"
        );
    }
    let pushed = json!({ "analysis": { "debounceMs": 9, "requestWaitMs": "slow" } });
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": pushed } }),
    );
    let settings = client.settings();
    assert_eq!(settings["analysis"]["debounceMs"], 9);
    assert_eq!(settings["analysis"]["requestWaitMs"], 5000);
    let logs = client.logs();
    assert!(
        logs.iter().any(|l| l.contains("analysis.requestWaitMs")),
        "no warning for analysis.requestWaitMs: {logs:?}"
    );
    client.stop();
}

#[test]
fn unknown_setting_is_ignored() {
    let options = json!({
        "analysis": { "debounceMs": 7, "colour": "red" },
        "nope": { "x": 1 },
    });
    let (mut client, _) = Client::start(
        json!({ "capabilities": {}, "initializationOptions": options }),
        Vec::new(),
    );
    let settings = client.settings();
    assert_eq!(settings["analysis"]["debounceMs"], 7);
    let mut expected = serde_json::to_value(super::settings::Settings::default()).unwrap();
    expected["analysis"]["debounceMs"] = json!(7);
    assert_eq!(settings, expected);
    let logs = client.logs();
    for key in ["analysis.colour", "`nope`"] {
        let found = logs.iter().filter(|l| l.contains(key)).count();
        assert_eq!(found, 1, "one warning for {key}: {logs:?}");
    }
    client.stop();
}

#[test]
fn settings_precedence() {
    let options = json!({ "analysis": { "debounceMs": 5, "requestWaitMs": 200 } });
    let answer =
        json!({ "analysis": { "debounceMs": 7 }, "constants": { "childTimeoutSeconds": 0 } });
    let (mut client, _) = Client::start(
        json!({
            "capabilities": { "workspace": { "configuration": true } },
            "initializationOptions": options,
        }),
        vec![answer],
    );
    client.wait_request("workspace/configuration");
    let settings = client.settings();
    // The configuration wins over initializationOptions per key, which
    // wins over the defaults.
    assert_eq!(settings["analysis"]["debounceMs"], 7);
    assert_eq!(settings["analysis"]["requestWaitMs"], 200);
    assert_eq!(settings["diagnostics"]["explainLinks"], true);
    // Out-of-range values are clamped.
    assert_eq!(settings["constants"]["childTimeoutSeconds"], 1);
    client.stop();
}

#[test]
fn null_configuration_keeps_settings() {
    // A client that answers `workspace/configuration`.
    let (mut client, _) = Client::start(
        json!({
            "capabilities": { "workspace": { "configuration": true } },
            "initializationOptions": { "analysis": { "debounceMs": 5 } },
        }),
        vec![json!({ "analysis": { "debounceMs": 9 } }), Value::Null],
    );
    client.wait_request("workspace/configuration");
    assert_eq!(client.settings()["analysis"]["debounceMs"], 9);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    client.wait_request("workspace/configuration");
    assert_eq!(client.settings()["analysis"]["debounceMs"], 9);
    client.stop();

    // A client that pushes its settings.
    let (mut client, _) = Client::start(json!({ "capabilities": {} }), Vec::new());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "analysis": { "debounceMs": 11 } } } }),
    );
    assert_eq!(client.settings()["analysis"]["debounceMs"], 11);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": null } }),
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": {} }),
    );
    assert_eq!(client.settings()["analysis"]["debounceMs"], 11);
    client.stop();
}

#[test]
fn inlay_settings_refresh_the_clients_hints() {
    // A §12.3: a change of `inlayHints.*` asks a client that can take it
    // for its hints again.
    let (mut client, _) = Client::start(
        json!({ "capabilities": { "workspace": { "inlayHint": { "refreshSupport": true } } } }),
        Vec::new(),
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "inlayHints": { "enable": false } } } }),
    );
    let refresh = client.wait_request("workspace/inlayHint/refresh");
    assert!(refresh.params.is_null(), "{refresh:?}");
    assert_eq!(client.settings()["inlayHints"]["enable"], false);
    client.stop();
}

#[test]
fn closed_file_published_at_start() {
    let package = Temp::package(
        "closed",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    // Nothing is open: the workspace folder's session publishes every
    // file of the package, with no version.
    let bad = client.wait_publish(&package.at("src/bad.cheby"), |_| true);
    assert!(has_error(&bad), "{bad}");
    assert!(bad.get("version").is_none_or(Value::is_null), "{bad}");
    let main = client.wait_publish(&package.at("src/main.cheby"), |_| true);
    assert!(!has_error(&main), "{main}");
    client.wait_publish(&package.at("cheby.toml"), |_| true);
    client.stop();
}

#[test]
fn deleted_file_gets_one_empty_publish() {
    let package = Temp::package(
        "deleted",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let main = package.at("src/main.cheby");
    let bad = package.at("src/bad.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    client.wait_publish(&bad, has_error);
    open(&client, &main, MAIN, 1);
    client.wait_publish(&main, |p| p["version"] == 1);

    std::fs::remove_file(&bad).unwrap();
    client.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri(&main) } }),
    );
    client.wait_publish(&bad, is_empty);
    // Later runs publish nothing more for it.
    change(&client, &main, MAIN_BAD, 2);
    client.wait_publish(&main, |p| p["version"] == 2);
    change(&client, &main, MAIN, 3);
    client.wait_publish(&main, |p| p["version"] == 3);
    let publishes = client.publishes(&bad);
    let empty: Vec<&Value> = publishes.iter().filter(|p| is_empty(p)).collect();
    assert_eq!(empty.len(), 1, "{publishes:?}");
    assert!(is_empty(publishes.last().unwrap()), "{publishes:?}");
    client.stop();
}

#[test]
fn typing_republishes_with_new_version() {
    let package = Temp::package("typing", &[("src/main.cheby", MAIN), ("src/b.cheby", B)]);
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &main, MAIN_BAD, 1);
    let first = client.wait_publish(&main, |p| p["version"] == 1);
    assert!(has_error(&first), "{first}");
    change(&client, &main, MAIN, 2);
    let fixed = client.wait_publish(&main, |p| p["version"] == 2);
    assert!(is_empty(&fixed), "{fixed}");
    // The same list again, and the same text again: each version is still
    // published, which is what a client waits for.
    change(&client, &main, &format!("{MAIN}\n"), 3);
    assert!(is_empty(&client.wait_publish(&main, |p| p["version"] == 3)));
    change(&client, &main, MAIN, 4);
    assert!(is_empty(&client.wait_publish(&main, |p| p["version"] == 4)));
    // A broken text publishes its phase-A errors, and typing on in it
    // republishes with each version.
    change(&client, &main, "//! Root.\n\nfn main() {\n", 5);
    let broken = client.wait_publish(&main, |p| p["version"] == 5);
    assert!(has_error(&broken), "{broken}");
    change(&client, &main, "//! Root.\n\nfn main() {\n", 6);
    let again = client.wait_publish(&main, |p| p["version"] == 6);
    assert_eq!(codes(&again), codes(&broken));
    // While another file is broken, a new version of a text already
    // checked gives no new report, since the strict inputs did not change:
    // its list is published again with the new version.
    let b = package.at("src/b.cheby");
    change(&client, &main, MAIN, 7);
    assert!(is_empty(&client.wait_publish(&main, |p| p["version"] == 7)));
    open(&client, &b, "//! B.\n\npub fn g() -> Int {\n", 1);
    assert!(has_error(&client.wait_publish(&b, |p| p["version"] == 1)));
    let before = client.publishes(&main).len();
    change(&client, &main, MAIN, 8);
    let caught_up = client.wait_publish(&main, |p| p["version"] == 8);
    assert!(is_empty(&caught_up), "{caught_up}");
    assert_eq!(client.publishes(&main).len(), before + 1);
    client.stop();
}

#[test]
fn manifest_error_still_publishes_open_module() {
    let package = Temp::package("manifest", &[("src/main.cheby", MAIN)]);
    std::fs::write(package.at("cheby.toml"), "name = \"app\"\nversion = 1\n").unwrap();
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    // The run publishes only the manifest's errors, yet the open module
    // still gets a publish with its version (D-1373).
    open(&client, &main, MAIN, 1);
    let published = client.wait_publish(&main, |p| p["version"] == 1);
    assert!(is_empty(&published), "{published}");
    let manifest = client.wait_publish(&package.at("cheby.toml"), has_error);
    assert!(
        manifest.get("version").is_none_or(Value::is_null),
        "{manifest}"
    );
    client.stop();
}

#[test]
fn exit_without_shutdown_ends_with_one() {
    let (client, _) = Client::start(json!({ "capabilities": {} }), Vec::new());
    client.notify("exit", Value::Null);
    let mut client = client;
    let code = client.server.take().unwrap().join().unwrap().unwrap();
    assert_eq!(code, ExitCode::FAILURE);
}

#[test]
fn second_initialize_is_invalid_request() {
    let (mut client, _) = Client::start(json!({ "capabilities": {} }), Vec::new());
    let answer = client.request("initialize", json!({ "capabilities": {} }));
    assert_eq!(answer, Err(-32_600));
    client.stop();
}

/// A client with `experimental.chebyIdle` gets `cheby/idle` once the
/// package's first job is done, and `cheby/status` lists the package. In
/// process no child runs (`ServerOptions::child` is `None`), so idle
/// follows the analysis job alone (D-1415). The payload is `{ root,
/// generation }`: the package root as a URI, and the generation of its
/// newest snapshot, the one `cheby/status` names (D-1492).
#[test]
fn idle_follows_the_first_job_and_status_lists_the_package() {
    let package = Temp::package("idle", &[("src/main.cheby", MAIN)]);
    let params = package.params(&json!({
        "capabilities": { "experimental": { "chebyIdle": true } },
    }));
    let (mut client, _) = Client::start(params, Vec::new());
    let root = uri(&package.at(""));
    let root = root.as_str().unwrap().trim_end_matches('/').to_owned();
    let generation = loop {
        if let Message::Notification(note) = client.next()
            && note.method == "cheby/idle"
        {
            let mut keys: Vec<&String> = note.params.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["generation", "root"], "{note:?}");
            assert_eq!(note.params["root"], root.as_str(), "{note:?}");
            break note.params["generation"].as_u64().unwrap();
        }
    };
    assert!(generation >= 1, "the first job made a snapshot");
    let status = client.request("cheby/status", Value::Null).unwrap();
    assert_eq!(status["packages"][0]["generation"], generation, "{status}");
    assert_eq!(status["idleNotification"], true, "{status}");
    let packages = status["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 1, "{status}");
    assert_eq!(packages[0]["kind"], "package", "{status}");
    assert_eq!(packages[0]["run"], "idle", "{status}");
    let only = client
        .request("cheby/status", json!({ "root": root }))
        .unwrap();
    assert_eq!(only["packages"], status["packages"]);
    assert_eq!(client.request("cheby/status", json!(3)), Err(-32_602));
    client.stop();
}

/// D-1495: a change of `analysis.target` rebuilds the session, whose
/// snapshots number from 1 again, and the generation of `cheby/idle` and
/// `cheby/status` still grows.
#[test]
fn idle_generation_grows_across_a_target_change() {
    let package = Temp::package("idle_target", &[("src/main.cheby", MAIN)]);
    let params = package.params(&json!({
        "capabilities": { "experimental": { "chebyIdle": true } },
    }));
    let (mut client, _) = Client::start(params, Vec::new());
    let next_idle = |client: &mut Client| loop {
        if let Message::Notification(note) = client.next()
            && note.method == "cheby/idle"
        {
            break note.params["generation"].as_u64().unwrap();
        }
    };
    let before = next_idle(&mut client);
    assert!(before >= 1, "the first job made a snapshot");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "analysis": { "target": "js" } } } }),
    );
    let after = next_idle(&mut client);
    assert!(after > before, "{before} then {after}");
    let status = client.request("cheby/status", Value::Null).unwrap();
    assert_eq!(status["packages"][0]["generation"], after, "{status}");
    client.stop();
}

/// D-1374, D-1494: for a client with no watcher, a sweep that finds a
/// change on disk, here a new module, reruns the constants as a watched
/// change does.
#[test]
#[cfg(unix)]
fn a_swept_change_reruns_the_constants() {
    use std::os::unix::fs::PermissionsExt as _;

    const SOURCE: &str =
        "//! Root.\n\nconst limit: Int = 20\n\nfn main() {\n  assert limit > 0\n}\n";
    let package = Temp::package("swept_rerun", &[("src/main.cheby", SOURCE)]);
    let tools = Temp::package("swept_rerun_tools", &[]);
    let runs = tools.at("runs.txt");
    let exe = tools.at("fake-cheby");
    std::fs::write(
        &exe,
        format!("#!/bin/sh\necho run >> '{}'\n", runs.display()),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let count = || std::fs::read_to_string(&runs).map_or(0, |text| text.lines().count());
    // No `didChangeWatchedFiles`: the server sweeps (D-1374).
    let params = package.params(&json!({
        "capabilities": { "experimental": { "chebyIdle": true } },
    }));
    let (mut client, _) = Client::start_with(params, Vec::new(), Some(exe));
    open(&client, &package.at("src/main.cheby"), SOURCE, 1);
    let next_idle = |client: &mut Client| loop {
        if let Message::Notification(note) = client.next()
            && note.method == "cheby/idle"
        {
            break note.params["generation"].as_u64().unwrap();
        }
    };
    let mut first = next_idle(&mut client);
    while count() == 0 {
        first = next_idle(&mut client);
    }
    assert_eq!(count(), 1, "the first clean report's run");
    std::fs::write(package.at("src/other.cheby"), "//! Other.\n").unwrap();
    let mut after = next_idle(&mut client);
    while after <= first {
        after = next_idle(&mut client);
    }
    assert_eq!(count(), 2, "the sweep's change reran the constants");
    client.stop();
}

/// A client without `experimental.chebyIdle` never gets `cheby/idle`.
#[test]
fn no_idle_without_the_capability() {
    let package = Temp::package("no_idle", &[("src/main.cheby", MAIN)]);
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    client.wait_publish(&package.at("src/main.cheby"), |_| true);
    // Answered after every notification sent before it.
    client.settings();
    assert!(
        client
            .notifications
            .iter()
            .all(|n| n.method != "cheby/idle"),
        "{:?}",
        client.notifications
    );
    client.stop();
}
