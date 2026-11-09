//! `textDocument/selectionRange` (X §5.3, syntax map §7.3), from the live
//! text (class `Text`, D-1367).
//!
//! For each position: the token there, then each enclosing node. A node
//! whose doc comments are part of it gives two steps, its code and then
//! the code with its docs, and a bracketed list gives its contents before
//! the list with its brackets. Equal ranges are given once. One answer per
//! position, in request order.

use cheby_driver::analysis::{TextRange, TextSize};
use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use lsp_types::{SelectionRange, SelectionRangeParams};
use serde_json::Value;

use crate::features::ide::cursor::{TokenAt, is_significant, token_at};
use crate::features::{DocText, HandlerOutput, RequestCx, RequestError, params};

/// `textDocument/selectionRange`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape.
pub(crate) fn selection_range(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: SelectionRangeParams = params(params_value)?;
    let root = doc.tree.root();
    let answers: Vec<SelectionRange> = p
        .positions
        .iter()
        .map(|&position| {
            let offset = doc.lines.offset(&doc.text, position, cx.encoding);
            let chain = chain(root, offset);
            // Innermost first: build from the outside in.
            chain
                .into_iter()
                .rev()
                .fold(None, |parent, range| {
                    Some(SelectionRange {
                        range: doc.lines.range(&doc.text, range, cx.encoding),
                        parent: parent.map(Box::new),
                    })
                })
                .unwrap_or_else(|| SelectionRange {
                    range: lsp_types::Range::new(position, position),
                    parent: None,
                })
        })
        .collect();
    serde_json::to_value(answers)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The ranges a selection grows through from `offset`, innermost first,
/// each strictly holding the one before.
fn chain(root: SyntaxNode<'_>, offset: TextSize) -> Vec<TextRange> {
    let at = token_at(root, offset);
    // Left-biased (syntax map §7.3), but a name, keyword or literal on the
    // right beats punctuation on the left, and anything beats space.
    let token = match at {
        TokenAt::Between(l, r) if weight(r) > weight(l) => Some(r),
        _ => at.left(),
    };
    let Some(token) = token else {
        return Vec::new();
    };
    let mut ranges = vec![token.text_range()];
    let mut push = |range: TextRange| {
        let grows = ranges
            .last()
            .is_none_or(|last| range != *last && range.contains_range(*last));
        if grows {
            ranges.push(range);
        }
    };
    for node in token.parent().into_iter().flat_map(SyntaxNode::ancestors) {
        if let Some(inner) = list_contents(node) {
            push(inner);
        }
        if let Some(code) = significant(node) {
            push(code);
        }
        push(node.text_range());
    }
    ranges
}

/// How much a selection wants to start at `token`: 2 for a name, keyword
/// or literal, 1 for other code, 0 for space, comments and line breaks.
fn weight(token: SyntaxToken<'_>) -> u8 {
    use SyntaxKind as K;
    let kind = token.kind();
    if !is_significant(kind) {
        0
    } else if kind.is_keyword()
        || matches!(
            kind,
            K::LOWER_IDENT
                | K::UPPER_IDENT
                | K::UNDERSCORE
                | K::INT_NUMBER
                | K::FLOAT_NUMBER
                | K::RAW_STRING
                | K::STRING_TEXT
        )
    {
        2
    } else {
        1
    }
}

/// The range from `node`'s first to its last token that is neither trivia
/// nor a line break, which leaves out leading doc comments.
fn significant(node: SyntaxNode<'_>) -> Option<TextRange> {
    let first = node.tokens().find(|t| is_significant(t.kind()))?;
    let last = node.tokens().rev().find(|t| is_significant(t.kind()))?;
    Some(first.text_range().cover(last.text_range()))
}

/// The contents of a bracketed list without its brackets, from the first
/// to the last significant token between them.
fn list_contents(node: SyntaxNode<'_>) -> Option<TextRange> {
    use SyntaxKind as K;
    let is_list = matches!(
        node.kind(),
        K::ARG_LIST
            | K::PARAM_LIST
            | K::LIST_EXPR
            | K::TUPLE_EXPR
            | K::IMPORT_LIST
            | K::POS_FIELD_LIST
            | K::RECORD_FIELD_LIST
            | K::VARIANT_LIST
            | K::FIELD_PAT_LIST
            | K::POS_PAT_LIST
            | K::LIST_PAT
            | K::TUPLE_PAT
            | K::TYPE_ARG_LIST
            | K::TYPE_PARAM_LIST
            | K::TYPE_LIST
            | K::ATTR_ARG_LIST
    );
    if !is_list {
        return None;
    }
    let mut inside = node
        .tokens()
        .filter(|t| is_significant(t.kind()))
        .skip_while(|t| !is_opener(*t));
    inside.next()?;
    let tokens: Vec<SyntaxToken<'_>> = inside.collect();
    let (close, between) = tokens.split_last()?;
    if !is_closer(*close) {
        return None;
    }
    let first = between.first()?;
    let last = between.last()?;
    Some(first.text_range().cover(last.text_range()))
}

/// Whether `token` opens a list.
fn is_opener(token: SyntaxToken<'_>) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::L_PAREN | SyntaxKind::L_BRACK | SyntaxKind::L_BRACE | SyntaxKind::LT
    )
}

