//! `textDocument/signatureHelp` (X §4.7): what `ide::signature` says about
//! the call at the cursor, as one `SignatureInformation`.
//!
//! Class `Doc`: the answer reads one document's snapshot at the exact text
//! the request saw (D-1367), and no other document. The answer is computed
//! afresh on every request, so a retrigger, as the client sends it while
//! the user types in the list, needs nothing from `context`: the request's
//! `context` is read by no one. A cursor that leaves the list gets `null`,
//! which closes the client's popup.

use std::path::Path;

use cheby_driver::analysis::TextRange;
use lsp_types::{Position, SignatureHelpParams};
use serde_json::{Value, json};

use crate::features::ide::hover::{StdDocs, source};
use crate::features::ide::signature::{SignatureText, file_at, signature_help as help_at};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{LspLines, PositionEncoding};
use crate::text::uri::{normalize, uri_to_path};

/// What the client can take in signature help, read once from
/// `initialize`: `textDocument.signatureHelp.signatureInformation`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SignatureCaps {
    /// `documentationFormat` lists `markdown`.
    pub markdown: bool,
    /// `parameterInformation.labelOffsetSupport`: a parameter's label may
    /// be a range of the signature's label.
    pub label_offsets: bool,
    /// `noActiveParameterSupport` (LSP 3.18): `activeParameter` may be
    /// `null`, so that no parameter is active.
    pub no_active_parameter: bool,
}

impl SignatureCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters. A
    /// client that names nothing gets plain text and string labels, the
    /// protocol's defaults.
    pub(crate) fn from_params(params: &Value) -> Self {
        let info = params.pointer("/capabilities/textDocument/signatureHelp/signatureInformation");
        let flag = |pointer: &str| {
            info.and_then(|i| i.pointer(pointer))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let markdown = info
            .and_then(|i| i.get("documentationFormat"))
            .and_then(Value::as_array)
            .is_some_and(|formats| formats.iter().any(|f| f.as_str() == Some("markdown")));
        Self {
            markdown,
            label_offsets: flag("/parameterInformation/labelOffsetSupport"),
            no_active_parameter: flag("/noActiveParameterSupport"),
        }
    }
}

/// `textDocument/signatureHelp`, class `Doc`. `caps` comes from
/// `initialize`, and `std` is where the docs of std items in a package run
/// come from (D-1385), asked only for those. The answer is `null` outside
/// an argument list and where the callee is unknown (X §1.3).
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// and `Cancelled` when the client cancelled the request.
pub(crate) fn signature_help(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    caps: SignatureCaps,
    std: StdDocs<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: SignatureHelpParams = params(params_value)?;
    let at = p.text_document_position_params;
    let Some(path) = uri_to_path(&at.text_document.uri) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let answer = answer(cx, analyzed, &normalize(&path), at.position, caps, std);
    cx.cancel.check()?;
    Ok(HandlerOutput::value(answer.unwrap_or(Value::Null)))
}

/// The answer at `position` of the document at `path`, if any.
fn answer(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    path: &Path,
    position: Position,
    caps: SignatureCaps,
    std: StdDocs<'_>,
) -> Option<Value> {
    let doc = source(&analyzed.snapshot, path)?;
    let file = file_at(&analyzed.snapshot, path)?;
    let lines = LspLines::new(doc.text, doc.bom);
    let offset = lines.offset(doc.text, position, cx.encoding);
    let found = help_at(analyzed, file, offset, std)?;
    Some(render(&found, caps, cx.encoding))
}

