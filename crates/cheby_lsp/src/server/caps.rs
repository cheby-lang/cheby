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
    CallHierarchyServerCapability, CodeActionKind, CodeActionOptions, CodeActionProviderCapability,
    CompletionOptions, DeclarationCapability, DiagnosticTag, DocumentLinkOptions,
    ExecuteCommandOptions, FileOperationFilter, FileOperationPattern, FileOperationPatternKind,
    FileOperationRegistrationOptions, FoldingRangeProviderCapability, HoverProviderCapability,
    InlayHintOptions, InlayHintServerCapabilities, OneOf, PositionEncodingKind, RenameOptions,
    SaveOptions, SelectionRangeProviderCapability, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability, WorkDoneProgressOptions,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};
use serde_json::Value;

use crate::features::handlers::completion::CompletionCaps;
use crate::features::handlers::edit::EditCaps;
use crate::features::handlers::explain::EXPLAIN_COMMAND;
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
    /// `window.workDoneProgress`: child runs show as `$/progress`
    /// (D-1415).
    pub work_done_progress: bool,
    /// `experimental.chebyIdle`: the client takes `cheby/idle` (D-1415).
    pub idle_notification: bool,
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
    /// `workspace.workspaceEdit` and `textDocument.rename`: the shape of a
    /// workspace edit and its annotations.
    pub edit: EditCaps,
    /// `textDocument.codeAction`: what a code action may carry.
    pub actions: CodeActionCaps,
    /// `textDocument.publishDiagnostics`: what a published diagnostic may
    /// carry.
    pub diagnostics: DiagnosticCaps,
    /// `window.showDocument.support`: the client opens a document the
    /// server names, as Explain's text (D-1693).
    pub show_document: bool,
}

/// What the client can do with code actions (X §6.1), read from
/// `textDocument.codeAction`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per client capability"
)]
pub(crate) struct CodeActionCaps {
    /// `codeActionLiteralSupport` is present: the client takes
    /// `CodeAction` literals, not only `Command`s (D-1734).
    pub literals: bool,
    /// `codeActionLiteralSupport.codeActionKind.valueSet`: the kinds the
    /// client knows.
    pub kinds: Vec<String>,
    /// `resolveSupport.properties`: what the client resolves lazily, such
    /// as `edit` (D-1684).
    pub resolve_properties: Vec<String>,
    /// `dataSupport`: the client keeps an action's `data` until it
    /// resolves the action (D-1683).
    pub data: bool,
    /// `disabledSupport`: the client shows a `disabled` action with its
    /// reason (D-1690).
    pub disabled: bool,
    /// `isPreferredSupport`: the client reads `isPreferred`.
    pub is_preferred: bool,
}

impl CodeActionCaps {
    /// Reads the code-action capabilities of `params`, the `initialize`
    /// parameters. A capability that is missing or of the wrong shape is
    /// absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let at = |pointer: &str| {
            params.pointer(&format!("/capabilities/textDocument/codeAction{pointer}"))
        };
        let flag = |pointer: &str| at(pointer).and_then(Value::as_bool).unwrap_or(false);
        let strings = |pointer: &str| {
            at(pointer)
                .and_then(Value::as_array)
                .map_or_default(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        };
        Self {
            literals: at("/codeActionLiteralSupport").is_some_and(Value::is_object),
            kinds: strings("/codeActionLiteralSupport/codeActionKind/valueSet"),
            resolve_properties: strings("/resolveSupport/properties"),
            data: flag("/dataSupport"),
            disabled: flag("/disabledSupport"),
            is_preferred: flag("/isPreferredSupport"),
        }
    }

    /// The client resolves an action's `edit`: it keeps `data` and names
    /// `edit` among the properties it resolves (D-1684). A fix that is
    /// checked through `verify` on resolve is offered only to such a client
    /// (D-1753, D-1754).
    pub(crate) fn resolves_edits(&self) -> bool {
        self.data && self.resolve_properties.iter().any(|p| p == "edit")
    }
}

