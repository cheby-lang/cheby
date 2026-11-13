//! `textDocument/foldingRange` (X §5.3, syntax map §7.2), from the live
//! text (class `Text`, D-1367).
//!
//! One walk over the tree folds bodies and multi-line lists, strings and
//! pipelines. Bodies carry no kind, and lists, strings and pipelines are
//! `region`, as the syntax map's table has it. Runs of imports fold as
//! `imports`, and runs of comments on adjacent lines as `comment`, of any
//! mix of `//`, `///` and `//!`, so a file's header and its module doc
//! fold together.

use cheby_driver::analysis::TextRange;
use cheby_syntax::ast::{AstNode, BinExpr};
use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use lsp_types::{FoldingRange, FoldingRangeKind, FoldingRangeParams, Position};
use serde_json::Value;

use crate::features::{DocText, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::LspLines;

/// What the client can take in a `foldingRange` answer, read once from
/// `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldingCaps {
    /// `textDocument.foldingRange.lineFoldingOnly`: whole lines only, so a
    /// fold ends on the line before its closing bracket.
    pub line_folding_only: bool,
    /// `textDocument.foldingRange.rangeLimit`.
    pub range_limit: Option<usize>,
    /// `textDocument.foldingRange.foldingRangeKind.valueSet`, when given:
    /// a kind outside it is not sent.
    pub kinds: Option<Vec<FoldingRangeKind>>,
}

impl FoldingCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters.
    /// A missing or odd capability is absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let base = "/capabilities/textDocument/foldingRange";
        let at = |key: &str| params.pointer(&format!("{base}/{key}"));
        Self {
            line_folding_only: at("lineFoldingOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            range_limit: at("rangeLimit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
            kinds: at("foldingRangeKind/valueSet")
                .and_then(Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .filter_map(|k| serde_json::from_value(k.clone()).ok())
                        .collect()
                }),
        }
    }
}

/// `textDocument/foldingRange`. `caps` comes from `initialize`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape.
pub(crate) fn folding_range(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
    caps: &FoldingCaps,
) -> Result<HandlerOutput, RequestError> {
    let _: FoldingRangeParams = params(params_value)?;
    let ranges = folds(doc.tree.root(), &doc.text, &doc.lines, cx, caps)?;
    serde_json::to_value(ranges)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// One fold before conversion.
struct Fold {
    range: TextRange,
    kind: Option<FoldingRangeKind>,
    /// The fold ends at a closing bracket, which line folding leaves
    /// visible.
    closer: bool,
    /// How many folds hold this one, for `rangeLimit`.
    depth: usize,
}

/// The folds of the tree under `root`, sorted by start and then by end
/// outermost first, at most `caps.range_limit` of them.
/// It stops at the first check after `$/cancelRequest` (A §3.5).
fn folds(
    root: SyntaxNode<'_>,
    text: &str,
    lines: &LspLines,
    cx: &RequestCx<'_>,
    caps: &FoldingCaps,
) -> Result<Vec<FoldingRange>, RequestError> {
    let enc = cx.encoding;
    let mut found: Vec<Fold> = Vec::new();
    nodes(root, cx, &mut found)?;
    comments(root, &mut found);
    cx.cancel.check()?;
    let line = |offset| lines.position(text, offset, enc);
    let mut out: Vec<(Fold, Position, Position)> = found
        .into_iter()
        .filter_map(|fold| {
            let start = line(fold.range.start());
            let mut end = line(fold.range.end());
            if caps.line_folding_only && fold.closer {
                end = Position::new(end.line.checked_sub(1)?, 0);
            }
            (end.line > start.line).then_some((fold, start, end))
        })
        .collect();
    out.sort_by_key(|(fold, ..)| (fold.range.start(), std::cmp::Reverse(fold.range.end())));
    // Two nodes over the same lines fold the same text: keep the outer.
    out.dedup_by_key(|(_, start, end)| (start.line, end.line));
    if let Some(limit) = caps.range_limit
        && out.len() > limit
    {
        // Drop the innermost first (X §5.3).
        let mut by_depth: Vec<usize> = (0..out.len()).collect();
        by_depth.sort_by_key(|&i| out.get(i).map_or(usize::MAX, |(f, ..)| f.depth));
        let mut keep = vec![false; out.len()];
        for i in by_depth.into_iter().take(limit) {
            if let Some(k) = keep.get_mut(i) {
                *k = true;
            }
        }
        out = out
            .into_iter()
            .zip(keep)
            .filter_map(|(fold, k)| k.then_some(fold))
            .collect();
    }
    cx.cancel.check()?;
    Ok(out
        .into_iter()
        .map(|(fold, start, end)| {
            let kind = fold.kind.filter(|kind| {
                caps.kinds
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(kind))
            });
            let characters = !caps.line_folding_only;
            FoldingRange {
                start_line: start.line,
                start_character: characters.then_some(start.character),
                end_line: end.line,
                end_character: characters.then_some(end.character),
                kind,
                collapsed_text: None,
            }
        })
        .collect())
}