/// `found` as a `SignatureHelp` with one signature (X §4.7).
///
/// Parameter labels are ranges of the label in `enc` for a client with
/// `labelOffsetSupport`, else substrings. With more arguments than
/// parameters no parameter is active (D-1481): `activeParameter` is `null`
/// for a client with `noActiveParameterSupport` (LSP 3.18). Any other
/// client reads a missing or `null` value as 0 (LSP 3.17), so it gets the
/// argument's index, past the last parameter, and the signature stays up
/// while the extra argument is fixed. A client that reads an index out of
/// range as 0 marks the first parameter; the CP2 session checks Zed.
fn render(found: &SignatureText, caps: SignatureCaps, enc: PositionEncoding) -> Value {
    let parameters: Vec<Value> = (0..found.params.len())
        .filter_map(|n| {
            let label = if caps.label_offsets {
                let range = found.params.get(n)?;
                let before = TextRange::new(0, range.start()).slice(&found.label)?;
                let text = found.param_text(n)?;
                let start = units(before, enc);
                json!([start, start.saturating_add(units(text, enc))])
            } else {
                json!(found.param_text(n)?)
            };
            Some(json!({ "label": label }))
        })
        .collect();
    let mut signature = serde_json::Map::new();
    signature.insert("label".to_owned(), json!(found.label));
    signature.insert("parameters".to_owned(), json!(parameters));
    if let Some(docs) = found.documentation(caps.markdown) {
        let kind = if caps.markdown {
            "markdown"
        } else {
            "plaintext"
        };
        signature.insert(
            "documentation".to_owned(),
            json!({ "kind": kind, "value": docs }),
        );
    }
    // D-1481: `null` only for a client that announces it can take it.
    let active = match found.active() {
        Some(n) => json!(n),
        None if caps.no_active_parameter => Value::Null,
        None => json!(found.index),
    };
    json!({
        "signatures": [signature],
        "activeSignature": 0,
        "activeParameter": active,
    })
}

