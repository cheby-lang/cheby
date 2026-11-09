//! What the client can do, read once from `initialize`, and what the
//! server advertises (A §12.1, §12.2, D-1379).
//!
//! The client's capabilities are read leniently from the raw parameters,
//! by JSON pointer: a capability that is missing or of the wrong shape is
//! simply absent, so an odd client never fails the handshake.
//!
//! The server advertises only what it serves now. D-1379 lists the whole
//! surface; each part joins the answer with the wave that serves it, so a
//! client never sends a request that has no route. A test checks that the
//! advertised requests are exactly the queries of `routes`.
//!
//! File watchers are not advertised here: the client's
//! `didChangeWatchedFiles` has no static form, so `watch` registers them
//! at `initialized` when the client allows dynamic registration (A §8.1).

use lsp_types::{
    FoldingRangeProviderCapability, OneOf, PositionEncodingKind, SaveOptions,
    SelectionRangeProviderCapability, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};
use serde_json::Value;

use crate::features::handlers::folding::FoldingCaps;
use crate::features::handlers::symbols::SymbolCaps;
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
    /// `textDocument.documentSymbol`: what an outline may hold.
    pub symbols: SymbolCaps,
    /// `textDocument.foldingRange`: what a fold may be.
    pub folding: FoldingCaps,
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
            symbols: SymbolCaps::from_params(params),
            folding: FoldingCaps::from_params(params),
        }
    }

    /// The position encoding to use with this client (D-1368).
    pub(crate) fn encoding(&self) -> PositionEncoding {
        PositionEncoding::negotiate(self.encodings.as_deref())
    }
}

/// The server's capabilities: the position encoding, incremental text sync
/// with open, close and save, which is what diagnostics need, the text
/// requests of `routes` (plan §1.10), and workspace folders with their
/// changes (A §6.5).
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
        document_symbol_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..ServerCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each request `routes` serves off the main loop is advertised, and
    /// nothing else is: a provider with no route would get
    /// `MethodNotFound` (plan §1.10).
    #[test]
    fn advertised_requests_are_the_routes() {
        let caps = serde_json::to_value(server_capabilities(PositionEncoding::Utf16))
            .expect("capabilities serialize");
        let provider = |method: &str| match method {
            "textDocument/documentSymbol" => "documentSymbolProvider",
            "textDocument/foldingRange" => "foldingRangeProvider",
            "textDocument/selectionRange" => "selectionRangeProvider",
            "textDocument/formatting" => "documentFormattingProvider",
            "textDocument/rangeFormatting" => "documentRangeFormattingProvider",
            other => panic!("no provider is known for `{other}`"),
        };
        let mut routed: Vec<&str> = super::super::routes::query_methods()
            .map(provider)
            .collect();
        routed.sort_unstable();
        let mut advertised: Vec<&str> = caps
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .filter(|key| key.ends_with("Provider"))
            .collect();
        advertised.sort_unstable();
        assert_eq!(advertised, routed);
    }
}