/// How many nodes the walk visits between two cancel checks.
const CHECK_EVERY: usize = 4096;

/// The folds of nodes and of runs of imports, in one walk.
fn nodes(
    root: SyntaxNode<'_>,
    cx: &RequestCx<'_>,
    out: &mut Vec<Fold>,
) -> Result<(), RequestError> {
    for (seen, node) in root.descendants().enumerate() {
        if seen % CHECK_EVERY == 0 {
            cx.cancel.check()?;
        }
        let depth = node.ancestors().skip(1).filter(|a| folds_node(*a)).count();
        if folds_node(node) {
            // A pipeline's last token ends its last stage, such as the `)`
            // of a call, which is not a closer of the pipeline: line
            // folding keeps all its lines.
            let closer = node.kind() != SyntaxKind::BIN_EXPR
                && node.last_token().is_some_and(|t| is_closer(t.kind()));
            out.push(Fold {
                range: node.text_range(),
                kind: (!is_body(node.kind())).then_some(FoldingRangeKind::Region),
                closer,
                depth,
            });
        }
    }
    for token in root.tokens().filter(|t| t.kind() == SyntaxKind::RAW_STRING) {
        let depth = token
            .parent()
            .map_or(0, |p| p.ancestors().filter(|a| folds_node(*a)).count());
        out.push(Fold {
            range: token.text_range(),
            kind: Some(FoldingRangeKind::Region),
            closer: false,
            depth,
        });
    }
    // Runs of imports: adjacent `IMPORT_DECL`s of the file, with only
    // trivia and line breaks between them.
    let mut run: Option<TextRange> = None;
    for child in root.children_with_tokens() {
        match child.kind() {
            SyntaxKind::IMPORT_DECL => {
                let r = child.text_range();
                run = Some(run.map_or(r, |open| open.cover(r)));
            }
            kind if kind.is_trivia() || kind == SyntaxKind::NEWLINE => {}
            _ => push_imports(run.take(), out),
        }
    }
    push_imports(run, out);
    Ok(())
}

/// A run of imports, if there is one.
fn push_imports(run: Option<TextRange>, out: &mut Vec<Fold>) {
    if let Some(range) = run {
        out.push(Fold {
            range,
            kind: Some(FoldingRangeKind::Imports),
            closer: false,
            depth: 0,
        });
    }
}

