//! `textDocument/semanticTokens/full` and `/range` (X §5.5, D-1396): the
//! tokens of `ide::tokens`, encoded relative to each other in the
//! session's position encoding.
//!
//! Both are class `Doc`: they read the snapshot of the document's exact
//! text and no other document's. There is no `delta` and no `resultId`
//! (D-1396). With `semanticTokens.enabled` off, both answer `null`, and the
//! client keeps its own colours (D-1376, D-1396).

use std::path::Path;

use cheby_driver::analysis::{FileLocation, Snapshot};
use lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensServerCapabilities, TextDocumentIdentifier,
};
use serde_json::Value;

use crate::features::ide::tokens::{Modifiers, Token, TokenType, tokens};
use crate::features::index::ModuleIndex;
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{Bom, LspLines};
use crate::text::uri::{normalize, uri_to_path};

/// The token types of the legend, in order (X §5.5).
pub(crate) const TOKEN_TYPES: [&str; 12] = [
    TokenType::Namespace.name(),
    TokenType::Type.name(),
    TokenType::Enum.name(),
    TokenType::Struct.name(),
    TokenType::Interface.name(),
    TokenType::TypeParameter.name(),
    TokenType::Parameter.name(),
    TokenType::Variable.name(),
    TokenType::Property.name(),
    TokenType::EnumMember.name(),
    TokenType::Function.name(),
    TokenType::Method.name(),
];

/// The token modifiers of the legend, bit 0 first (X §5.5).
pub(crate) const TOKEN_MODIFIERS: [&str; 5] = Modifiers::NAMES;

/// The legend the server advertises (X §5.5).
pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES
            .iter()
            .map(|&name| SemanticTokenType::new(name))
            .collect(),
        token_modifiers: TOKEN_MODIFIERS
            .iter()
            .map(|&name| SemanticTokenModifier::new(name))
            .collect(),
    }
}

/// `semanticTokensProvider` as `initialize` advertises it: the legend,
/// `full` with no `delta`, and `range` (D-1396).
pub(crate) fn provider() -> SemanticTokensServerCapabilities {
    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
        work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
        legend: legend(),
        range: Some(true),
        full: Some(SemanticTokensFullOptions::Bool(true)),
    })
}

/// `textDocument/semanticTokens/full`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape, and
/// `Cancelled` on `$/cancelRequest`.
pub(crate) fn full(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: SemanticTokensParams = params(params_value)?;
    answer(cx, analyzed, &p.text_document, None)
}

/// `textDocument/semanticTokens/range`: the tokens that overlap the range.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape, and
/// `Cancelled` on `$/cancelRequest`.
pub(crate) fn range(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: SemanticTokensRangeParams = params(params_value)?;
    answer(cx, analyzed, &p.text_document, Some(p.range))
}

/// The tokens of `document`, all of them or those in `range`. `null` when
/// the setting is off or the snapshot has no module for the document.
fn answer(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    document: &TextDocumentIdentifier,
    range: Option<lsp_types::Range>,
) -> Result<HandlerOutput, RequestError> {
    if !cx.settings.semantic_tokens.enabled {
        return Ok(HandlerOutput::value(Value::Null));
    }
    let snapshot = &*analyzed.snapshot;
    let Some(path) = uri_to_path(&document.uri).map(|p| normalize(&p)) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(module) = module_of(analyzed, &path) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(source) = module.file.and_then(|file| snapshot.source(file)) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let text = source.text();
    // The client's own text counts a BOM, disk text hides it (D-1368).
    let bom = if snapshot.version_of(&path).is_some() {
        Bom::Counted
    } else {
        Bom::Hidden
    };
    let lines = LspLines::new(text, bom);
    let within = range.map(|r| lines.text_range(text, r, cx.encoding));
    let found = tokens(snapshot, module, within, cx.cancel)?;
    cx.cancel.check()?;
    let data = encode(&found, text, &lines, cx);
    serde_json::to_value(SemanticTokens {
        result_id: None,
        data,
    })
    .map(HandlerOutput::value)
    .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The index of the module at `path`: a package file, or a std source
/// opened from its materialized directory, whose display path ends it
/// (A §6.3).
fn module_of<'a>(analyzed: &'a Analyzed, path: &Path) -> Option<&'a ModuleIndex> {
    let snapshot: &Snapshot = &analyzed.snapshot;
    let file = snapshot.file_of(path).or_else(|| {
        snapshot
            .sources()
            .files()
            .find_map(|(file, _)| match snapshot.location(file)? {
                FileLocation::Std(display) => path.ends_with(display).then_some(file),
                FileLocation::Package(_) | FileLocation::Synthetic => None,
            })
    })?;
    analyzed.index().of_file(file)
}

/// `found`, in text order, as the protocol's relative tokens in the
/// session's encoding. Identifiers never span lines, so each token's
/// length is the column difference of its ends.
fn encode(found: &[Token], text: &str, lines: &LspLines, cx: &RequestCx<'_>) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(found.len());
    let (mut line, mut column) = (0_u32, 0_u32);
    for token in found {
        let start = lines.position(text, token.range.start(), cx.encoding);
        let end = lines.position(text, token.range.end(), cx.encoding);
        let delta_line = start.line.saturating_sub(line);
        let delta_start = if delta_line == 0 {
            start.character.saturating_sub(column)
        } else {
            start.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: end.character.saturating_sub(start.character),
            token_type: token.ty.index(),
            token_modifiers_bitset: token.mods.bits(),
        });
        line = start.line;
        column = start.character;
    }
    out
}

#[cfg(test)]
mod tests;