/// Whether `token` closes a list.
fn is_closer(token: SyntaxToken<'_>) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_BRACE | SyntaxKind::GT
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use lsp_types::Position;

    use super::selection_range;
    use crate::features::handlers::symbols::tests::{doc, text_document, with_cx};
    use crate::text::lines::{Bom, LspLines, PositionEncoding};

    /// The texts the selection at each `(line, character)` grows through.
    fn steps(text: &str, positions: &[(u32, u32)]) -> Vec<Vec<String>> {
        let doc = doc(text);
        let positions: Vec<Value> = positions
            .iter()
            .map(|&(line, character)| json!({ "line": line, "character": character }))
            .collect();
        let p = json!({ "textDocument": text_document(), "positions": positions });
        let value = with_cx(|cx| selection_range(cx, &doc, p))
            .expect("an answer")
            .value;
        let lines = LspLines::new(text, Bom::Counted);
        let offset = |pos: &Value| -> usize {
            let pos: Position = serde_json::from_value(pos.clone()).expect("a position");
            let at = lines.offset(text, pos, PositionEncoding::Utf16);
            usize::try_from(at).expect("small")
        };
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|mut range| {
                let mut out = Vec::new();
                loop {
                    let r = &range["range"];
                    let part = text.get(offset(&r["start"])..offset(&r["end"]));
                    out.push(part.expect("a range of the text").to_owned());
                    match range.get("parent") {
                        Some(parent) => range = parent,
                        None => break,
                    }
                }
                out
            })
            .collect()
    }

    const TEXT: &str = "/// Docs.\nfn f(a: Int, b: Int) -> Int {\n  g(a, b)\n}\n";

    #[test]
    fn grows_from_the_token_through_list_contents_and_docs() {
        let got = steps(TEXT, &[(2, 4)]);
        let want = [
            "a",
            "a, b",
            "(a, b)",
            "g(a, b)",
            "{\n  g(a, b)\n}",
            "fn f(a: Int, b: Int) -> Int {\n  g(a, b)\n}",
            "/// Docs.\nfn f(a: Int, b: Int) -> Int {\n  g(a, b)\n}",
            TEXT,
        ];
        let got = got.first().expect("one answer");
        // The path and the call's callee may add steps between; the order
        // of these is what matters.
        let mut at = got.iter();
        for step in want {
            assert!(
                at.any(|g| g == step),
                "{step:?} missing in order from {got:#?}"
            );
        }
    }

    #[test]
    fn one_answer_per_position_in_order() {
        let got = steps(TEXT, &[(1, 5), (2, 2), (99, 0)]);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.first().and_then(|c| c.first()).map(String::as_str),
            Some("a")
        );
        assert_eq!(
            got.get(1).and_then(|c| c.first()).map(String::as_str),
            Some("g")
        );
        for chain in &got {
            for pair in chain.windows(2) {
                assert!(pair[1].len() > pair[0].len(), "{chain:#?}");
            }
        }
    }

    #[test]
    fn utf16_positions_after_non_ascii() {
        let text = "fn f() {\n  \"🙂\" + x\n}\n";
        let got = steps(text, &[(1, 9)]);
        assert_eq!(
            got.first().and_then(|c| c.first()).map(String::as_str),
            Some("x")
        );
    }
}
