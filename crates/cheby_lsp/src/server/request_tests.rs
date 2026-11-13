//! In-process tests of request dispatch through `run_on` (A §3.8, §4):
//! parking until the exact text is analyzed (D-1367), `ContentModified`,
//! `RequestFailed` after `analysis.requestWaitMs`, fallbacks, cancellation,
//! effects before the response, and a handler's panic contained.
//!
//! The test requests of `features::testing` stand in for real handlers,
//! and `cheby/testHold` holds the analysis thread, so a test decides which
//! text the next job reads and needs no clock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::JoinHandle;
use std::time::Duration;

use cheby_driver::cache::BuildId;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};

use super::codes;
use crate::text::uri::{normalize, path_to_uri};
use crate::{ServerOptions, run_on};

/// How long a test waits for one message: the first run checks std.
const WAIT: Duration = Duration::from_secs(120);

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
const MAIN: &str = "//! Root.\n\nfn main() {\n  let _ = 1\n}\n";
const B: &str = "//! B.\n\npub fn g() -> Int {\n  2\n}\n";

/// A client talking to a server on its own thread.
struct Client {
    connection: Connection,
    server: Option<JoinHandle<Result<ExitCode, String>>>,
    next_id: i32,
    /// Responses received and not yet taken, by request.
    responses: BTreeMap<RequestId, Result<Value, i32>>,
    /// Every notification received, in order.
    notifications: Vec<Notification>,
    /// The number of notifications received before each response, by
    /// request.
    seen_before: BTreeMap<RequestId, usize>,
}

impl Client {
    /// Starts a server on `root` with these `initializationOptions`.
    fn start(root: &Path, options: &Value) -> Self {
        let (server_side, client_side) = Connection::memory();
        let server_options = ServerOptions {
            build: BuildId([0; 16]),
            child: None,
        };
        let server = std::thread::Builder::new()
            .name("cheby-test-server".to_owned())
            .stack_size(256 << 20)
            .spawn(move || run_on(server_side, server_options))
            .unwrap();
        let mut client = Self {
            connection: client_side,
            server: Some(server),
            next_id: 0,
            responses: BTreeMap::new(),
            notifications: Vec::new(),
            seen_before: BTreeMap::new(),
        };
        let params = json!({
            "capabilities": {},
            "rootUri": uri(root),
            "initializationOptions": options,
        });
        client.request("initialize", params).unwrap();
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        let message = Notification::new(method.to_owned(), params);
        self.connection.sender.send(message.into()).unwrap();
    }

    /// Sends a request and returns its id, without waiting.
    fn send(&mut self, method: &str, params: Value) -> RequestId {
        self.next_id = self.next_id.checked_add(1).unwrap();
        let id = RequestId::from(self.next_id);
        let request = Request::new(id.clone(), method.to_owned(), params);
        self.connection.sender.send(request.into()).unwrap();
        id
    }

    /// Waits for the answer to `id`, handling everything before it.
    fn answer(&mut self, id: &RequestId) -> Result<Value, i32> {
        loop {
            if let Some(result) = self.responses.remove(id) {
                return result;
            }
            self.next();
        }
    }

    /// Sends a request and waits for its answer.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, i32> {
        let id = self.send(method, params);
        self.answer(&id)
    }

    /// Reads one message, answering a request of the server.
    fn next(&mut self) {
        let message = self
            .connection
            .receiver
            .recv_timeout(WAIT)
            .expect("the server sent nothing in time");
        match message {
            Message::Request(request) => {
                let result = match request.method.as_str() {
                    "workspace/configuration" => json!([null]),
                    _ => Value::Null,
                };
                let response = Response::new_ok(request.id, result);
                self.connection.sender.send(response.into()).unwrap();
            }
            Message::Notification(notification) => self.notifications.push(notification),
            Message::Response(response) => {
                self.seen_before
                    .insert(response.id.clone(), self.notifications.len());
                let result = response.response_result.map_err(|e| e.code);
                self.responses.insert(response.id, result);
            }
        }
    }

    /// `didOpen` of `path` at `version`.
    fn open(&self, path: &Path, text: &str, version: i32) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri(path), "languageId": "cheby", "version": version, "text": text,
            }}),
        );
    }

    /// A whole-text `didChange` of `path` to `version`.
    fn change(&self, path: &Path, text: &str, version: i32) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri(path), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// `didClose` of `path`.
    fn close(&self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri(path) } }),
        );
    }

    /// Holds or releases the analysis thread.
    fn hold(&mut self, hold: bool) {
        let answer = self.request("cheby/testHold", json!({ "hold": hold }));
        assert_eq!(answer, Ok(Value::Null));
    }

    /// Sends `cheby/testDoc` on `path` without waiting.
    fn send_doc(&mut self, path: &Path) -> RequestId {
        self.send(
            "cheby/testDoc",
            json!({ "textDocument": { "uri": uri(path) } }),
        )
    }

    /// The version of `path` in the snapshot that answers `cheby/testDoc`.
    fn doc_version(&mut self, path: &Path) -> Result<Value, i32> {
        let id = self.send_doc(path);
        self.answer(&id).map(|value| value["version"].clone())
    }

    /// The `window/logMessage` texts received so far.
    fn logs(&self) -> Vec<String> {
        self.notifications
            .iter()
            .filter(|n| n.method == "window/logMessage")
            .filter_map(|n| n.params["message"].as_str().map(str::to_owned))
            .collect()
    }

    /// `shutdown`, then `exit`: the server ends with status 0.
    fn stop(mut self) {
        assert_eq!(self.request("shutdown", Value::Null), Ok(Value::Null));
        self.notify("exit", Value::Null);
        let code = self.server.take().unwrap().join().unwrap().unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }
}

