//! The dispatch table: each method the server serves, once (A §3.3).
//!
//! A request is answered on the main loop when it changes the loop's own
//! state, as `shutdown` does, and is otherwise a query: a
//! [`Route`] whose [`Class`] says what its handler reads and so when it
//! can run (D-1367). An unknown request is answered `MethodNotFound`, and
//! an unknown notification is dropped, as the protocol asks.
//!
//! `caps` advertises exactly the queries listed here (plan §1.10).

use lsp_server::ResponseError;
use serde_json::Value;

use super::Server;
use crate::features::handlers::nav::NavEnv;
use crate::features::handlers::workspace_symbol::StdSymbols;
use crate::features::handlers::{
    completion, folding, format, hierarchy, hover, inlay, links, nav, selection, signature,
    symbols, tokens, workspace_symbol,
};
use crate::features::ide::completion::StdAccess;
use crate::features::ide::hover::StdDocs;
use crate::features::{Class, RequestCx, Route};

/// A notification's handler.
pub(super) type NotificationHandler = fn(&mut Server, Value);

/// A request answered on the main loop.
pub(super) type RequestHandler = fn(&mut Server, Value) -> Result<Value, ResponseError>;

/// The notifications the server reads. `exit` and `$/cancelRequest` are
/// the loop's own.
const NOTIFICATIONS: &[(&str, NotificationHandler)] = &[
    ("initialized", Server::on_initialized),
    ("textDocument/didOpen", Server::did_open),
    ("textDocument/didChange", Server::did_change),
    ("textDocument/didClose", Server::did_close),
    ("textDocument/didSave", Server::did_save),
    (
        "workspace/didChangeConfiguration",
        Server::on_did_change_configuration,
    ),
    (
        "workspace/didChangeWatchedFiles",
        Server::did_change_watched_files,
    ),
    (
        "workspace/didChangeWorkspaceFolders",
        Server::did_change_workspace_folders,
    ),
];

/// The requests the server answers on the main loop. The handshake
/// answers the first `initialize`, so this one is a second.
const REQUESTS: &[(&str, RequestHandler)] = &[
    ("initialize", Server::on_initialize_again),
    ("shutdown", Server::on_shutdown),
];

/// The answer when a query has nothing to read, such as a document that is
/// not open.
const fn null() -> Value {
    Value::Null
}

/// What navigation reads besides its snapshot.
fn nav_env<'a>(cx: &RequestCx<'a>) -> NavEnv<'a> {
    NavEnv {
        links: cx.caps.links,
        std_dir: cx.std.dir(),
    }
}

