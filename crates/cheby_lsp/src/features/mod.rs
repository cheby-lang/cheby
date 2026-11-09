//! Language features (A §3.3, plan §1.5): the code that may hold generation
//! handles (D-1366).
//!
//! A request that the main loop does not answer itself has a [`Route`],
//! listed in `server::routes`, whose [`Class`] says what it reads:
//!
//! - [`Class::Text`]: the document's live text and its own parse, a
//!   [`DocText`]. It never waits, since the text is there (D-1367).
//! - [`Class::Doc`]: a snapshot of the document's exact text (A §4).
//! - [`Class::Package`]: a snapshot of the exact text of every open
//!   document of the package, for answers that span files.
//!
//! A handler runs on the query pool (`server::pool`), never on the main
//! loop. It gets a [`RequestCx`] and returns a [`HandlerOutput`]: a value
//! and the effects the main loop applies before it sends the response
//! (D-1364). Handlers never write to the connection themselves.

pub(crate) mod handlers;
pub(crate) mod ide;
#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Placed, PlainDiagnostic, Severity, Snapshot, SourceMap, plain};
use cheby_syntax::SyntaxTree;
use lsp_server::ResponseError;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::server::codes;
use crate::server::{ClientCaps, Effect, Settings};
use crate::text::lines::{Bom, LspLines, PositionEncoding};

/// One snapshot as handlers read it, with its memos (D-1364). The main
/// loop makes one per snapshot and shares it with every request that the
/// snapshot answers.
#[derive(Debug)]
pub(crate) struct Analyzed {
    /// The snapshot: immutable, and the owner of every handle it exposes.
    pub snapshot: Arc<Snapshot>,
    #[cfg_attr(not(test), expect(dead_code, reason = "W7's index is the first memo"))]
    /// What handlers compute from the snapshot once and share.
    pub memo: Memo,
}

impl Analyzed {
    /// `snapshot`, with nothing computed yet.
    pub(crate) fn new(snapshot: Arc<Snapshot>) -> Self {
        Self {
            snapshot,
            memo: Memo::default(),
        }
    }
}

/// Values computed from one snapshot on first need, in `OnceLock` fields:
/// the package index, item ranges and satisfaction join it with the waves
/// that read them (plan §1.5).
#[derive(Debug, Default)]
pub(crate) struct Memo {}

/// What a handler gives back: the response's value, and the effects the
/// main loop applies before it sends the response (plan §1.5).
#[derive(Debug)]
pub(crate) struct HandlerOutput {
    /// The response's result.
    pub value: Value,
    /// Messages to send first, in order.
    pub effects: Vec<Effect>,
}

impl HandlerOutput {
    /// A value with no effects.
    pub(crate) const fn value(value: Value) -> Self {
        Self {
            value,
            effects: Vec::new(),
        }
    }
}

/// Why a request has no result (A §3.3, plan §1.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    /// The text moved on before the request could be answered exactly
    /// (D-1367): `ContentModified`.
    ContentModified,
    /// `$/cancelRequest` (A §3.5): `RequestCancelled`.
    Cancelled,
    /// The parameters do not have the method's shape: `InvalidParams`.
    InvalidParams(String),
    /// The request cannot be done, with the reason: `RequestFailed`. Also a
    /// wait past `analysis.requestWaitMs` (D-1367).
    Failed(String),
}

impl RequestError {
    /// The error as the response carries it.
    pub(crate) fn into_response(self) -> ResponseError {
        let (code, message) = match self {
            Self::ContentModified => (
                codes::CONTENT_MODIFIED,
                "the document changed before the request could be answered".to_owned(),
            ),
            Self::Cancelled => (codes::REQUEST_CANCELLED, "cancelled".to_owned()),
            Self::InvalidParams(message) => (codes::INVALID_PARAMS, message),
            Self::Failed(message) => (codes::REQUEST_FAILED, message),
        };
        ResponseError {
            code,
            message,
            data: None,
        }
    }
}

/// The parameters `value` as `T`, or `InvalidParams`, never a panic
/// (A §3.3).
pub(crate) fn params<T: DeserializeOwned>(value: Value) -> Result<T, RequestError> {
    serde_json::from_value(value).map_err(|e| RequestError::InvalidParams(e.to_string()))
}

/// The flag `$/cancelRequest` sets on a request on the query pool (A §3.5).
/// Handlers call [`CancelToken::check`] at loop heads: per module, per
/// item, per file.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not set.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets the token: the handler stops at its next check.
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the token is set.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// `Err(Cancelled)` once the token is set.
    ///
    /// # Errors
    ///
    /// [`RequestError::Cancelled`] when the client cancelled the request.
    pub(crate) fn check(&self) -> Result<(), RequestError> {
        if self.is_cancelled() {
            Err(RequestError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// What every handler may read besides its target (A §3.3). Its fields
/// come from values the main loop moved into the job, so a handler never
/// reads the main loop's state.
#[derive(Debug)]
pub(crate) struct RequestCx<'a> {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "W8's completion reads the first setting")
    )]
    /// The settings in force when the request arrived.
    pub settings: &'a Settings,
    /// What the client can do.
    pub caps: &'a ClientCaps,
    /// The position encoding of the session (D-1368).
    pub encoding: PositionEncoding,
    /// Set by `$/cancelRequest`.
    pub cancel: &'a CancelToken,
}