/// How many units of `enc` `text` takes.
fn units(text: &str, enc: PositionEncoding) -> u32 {
    let n = match enc {
        PositionEncoding::Utf8 => text.len(),
        PositionEncoding::Utf16 => text.encode_utf16().count(),
        PositionEncoding::Utf32 => text.chars().count(),
    };
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::TextRange;
    use serde_json::{Value, json};

    use super::{SignatureCaps, render, signature_help};
    use crate::features::handlers::symbols::tests::with_cx;
    use crate::features::ide::hover::StdDocs;
    use crate::features::ide::hover::tests::{Temp, analyze};
    use crate::features::ide::signature::SignatureText;
    use crate::text::lines::PositionEncoding;
    use crate::text::uri::path_to_uri;

    const STANDARD: SignatureCaps = SignatureCaps {
        markdown: true,
        label_offsets: true,
        no_active_parameter: false,
    };

    /// `fn(Int, é: String) -> Int`, at argument `index`.
    fn found(index: u32) -> SignatureText {
        SignatureText {
            label: "fn f(é: String, n: Int) -> Int".to_owned(),
            params: vec![TextRange::new(5, 15), TextRange::new(17, 23)],
            index,
            docs: Some("Docs.".to_owned()),
            ..SignatureText::default()
        }
    }

    #[test]
    fn caps_read_the_signature_information() {
        let caps = SignatureCaps::from_params(&json!({
            "capabilities": { "textDocument": { "signatureHelp": { "signatureInformation": {
                "documentationFormat": ["plaintext", "markdown"],
                "parameterInformation": { "labelOffsetSupport": true },
                "noActiveParameterSupport": true,
            } } } }
        }));
        assert_eq!(
            caps,
            SignatureCaps {
                markdown: true,
                label_offsets: true,
                no_active_parameter: true,
            }
        );
        assert_eq!(
            SignatureCaps::from_params(&json!({})),
            SignatureCaps::default()
        );
    }

    #[test]
    fn offsets_count_in_the_session_encoding() {
        let at = |enc| render(&found(1), STANDARD, enc)["signatures"][0]["parameters"].clone();
        assert_eq!(
            at(PositionEncoding::Utf16),
            json!([{ "label": [5, 14] }, { "label": [16, 22] }])
        );
        assert_eq!(
            at(PositionEncoding::Utf8),
            json!([{ "label": [5, 15] }, { "label": [17, 23] }])
        );
        let strings = render(&found(1), SignatureCaps::default(), PositionEncoding::Utf16);
        assert_eq!(
            strings["signatures"][0]["parameters"],
            json!([{ "label": "é: String" }, { "label": "n: Int" }])
        );
        assert_eq!(
            strings["signatures"][0]["documentation"],
            json!({ "kind": "plaintext", "value": "Docs." })
        );
    }

    /// D-1481, on the rendered answer.
    #[test]
    fn past_the_last_parameter_none_is_active() {
        let enc = PositionEncoding::Utf16;
        assert_eq!(render(&found(1), STANDARD, enc)["activeParameter"], 1);
        assert_eq!(render(&found(2), STANDARD, enc)["activeParameter"], 2);
        let caps = SignatureCaps {
            no_active_parameter: true,
            ..STANDARD
        };
        assert_eq!(render(&found(2), caps, enc)["activeParameter"], Value::Null);
        assert_eq!(render(&found(0), caps, enc)["activeParameter"], 0);
    }

    #[test]
    fn answers_a_call_and_null_elsewhere() {
        let text = "/// Adds.\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn main() {\n  let total = add(1, 2)\n  assert total == 3\n}\n";
        let temp = Temp::package("signature", &[("main.cheby", text)]);
        let path = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
        let uri = path_to_uri(&path).unwrap();
        let at = |line: u32, character: u32| {
            let p = json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": character },
            });
            with_cx(|cx| signature_help(cx, &analyzed, p, STANDARD, StdDocs::None))
                .unwrap()
                .value
        };
        assert_eq!(
            at(6, 21),
            json!({
                "activeParameter": 1,
                "activeSignature": 0,
                "signatures": [{
                    "documentation": { "kind": "markdown", "value": "Adds." },
                    "label": "fn add(a: Int, b: Int) -> Int",
                    "parameters": [{ "label": [7, 13] }, { "label": [15, 21] }],
                }],
            })
        );
        assert_eq!(at(6, 18)["activeParameter"], 0);
        assert_eq!(at(7, 4), Value::Null);
        let malformed =
            with_cx(|cx| signature_help(cx, &analyzed, json!(1), STANDARD, StdDocs::None));
        assert!(malformed.is_err());
    }

    /// D-1481 and D-1482 through the handler, on analyzed text: past the
    /// last parameter, `null` for a client with `noActiveParameterSupport`
    /// and the argument's index for any other; a generic callee keeps its
    /// declared label, with the instantiated form first in the
    /// documentation. The golden harness's clients cannot announce
    /// `noActiveParameterSupport`, so this test pins the `null`.
    #[test]
    fn extra_arguments_and_generic_callees() {
        let text = "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn first_or<T>(xs: List<T>, fallback: T) -> T {\n  case xs {\n    [head, ..] => head\n    [] => fallback\n  }\n}\n\nfn main() {\n  let extra = add(1, 2, 3)\n  assert extra > 0\n}\n\nfn second() -> Int {\n  first_or([1], 0)\n}\n";
        let temp = Temp::package("signature_extra", &[("main.cheby", text)]);
        let path = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
        let uri = path_to_uri(&path).unwrap();
        let at = |line: u32, character: u32, caps: SignatureCaps| {
            let p = json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": character },
            });
            with_cx(|cx| signature_help(cx, &analyzed, p, caps, StdDocs::None))
                .unwrap()
                .value
        };
        let announcing = SignatureCaps {
            no_active_parameter: true,
            ..STANDARD
        };
        // `add(1, 2, 3|)`: the third argument of two parameters.
        assert_eq!(at(12, 24, STANDARD)["activeParameter"], 2);
        assert_eq!(at(12, 24, announcing)["activeParameter"], Value::Null);
        assert_eq!(at(12, 21, announcing)["activeParameter"], 1);
        let generic = at(17, 16, STANDARD);
        assert_eq!(generic["activeParameter"], 1);
        assert_eq!(
            generic["signatures"][0]["label"],
            "fn first_or<T>(xs: List<T>, fallback: T) -> T"
        );
        assert_eq!(
            generic["signatures"][0]["documentation"],
            json!({ "kind": "markdown", "value": "```cheby\nfn(List<Int>, Int) -> Int\n```" })
        );
    }
}