/// The requests served on the query pool, by class (A §3.3, plan §1.10).
/// The text requests read only the document's live text, so they never
/// wait. A request that may answer with a place in another open document
/// is `Package`, so that place is where the client sees it; `declaration`
/// is `Doc`, as plan §1.10 has it. `completionItem/resolve` names no
/// document, only the item's `data`, so it is `Workspace`.
const QUERIES: &[Route] = &[
    Route {
        method: "textDocument/documentSymbol",
        class: Class::Text(|cx, doc, params| {
            symbols::document_symbol(cx, doc, params, cx.caps.symbols)
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/foldingRange",
        class: Class::Text(|cx, doc, params| {
            folding::folding_range(cx, doc, params, &cx.caps.folding)
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/selectionRange",
        class: Class::Text(selection::selection_range),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/formatting",
        class: Class::Text(format::formatting),
        fallback: null,
        edits: true,
    },
    Route {
        method: "textDocument/rangeFormatting",
        class: Class::Text(format::range_formatting),
        fallback: null,
        edits: true,
    },
    Route {
        method: "textDocument/hover",
        class: Class::Doc(|cx, analyzed, params| {
            hover::hover(
                cx,
                analyzed,
                params,
                cx.caps.hover,
                StdDocs::Service(cx.std),
            )
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/completion",
        class: Class::Doc(|cx, analyzed, params| {
            completion::completion(
                cx,
                analyzed,
                params,
                cx.caps.completion,
                StdAccess::Service(cx.std),
            )
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "completionItem/resolve",
        class: Class::Workspace(|cx, packages, params| {
            completion::resolve(
                cx,
                packages,
                params,
                cx.caps.completion,
                StdAccess::Service(cx.std),
            )
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/signatureHelp",
        class: Class::Doc(|cx, analyzed, params| {
            signature::signature_help(
                cx,
                analyzed,
                params,
                cx.caps.signature,
                StdDocs::Service(cx.std),
            )
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/inlayHint",
        class: Class::Doc(inlay::inlay_hint),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/definition",
        class: Class::Package(|cx, analyzed, params| {
            nav::definition(cx, analyzed, params, &nav_env(cx))
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/declaration",
        class: Class::Doc(|cx, analyzed, params| {
            nav::declaration(cx, analyzed, params, &nav_env(cx))
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/typeDefinition",
        class: Class::Package(|cx, analyzed, params| {
            nav::type_definition(cx, analyzed, params, &nav_env(cx))
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/references",
        class: Class::Package(|cx, analyzed, params| {
            nav::references(cx, analyzed, params, &nav_env(cx))
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/documentHighlight",
        class: Class::Doc(|cx, analyzed, params| {
            nav::document_highlight(cx, analyzed, params, &nav_env(cx))
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/prepareCallHierarchy",
        class: Class::Package(|cx, analyzed, params| {
            hierarchy::prepare(cx, analyzed, params, cx.std.dir())
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "callHierarchy/incomingCalls",
        class: Class::Package(|cx, analyzed, params| {
            hierarchy::incoming_calls(cx, analyzed, params, cx.std.dir())
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "callHierarchy/outgoingCalls",
        class: Class::Package(|cx, analyzed, params| {
            hierarchy::outgoing_calls(cx, analyzed, params, cx.std.dir())
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/documentLink",
        class: Class::Doc(|cx, analyzed, params| {
            links::document_link(cx, analyzed, params, cx.std.dir())
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "workspace/symbol",
        class: Class::Workspace(|cx, packages, params| {
            let std = cx
                .std
                .index()
                .zip(cx.std.dir())
                .map(|(index, dir)| StdSymbols { index, dir });
            workspace_symbol::workspace_symbol(cx, packages, params, std)
        }),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/semanticTokens/full",
        class: Class::Doc(tokens::full),
        fallback: null,
        edits: false,
    },
    Route {
        method: "textDocument/semanticTokens/range",
        class: Class::Doc(tokens::range),
        fallback: null,
        edits: false,
    },
];

/// The handler of the notification `method`.
pub(super) fn notification(method: &str) -> Option<NotificationHandler> {
    NOTIFICATIONS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|&(_, handler)| handler)
}

/// The handler of the main-loop request `method`.
pub(super) fn request(method: &str) -> Option<RequestHandler> {
    REQUESTS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|&(_, handler)| handler)
        .or_else(|| test_request(method))
}

/// The route of the query `method`.
pub(super) fn query(method: &str) -> Option<&'static Route> {
    QUERIES
        .iter()
        .find(|route| route.method == method)
        .or_else(|| test_query(method))
}

/// The methods of [`QUERIES`], for `caps`' tests.
#[cfg(test)]
pub(super) fn query_methods() -> impl Iterator<Item = &'static str> {
    QUERIES.iter().map(|route| route.method)
}

/// Requests that only tests send: `cheby/testSettings` answers the
/// settings in force.
#[cfg(test)]
fn test_request(method: &str) -> Option<RequestHandler> {
    let handler: RequestHandler = Server::test_settings;
    (method == "cheby/testSettings").then_some(handler)
}

/// No test requests outside tests.
#[cfg(not(test))]
const fn test_request(_method: &str) -> Option<RequestHandler> {
    None
}

/// Queries that only tests send, one per class (`features::testing`).
#[cfg(test)]
fn test_query(method: &str) -> Option<&'static Route> {
    crate::features::test_route(method)
}

/// No test queries outside tests.
#[cfg(not(test))]
const fn test_query(_method: &str) -> Option<&'static Route> {
    None
}

#[cfg(test)]
impl Server {
    /// `cheby/testSettings`.
    fn test_settings(&mut self, _params: Value) -> Result<Value, ResponseError> {
        serde_json::to_value(&*self.settings).map_err(|e| ResponseError {
            code: super::codes::INTERNAL_ERROR,
            message: e.to_string(),
            data: None,
        })
    }
}