/// A document's live text and its own parse, never a snapshot (A §3.3).
///
/// The parse uses a source map of its own, so nothing in it can meet a
/// snapshot's handles, and a text handler, which gets no snapshot, cannot
/// mix the two.
#[derive(Debug)]
pub(crate) struct DocText {
    #[cfg_attr(not(test), expect(dead_code, reason = "W7's text features read it"))]
    /// The document's normalized path.
    pub path: PathBuf,
    #[cfg_attr(not(test), expect(dead_code, reason = "W7's text features read it"))]
    /// The client's version of the text.
    pub version: Option<i32>,
    /// The text, as the client holds it.
    pub text: Arc<str>,
    /// Its syntax tree, which covers all of `text`.
    pub tree: SyntaxTree,
    /// Its line index. A BOM counts, since the client sent the text
    /// (D-1368).
    pub lines: LspLines,
    #[cfg_attr(not(test), expect(dead_code, reason = "W7's text features read it"))]
    /// Its phase-A errors, grammar and syntax validation (D-1355), with
    /// ranges in `text`.
    pub errors: Vec<PlainDiagnostic>,
}

impl DocText {
    /// Parses `text`. `None` when the text is too large for the compiler's
    /// offsets.
    pub(crate) fn new(path: PathBuf, version: Option<i32>, text: Arc<str>) -> Option<Self> {
        let mut sources = SourceMap::new();
        let file = sources
            .add_file(path.display().to_string(), Arc::clone(&text))
            .ok()?;
        let (tree, grammar) = cheby_syntax::parse(file, Arc::clone(&text)).into_parts();
        let placed = Placed {
            path: &path,
            text: &text,
        };
        let mut errors: Vec<PlainDiagnostic> = grammar
            .iter()
            .map(|d| plain(d, file, |other| (other == file).then_some(placed)))
            .collect();
        // Validation assumes the shapes the grammar allows, so on a tree
        // with grammar errors only its token checks run, as in a check
        // (D-425).
        let checks = if errors.iter().any(|d| d.severity == Severity::Error) {
            cheby_syntax::validate_tokens(&tree)
        } else {
            cheby_syntax::validate(&tree)
        };
        errors.extend(
            checks
                .iter()
                .map(|d| plain(d, file, |other| (other == file).then_some(placed))),
        );
        let lines = LspLines::new(&text, Bom::Counted);
        Some(Self {
            path,
            version,
            text,
            tree,
            lines,
            errors,
        })
    }
}

/// One version of an open document, shared by the text requests that
/// read it: its [`DocText`] is made on a query thread on first need, and
/// kept until the document changes (A §3.4).
#[derive(Debug)]
pub(crate) struct LiveText {
    path: PathBuf,
    version: Option<i32>,
    text: Arc<str>,
    parsed: OnceLock<Option<DocText>>,
}

impl LiveText {
    /// `text` at `version`, not parsed yet.
    pub(crate) const fn new(path: PathBuf, version: Option<i32>, text: Arc<str>) -> Self {
        Self {
            path,
            version,
            text,
            parsed: OnceLock::new(),
        }
    }

    /// The parse, made on the first call. `None` when the text is too
    /// large.
    pub(crate) fn doc_text(&self) -> Option<&DocText> {
        self.parsed
            .get_or_init(|| DocText::new(self.path.clone(), self.version, Arc::clone(&self.text)))
            .as_ref()
    }
}

/// A handler of a [`Class::Text`] request.
pub(crate) type TextHandler =
    fn(&RequestCx<'_>, &DocText, Value) -> Result<HandlerOutput, RequestError>;

/// A handler of a request answered from a snapshot.
pub(crate) type SnapshotHandler =
    fn(&RequestCx<'_>, &Analyzed, Value) -> Result<HandlerOutput, RequestError>;

/// What a request reads, and so when it can run (A §3.3, §4).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Class {
    /// The live text of an open document, at once.
    Text(TextHandler),
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "W7's navigation is the first snapshot request")
    )]
    /// A snapshot holding the document's text as the request saw it.
    Doc(SnapshotHandler),
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "W7's navigation is the first snapshot request")
    )]
    /// A snapshot holding every open document of the package as the
    /// request saw them.
    Package(SnapshotHandler),
}

/// One method served off the main loop (A §3.3).
#[derive(Debug)]
pub(crate) struct Route {
    /// The LSP method.
    pub method: &'static str,
    /// What it reads.
    pub class: Class,
    /// The answer when nothing can be read: no open document, no package,
    /// or a failed run (A §3.7). Usually `null` or `[]`.
    pub fallback: fn() -> Value,
    /// The answer edits the document, so a document the user may not edit,
    /// such as a std source, gets the fallback without running the handler
    /// (A §6.3, X §5.4).
    pub edits: bool,
}

/// The route of the test request `method` (`testing`).
#[cfg(test)]
pub(crate) fn test_route(method: &str) -> Option<&'static Route> {
    testing::ROUTES.iter().find(|route| route.method == method)
}