/// The URI the server uses for `path`.
fn uri(path: &Path) -> Value {
    json!(path_to_uri(path).unwrap().as_str())
}

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// A package `name` with `manifest` and these files.
    fn package(name: &str, manifest: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_req_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("cheby.toml"), manifest).unwrap();
        for (path, text) in files {
            std::fs::write(dir.join(path), text).unwrap();
        }
        Self(normalize(&std::path::absolute(dir).unwrap()))
    }

    fn at(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

/// `initializationOptions` with no debounce.
fn quick() -> Value {
    json!({ "analysis": { "debounceMs": 0 } })
}

/// A package with `src/main.cheby` open at version 1 and analyzed.
fn opened(name: &str, options: &Value) -> (Temp, Client, PathBuf) {
    let temp = Temp::package(name, MANIFEST, &[("src/main.cheby", MAIN)]);
    let main = temp.at("src/main.cheby");
    let mut client = Client::start(&temp.0, options);
    client.open(&main, MAIN, 1);
    assert_eq!(client.doc_version(&main), Ok(json!(1)));
    (temp, client, main)
}

/// A request on a version that is not analyzed yet waits for it, and is
/// not answered from the snapshot it arrived with (A §4). The edit's
/// debounce never ends here, so only the request's flush starts the run.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn hover_waits_for_the_new_version() {
    let (_temp, mut client, main) = opened(
        "waits",
        &json!({ "analysis": { "debounceMs": 0, "requestWaitMs": 60_000 } }),
    );
    assert_eq!(
        client.request("cheby/testNoDebounce", Value::Null),
        Ok(Value::Null)
    );
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    assert_eq!(client.doc_version(&main), Ok(json!(2)));
    client.stop();
}

/// A request whose text was replaced before any run read it is answered
/// `ContentModified` once a run reads the newer text (D-1367).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn stale_request_gets_content_modified() {
    let (_temp, mut client, main) = opened("stale", &quick());
    client.hold(true);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    let id = client.send_doc(&main);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 3\n}\n", 3);
    client.hold(false);
    assert_eq!(client.answer(&id), Err(codes::CONTENT_MODIFIED));
    assert_eq!(client.doc_version(&main), Ok(json!(3)));
    client.stop();
}

/// A snapshot of version N answers a request parked at N, even when the
/// document reached N + 1 while the run was on (A §4).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn snapshot_n_answers_request_n_after_edit() {
    let (_temp, mut client, main) = opened("snapshot_n", &quick());
    client.hold(true);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    let id = client.send_doc(&main);
    // The job for version 2 starts as the hold ends, before the next edit
    // is read, since the loop schedules after each message.
    client.hold(false);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 3\n}\n", 3);
    assert_eq!(client.answer(&id), Ok(json!({ "version": 2 })));
    assert_eq!(client.doc_version(&main), Ok(json!(3)));
    client.stop();
}

/// A package request waits for every open document of the package, while
/// a document request on an unchanged document is answered at once from
/// the snapshot it has (A §3.3).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn references_waits_for_all_open_docs() {
    let temp = Temp::package(
        "package",
        MANIFEST,
        &[("src/main.cheby", MAIN), ("src/b.cheby", B)],
    );
    let main = temp.at("src/main.cheby");
    let b = temp.at("src/b.cheby");
    let mut client = Client::start(&temp.0, &quick());
    client.open(&main, MAIN, 1);
    client.open(&b, B, 1);
    assert_eq!(client.doc_version(&b), Ok(json!(1)));
    client.hold(true);
    client.change(&b, "//! B.\n\npub fn g() -> Int {\n  3\n}\n", 2);
    let uris = json!([uri(&main), uri(&b)]);
    let doc = client.send(
        "cheby/testDoc",
        json!({ "textDocument": { "uri": uri(&main) } }),
    );
    assert_eq!(client.answer(&doc), Ok(json!({ "version": 1 })));
    let package = client.send(
        "cheby/testPackage",
        json!({ "textDocument": { "uri": uri(&main) }, "uris": uris }),
    );
    client.hold(false);
    assert_eq!(client.answer(&package), Ok(json!({ "versions": [1, 2] })));
    client.stop();
}

