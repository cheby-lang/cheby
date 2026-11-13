//! `textDocument/hover` (X §2.1, F §1.8): what `ide::hover` says about the
//! token at the cursor, as Markdown, or as plain text for a client that
//! takes no Markdown.
//!
//! Class `Doc`: the answer reads one document's snapshot at the exact text
//! the request saw (D-1367), and no other document.

use std::path::Path;

use lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};
use serde_json::Value;

use crate::features::ide::hover::{StdDocs, hover as hover_at, source};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::LspLines;
use crate::text::uri::{normalize, uri_to_path};

/// What the client can take in a hover, read once from `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HoverCaps {
    /// `textDocument.hover.contentFormat` lists `markdown`.
    pub markdown: bool,
}

impl HoverCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters. A
    /// client that names no format gets plain text, the protocol's
    /// default.
    pub(crate) fn from_params(params: &Value) -> Self {
        let markdown = params
            .pointer("/capabilities/textDocument/hover/contentFormat")
            .and_then(Value::as_array)
            .is_some_and(|formats| formats.iter().any(|f| f.as_str() == Some("markdown")));
        Self { markdown }
    }
}

/// `textDocument/hover`, class `Doc`. `caps` comes from `initialize`, and
/// `std` is where the docs of std items in a package run come from
/// (D-1385), asked only for those. The answer is `null` where hover has nothing to
/// say, such as on a keyword or punctuation (X §1.4).
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// and `Cancelled` when the client cancelled the request.
pub(crate) fn hover(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    caps: HoverCaps,
    std: StdDocs<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: HoverParams = params(params_value)?;
    let at = p.text_document_position_params;
    let Some(path) = uri_to_path(&at.text_document.uri) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let answer = answer(cx, analyzed, &normalize(&path), at.position, caps, std);
    cx.cancel.check()?;
    serde_json::to_value(answer)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The hover at `position` of the document at `path`, if any.
fn answer(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    path: &Path,
    position: lsp_types::Position,
    caps: HoverCaps,
    std: StdDocs<'_>,
) -> Option<Hover> {
    let doc = source(&analyzed.snapshot, path)?;
    let lines = LspLines::new(doc.text, doc.bom);
    let offset = lines.offset(doc.text, position, cx.encoding);
    let found = hover_at(analyzed, &doc, offset, std, cx.constants.as_deref())?;
    let kind = if caps.markdown {
        MarkupKind::Markdown
    } else {
        MarkupKind::PlainText
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind,
            value: found.render(caps.markdown),
        }),
        range: Some(lines.range(doc.text, found.range, cx.encoding)),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{HoverCaps, StdDocs, hover};
    use crate::features::handlers::symbols::tests::with_cx;
    use crate::features::ide::hover::tests::{Temp, analyze};
    use crate::text::uri::path_to_uri;

    #[test]
    fn answers_markdown_or_plain_text_with_the_token_range() {
        let text = "fn main() {\n  let total = 1 + 2\n  assert total == 3\n}\n";
        let temp = Temp::package("handler", &[("main.cheby", text)]);
        let path = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
        let uri = path_to_uri(&path).unwrap();
        let at = |line: u32, character: u32, markdown: bool| {
            let p = json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": character },
            });
            with_cx(|cx| hover(cx, &analyzed, p, HoverCaps { markdown }, StdDocs::None))
                .unwrap()
                .value
        };
        let range = json!({
            "start": { "line": 2, "character": 9 },
            "end": { "line": 2, "character": 14 },
        });
        assert_eq!(
            at(2, 11, true),
            json!({
                "contents": { "kind": "markdown", "value": "```cheby\nlet total: Int\n```" },
                "range": range,
            })
        );
        assert_eq!(
            at(2, 14, false),
            json!({
                "contents": { "kind": "plaintext", "value": "let total: Int" },
                "range": range,
            })
        );
        assert_eq!(at(0, 0, true), serde_json::Value::Null);
        let other = json!({
            "textDocument": { "uri": "file:///nowhere/main.cheby" },
            "position": { "line": 0, "character": 0 },
        });
        let answer = with_cx(|cx| hover(cx, &analyzed, other, HoverCaps::default(), StdDocs::None));
        assert_eq!(answer.unwrap().value, serde_json::Value::Null);
        let malformed =
            with_cx(|cx| hover(cx, &analyzed, json!(1), HoverCaps::default(), StdDocs::None));
        assert!(malformed.is_err());
    }

    #[test]
    fn markdown_only_when_the_client_lists_it() {
        let caps = |formats: serde_json::Value| {
            HoverCaps::from_params(&json!({
                "capabilities": { "textDocument": { "hover": { "contentFormat": formats } } }
            }))
        };
        assert!(caps(json!(["markdown", "plaintext"])).markdown);
        assert!(caps(json!(["plaintext", "markdown"])).markdown);
        assert!(!caps(json!(["plaintext"])).markdown);
        assert!(!HoverCaps::from_params(&json!({})).markdown);
    }
}