/// Runs of comments of any kind on adjacent lines, with nothing but
/// spaces between them.
fn comments(root: SyntaxNode<'_>, out: &mut Vec<Fold>) {
    let mut run: Option<TextRange> = None;
    let mut breaks = 0_usize;
    let mut flush = |run: &mut Option<TextRange>| {
        if let Some(range) = run.take() {
            out.push(Fold {
                range,
                kind: Some(FoldingRangeKind::Comment),
                closer: false,
                depth: 0,
            });
        }
    };
    for token in root.tokens() {
        let kind = token.kind();
        match kind {
            SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT | SyntaxKind::MODULE_DOC => {
                let range = comment_range(token);
                match &mut run {
                    Some(open) if breaks <= 1 => *open = open.cover(range),
                    _ => {
                        flush(&mut run);
                        run = Some(range);
                    }
                }
                breaks = 0;
            }
            SyntaxKind::WHITESPACE => {}
            SyntaxKind::NEWLINE => breaks = breaks.saturating_add(1),
            _ => {
                flush(&mut run);
                breaks = 0;
            }
        }
    }
    flush(&mut run);
}

/// A comment's range without the line break some comment tokens hold.
fn comment_range(token: SyntaxToken<'_>) -> TextRange {
    let range = token.text_range();
    let trimmed = token.text().trim_end_matches(['\n', '\r']);
    let len = cheby_driver::analysis::TextSize::try_from(trimmed.len()).unwrap_or(range.len());
    TextRange::at(range.start(), len)
}

/// Whether `node` folds when it spans lines (syntax map §7.2).
fn folds_node(node: SyntaxNode<'_>) -> bool {
    use SyntaxKind as K;
    match node.kind() {
        K::BLOCK
        | K::CASE_BODY
        | K::VARIANT_LIST
        | K::RECORD_FIELD_LIST
        | K::INTERFACE_BODY
        | K::ARG_LIST
        | K::PARAM_LIST
        | K::LIST_EXPR
        | K::TUPLE_EXPR
        | K::RECORD_EXPR
        | K::IMPORT_LIST
        | K::POS_FIELD_LIST
        | K::FIELD_PAT_LIST
        | K::LIST_PAT
        | K::TUPLE_PAT
        | K::TYPE_ARG_LIST
        | K::TYPE_PARAM_LIST
        | K::STRING_LIT => true,
        // A pipeline folds once, at its outermost `|>`.
        K::BIN_EXPR => is_pipe(node) && !node.parent().is_some_and(is_pipe),
        _ => false,
    }
}

/// Whether a node of `kind` is a body, which folds with no kind; the
/// other folding nodes are `region` (syntax map §7.2).
fn is_body(kind: SyntaxKind) -> bool {
    use SyntaxKind as K;
    matches!(
        kind,
        K::BLOCK | K::CASE_BODY | K::VARIANT_LIST | K::RECORD_FIELD_LIST | K::INTERFACE_BODY
    )
}

/// Whether `node` is a `|>` stage.
fn is_pipe(node: SyntaxNode<'_>) -> bool {
    BinExpr::cast(node)
        .and_then(BinExpr::op_token)
        .is_some_and(|op| op.kind() == SyntaxKind::PIPE_GT)
}

/// Whether `kind` closes a bracket.
fn is_closer(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::R_BRACE | SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::GT
    )
}

#[cfg(test)]
mod tests {
    use lsp_types::FoldingRangeKind;
    use serde_json::{Value, json};

    use super::{FoldingCaps, folding_range};
    use crate::features::handlers::symbols::tests::{doc, text_document, with_cx};