/// A run that gives no snapshot answers the requests waiting on it with
/// their fallback (A §3.7): here the manifest was never usable. A request
/// on the same text then gets its fallback with no run, since that run
/// would fail again.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn failed_run_answers_fallback() {
    let temp = Temp::package("failed", "name = \n", &[("src/main.cheby", MAIN)]);
    let main = temp.at("src/main.cheby");
    let mut client = Client::start(&temp.0, &quick());
    client.open(&main, MAIN, 1);
    let id = client.send_doc(&main);
    assert_eq!(client.answer(&id), Ok(json!("fallback")));
    // The hold would keep a run from answering, so only the failed run's
    // texts can.
    client.hold(true);
    let id = client.send_doc(&main);
    assert_eq!(client.answer(&id), Ok(json!("fallback")));
    client.hold(false);
    client.stop();
}

/// A panic in a run loses its result only (R-14): the request waiting on
/// that run gets its fallback, even when an edit came during the run, and
/// the next run answers again (A §3.7, D-1362).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn panic_in_run_keeps_serving() {
    let (_temp, mut client, main) = opened("panic_run", &quick());
    client.hold(true);
    assert_eq!(
        client.request("cheby/testPanicRun", Value::Null),
        Ok(Value::Null)
    );
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    let id = client.send_doc(&main);
    // The job of version 2 starts as the hold ends, before the next edit
    // is read, so a run of version 3 follows the one that panics.
    client.hold(false);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 3\n}\n", 3);
    assert_eq!(client.answer(&id), Ok(json!("fallback")));
    assert!(
        client
            .logs()
            .iter()
            .any(|log| log.contains("a run panicked on purpose")),
        "{:?}",
        client.logs()
    );
    assert_eq!(client.doc_version(&main), Ok(json!(3)));
    client.stop();
}

/// A text request reads the live text, so it is answered while no run can
/// start (D-1367).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn text_request_never_waits() {
    let (temp, mut client, main) = opened("text", &quick());
    client.hold(true);
    let text = "//! Root.\n\nfn main( {\n";
    client.change(&main, text, 2);
    let answer = client
        .request(
            "cheby/testText",
            json!({ "textDocument": { "uri": uri(&main) } }),
        )
        .unwrap();
    assert_eq!(answer["version"], 2);
    assert_eq!(answer["text"], text);
    assert_eq!(answer["path"], json!(main));
    assert!(answer["errors"].as_u64().unwrap() > 0, "{answer}");
    assert_eq!(answer["debounceMs"], 0);
    // A document that is not open has no live text: the fallback.
    let closed = client.request(
        "cheby/testText",
        json!({ "textDocument": { "uri": uri(&temp.at("src/none.cheby")) } }),
    );
    assert_eq!(closed, Ok(json!("fallback")));
    client.hold(false);
    client.stop();
}

/// A handler that panics is answered `InternalError` and logged, and the
/// query threads go on: more panics than threads, then a request that
/// works (A §3.7).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn panic_in_handler_answers_internal_error() {
    let (_temp, mut client, main) = opened("panic", &quick());
    let params = json!({ "textDocument": { "uri": uri(&main) } });
    for _ in 0..6 {
        let answer = client.request("cheby/testPanic", params.clone());
        assert_eq!(answer, Err(codes::INTERNAL_ERROR));
    }
    assert!(
        client
            .logs()
            .iter()
            .any(|log| log.contains("cheby/testPanic") && log.contains("panicked on purpose")),
        "{:?}",
        client.logs()
    );
    let answer = client.request("cheby/testText", params).unwrap();
    assert_eq!(answer["version"], 1);
    assert_eq!(client.doc_version(&main), Ok(json!(1)));
    client.stop();
}

/// A wait past `analysis.requestWaitMs` is `RequestFailed` (D-1367).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn wait_past_request_wait_ms_is_request_failed() {
    let (_temp, mut client, main) = opened("timeout", &quick());
    // The first run, which checks std, may take longer than the wait, so
    // the wait is shortened after it. The hold that follows is read after
    // the settings, so they are in force for the request.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": {
            "analysis": { "debounceMs": 0, "requestWaitMs": 100 },
        }}}),
    );
    client.hold(true);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    assert_eq!(client.doc_version(&main), Err(codes::REQUEST_FAILED));
    // A run in a debug build may itself take 100 ms, so the default wait
    // comes back before the run that answers.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "analysis": { "debounceMs": 0 } } } }),
    );
    client.hold(false);
    assert_eq!(client.doc_version(&main), Ok(json!(2)));
    client.stop();
}

