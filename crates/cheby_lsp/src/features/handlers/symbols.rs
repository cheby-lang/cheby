//! `textDocument/documentSymbol` (X §5.1): the outline of
//! `ide::symbols`, from the live text (class `Text`, D-1367).
//!
//! The answer is hierarchical when the client has
//! `hierarchicalDocumentSymbolSupport`, else flat `SymbolInformation`s
//! whose `containerName` is the parent's name.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, Location, SymbolInformation, SymbolKind, SymbolTag, Uri,
};
use serde_json::Value;

use crate::features::ide::symbols::{OutlineItem, OutlineKind, outline};
use crate::features::{DocText, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{LspLines, PositionEncoding};

/// What the client can take in a `documentSymbol` answer, read once from
/// `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SymbolCaps {
    /// `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`.
    pub hierarchical: bool,
    /// `textDocument.documentSymbol.tagSupport` lists `Deprecated`, so a
    /// deprecated item is tagged rather than flagged.
    pub deprecated_tag: bool,
}

impl SymbolCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters.
    /// A missing or odd capability is absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let base = "/capabilities/textDocument/documentSymbol";
        let hierarchical = params
            .pointer(&format!("{base}/hierarchicalDocumentSymbolSupport"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let deprecated_tag = params
            .pointer(&format!("{base}/tagSupport/valueSet"))
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));
        Self {
            hierarchical,
            deprecated_tag,
        }
    }
}

/// `textDocument/documentSymbol`. `caps` comes from `initialize`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape.
pub(crate) fn document_symbol(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
    caps: SymbolCaps,
) -> Result<HandlerOutput, RequestError> {
    let p: DocumentSymbolParams = params(params_value)?;
    let items = outline(doc.tree.root());
    cx.cancel.check()?;
    let conv = Conv {
        text: &doc.text,
        lines: &doc.lines,
        enc: cx.encoding,
        caps,
    };
    // One check per top-level item (A §3.5).
    let value = if caps.hierarchical {
        let mut symbols: Vec<DocumentSymbol> = Vec::with_capacity(items.len());
        for item in &items {
            cx.cancel.check()?;
            symbols.push(conv.nested(item));
        }
        serde_json::to_value(symbols)
    } else {
        let mut flat = Vec::new();
        for item in &items {
            cx.cancel.check()?;
            conv.flat(
                std::slice::from_ref(item),
                None,
                &p.text_document.uri,
                &mut flat,
            );
        }
        serde_json::to_value(flat)
    };
    cx.cancel.check()?;
    value
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// What converting an outline needs.
struct Conv<'a> {
    text: &'a str,
    lines: &'a LspLines,
    enc: PositionEncoding,
    caps: SymbolCaps,
}

impl Conv<'_> {
    /// `item` and its children as a `DocumentSymbol`.
    #[expect(deprecated, reason = "the protocol's struct still has the field")]
    fn nested(&self, item: &OutlineItem) -> DocumentSymbol {
        let (tags, deprecated) = self.deprecation(item);
        let children: Vec<DocumentSymbol> = item.children.iter().map(|c| self.nested(c)).collect();
        DocumentSymbol {
            name: item.name.clone(),
            detail: item.detail.clone(),
            kind: kind(item.kind),
            tags,
            deprecated,
            range: self.lines.range(self.text, item.range, self.enc),
            selection_range: self.lines.range(self.text, item.selection, self.enc),
            children: (!children.is_empty()).then_some(children),
        }
    }

    /// `items` and their children as `SymbolInformation`s, in outline
    /// order, each naming its parent.
    #[expect(deprecated, reason = "the protocol's struct still has the field")]
    fn flat(
        &self,
        items: &[OutlineItem],
        container: Option<&str>,
        uri: &Uri,
        out: &mut Vec<SymbolInformation>,
    ) {
        for item in items {
            let (tags, deprecated) = self.deprecation(item);
            out.push(SymbolInformation {
                name: item.name.clone(),
                kind: kind(item.kind),
                tags,
                deprecated,
                location: Location::new(
                    uri.clone(),
                    self.lines.range(self.text, item.range, self.enc),
                ),
                container_name: container.map(str::to_owned),
            });
            self.flat(&item.children, Some(&item.name), uri, out);
        }
    }

    /// The tag, or for a client without tag support the old flag, of a
    /// deprecated item.
    fn deprecation(&self, item: &OutlineItem) -> (Option<Vec<SymbolTag>>, Option<bool>) {
        match (item.deprecated, self.caps.deprecated_tag) {
            (false, _) => (None, None),
            (true, true) => (Some(vec![SymbolTag::DEPRECATED]), None),
            (true, false) => (None, Some(true)),
        }
    }
}