    /// The folds as `(start line, end line, kind)`.
    fn run(text: &str, caps: &FoldingCaps) -> Vec<(u64, u64, Value)> {
        let doc = doc(text);
        let p = json!({ "textDocument": text_document() });
        let value = with_cx(|cx| folding_range(cx, &doc, p, caps))
            .expect("an answer")
            .value;
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|r| {
                (
                    r["startLine"].as_u64().expect("a line"),
                    r["endLine"].as_u64().expect("a line"),
                    r.get("kind").cloned().unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    const TEXT: &str = r#"//! A module
//! with docs.

import std::io
import std::list

// One
// two

/// Docs.
pub fn main() -> Nil {
  let xs = [
    1,
    2,
  ]
  let s = "a
b"
  xs
  |> list::map(_, fn(x) { x })
  |> list::length
  io::println(s)
}

type T {
  A,
  B,
}
"#;

    fn s(kind: &str) -> Value {
        json!(kind)
    }

    const N: Value = Value::Null;

    #[test]
    fn folds_follow_the_syntax_map() {
        let got = run(TEXT, &FoldingCaps::default());
        assert_eq!(
            got,
            [
                (0, 1, s("comment")),
                (3, 4, s("imports")),
                (6, 7, s("comment")),
                (10, 21, N),
                (11, 14, s("region")),
                (15, 16, s("region")),
                (17, 19, s("region")),
                (23, 26, N),
            ]
        );
    }

    /// A run of comments folds as one whatever their kinds, and a blank
    /// line ends it.
    #[test]
    fn comments_of_any_kind_fold_together() {
        let text = "// @requests: foldingRange
//! A module.

// A note
/// Docs.
fn f() {}
";
        let got = run(text, &FoldingCaps::default());
        assert_eq!(got, [(0, 1, s("comment")), (3, 4, s("comment"))]);
    }

    #[test]
    fn line_folding_ends_before_the_closer() {
        let caps = FoldingCaps {
            line_folding_only: true,
            ..FoldingCaps::default()
        };
        let got = run(TEXT, &caps);
        assert!(got.contains(&(10, 20, N)), "{got:?}");
        assert!(got.contains(&(11, 13, s("region"))), "{got:?}");
        assert!(got.contains(&(23, 25, N)), "{got:?}");
        // A string has no closing bracket, so it keeps its last line.
        assert!(got.contains(&(15, 16, s("region"))), "{got:?}");
        // Nor does a pipeline whose last stage ends in a call's `)`.
        let pipeline = "fn f(xs: List<Int>) -> List<Int> {\n  xs\n  |> list::map(_, g)\n  |> list::filter(_, h)\n}\n";
        let got = run(pipeline, &caps);
        assert!(got.contains(&(1, 3, s("region"))), "{got:?}");
        let doc = doc(TEXT);
        let p = json!({ "textDocument": text_document() });
        let value = with_cx(|cx| folding_range(cx, &doc, p, &caps))
            .expect("an answer")
            .value;
        assert!(value[0].get("startCharacter").is_none(), "{value}");
    }

    #[test]
    fn range_limit_drops_the_innermost() {
        let caps = FoldingCaps {
            range_limit: Some(5),
            ..FoldingCaps::default()
        };
        let got = run(TEXT, &caps);
        assert_eq!(got.len(), 5);
        assert!(
            got.contains(&(10, 21, N)) && got.contains(&(23, 26, N)),
            "{got:?}"
        );
        assert!(!got.contains(&(11, 14, N)), "{got:?}");
    }

    #[test]
    fn kinds_outside_the_value_set_are_not_sent() {
        let caps = FoldingCaps {
            kinds: Some(vec![FoldingRangeKind::Comment]),
            ..FoldingCaps::default()
        };
        let got = run(TEXT, &caps);
        assert!(got.contains(&(3, 4, N)), "{got:?}");
        assert!(got.contains(&(0, 1, s("comment"))), "{got:?}");
    }

    #[test]
    fn a_broken_tree_still_folds() {
        let got = run("fn f() {\n  let x = [\n    1,\n", &FoldingCaps::default());
        assert_ne!(got.len(), 0);
    }

    #[test]
    fn caps_are_read_from_initialize() {
        let p = json!({ "capabilities": { "textDocument": { "foldingRange": {
            "lineFoldingOnly": true,
            "rangeLimit": 5000,
            "foldingRangeKind": { "valueSet": ["comment", "imports", "region"] }
        } } } });
        let caps = FoldingCaps::from_params(&p);
        assert!(caps.line_folding_only);
        assert_eq!(caps.range_limit, Some(5000));
        assert_eq!(caps.kinds.map(|k| k.len()), Some(3));
        assert_eq!(
            FoldingCaps::from_params(&Value::Null),
            FoldingCaps::default()
        );
    }
}
