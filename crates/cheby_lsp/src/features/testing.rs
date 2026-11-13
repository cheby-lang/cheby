//! Requests that only tests send, one per request class, so the tests of
//! `server::request_tests` can see what a handler saw (A §3.8).

#![expect(
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::panic_in_result_fn,
    reason = "test handlers have the route's signature"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lsp_types::{MessageType, Uri};
use serde_json::{Value, json};

use super::{Analyzed, Class, DocText, HandlerOutput, RequestCx, RequestError, Route};
use crate::server::Effect;
use crate::text::uri::{normalize, uri_to_path};

/// The fallback of every test route, so a test can tell it from a value.
fn fallback() -> Value {
    json!("fallback")
}

/// The test routes.
pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "cheby/testText",
        class: Class::Text(text),
        fallback,
        edits: false,
    },
    Route {
        method: "cheby/testDoc",
        class: Class::Doc(doc),
        fallback,
        edits: false,
    },
    Route {
        method: "cheby/testPackage",
        class: Class::Package(package),
        fallback,
        edits: false,
    },
    Route {
        method: "cheby/testPanic",
        class: Class::Text(panics),
        fallback,
        edits: false,
    },
    Route {
        method: "cheby/testWaitCancel",
        class: Class::Text(wait_cancel),
        fallback,
        edits: false,
    },
];

/// The path a URI string names.
fn path(uri: &Value) -> PathBuf {
    let uri: Uri = uri.as_str().unwrap().parse().unwrap();
    normalize(&uri_to_path(&uri).unwrap())
}

/// `cheby/testText`: the live text the handler read, and its context.
fn text(cx: &RequestCx<'_>, doc: &DocText, _params: Value) -> Result<HandlerOutput, RequestError> {
    Ok(HandlerOutput::value(json!({
        "version": doc.version,
        "text": &*doc.text,
        "errors": doc.errors.len(),
        "lines": doc.lines.line_count(),
        "nodes": doc.tree.root().children().count(),
        "path": doc.path,
        "encoding": format!("{:?}", cx.encoding),
        "debounceMs": cx.settings.analysis.debounce_ms,
        "configuration": cx.caps.configuration,
    })))
}

/// `cheby/testDoc`: the snapshot's version of the document, after a
/// `window/logMessage` effect.
fn doc(
    _cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params: Value,
) -> Result<HandlerOutput, RequestError> {
    let path = path(&params["textDocument"]["uri"]);
    let version = analyzed.snapshot.version_of(&path);
    let _ = &analyzed.memo;
    Ok(HandlerOutput {
        value: json!({ "version": version }),
        effects: vec![Effect::log(MessageType::INFO, "cheby/testDoc ran")],
    })
}

/// `cheby/testPackage`: the snapshot's versions of `params.uris`.
fn package(
    _cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params: Value,
) -> Result<HandlerOutput, RequestError> {
    let versions: Vec<Option<i32>> = params["uris"]
        .as_array()
        .unwrap()
        .iter()
        .map(|uri| analyzed.snapshot.version_of(&path(uri)))
        .collect();
    Ok(HandlerOutput::value(json!({ "versions": versions })))
}

/// `cheby/testPanic`: a handler with a bug.
fn panics(
    _cx: &RequestCx<'_>,
    _doc: &DocText,
    _params: Value,
) -> Result<HandlerOutput, RequestError> {
    panic!("a handler panicked on purpose");
}

/// `cheby/testWaitCancel`: runs until the request is cancelled, checking
/// its token as a long handler does, or answers after a minute.
fn wait_cancel(
    cx: &RequestCx<'_>,
    _doc: &DocText,
    _params: Value,
) -> Result<HandlerOutput, RequestError> {
    let end = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
    while Instant::now() < end {
        cx.cancel.check()?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(HandlerOutput::value(json!("not cancelled")))
}
