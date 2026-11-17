//! `textDocument/codeAction`, from the live text (class `Text`, D-1367).
//!
//! The one action so far is "Use a field path" (D-1645), a
//! `refactor.rewrite` that `ide::field_path` finds from the syntax tree
//! alone, so it never waits for analysis. Its edit is sent whole, with no
//! resolve. A request whose `only` list leaves out its kind gets `[]`.

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, TextEdit, WorkspaceEdit,
};
use serde_json::Value;

use crate::features::ide::field_path;
use crate::features::{DocText, HandlerOutput, RequestCx, RequestError, params};

/// `textDocument/codeAction`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape.
pub(crate) fn code_action(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: CodeActionParams = params(params_value)?;
    let kind = CodeActionKind::REFACTOR_REWRITE;
    let wanted = p.context.only.as_ref().is_none_or(|only| {
        only.iter().any(|k| {
            kind.as_str() == k.as_str() || kind.as_str().starts_with(&format!("{}.", k.as_str()))
        })
    });
    let mut actions: Vec<CodeActionOrCommand> = Vec::new();
    if wanted {
        let range = doc.lines.text_range(&doc.text, p.range, cx.encoding);
        cx.cancel.check()?;
        if let Some(found) = field_path::rewrite(doc.tree.root(), &doc.text, range) {
            let edit = TextEdit {
                range: doc.lines.range(&doc.text, found.range, cx.encoding),
                new_text: found.text,
            };
            #[expect(
                clippy::mutable_key_type,
                reason = "`WorkspaceEdit` keys its changes by `Uri`, and this map holds one"
            )]
            let changes = HashMap::from([(p.text_document.uri, vec![edit])]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: field_path::TITLE.to_owned(),
                kind: Some(kind),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                ..CodeAction::default()
            }));
        }
    }
    serde_json::to_value(actions)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}