/// What a published diagnostic may carry for the client, read from
/// `textDocument.publishDiagnostics` (X §8.1, §8.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiagnosticCaps {
    /// `codeDescriptionSupport`: the client shows `codeDescription.href`
    /// (D-1400).
    pub code_description: bool,
    /// `tagSupport.valueSet`: the tags the client shows, such as
    /// `UNNECESSARY` and `DEPRECATED`.
    pub tags: Vec<DiagnosticTag>,
}

impl DiagnosticCaps {
    /// Reads the diagnostic capabilities of `params`, the `initialize`
    /// parameters. A capability that is missing or of the wrong shape is
    /// absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let base = "/capabilities/textDocument/publishDiagnostics";
        Self {
            code_description: params
                .pointer(&format!("{base}/codeDescriptionSupport"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tags: params
                .pointer(&format!("{base}/tagSupport/valueSet"))
                .and_then(Value::as_array)
                .map_or_default(|items| {
                    items
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                }),
        }
    }
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
            work_done_progress: flag("/capabilities/window/workDoneProgress"),
            idle_notification: flag("/capabilities/experimental/chebyIdle"),
            symbols: SymbolCaps::from_params(params),
            folding: FoldingCaps::from_params(params),
            hover: HoverCaps::from_params(params),
            links: LinkCaps::from_params(params),
            completion: CompletionCaps::from_params(params),
            signature: SignatureCaps::from_params(params),
            edit: EditCaps::from_params(params),
            actions: CodeActionCaps::from_params(params),
            diagnostics: DiagnosticCaps::from_params(params),
            show_document: flag("/capabilities/window/showDocument/support"),
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
/// (A §6.5). Code actions are quick fixes, rewrites and source actions,
/// resolved lazily (D-1683), and `cheby.explain` is the one command
/// (D-1378, D-1693). Completion triggers on `.`, `:`, `@` and `>` and resolves
/// its items' docs later, signature help triggers on `(` and `,`
/// (D-1379, X §9.2), and inlay hints are whole when sent (plan §1.10).
/// Semantic tokens are `full` and `range` with no `delta` (D-1396).
/// Rename has `prepareRename`, which refuses with a reason (D-1673), and
/// `willRenameFiles` covers `.cheby` files and folders of `file` URIs, so
/// a move edits its importers (D-1379, D-1675).
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
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::QUICKFIX,
                CodeActionKind::REFACTOR_REWRITE,
                CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                CodeActionKind::SOURCE,
            ]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
            resolve_provider: Some(true),
        })),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![EXPLAIN_COMMAND.to_owned()],
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
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
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                will_rename: Some(will_rename_filters()),
                ..WorkspaceFileOperationsServerCapabilities::default()
            }),
        }),
        ..ServerCapabilities::default()
    }
}