/// The protocol's kind of an outline entry (syntax map §7.4).
fn kind(kind: OutlineKind) -> SymbolKind {
    match kind {
        OutlineKind::Function | OutlineKind::Test => SymbolKind::FUNCTION,
        OutlineKind::Enum => SymbolKind::ENUM,
        OutlineKind::Struct => SymbolKind::STRUCT,
        OutlineKind::Opaque => SymbolKind::CLASS,
        OutlineKind::Alias => SymbolKind::TYPE_PARAMETER,
        OutlineKind::Variant => SymbolKind::ENUM_MEMBER,
        OutlineKind::Field => SymbolKind::FIELD,
        OutlineKind::Constant => SymbolKind::CONSTANT,
        OutlineKind::Interface => SymbolKind::INTERFACE,
        OutlineKind::InterfaceFn => SymbolKind::METHOD,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::{SymbolCaps, document_symbol};
    use crate::features::{CancelToken, DocText, RequestCx};
    use crate::server::{ClientCaps, Settings};
    use crate::text::lines::PositionEncoding;

    /// Runs `f` with a request context over default settings, in UTF-16.
    pub(crate) fn with_cx<T>(f: impl FnOnce(&RequestCx<'_>) -> T) -> T {
        let settings = Settings::default();
        let caps = ClientCaps::default();
        let cancel = CancelToken::new();
        let cx = RequestCx {
            settings: &settings,
            caps: &caps,
            encoding: PositionEncoding::Utf16,
            cancel: &cancel,
        };
        f(&cx)
    }

    /// The document `text` at version 1.
    pub(crate) fn doc(text: &str) -> DocText {
        DocText::new(PathBuf::from("/p/src/main.cheby"), Some(1), Arc::from(text))
            .expect("a small text")
    }

    /// The `textDocument` parameter of the test document.
    pub(crate) fn text_document() -> Value {
        json!({ "uri": "file:///p/src/main.cheby" })
    }

    const TEXT: &str =
        "/// Docs.\n@deprecated(\"no\")\npub type T {\n  A,\n  B(Int),\n}\n\nfn é() {\n  1\n}\n";

    fn run(caps: SymbolCaps) -> Value {
        let doc = doc(TEXT);
        let p = json!({ "textDocument": text_document() });
        with_cx(|cx| document_symbol(cx, &doc, p, caps))
            .expect("an answer")
            .value
    }

    #[test]
    fn hierarchical_answer() {
        let caps = SymbolCaps {
            hierarchical: true,
            deprecated_tag: true,
        };
        let got = run(caps);
        let want = json!([
            {
                "name": "T",
                "kind": 10,
                "tags": [1],
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": {
                    "start": { "line": 2, "character": 9 },
                    "end": { "line": 2, "character": 10 }
                },
                "children": [
                    {
                        "name": "A",
                        "kind": 22,
                        "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 3 } },
                        "selectionRange": {
                            "start": { "line": 3, "character": 2 },
                            "end": { "line": 3, "character": 3 }
                        }
                    },
                    {
                        "name": "B",
                        "detail": "(Int)",
                        "kind": 22,
                        "range": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 8 } },
                        "selectionRange": {
                            "start": { "line": 4, "character": 2 },
                            "end": { "line": 4, "character": 3 }
                        }
                    }
                ]
            },
            {
                "name": "é",
                "detail": "()",
                "kind": 12,
                "range": { "start": { "line": 7, "character": 0 }, "end": { "line": 9, "character": 1 } },
                "selectionRange": {
                    "start": { "line": 7, "character": 3 },
                    "end": { "line": 7, "character": 4 }
                }
            }
        ]);
        assert_eq!(got, want);
    }

    #[test]
    fn flat_answer_names_containers() {
        let got = run(SymbolCaps::default());
        let rows: Vec<(String, Value, Value)> = got
            .as_array()
            .expect("a list")
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().expect("a name").to_owned(),
                    s["containerName"].clone(),
                    s["deprecated"].clone(),
                )
            })
            .collect();
        let s = |x: &str| x.to_owned();
        assert_eq!(
            rows,
            [
                (s("T"), Value::Null, json!(true)),
                (s("A"), json!("T"), Value::Null),
                (s("B"), json!("T"), Value::Null),
                (s("é"), Value::Null, Value::Null),
            ]
        );
        assert_eq!(got[0]["location"]["uri"], json!("file:///p/src/main.cheby"));
    }

    #[test]
    fn caps_are_read_from_initialize() {
        let p = json!({ "capabilities": { "textDocument": { "documentSymbol": {
            "hierarchicalDocumentSymbolSupport": true,
            "tagSupport": { "valueSet": [1] }
        } } } });
        let caps = SymbolCaps::from_params(&p);
        assert!(caps.hierarchical && caps.deprecated_tag);
        assert_eq!(SymbolCaps::from_params(&json!({})), SymbolCaps::default());
    }

    #[test]
    fn bad_params_are_invalid_params() {
        let doc = doc(TEXT);
        let got = with_cx(|cx| document_symbol(cx, &doc, json!(3), SymbolCaps::default()));
        assert!(matches!(
            got,
            Err(crate::features::RequestError::InvalidParams(_))
        ));
    }
}
