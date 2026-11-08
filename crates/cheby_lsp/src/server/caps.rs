//! What the client can do, read once from `initialize`, and what the
//! server advertises (A §12.1, §12.2, D-1379).
//!
//! The client's capabilities are read leniently from the raw parameters,
//! by JSON pointer: a capability that is missing or of the wrong shape is
//! simply absent, so an odd client never fails the handshake.
//!
//! The server advertises only what it serves now. D-1379 lists the whole
//! surface; each part joins the answer with the wave that serves it, so a
//! client never sends a request that has no route.

use lsp_types::{
    PositionEncodingKind, SaveOptions, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
};
use serde_json::Value;

use crate::text::lines::PositionEncoding;

/// The client capabilities the server reads, as plain fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientCaps {
    /// `general.positionEncodings`, in the client's order.
    pub encodings: Option<Vec<PositionEncodingKind>>,
    /// `workspace.configuration`: the client answers
    /// `workspace/configuration`.
    pub configuration: bool,
    /// `workspace.didChangeConfiguration.dynamicRegistration`.
    pub config_dynamic: bool,
    /// `textDocument.publishDiagnostics.relatedInformation`.
    pub related_information: bool,
}

impl ClientCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters.
    pub(crate) fn from_params(params: &Value) -> Self {
        let flag = |pointer: &str| {
            params
                .pointer(pointer)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let encodings = params
            .pointer("/capabilities/general/positionEncodings")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| PositionEncodingKind::from(name.to_owned()))
                    .collect()
            });
        Self {
            encodings,
            configuration: flag("/capabilities/workspace/configuration"),
            config_dynamic: flag(
                "/capabilities/workspace/didChangeConfiguration/dynamicRegistration",
            ),
            related_information: flag(
                "/capabilities/textDocument/publishDiagnostics/relatedInformation",
            ),
        }
    }

    /// The position encoding to use with this client (D-1368).
    pub(crate) fn encoding(&self) -> PositionEncoding {
        PositionEncoding::negotiate(self.encodings.as_deref())
    }
}

/// The server's capabilities: the position encoding, and incremental text
/// sync with open, close and save, which is what diagnostics need.
pub(crate) fn server_capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.kind()),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: None,
                will_save_wait_until: None,
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
            },
        )),
        ..ServerCapabilities::default()
    }
}