/// The files `willRenameFiles` is sent for: `.cheby` files and folders,
/// which may hold some (D-1379).
fn will_rename_filters() -> FileOperationRegistrationOptions {
    let filter = |glob: &str, matches| FileOperationFilter {
        scheme: Some("file".to_owned()),
        pattern: FileOperationPattern {
            glob: glob.to_owned(),
            matches: Some(matches),
            options: None,
        },
    };
    FileOperationRegistrationOptions {
        filters: vec![
            filter("**/*.cheby", FileOperationPatternKind::File),
            filter("**", FileOperationPatternKind::Folder),
        ],
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
            "textDocument/codeAction" | "codeAction/resolve" => "codeActionProvider",
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
            "textDocument/prepareRename" | "textDocument/rename" => "renameProvider",
            "workspace/willRenameFiles" => WILL_RENAME,
            "workspace/executeCommand" => "executeCommandProvider",
            other => panic!("no provider is known for `{other}`"),
        };
        // A `cheby/` request is the server's own, and has no provider
        // (D-1378).
        let mut routed: Vec<&str> = super::super::routes::query_methods()
            .filter(|method| !method.starts_with("cheby/"))
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
        // File operations are advertised under `workspace`, not as a
        // provider.
        if caps.pointer(WILL_RENAME).is_some_and(|v| !v.is_null()) {
            advertised.push(WILL_RENAME);
        }
        advertised.sort_unstable();
        assert_eq!(advertised, routed);
    }

    /// Where `willRenameFiles` is advertised.
    const WILL_RENAME: &str = "/workspace/fileOperations/willRename";

    /// Rename asks `prepareRename` first, and `willRenameFiles` is sent
    /// for `.cheby` files and for folders, on disk only (D-1379).
    #[test]
    fn rename_has_prepare_and_will_rename_covers_files_and_folders() {
        let caps = serde_json::to_value(server_capabilities(PositionEncoding::Utf16))
            .expect("capabilities serialize");
        assert_eq!(
            caps["renameProvider"],
            serde_json::json!({ "prepareProvider": true })
        );
        assert_eq!(
            caps.pointer(WILL_RENAME),
            Some(&serde_json::json!({ "filters": [
                { "scheme": "file", "pattern": { "glob": "**/*.cheby", "matches": "file" } },
                { "scheme": "file", "pattern": { "glob": "**", "matches": "folder" } },
            ] }))
        );
        assert_eq!(
            caps.pointer("/workspace/fileOperations")
                .and_then(Value::as_object)
                .map(|ops| ops.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["willRename".to_owned()])
        );
    }

    /// Code actions have the four kinds W11 sends and are resolved, and
    /// `cheby.explain` is the one command, of D-1378's five, that the
    /// server serves (D-1683, D-1693).
    #[test]
    fn code_actions_resolve_and_explain_is_the_one_command() {
        let caps = serde_json::to_value(server_capabilities(PositionEncoding::Utf16))
            .expect("capabilities serialize");
        assert_eq!(
            caps["codeActionProvider"],
            serde_json::json!({
                "codeActionKinds": ["quickfix", "refactor.rewrite", "source.organizeImports", "source"],
                "resolveProvider": true,
            })
        );
        assert_eq!(
            caps["executeCommandProvider"],
            serde_json::json!({ "commands": ["cheby.explain"] })
        );
    }

    /// The client's code-action, diagnostic and `showDocument` capabilities
    /// are read as plain fields, and an empty client has none of them.
    #[test]
    fn code_action_and_diagnostic_caps_are_read() {
        let params = serde_json::json!({ "capabilities": {
            "textDocument": {
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": { "valueSet": ["quickfix", "source"] },
                    },
                    "resolveSupport": { "properties": ["edit"] },
                    "dataSupport": true,
                    "disabledSupport": true,
                    "isPreferredSupport": true,
                },
                "publishDiagnostics": {
                    "codeDescriptionSupport": true,
                    "tagSupport": { "valueSet": [1, 2] },
                },
            },
            "window": { "showDocument": { "support": true } },
        } });
        let caps = ClientCaps::from_params(&params);
        assert_eq!(
            caps.actions,
            CodeActionCaps {
                literals: true,
                kinds: vec!["quickfix".to_owned(), "source".to_owned()],
                resolve_properties: vec!["edit".to_owned()],
                data: true,
                disabled: true,
                is_preferred: true,
            }
        );
        assert_eq!(
            caps.diagnostics,
            DiagnosticCaps {
                code_description: true,
                tags: vec![DiagnosticTag::UNNECESSARY, DiagnosticTag::DEPRECATED],
            }
        );
        assert!(caps.show_document);
        let empty = ClientCaps::from_params(&serde_json::json!({ "capabilities": {} }));
        assert_eq!(empty.actions, CodeActionCaps::default());
        assert_eq!(empty.diagnostics, DiagnosticCaps::default());
        assert!(!empty.show_document);
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