/// `$/cancelRequest` of a parked request answers it `RequestCancelled` at
/// once (A §3.5).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn cancel_of_parked_request_answers_cancelled() {
    let (_temp, mut client, main) = opened("cancel_parked", &quick());
    client.hold(true);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    let id = client.send_doc(&main);
    client.notify("$/cancelRequest", json!({ "id": id }));
    assert_eq!(client.answer(&id), Err(codes::REQUEST_CANCELLED));
    client.hold(false);
    client.stop();
}

/// `$/cancelRequest` of a request on the query pool sets its token, and
/// the handler's next check ends it `RequestCancelled` (A §3.5).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn cancel_of_running_request_answers_cancelled() {
    let (_temp, mut client, main) = opened("cancel_running", &quick());
    let id = client.send(
        "cheby/testWaitCancel",
        json!({ "textDocument": { "uri": uri(&main) } }),
    );
    client.notify("$/cancelRequest", json!({ "id": id }));
    assert_eq!(client.answer(&id), Err(codes::REQUEST_CANCELLED));
    // An unknown or answered id is ignored.
    client.notify("$/cancelRequest", json!({ "id": id }));
    client.notify("$/cancelRequest", json!({ "id": "nothing" }));
    assert_eq!(client.doc_version(&main), Ok(json!(1)));
    client.stop();
}

/// A handler's effects are sent before its response (plan §1.5).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn effects_arrive_before_the_response() {
    let (_temp, mut client, main) = opened("effects", &quick());
    let id = client.send_doc(&main);
    assert_eq!(client.answer(&id), Ok(json!({ "version": 1 })));
    let before = client.seen_before[&id];
    let log = client.notifications[..before]
        .iter()
        .rposition(|n| n.params["message"] == "cheby/testDoc ran");
    assert!(log.is_some(), "no effect before the response");
    client.stop();
}

/// `shutdown` answers every parked request `RequestCancelled` before its
/// own answer (A §3.2).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn shutdown_cancels_parked_requests() {
    let (_temp, mut client, main) = opened("shutdown", &quick());
    client.hold(true);
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 2\n}\n", 2);
    let parked = client.send_doc(&main);
    let shutdown = client.send("shutdown", Value::Null);
    assert_eq!(client.answer(&shutdown), Ok(Value::Null));
    assert_eq!(
        client.responses.remove(&parked),
        Some(Err(codes::REQUEST_CANCELLED)),
        "the parked request was not answered first"
    );
    client.notify("exit", Value::Null);
    let code = client.server.take().unwrap().join().unwrap().unwrap();
    assert_eq!(code, ExitCode::SUCCESS);
}

/// A reopened document starts at version 0 again, as Zed does on every
/// move and reopen (cp-notes, CP1). A snapshot of the old text, version
/// 8, neither answers nor stales a request on the new version 0: requests
/// name texts by edit stamp, not by version.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn reopened_document_waits_for_its_own_text() {
    let (_temp, mut client, main) = opened("reopen", &quick());
    // The job of version 8 starts at once; the hold keeps the next one.
    client.change(&main, "//! Root.\n\nfn main() {\n  let _ = 8\n}\n", 8);
    client.hold(true);
    client.close(&main);
    client.open(&main, "//! Root.\n\nfn main() {\n  let _ = 0\n}\n", 0);
    let id = client.send_doc(&main);
    client.hold(false);
    assert_eq!(client.answer(&id), Ok(json!({ "version": 0 })));
    client.stop();
}

/// A request with no document URI is `InvalidParams`, never a panic
/// (A §3.3), and a document outside every package, or a URI that names no
/// file, gets the fallback.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn malformed_and_unrouted_requests() {
    let (temp, mut client, _main) = opened("params", &quick());
    let answer = client.request("cheby/testDoc", json!({ "textDocument": 3 }));
    assert_eq!(answer, Err(codes::INVALID_PARAMS));
    let outside = temp.0.parent().unwrap().join("loose.cheby");
    let answer = client.request(
        "cheby/testDoc",
        json!({ "textDocument": { "uri": uri(&outside) } }),
    );
    assert_eq!(answer, Ok(json!("fallback")));
    let answer = client.request(
        "cheby/testDoc",
        json!({ "textDocument": { "uri": "untitled:Untitled-1" } }),
    );
    assert_eq!(answer, Ok(json!("fallback")));
    client.stop();
}
