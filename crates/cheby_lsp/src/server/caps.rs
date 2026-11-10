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
    CallHierarchyServerCapability, CompletionOptions, DeclarationCapability, DocumentLinkOptions,
    FoldingRangeProviderCapability, HoverProviderCapability, InlayHintOptions,
    InlayHintServerCapabilities, OneOf, PositionEncodingKind, SaveOptions,
    SelectionRangeProviderCapability, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability, WorkDoneProgressOptions,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};
use serde_json::Value;

use crate::features::handlers::completion::CompletionCaps;
use crate::features::handlers::folding::FoldingCaps;
use crate::features::handlers::hover::HoverCaps;
use crate::features::handlers::nav::LinkCaps;
use crate::features::handlers::signature::SignatureCaps;
use crate::features::handlers::symbols::SymbolCaps;
use crate::features::handlers::tokens;
use crate::text::lines::PositionEncoding;

/// The client capabilities the server reads, as plain fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per client capability"
)]
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
    /// `workspace.inlayHint.refreshSupport`: the client takes
    /// `workspace/inlayHint/refresh` (A §12.3).
    pub inlay_refresh: bool,
    /// `textDocument.documentSymbol`: what an outline may hold.
    pub symbols: SymbolCaps,
    /// `textDocument.foldingRange`: what a fold may be.
    pub folding: FoldingCaps,
    /// `textDocument.hover.contentFormat`: Markdown or plain text.
    pub hover: HoverCaps,
    /// The `linkSupport` of definition, declaration and typeDefinition.
    pub links: LinkCaps,
    /// `textDocument.completion.completionItem` and `clientInfo.name`.
    pub completion: CompletionCaps,
    /// `textDocument.signatureHelp.signatureInformation`.
    pub signature: SignatureCaps,
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
            inlay_refresh: flag("/capabilities/workspace/inlayHint/refreshSupport"),
            symbols: SymbolCaps::from_params(params),
            folding: FoldingCaps::from_params(params),
            hover: HoverCaps::from_params(params),
            links: LinkCaps::from_params(params),
            completion: CompletionCaps::from_params(params),
            signature: SignatureCaps::from_params(params),
        }
    }

    /// The position encoding to use with this client (D-1368).
    pub(crate) fn encoding(&self) -> PositionEncoding {
        PositionEncoding::negotiate(self.encodings.as_deref())
    }
}

/// The server's capabilities: the position encoding, incremental text sync
/// with open, close and save, which is what diagnostics need, the requests
/// of `routes` (plan §1.10), and workspace folders with their changes
/// (A §6.5). Completion triggers on `.`, `:`, `@` and `>` and resolves
/// its items' docs later, signature help triggers on `(` and `,`
/// (D-1379, X §9.2), and inlay hints are whole when sent (plan §1.10).
/// Semantic tokens are `full` and `range` with no `delta` (D-1396).
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
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some([".", ":", "@", ">"].map(str::to_owned).to_vec()),
            resolve_provider: Some(true),
            ..CompletionOptions::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(["(", ","].map(str::to_owned).to_vec()),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
            InlayHintOptions {
                resolve_provider: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        ))),
        definition_provider: Some(OneOf::Left(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(tokens::provider()),
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
            "textDocument/hover" => "hoverProvider",
            "textDocument/completion" | "completionItem/resolve" => "completionProvider",
            "textDocument/signatureHelp" => "signatureHelpProvider",
            "textDocument/inlayHint" => "inlayHintProvider",
            "textDocument/definition" => "definitionProvider",
            "textDocument/declaration" => "declarationProvider",
            "textDocument/typeDefinition" => "typeDefinitionProvider",
            "textDocument/references" => "referencesProvider",
            "textDocument/documentHighlight" => "documentHighlightProvider",
            "textDocument/prepareCallHierarchy"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls" => "callHierarchyProvider",
            "textDocument/documentLink" => "documentLinkProvider",
            "workspace/symbol" => "workspaceSymbolProvider",
            "textDocument/semanticTokens/full" | "textDocument/semanticTokens/range" => {
                "semanticTokensProvider"
            }
            other => panic!("no provider is known for `{other}`"),
        };
        let mut routed: Vec<&str> = super::super::routes::query_methods()
            .map(provider)
            .collect();
        routed.sort_unstable();
        routed.dedup();
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

    /// Semantic tokens are `full` and `range` with E's legend, and no
    /// `delta` (D-1396).
    #[test]
    fn semantic_tokens_are_full_and_range_without_delta() {
        let caps = serde_json::to_value(server_capabilities(PositionEncoding::Utf16))
            .expect("capabilities serialize");
        let tokens = &caps["semanticTokensProvider"];
        assert_eq!(tokens["full"], serde_json::json!(true));
        assert_eq!(tokens["range"], serde_json::json!(true));
        assert_eq!(
            tokens["legend"],
            serde_json::to_value(tokens::legend()).expect("the legend serializes")
        );
    }
}
