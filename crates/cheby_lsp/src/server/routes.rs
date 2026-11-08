//! The dispatch table: each method the server serves, once (A §3.3).
//!
//! An unknown request is answered `MethodNotFound`, and an unknown
//! notification is dropped, as the protocol asks. Later waves add the
//! request classes of A §3.3 here.

use lsp_server::ResponseError;
use serde_json::Value;

use super::Server;

/// A notification's handler.
pub(super) type NotificationHandler = fn(&mut Server, Value);

/// A request answered on the main loop.
pub(super) type RequestHandler = fn(&mut Server, Value) -> Result<Value, ResponseError>;

/// The notifications the server reads. `exit` is the loop's own.
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
];

/// The requests the server answers. The handshake answers the first
/// `initialize`, so this one is a second.
const REQUESTS: &[(&str, RequestHandler)] = &[
    ("initialize", Server::on_initialize_again),
    ("shutdown", Server::on_shutdown),
];

/// The handler of the notification `method`.
pub(super) fn notification(method: &str) -> Option<NotificationHandler> {
    NOTIFICATIONS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|&(_, handler)| handler)
}

/// The handler of the request `method`.
pub(super) fn request(method: &str) -> Option<RequestHandler> {
    REQUESTS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|&(_, handler)| handler)
        .or_else(|| test_request(method))
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
