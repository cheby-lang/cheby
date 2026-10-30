//! The corpus test of the formatter (D-1031): every example source
//! (`docs/examples/**/src/**/*.cheby`) and every std source
//! (`std/**/*.cheby`), formatted at 80, 100 and 120 columns.
//!
//! - Each formats without error, and a second pass changes nothing.
//! - Formatting keeps the meaning (D-1153): the tokens
//!   without whitespace, line breaks and comments are unchanged, and so are
//!   the comments' texts in order; the formatted text parses with no grammar
//!   error into a tree whose node kinds, trivia aside, equal the original's.
//!   Two changes are the style's own and are compared as such: imports are
//!   sorted within their block (D-1030), so the imports are compared as a
//!   set, and a list the printer lays out gains a trailing comma when it
//!   breaks and loses it when it fits, so that comma is left out, except
//!   the comma of a one-element tuple, which makes it a tuple and is
//!   always compared (D-1152).
//!
//! The same checks run over the UI test sources without a grammar error.
//! The test only reads the repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::FileId;
use cheby_fmt::{Coverage, coverage, format_source};
use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxToken, SyntaxTree, parse};
use cheby_test_support::{display_relative, example, files_with_extension, repo_root};

/// The widths of the CP2 measurement (D-1030), from which CP2 chose 100
/// (D-1252); the corpus tests keep checking all three.
const WIDTHS: [usize; 3] = [80, 100, 120];

/// Every example and std source, sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable corpus fails the test"
)]
fn corpus(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for ex in example::all(root).unwrap() {
        files.extend(ex.sources().unwrap());
    }
    files.extend(files_with_extension(&root.join("std"), "cheby").unwrap());
    files
}

/// The lists whose trailing comma the printer adds or removes.
const fn is_laid_out_list(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::TYPE_PARAM_LIST
            | SyntaxKind::PARAM_LIST
            | SyntaxKind::TYPE_LIST
            | SyntaxKind::POS_FIELD_LIST
            | SyntaxKind::ATTR_ARG_LIST
            | SyntaxKind::IMPORT_LIST
            | SyntaxKind::VARIANT_LIST
            | SyntaxKind::RECORD_FIELD_LIST
            | SyntaxKind::ARG_LIST
            | SyntaxKind::LIST_EXPR
            | SyntaxKind::RECORD_EXPR
            | SyntaxKind::TUPLE_EXPR
            | SyntaxKind::TYPE_ARG_LIST
            | SyntaxKind::TUPLE_TYPE
            | SyntaxKind::POS_PAT_LIST
            | SyntaxKind::FIELD_PAT_LIST
            | SyntaxKind::LIST_PAT
            | SyntaxKind::TUPLE_PAT
    )
}

/// Whether `node` is a one-element tuple, whose comma makes it a tuple:
/// `(x,)` is one and `(x)` is not, so that comma is always compared.
fn is_one_element_tuple(node: SyntaxNode<'_>) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::TUPLE_EXPR | SyntaxKind::TUPLE_PAT | SyntaxKind::TUPLE_TYPE
    ) && node.children().count() == 1
}

fn is_significant(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && kind != SyntaxKind::NEWLINE
}

/// What formatting must keep of one top-level node: its node kinds in
/// preorder and its significant tokens, without a laid-out list's
/// trailing comma; a one-element tuple's comma is kept.
fn shape(node: SyntaxNode<'_>) -> (Vec<SyntaxKind>, Vec<String>) {
    let kinds = node.descendants().map(SyntaxNode::kind).collect();
    let tokens: Vec<_> = node.tokens().filter(|t| is_significant(t.kind())).collect();
    let mut texts = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let trailing_comma = token.kind() == SyntaxKind::COMMA
            && token
                .parent()
                .is_some_and(|p| is_laid_out_list(p.kind()) && !is_one_element_tuple(p))
            && tokens.get(i.saturating_add(1)).is_some_and(|next| {
                matches!(
                    next.kind(),
                    SyntaxKind::R_PAREN
                        | SyntaxKind::GT
                        | SyntaxKind::R_BRACE
                        | SyntaxKind::R_BRACK
                ) && next.parent() == token.parent()
            });
        if !trailing_comma {
            texts.push(token.text().replace("\r\n", "\n"));
        }
    }
    (kinds, texts)
}

/// What formatting must keep of a file: the imports as a sorted list,
/// the other top-level nodes in order, and the comments in order.
#[derive(Debug, PartialEq, Eq)]
struct Meaning {
    imports: Vec<(Vec<SyntaxKind>, Vec<String>)>,
    others: Vec<(Vec<SyntaxKind>, Vec<String>)>,
    comments: Vec<String>,
}

fn meaning(tree: &SyntaxTree) -> Meaning {
    let mut imports = Vec::new();
    let mut others = Vec::new();
    for node in tree.root().children() {
        if node.kind() == SyntaxKind::IMPORT_DECL {
            imports.push(shape(node));
        } else {
            others.push(shape(node));
        }
    }
    imports.sort();
    let comments = tree
        .root()
        .tokens()
        .filter(|t| {
            matches!(
                t.kind(),
                SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT | SyntaxKind::MODULE_DOC
            )
        })
        .map(|t| t.text().trim_end().to_owned())
        .collect();
    Meaning {
        imports,
        others,
        comments,
    }
}

fn parse_clean(text: &str) -> Result<SyntaxTree, String> {
    let parse = parse(FileId::new(0), Arc::from(text));
    if parse.errors().is_empty() {
        Ok(parse.tree().clone())
    } else {
        Err(format!("{} grammar errors", parse.errors().len()))
    }
}

/// Checks one file at one width; returns a message for each failure.
fn check(text: &str, width: usize) -> Result<(), String> {
    let original = parse_clean(text).map_err(|e| format!("the source has {e}"))?;
    let once = format_source(text, width).map_err(|e| e.to_string())?;
    let twice = format_source(&once, width).map_err(|e| format!("second pass: {e}"))?;
    if once != twice {
        return Err(format!(
            "a second pass changes the output:\n{}",
            first_difference(&once, &twice)
        ));
    }
    let formatted = parse_clean(&once).map_err(|e| format!("the output has {e}"))?;
    let (before, after) = (meaning(&original), meaning(&formatted));
    if before.comments != after.comments {
        return Err("the comments changed".to_owned());
    }
    if before.imports != after.imports {
        return Err("the imports changed".to_owned());
    }
    for (b, a) in before.others.iter().zip(&after.others) {
        if b.1 != a.1 {
            return Err(format!("the tokens changed: {:?} became {:?}", b.1, a.1));
        }
        if b.0 != a.0 {
            return Err(format!("the node kinds changed in {:?}", b.1.first()));
        }
    }
    if before.others.len() != after.others.len() {
        return Err("the number of top-level nodes changed".to_owned());
    }
    if once.contains('\r') && !text.contains('\r') {
        return Err("the output has a carriage return".to_owned());
    }
    if !once.is_empty() && (!once.ends_with('\n') || once.ends_with("\n\n")) {
        return Err("the output does not end with exactly one line break".to_owned());
    }
    Ok(())
}

/// The first line where `a` and `b` differ, for a readable failure.
fn first_difference(a: &str, b: &str) -> String {
    for (n, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}:\n- {x}\n+ {y}", n.saturating_add(1));
        }
    }
    "the line counts differ".to_owned()
}

#[test]
fn fmt_corpus_formats_idempotently_and_keeps_meaning() {
    let root = repo_root();
    let files = corpus(&root);
    assert!(files.len() > 100, "only {} corpus files", files.len());
    let mut failures = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        for width in WIDTHS {
            if let Err(e) = check(&text, width) {
                failures.push(format!("{} at {width}: {e}", display_relative(path, &root)));
            }
        }
    }
    println!(
        "formatted {} corpus files at widths {WIDTHS:?}, {} failures",
        files.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The same checks over every UI test source without a grammar error:
/// they hold the odd layouts, comment positions, line endings and byte
/// order marks that the examples avoid.
#[test]
fn fmt_ui_sources_format_idempotently_and_keep_meaning() {
    let root = repo_root();
    let files = files_with_extension(&root.join("tests/ui"), "cheby").unwrap();
    let mut checked = 0;
    let mut failures = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if parse_clean(&text).is_err() {
            continue;
        }
        checked += 1;
        for width in WIDTHS {
            if let Err(e) = check(&text, width) {
                failures.push(format!("{} at {width}: {e}", display_relative(path, &root)));
            }
        }
    }
    println!("formatted {checked} UI sources at widths {WIDTHS:?}");
    assert!(checked > 100, "only {checked} UI sources");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The meaning check's trailing-comma exception never covers the comma of
/// a one-element tuple: dropping it turns `(x,)` into `(x)`.
#[test]
fn fmt_corpus_one_element_tuple_comma_is_compared() {
    let shapes = |text: &str| meaning(&parse_clean(text).unwrap()).others;
    for (tuple, paren) in [
        ("const a: Int = (x,)\n", "const a: Int = (x)\n"),
        (
            "fn f() {\n  let (x,) = y\n}\n",
            "fn f() {\n  let (x) = y\n}\n",
        ),
        ("type T = (Int,)\n", "type T = (Int)\n"),
    ] {
        assert_ne!(shapes(tuple), shapes(paren), "{tuple}");
    }
    // A longer tuple's trailing comma is the style's own.
    assert_eq!(
        shapes("const a: Int = (x, y,)\n"),
        shapes("const a: Int = (x, y)\n")
    );
}

/// Whether `token` is the `=>` of a `case` arm or a subjectless `case`'s
/// arm, with only spaces before it on its line: the place padding goes.
fn is_padded_place(token: SyntaxToken<'_>) -> bool {
    token.kind() == SyntaxKind::FAT_ARROW
        && token
            .parent()
            .is_some_and(|p| matches!(p.kind(), SyntaxKind::CASE_ARM | SyntaxKind::COND_ARM))
        && token
            .prev_token()
            .is_some_and(|p| p.kind() == SyntaxKind::WHITESPACE)
        && token
            .prev_token()
            .and_then(SyntaxToken::prev_token)
            .is_some_and(|p| p.kind() != SyntaxKind::NEWLINE)
}

/// D-1276: an arm has one space before `=>`, never padding.
/// Every corpus source with spaces added before each arm's `=>` (the
/// padding of the withdrawn alignment) formats exactly as the source does,
/// and in the output every arm's `=>` after its patterns has one space
/// before it.
#[test]
fn fmt_corpus_padded_arms_format_to_one_space() {
    let root = repo_root();
    let files = corpus(&root);
    let mut padded_arms = 0_usize;
    let mut failures = Vec::new();
    for path in &files {
        let name = display_relative(path, &root);
        let text = std::fs::read_to_string(path).unwrap();
        let tree = parse_clean(&text).unwrap();
        let mut padded = String::with_capacity(text.len());
        let mut last = 0;
        for token in tree.root().tokens().filter(|t| is_padded_place(*t)) {
            let start = usize::try_from(token.text_range().start()).unwrap();
            padded.push_str(text.get(last..start).unwrap());
            padded.push_str("      ");
            last = start;
            padded_arms = padded_arms.saturating_add(1);
        }
        padded.push_str(text.get(last..).unwrap());
        for width in WIDTHS {
            let want = format_source(&text, width).unwrap();
            match format_source(&padded, width) {
                Ok(got) if got == want => {}
                Ok(got) => failures.push(format!(
                    "{name} at {width}: the padded source formats differently:\n{}",
                    first_difference(&want, &got)
                )),
                Err(e) => failures.push(format!("{name} at {width}: {e}")),
            }
            let out = parse_clean(&want).unwrap();
            for token in out.root().tokens().filter(|t| is_padded_place(*t)) {
                if token.prev_token().is_some_and(|p| p.text() != " ") {
                    let start = usize::try_from(token.text_range().start()).unwrap();
                    failures.push(format!(
                        "{name} at {width}: padding before `=>` at byte {start}"
                    ));
                }
            }
        }
    }
    println!("padded {padded_arms} arms in {} corpus files", files.len());
    assert!(padded_arms > 100, "only {padded_arms} arms padded");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Prints, as a Markdown table, how many nodes of each kind the printer
/// laid out and how many it printed verbatim over the corpus at the
/// formatter's width. Run with `--nocapture` to read it.
#[test]
fn fmt_corpus_coverage() {
    let root = repo_root();
    let files = corpus(&root);
    let mut total = Coverage::default();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        total.add(&coverage(&text, cheby_fmt::WIDTH).unwrap());
    }
    let mut kinds: Vec<SyntaxKind> = total
        .laid_out
        .keys()
        .chain(total.verbatim.keys())
        .copied()
        .collect();
    kinds.sort();
    kinds.dedup();
    let width = kinds
        .iter()
        .map(|k| k.name().len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{} files; items printed verbatim: {}",
        files.len(),
        total.verbatim_items
    );
    println!();
    println!("| {:width$} | Laid out | Verbatim |", "Kind");
    println!("| {:-<width$} | -------: | -------: |", "");
    for kind in kinds {
        let laid = total.laid_out.get(&kind).copied().unwrap_or(0);
        let verb = total.verbatim.get(&kind).copied().unwrap_or(0);
        println!("| {:width$} | {laid:>8} | {verb:>8} |", kind.name());
    }
    assert!(total.laid_out.contains_key(&SyntaxKind::FN_DECL));

    // The UI sources hold the odd comment positions and the errors the
    // formatter keeps as written: the nodes still printed verbatim there.
    let mut ui = Coverage::default();
    let mut checked = 0;
    for path in files_with_extension(&root.join("tests/ui"), "cheby").unwrap() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(c) = coverage(&text, cheby_fmt::WIDTH) {
            checked += 1;
            if !c.verbatim.is_empty() || c.verbatim_items > 0 {
                println!(
                    "verbatim in {}: {} item(s), {:?}",
                    display_relative(&path, &root),
                    c.verbatim_items,
                    c.verbatim
                        .iter()
                        .map(|(k, n)| format!("{} {n}", k.name()))
                        .collect::<Vec<_>>()
                );
            }
            ui.add(&c);
        }
    }
    println!(
        "{checked} UI sources; items printed verbatim: {}; nodes printed verbatim: {}",
        ui.verbatim_items,
        ui.verbatim.values().sum::<u64>()
    );
}

/// What a source parses to, trivia aside: its significant tokens, its node
/// kinds in preorder, and how many diagnostics validation reports.
#[derive(PartialEq, Eq)]
struct Parsed {
    tokens: Vec<String>,
    kinds: Vec<SyntaxKind>,
    diagnostics: usize,
}

fn parsed(text: &str) -> Option<Parsed> {
    let tree = parse_clean(text).ok()?;
    Some(Parsed {
        tokens: tree
            .root()
            .tokens()
            .filter(|t| is_significant(t.kind()))
            .map(|t| t.text().to_owned())
            .collect(),
        kinds: tree.root().descendants().map(SyntaxNode::kind).collect(),
        diagnostics: cheby_syntax::validate(&tree).len(),
    })
}

/// A place where the corpus test injects a comment: at the end of line
/// `line`, or on a line of its own before it.
#[derive(Clone, Copy)]
enum Point {
    End(usize),
    Before(usize),
}

/// `text` with a comment at each of `points`: `// eN` at the end of line
/// `N`, and `// bN` on a line of its own before it.
fn inject(text: &str, points: &[Point]) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut ends = vec![false; lines.len()];
    let mut befores = vec![false; lines.len()];
    for point in points {
        let slot = match *point {
            Point::End(n) => ends.get_mut(n),
            Point::Before(n) => befores.get_mut(n),
        };
        if let Some(slot) = slot {
            *slot = true;
        }
    }
    let mut out = String::with_capacity(text.len().saturating_mul(2));
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if befores.get(n).copied().unwrap_or(false) {
            out.push_str("// b");
            out.push_str(&n.to_string());
            out.push('\n');
        }
        if ends.get(n).copied().unwrap_or(false) {
            let (body, cr) = line
                .strip_suffix('\r')
                .map_or((*line, ""), |body| (body, "\r"));
            out.push_str(body);
            out.push_str(" // e");
            out.push_str(&n.to_string());
            out.push_str(cr);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// A source with comments injected everywhere they keep its parse.
struct Injected {
    text: String,
    /// Points: two per line.
    points: usize,
    /// Points left after those skipped up front, and tried.
    tried: usize,
    /// Points where a comment was injected.
    kept: usize,
}

/// Injects a comment at the end of every line and on a line of its own
/// before every line of `text`, at each point where the result parses to
/// the same tokens and node kinds, trivia aside, with as many validation
/// diagnostics. Points inside a string literal, and the end of a line that
/// already ends with a comment (which would only lengthen it), are skipped
/// up front; the others are tried together, and a group that changes the
/// parse is split until each point that does is skipped.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: the corpus sources parse"
)]
fn inject_everywhere(text: &str) -> Injected {
    let tree = parse_clean(text).unwrap();
    let original = parsed(text).unwrap();
    let breaks: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
    // The last segment is a line only if it holds something.
    let count = if text.is_empty() || text.ends_with('\n') {
        breaks.len()
    } else {
        breaks.len().saturating_add(1)
    };
    // The line that holds byte `offset`.
    let line_of = |offset: usize| breaks.partition_point(|&b| b < offset);
    // Lines that end inside a string, with a comment, and with a doc
    // comment, and lines that hold a token.
    let mut ends_in_string = vec![false; count.saturating_add(1)];
    let mut ends_in_comment = vec![false; count.saturating_add(1)];
    let mut ends_in_doc = vec![false; count.saturating_add(1)];
    let mut has_token = vec![false; count.saturating_add(1)];
    for token in tree.root().tokens() {
        if token.kind() == SyntaxKind::NEWLINE {
            continue;
        }
        let start = usize::try_from(token.text_range().start()).unwrap();
        for (i, c) in token.text().char_indices() {
            if c == '\n'
                && let Some(slot) = ends_in_string.get_mut(line_of(start.saturating_add(i)))
            {
                *slot = true;
            }
        }
        let line = line_of(start);
        if token.kind() != SyntaxKind::WHITESPACE
            && let Some(slot) = has_token.get_mut(line)
        {
            *slot = true;
        }
        if matches!(
            token.kind(),
            SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT | SyntaxKind::MODULE_DOC
        ) && let Some(slot) = ends_in_comment.get_mut(line)
        {
            *slot = true;
        }
        if token.kind() == SyntaxKind::DOC_COMMENT
            && let Some(slot) = ends_in_doc.get_mut(line)
        {
            *slot = true;
        }
    }
    let flag = |flags: &[bool], n: usize| flags.get(n).copied().unwrap_or(false);
    let mut candidates = Vec::new();
    // The last line before this one that holds a token.
    let mut last_full: Option<usize> = None;
    for n in 0..count {
        let starts_in_string = n.checked_sub(1).is_some_and(|p| flag(&ends_in_string, p));
        // A comment line after a `///` comment would part it from what it
        // documents (§2.2), and the corpus check would only skip it.
        let after_doc = last_full.is_some_and(|p| flag(&ends_in_doc, p));
        if !starts_in_string && !after_doc {
            candidates.push(Point::Before(n));
        }
        if flag(&has_token, n) {
            last_full = Some(n);
        }
        if !flag(&ends_in_string, n) && !flag(&ends_in_comment, n) {
            candidates.push(Point::End(n));
        }
    }
    let mut kept = Vec::new();
    keep_points(text, &original, &mut kept, &candidates);
    Injected {
        text: inject(text, &kept),
        points: count.saturating_mul(2),
        tried: candidates.len(),
        kept: kept.len(),
    }
}

/// Adds to `kept` the points of `group` whose comments keep `original`'s
/// parse, trying the whole group first and halving it on failure.
fn keep_points(text: &str, original: &Parsed, kept: &mut Vec<Point>, group: &[Point]) {
    if group.is_empty() {
        return;
    }
    let mut trial = kept.clone();
    trial.extend_from_slice(group);
    if parsed(&inject(text, &trial)).as_ref() == Some(original) {
        *kept = trial;
        return;
    }
    if group.len() == 1 {
        return;
    }
    let (left, right) = group.split_at(group.len().div_ceil(2));
    keep_points(text, original, kept, left);
    keep_points(text, original, kept, right);
}

/// The node counts the printer may print verbatim in `text`, by rule: a
/// gap between a unary `-` and a number (D-470, D-1158)
/// and an attribute with no item after it (a top-level `ERROR` node), with
/// everything inside them; and the number of such top-level nodes.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: the corpus sources parse"
)]
fn verbatim_by_rule(text: &str) -> (BTreeMap<SyntaxKind, u64>, u64) {
    let tree = parse_clean(text).unwrap();
    let mut counts: BTreeMap<SyntaxKind, u64> = BTreeMap::new();
    let mut items = 0u64;
    let mut add = |node: SyntaxNode<'_>| {
        for d in node.descendants() {
            let slot = counts.entry(d.kind()).or_insert(0);
            *slot = slot.saturating_add(1);
        }
    };
    for node in tree.root().children() {
        if node.kind() == SyntaxKind::ERROR {
            items = items.saturating_add(1);
            add(node);
        }
    }
    for node in tree.root().descendants() {
        if !matches!(
            node.kind(),
            SyntaxKind::PREFIX_EXPR | SyntaxKind::LITERAL_PAT
        ) || node.ancestors().any(|a| a.kind() == SyntaxKind::ERROR)
        {
            continue;
        }
        let mut parts = node.children_with_tokens();
        let minus = parts.next().is_some_and(|c| c.kind() == SyntaxKind::MINUS);
        let mut gap = false;
        let mut number = false;
        for part in parts {
            match part.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE => gap = true,
                kind if kind.is_trivia() => gap = true,
                SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => number = true,
                SyntaxKind::LITERAL => {
                    number = part
                        .as_node()
                        .and_then(SyntaxNode::first_token)
                        .is_some_and(|t| {
                            matches!(t.kind(), SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER)
                        });
                }
                _ => {}
            }
        }
        if minus && gap && number {
            add(node);
        }
    }
    (counts, items)
}

/// Checks that the printer lays out every node of `text` but those
/// [`verbatim_by_rule`] allows.
fn check_no_fallback(text: &str, total: &mut Coverage) -> Result<(), String> {
    let c = coverage(text, cheby_fmt::WIDTH).map_err(|e| e.to_string())?;
    total.add(&c);
    let (allowed, items) = verbatim_by_rule(text);
    if c.verbatim != allowed || c.verbatim_items != items {
        return Err(format!(
            "verbatim nodes {:?} and {} item(s), where the rules allow {:?} and {items}",
            c.verbatim, c.verbatim_items, allowed
        ));
    }
    Ok(())
}

/// The corpus with comments everywhere (D-1031, D-1160):
/// every example, std and UI source, with a comment at the end of every
/// line and a comment line before every line, wherever that keeps the
/// parse, formats at 80, 100 and 120 columns with no error, a second pass
/// changing nothing, the tokens, node kinds and comment texts kept, and
/// no node printed verbatim but by rule. The plain sources are held to the
/// last rule too. Copies in memory only: nothing is written.
#[test]
fn fmt_corpus_comments_everywhere() {
    let root = repo_root();
    let mut files = corpus(&root);
    for path in files_with_extension(&root.join("tests/ui"), "cheby").unwrap() {
        if std::fs::read_to_string(&path).is_ok_and(|t| parse_clean(&t).is_ok()) {
            files.push(path);
        }
    }
    let (mut points, mut tried, mut injected) = (0usize, 0usize, 0usize);
    let mut injecting = std::time::Duration::ZERO;
    let mut failures = Vec::new();
    let (mut plain, mut commented_total) = (Coverage::default(), Coverage::default());
    let mut holders: BTreeMap<SyntaxKind, u64> = BTreeMap::new();
    for path in &files {
        let name = display_relative(path, &root);
        let text = std::fs::read_to_string(path).unwrap();
        if let Err(e) = check_no_fallback(&text, &mut plain) {
            failures.push(format!("{name}: {e}"));
        }
        let started = std::time::Instant::now();
        let commented = inject_everywhere(&text);
        injecting = injecting.saturating_add(started.elapsed());
        points = points.saturating_add(commented.points);
        injected = injected.saturating_add(commented.kept);
        tried = tried.saturating_add(commented.tried);
        for width in WIDTHS {
            if let Err(e) = check(&commented.text, width) {
                failures.push(format!("{name} with comments at {width}: {e}"));
            }
        }
        if let Err(e) = check_no_fallback(&commented.text, &mut commented_total) {
            failures.push(format!("{name} with comments: {e}"));
        }
        // Where the comments landed: the kind of the node that holds each.
        if let Ok(tree) = parse_clean(&commented.text) {
            for token in tree.root().tokens() {
                if token.kind() == SyntaxKind::COMMENT
                    && let Some(parent) = token.parent()
                {
                    let slot = holders.entry(parent.kind()).or_insert(0u64);
                    *slot = slot.saturating_add(1);
                }
            }
        }
    }
    let mut holders: Vec<_> = holders.into_iter().collect();
    holders.sort_by_key(|&(kind, n)| (std::cmp::Reverse(n), kind));
    println!(
        "comments by the kind of the node that holds them: {:?}",
        holders
            .iter()
            .map(|(k, n)| format!("{} {n}", k.name()))
            .collect::<Vec<_>>()
    );
    for (what, c) in [("plain", &plain), ("with comments", &commented_total)] {
        println!(
            "verbatim, {what}: {} item(s), {} node(s) {:?}; laid out: {} node(s)",
            c.verbatim_items,
            c.verbatim.values().sum::<u64>(),
            c.verbatim
                .iter()
                .map(|(k, n)| format!("{} {n}", k.name()))
                .collect::<Vec<_>>(),
            c.laid_out.values().sum::<u64>()
        );
    }
    println!(
        "comments everywhere: {} files, {points} points, {injected} comments injected, {} skipped ({} up front: in a string, after a comment, after a doc comment; {} that changed the parse), widths {WIDTHS:?}, {} failures (injecting took {:.1} s)",
        files.len(),
        points.saturating_sub(injected),
        points.saturating_sub(tried),
        tried.saturating_sub(injected),
        failures.len(),
        injecting.as_secs_f64()
    );
    assert!(files.len() > 900, "only {} files", files.len());
    assert!(
        injected > points / 2,
        "only {injected} of {points} points injected"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// How many lines of `before` are not kept unchanged in `after`: the lines
/// outside a longest common subsequence of the two.
fn changed_lines(before: &str, after: &str) -> usize {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // `row[j]`: the longest common subsequence of the lines of `a` read so
    // far and the first `j` lines of `b`.
    let mut row = vec![0usize; b.len().saturating_add(1)];
    for x in &a {
        let above = row.clone();
        for (j, y) in b.iter().enumerate() {
            let next = j.saturating_add(1);
            let value = if x == y {
                above.get(j).copied().unwrap_or(0).saturating_add(1)
            } else {
                let up = above.get(next).copied().unwrap_or(0);
                up.max(row.get(j).copied().unwrap_or(0))
            };
            if let Some(slot) = row.get_mut(next) {
                *slot = value;
            }
        }
    }
    a.len().saturating_sub(row.last().copied().unwrap_or(0))
}

/// Prints, as a Markdown table, how many corpus files and source lines
/// change when the corpus is formatted at 80, 100 and 120 columns: the
/// informative measure behind the width's choice at CP2 (D-1030), where the
/// user chose 100 (D-1252). Run with `--ignored --nocapture`.
#[test]
#[ignore = "a measurement, printed on demand"]
fn fmt_corpus_change_table() {
    let root = repo_root();
    let files = corpus(&root);
    // Printed at once, so a test running beside it cannot cut the table.
    let mut report = vec![
        format!(
            "| Width | Files that change (of {}) | Source lines that change |",
            files.len()
        ),
        "| ----: | ------------------------: | -----------------------: |".to_owned(),
    ];
    for width in WIDTHS {
        let mut changed_files = 0;
        let mut lines = 0;
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap();
            let out = format_source(&text, width).unwrap();
            if out != text {
                changed_files += 1;
                lines += changed_lines(&text, &out);
            }
        }
        report.push(format!(
            "| {width:>5} | {changed_files:>25} | {lines:>24} |"
        ));
    }
    println!("{}\n", report.join("\n"));
}

// The width measurement for CP2 (D-1030, D-1150): what T1b's reformat
// (D-1029) changes at each width, split into the two parts it reformats,
// the lines still over the width and why, and the goldens' locations it
// would move. The tests below print Markdown tables (every table line
// starts with `|`) and choose no width: CP2 chose 100 from them (D-1252),
// and M3's wave 9 reformatted the corpus at 100, so at 100 they now show
// no file that changes.

/// The two parts of the corpus that T1b reformats (D-1029): the examples'
/// sources and `std/`, each sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable corpus fails the test"
)]
fn corpus_parts(root: &Path) -> [(&'static str, Vec<PathBuf>); 2] {
    let mut examples = Vec::new();
    for ex in example::all(root).unwrap() {
        examples.extend(ex.sources().unwrap());
    }
    examples.sort();
    let mut std = files_with_extension(&root.join("std"), "cheby").unwrap();
    std.sort();
    [("examples", examples), ("std", std)]
}

/// The width of `text` in columns: code points, as the printer counts them.
fn columns(text: &str) -> usize {
    text.chars().count()
}

#[test]
#[ignore = "a measurement, printed on demand"]
fn fmt_width_change_table() {
    let root = repo_root();
    let parts = corpus_parts(&root);
    let mut texts = Vec::new();
    for (part, files) in &parts {
        for path in files {
            texts.push((*part, std::fs::read_to_string(path).unwrap()));
        }
    }
    let totals = |part: &str| {
        let files = texts.iter().filter(|(p, _)| *p == part).count();
        let lines: usize = texts
            .iter()
            .filter(|(p, _)| *p == part)
            .map(|(_, t)| t.lines().count())
            .sum();
        (files, lines)
    };
    let (ex_files, ex_lines) = totals("examples");
    let (std_files, std_lines) = totals("std");
    // Printed at once, so a test running beside it cannot cut the table.
    let mut report = vec![
        format!(
            "| Width | Example files that change (of {ex_files}) | Example lines that change (of {ex_lines}) | Std files that change (of {std_files}) | Std lines that change (of {std_lines}) | Files that change (of {}) | Lines that change (of {}) |",
            ex_files + std_files,
            ex_lines + std_lines
        ),
        "| ----: | ---: | ---: | ---: | ---: | ---: | ---: |".to_owned(),
    ];
    for width in WIDTHS {
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (part, text) in &texts {
            let out = format_source(text, width).unwrap();
            let slot = counts.entry(part).or_insert((0, 0));
            if out != *text {
                slot.0 += 1;
                slot.1 += changed_lines(text, &out);
            }
        }
        let ex = counts.get("examples").copied().unwrap_or((0, 0));
        let st = counts.get("std").copied().unwrap_or((0, 0));
        report.push(format!(
            "| {width} | {} | {} | {} | {} | {} | {} |",
            ex.0,
            ex.1,
            st.0,
            st.1,
            ex.0 + st.0,
            ex.1 + st.1
        ));
    }
    println!("{}\n", report.join("\n"));
}

/// Why a formatted line is still over the width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Long {
    /// The line fits without its comment: comments are kept as written.
    Comment,
    /// The line fits with its string literals emptied, and one of them is
    /// longer than the room the line's indentation leaves: a literal is
    /// never split (D-1030), and on a line of its own it would not fit
    /// either.
    LiteralNoRoom,
    /// The line fits with its string literals emptied, and each literal
    /// would fit on a line of its own at the line's indentation: the
    /// literal shares its line with more (a hugged argument, or a head
    /// with no break the printer may take, such as `test "…" {`).
    LiteralWithRoom,
    /// None of the above.
    Other,
}

impl Long {
    const ALL: [Self; 4] = [
        Self::Comment,
        Self::LiteralNoRoom,
        Self::LiteralWithRoom,
        Self::Other,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Comment => "comment kept as written",
            Self::LiteralNoRoom => "string literal longer than the room",
            Self::LiteralWithRoom => "string literal shorter than the room, with more on its line",
            Self::Other => "other",
        }
    }
}

/// The lines of formatted `text` that are over `width` columns, as
/// (line from 1, columns, reason). `text` parses without a grammar error.
fn long_lines(text: &str, width: usize) -> Result<Vec<(usize, usize, Long)>, String> {
    let tree = parse_clean(text)?;
    let offset = |o: u32| usize::try_from(o).map_err(|e| e.to_string());
    // The string literals, outermost only, and the comments, as byte ranges.
    let mut literals = Vec::new();
    for node in tree.root().descendants() {
        if node.kind() == SyntaxKind::STRING_LIT
            && !node
                .ancestors()
                .skip(1)
                .any(|a| a.kind() == SyntaxKind::STRING_LIT)
        {
            let r = node.text_range();
            literals.push((offset(r.start())?, offset(r.end())?));
        }
    }
    let mut comments = Vec::new();
    for token in tree.root().tokens() {
        let r = token.text_range();
        let range = (offset(r.start())?, offset(r.end())?);
        match token.kind() {
            SyntaxKind::RAW_STRING
                if !token
                    .parent()
                    .is_some_and(|p| p.ancestors().any(|a| a.kind() == SyntaxKind::STRING_LIT)) =>
            {
                literals.push(range);
            }
            SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT | SyntaxKind::MODULE_DOC => {
                comments.push(range);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for (n, line) in text.split('\n').enumerate() {
        let end = start.saturating_add(line.len());
        let cols = columns(line);
        if cols > width {
            // The columns of `range`'s part on this line.
            let cols_in = |(a, b): (usize, usize)| {
                let (a, b) = (a.max(start), b.min(end));
                if a < b {
                    text.get(a..b).map_or(0, columns)
                } else {
                    0
                }
            };
            // A comment runs to the end of the line; drop the spaces
            // before it too.
            let comment = comments
                .iter()
                .find(|&&(a, _)| a >= start && a < end)
                .map_or(0, |&(a, _)| {
                    let head = text.get(start..a).unwrap_or("");
                    cols.saturating_sub(columns(head.trim_end()))
                });
            let pieces: Vec<usize> = literals
                .iter()
                .map(|&r| cols_in(r))
                .filter(|&c| c > 0)
                .collect();
            let indent = line.len().saturating_sub(line.trim_start().len());
            let without_comment = cols.saturating_sub(comment);
            let emptied = without_comment
                .saturating_sub(pieces.iter().sum::<usize>())
                .saturating_add(pieces.len().saturating_mul(2));
            let room = width.saturating_sub(indent);
            let reason = if comment > 0 && without_comment <= width {
                Long::Comment
            } else if !pieces.is_empty() && emptied <= width {
                if pieces.iter().any(|&p| p > room) {
                    Long::LiteralNoRoom
                } else {
                    Long::LiteralWithRoom
                }
            } else {
                Long::Other
            };
            out.push((n.saturating_add(1), cols, reason));
        }
        start = end.saturating_add(1);
    }
    Ok(out)
}

/// `text` as the content of a Markdown table cell: in a code span, with
/// `|` escaped.
fn cell_code(text: &str) -> String {
    format!("`{}`", text.trim().replace('|', "\\|"))
}

#[test]
#[ignore = "a measurement, printed on demand"]
fn fmt_width_long_lines() {
    use std::fmt::Write as _;
    let root = repo_root();
    let parts = corpus_parts(&root);
    let mut listed = Vec::new();
    // Printed at once, so a test running beside it cannot cut a table.
    let mut report = String::new();
    writeln!(
        report,
        "| Width | Part | Lines over the width before formatting | Lines over the width after | {} |",
        Long::ALL.map(Long::name).join(" | ")
    )
    .unwrap();
    writeln!(
        report,
        "| ----: | ---- | ---: | ---: | ---: | ---: | ---: | ---: |"
    )
    .unwrap();
    for width in WIDTHS {
        for (part, files) in &parts {
            let mut before = 0usize;
            let mut by_reason: BTreeMap<Long, usize> = BTreeMap::new();
            for path in files {
                let text = std::fs::read_to_string(path).unwrap();
                before += text.lines().filter(|l| columns(l) > width).count();
                let out = format_source(&text, width).unwrap();
                let lines: Vec<&str> = out.split('\n').collect();
                for (line, cols, reason) in long_lines(&out, width).unwrap() {
                    *by_reason.entry(reason).or_insert(0) += 1;
                    if matches!(reason, Long::LiteralWithRoom | Long::Other) {
                        let text = cell_code(lines[line - 1]);
                        let path = display_relative(path, &root);
                        listed.push((width, path, line, cols, reason, text));
                    }
                }
            }
            let after: usize = by_reason.values().sum();
            let cells = Long::ALL.map(|r| by_reason.get(&r).copied().unwrap_or(0).to_string());
            writeln!(
                report,
                "| {width} | {part} | {before} | {after} | {} |",
                cells.join(" | ")
            )
            .unwrap();
        }
    }
    writeln!(report).unwrap();
    writeln!(
        report,
        "Formatted lines over the width that the layout, not a literal's or a comment's own length, keeps long ({}):",
        listed.len()
    )
    .unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Width | Formatted line | Columns | Reason | Text |"
    )
    .unwrap();
    writeln!(
        report,
        "| ----: | -------------- | ------: | ------ | ---- |"
    )
    .unwrap();
    for (width, path, line, cols, reason, text) in listed {
        writeln!(
            report,
            "| {width} | {path}:{line} | {cols} | {} | {text} |",
            reason.name()
        )
        .unwrap();
    }
    println!("{report}");
}

/// The significant tokens of `text` paired with those of its formatted
/// form `out`, as (start in `text`, length, start in `out`), sorted by the
/// start in `text`. Formatting keeps the significant tokens (D-1153), so
/// the pairing reads the formatter's output: the top-level nodes in order,
/// the imports matched by their tokens (D-1030 sorts them), and inside a
/// node the tokens in order, a trailing comma the printer adds or removes
/// (D-1152) left unpaired.
fn token_pairs(text: &str, out: &str) -> Result<Vec<(usize, usize, usize)>, String> {
    let (before, after) = (parse_clean(text)?, parse_clean(out)?);
    let ((old_imports, old_others), (new_imports, new_others)) =
        (top_level(&before), top_level(&after));
    if old_others.len() != new_others.len() || old_imports.len() != new_imports.len() {
        return Err("the top-level nodes differ".to_owned());
    }
    let mut new_imports: Vec<Option<SyntaxNode<'_>>> = new_imports.into_iter().map(Some).collect();
    let mut nodes = Vec::new();
    for node in old_imports {
        let wanted = import_key(node);
        let found = new_imports
            .iter_mut()
            .find(|slot| slot.is_some_and(|n| import_key(n) == wanted))
            .and_then(Option::take)
            .ok_or("an import has no match")?;
        nodes.push((node, found));
    }
    nodes.extend(old_others.into_iter().zip(new_others));
    let start = |t: SyntaxToken<'_>| usize::try_from(t.text_range().start()).unwrap_or(usize::MAX);
    let mut pairs = Vec::new();
    for (old, new) in nodes {
        let olds: Vec<_> = old.tokens().filter(|t| is_significant(t.kind())).collect();
        let news: Vec<_> = new.tokens().filter(|t| is_significant(t.kind())).collect();
        let (mut at_old, mut at_new) = (0usize, 0usize);
        loop {
            match (olds.get(at_old).copied(), news.get(at_new).copied()) {
                (None, None) => break,
                (Some(from), Some(to)) if from.text() == to.text() => {
                    pairs.push((start(from), from.text().len(), start(to)));
                    at_old = at_old.saturating_add(1);
                    at_new = at_new.saturating_add(1);
                }
                (Some(from), _) if from.kind() == SyntaxKind::COMMA => {
                    at_old = at_old.saturating_add(1);
                }
                (_, Some(to)) if to.kind() == SyntaxKind::COMMA => {
                    at_new = at_new.saturating_add(1);
                }
                (from, to) => {
                    return Err(format!(
                        "the tokens differ: {:?} and {:?}",
                        from.map(|t| t.text().to_owned()),
                        to.map(|t| t.text().to_owned())
                    ));
                }
            }
        }
    }
    pairs.sort_unstable();
    Ok(pairs)
}

/// A file's top-level nodes: the imports, and the others.
fn top_level(tree: &SyntaxTree) -> (Vec<SyntaxNode<'_>>, Vec<SyntaxNode<'_>>) {
    tree.root()
        .children()
        .partition(|n| n.kind() == SyntaxKind::IMPORT_DECL)
}

/// An import's significant tokens but its commas: what sorting keeps.
fn import_key(node: SyntaxNode<'_>) -> Vec<String> {
    node.tokens()
        .filter(|t| is_significant(t.kind()) && t.kind() != SyntaxKind::COMMA)
        .map(|t| t.text().to_owned())
        .collect()
}

/// The byte offset of `line:column` (from 1, columns in code points,
/// D-421) in `text`; the end of a line is a position too.
fn offset_of(text: &str, line: usize, column: usize) -> Option<usize> {
    let start = match line.checked_sub(2) {
        None if line == 1 => 0,
        None => return None,
        Some(n) => text.match_indices('\n').nth(n)?.0.checked_add(1)?,
    };
    let body = text.get(start..)?.split('\n').next()?;
    let col = column.checked_sub(1)?;
    if col == columns(body) {
        return start.checked_add(body.len());
    }
    body.char_indices()
        .nth(col)
        .and_then(|(i, _)| start.checked_add(i))
}

/// The `line:column` (from 1, D-421) of byte `offset` in `text`.
fn position_of(text: &str, offset: usize) -> Option<(usize, usize)> {
    let head = text.get(..offset)?;
    let line = head.matches('\n').count().checked_add(1)?;
    let column = columns(head.rsplit('\n').next()?).checked_add(1)?;
    Some((line, column))
}

/// Where `line:column` of `text` lands in the formatted `out`, given
/// [`token_pairs`]: the start of the same token. A location the compiler
/// writes is the start of a span, so the start of a significant token; any
/// other location is an error, with the reason (the golden then describes
/// another text than the source's, such as an older one).
fn map_position(
    text: &str,
    out: &str,
    pairs: &[(usize, usize, usize)],
    (line, column): (usize, usize),
) -> Result<(usize, usize), &'static str> {
    let offset = offset_of(text, line, column).ok_or("not in the source")?;
    let &(start, len, to) = pairs
        .partition_point(|&(s, _, _)| s <= offset)
        .checked_sub(1)
        .and_then(|i| pairs.get(i))
        .ok_or("not in a token")?;
    match offset.checked_sub(start) {
        Some(0) => position_of(out, to).ok_or("not in the output"),
        Some(inside) if inside < len => Err("inside a token"),
        _ => Err("not in a token"),
    }
}

/// The golden-location mapping follows the tokens through the style's own
/// changes: sorted imports and a trailing comma dropped from a list that
/// fits (D-1030, D-1152).
#[test]
fn fmt_width_location_mapping_follows_tokens() {
    let text = "import std::list\nimport std::io\n\nfn f(a: Int, b: Int,) -> Int {\n  a\n}\n";
    let out = format_source(text, 100).unwrap();
    assert_eq!(
        out,
        "import std::io\nimport std::list\n\nfn f(a: Int, b: Int) -> Int {\n  a\n}\n"
    );
    let pairs = token_pairs(text, &out).unwrap();
    let map = |line, column| map_position(text, &out, &pairs, (line, column));
    // `list` moves down a line with its import; `io` moves up.
    assert_eq!(map(1, 13), Ok((2, 13)));
    assert_eq!(map(2, 13), Ok((1, 13)));
    // The dropped comma moves `->` one column left; the body stays.
    assert_eq!(map(4, 23), Ok((4, 22)));
    assert_eq!(map(5, 3), Ok((5, 3)));
    // A location that is not a token's start is not mapped.
    assert_eq!(map(4, 3), Err("not in a token"));
    assert_eq!(map(1, 14), Err("inside a token"));
    assert_eq!(map(9, 1), Err("not in the source"));
    assert_eq!(
        locations_in(
            "  str#0 = \"std/io.cheby:7:8\"\n --> src/main.cheby:3:14\n<repl:1>:1:1 a.cheby:2\n"
        ),
        vec![
            (1, "std/io.cheby".to_owned(), 7, 8),
            (2, "src/main.cheby".to_owned(), 3, 14),
        ]
    );
}

/// The text of line `line` (from 1) of `text`.
fn line_text(text: &str, line: usize) -> Option<&str> {
    text.split('\n').nth(line.checked_sub(1)?)
}

/// Every `path.cheby:line:column` in `text`, as (line of `text` from 1,
/// path, line, column).
fn locations_in(text: &str) -> Vec<(usize, String, usize, usize)> {
    let is_path = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
    let digits = |s: &str| {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        s.get(..n)
            .and_then(|d| d.parse::<usize>().ok())
            .map(|v| (v, n))
    };
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices(".cheby:") {
            let Some(head) = line.get(..at) else {
                continue;
            };
            let path_start = head
                .char_indices()
                .rev()
                .take_while(|&(_, c)| is_path(c))
                .last()
                .map_or(at, |(i, _)| i);
            let Some(path) = line.get(path_start..at.saturating_add(".cheby".len())) else {
                continue;
            };
            let Some(rest) = line.get(at.saturating_add(".cheby:".len())..) else {
                continue;
            };
            let Some((l, len)) = digits(rest) else {
                continue;
            };
            let Some(rest) = rest.get(len..).and_then(|r| r.strip_prefix(':')) else {
                continue;
            };
            let Some((c, _)) = digits(rest) else {
                continue;
            };
            out.push((n.saturating_add(1), path.to_owned(), l, c));
        }
    }
    out
}

/// Every file under `dir`, recursively, skipping dot files.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() {
            files_under(&entry.path(), out)?;
        } else {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// The goldens that can hold a location: each example's `expected.*`,
/// every file of `tests/ui/` and `tests/repl/` but sources, manifests and
/// READMEs, and every file of `crates/*/tests/golden/` but READMEs; sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable tree fails the test"
)]
fn golden_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for ex in example::all(root).unwrap() {
        for entry in std::fs::read_dir(&ex.dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("expected."))
            {
                files.push(path);
            }
        }
    }
    let mut more = Vec::new();
    files_under(&root.join("tests/ui"), &mut more).unwrap();
    files_under(&root.join("tests/repl"), &mut more).unwrap();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let golden = entry.unwrap().path().join("tests/golden");
        if golden.is_dir() {
            files_under(&golden, &mut more).unwrap();
        }
    }
    files.extend(more.into_iter().filter(|p| {
        let source = p.extension().is_some_and(|e| e == "cheby" || e == "toml");
        let readme = p.file_name().is_some_and(|n| n == "README.md");
        !source && !readme
    }));
    files.sort();
    files
}

/// The goldens that `cheby_mir`'s `golden_tests::build` writes: their
/// `std/io.cheby` is that test's inline stub `STD_IO`, not `std/io.cheby`,
/// so T1b's reformat does not move their locations in it.
const STUB_STD_IO_GOLDENS: [&str; 4] = [
    "crates/cheby_mir/tests/golden/001_hello_world.mir",
    "crates/cheby_mir/tests/golden/layouts_static_constructors_and_boxing.mir",
    "crates/cheby_mir/tests/golden/lets_dups_and_tail_calls.mir",
    "crates/cheby_mir/tests/golden/ui_hello_world.mir",
];

/// The source a golden's `path` names, if T1b reformats it (D-1029):
/// `std/…` always, and `src/…` when the golden belongs to an example, as
/// an example's own `expected.*` or a golden named after the example
/// (`crates/cheby_mir/tests/golden/examples/003_fizzbuzz.mir`).
fn reformatted_source(
    root: &Path,
    golden: &Path,
    examples: &[(String, PathBuf)],
    path: &str,
) -> Option<PathBuf> {
    if path.starts_with("std/") {
        return Some(root.join(path));
    }
    if !path.starts_with("src/") {
        return None;
    }
    let names: Vec<String> = golden
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| {
            Path::new(c.as_os_str())
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
        })
        .collect();
    examples
        .iter()
        .find(|(name, _)| names.iter().any(|n| n == name))
        .map(|(_, dir)| dir.join(path))
}

/// What a golden's location does when T1b reformats its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Moved {
    /// Its line number changes.
    Line,
    /// Its line stays, its column changes.
    Column,
    /// Its position stays, but the text of its line changes (a golden that
    /// shows the line changes).
    Text,
    /// Nothing changes.
    Kept,
    /// The location is not inside a token, or its source is missing.
    Unmapped,
}

/// A source's text and its formatted forms at [`WIDTHS`], each with its
/// [`token_pairs`].
type Formatted = (String, Vec<(String, Vec<(usize, usize, usize)>)>);

/// A source formatted at every width, or `None` if it cannot be read.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: the corpus sources format"
)]
fn formatted(source: &Path) -> Option<Formatted> {
    let text = std::fs::read_to_string(source).ok()?;
    let outs = WIDTHS
        .iter()
        .map(|&w| {
            let out = format_source(&text, w).unwrap();
            let pairs = token_pairs(&text, &out).unwrap();
            (out, pairs)
        })
        .collect();
    Some((text, outs))
}

/// What one location does at each width, as the kind of move and a cell.
fn location_cells(source: Option<&Formatted>, line: usize, column: usize) -> Vec<(Moved, String)> {
    let Some((text, outs)) = source else {
        return WIDTHS
            .iter()
            .map(|_| (Moved::Unmapped, "missing".to_owned()))
            .collect();
    };
    outs.iter()
        .map(
            |(out, pairs)| match map_position(text, out, pairs, (line, column)) {
                Err(why) => (Moved::Unmapped, why.to_owned()),
                Ok((l, c)) if l != line => (Moved::Line, format!("{l}:{c}")),
                Ok((l, c)) if c != column => (Moved::Column, format!("{l}:{c}")),
                Ok((l, _)) if line_text(text, line) != line_text(out, l) => {
                    (Moved::Text, "= (text)".to_owned())
                }
                Ok(_) => (Moved::Kept, "=".to_owned()),
            },
        )
        .collect()
}

/// The area of the repository a golden is in, for the summary.
fn golden_area(rel: &str) -> &'static str {
    if rel.starts_with("docs/examples/") {
        "docs/examples/*/expected.*"
    } else if rel.starts_with("tests/ui/") {
        "tests/ui"
    } else if rel.starts_with("tests/repl/") {
        "tests/repl"
    } else {
        "crates/*/tests/golden"
    }
}

/// One location of a golden in a source T1b reformats.
struct LocationRow {
    area: &'static str,
    /// The golden and its line, `path:line`.
    golden: String,
    /// The golden's `path`, as it names the source.
    golden_path: String,
    location: String,
    cells: Vec<(Moved, String)>,
}

/// Every location of every golden in a source T1b reformats, with what it
/// does at each width; and the counts of the other locations: those in
/// sources T1b does not reformat, and those in `cheby_mir`'s stub.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: the examples can be listed"
)]
fn golden_rows(root: &Path) -> (Vec<LocationRow>, usize, usize) {
    let examples: Vec<(String, PathBuf)> = example::all(root)
        .unwrap()
        .into_iter()
        .filter_map(|ex| {
            let name = ex.dir.file_name()?.to_string_lossy().into_owned();
            Some((name, ex.dir))
        })
        .collect();
    let mut sources: BTreeMap<PathBuf, Option<Formatted>> = BTreeMap::new();
    let mut rows = Vec::new();
    let (mut not_reformatted, mut stub) = (0usize, 0usize);
    for golden in golden_files(root) {
        let Ok(text) = std::fs::read_to_string(&golden) else {
            continue;
        };
        let rel = display_relative(&golden, root);
        for (golden_line, path, line, column) in locations_in(&text) {
            if path == "std/io.cheby" && STUB_STD_IO_GOLDENS.contains(&rel.as_str()) {
                stub = stub.saturating_add(1);
                continue;
            }
            let Some(source) = reformatted_source(root, &golden, &examples, &path) else {
                not_reformatted = not_reformatted.saturating_add(1);
                continue;
            };
            let source = sources
                .entry(source.clone())
                .or_insert_with(|| formatted(&source));
            rows.push(LocationRow {
                area: golden_area(&rel),
                golden: format!("{rel}:{golden_line}"),
                golden_path: rel.clone(),
                location: format!("{path}:{line}:{column}"),
                cells: location_cells(source.as_ref(), line, column),
            });
        }
    }
    (rows, not_reformatted, stub)
}

#[test]
#[ignore = "a measurement, printed on demand"]
fn fmt_width_golden_locations() {
    use std::fmt::Write as _;
    let root = repo_root();
    // The stub's goldens are still `golden_tests::build`'s.
    let golden_tests =
        std::fs::read_to_string(root.join("crates/cheby_mir/src/golden_tests.rs")).unwrap();
    assert!(golden_tests.contains("(\"std/io.cheby\".to_owned(), STD_IO.to_owned())"));
    for golden in STUB_STD_IO_GOLDENS {
        let name = Path::new(golden).file_stem().unwrap().to_string_lossy();
        assert!(
            golden_tests.contains(&format!("check_golden(\"{name}\"")),
            "{golden} is no longer written by golden_tests::build"
        );
    }
    let (rows, not_reformatted, stub) = golden_rows(&root);
    let mut report = String::new();
    writeln!(
        report,
        "| Width | Locations | Line moves | Column moves only | Position kept, line text changes | Unchanged | Not mapped | Goldens with a moved location (of {}) |",
        rows.iter()
            .map(|r| r.golden_path.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    )
    .unwrap();
    writeln!(
        report,
        "| ----: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )
    .unwrap();
    let moved = |r: &LocationRow, i: usize| matches!(r.cells[i].0, Moved::Line | Moved::Column);
    for (i, width) in WIDTHS.iter().enumerate() {
        let count = |m: Moved| rows.iter().filter(|r| r.cells[i].0 == m).count();
        let goldens: std::collections::BTreeSet<_> = rows
            .iter()
            .filter(|r| moved(r, i))
            .map(|r| r.golden_path.as_str())
            .collect();
        writeln!(
            report,
            "| {width} | {} | {} | {} | {} | {} | {} | {} |",
            rows.len(),
            count(Moved::Line),
            count(Moved::Column),
            count(Moved::Text),
            count(Moved::Kept),
            count(Moved::Unmapped),
            goldens.len()
        )
        .unwrap();
    }
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Golden area | Locations | Moved at {} | Moved at {} | Moved at {} |",
        WIDTHS[0], WIDTHS[1], WIDTHS[2]
    )
    .unwrap();
    writeln!(report, "| ----------- | ---: | ---: | ---: | ---: |").unwrap();
    let areas: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.area).collect();
    for area in areas {
        let of: Vec<_> = rows.iter().filter(|r| r.area == area).collect();
        let count = |i: usize| of.iter().filter(|r| moved(r, i)).count();
        writeln!(
            report,
            "| {area} | {} | {} | {} | {} |",
            of.len(),
            count(0),
            count(1),
            count(2)
        )
        .unwrap();
    }
    writeln!(report).unwrap();
    writeln!(
        report,
        "Not counted: {not_reformatted} locations that name a source T1b does not reformat (a UI test, a REPL fixture, a test's inline source), and {stub} in `cheby_mir`'s inline `std/io.cheby` stub."
    )
    .unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Golden line | Location | At {} | At {} | At {} |",
        WIDTHS[0], WIDTHS[1], WIDTHS[2]
    )
    .unwrap();
    writeln!(report, "| ----------- | -------- | ---- | ---- | ---- |").unwrap();
    for row in &rows {
        writeln!(
            report,
            "| {} | {} | {} |",
            row.golden,
            row.location,
            row.cells
                .iter()
                .map(|(_, c)| c.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        )
        .unwrap();
    }
    println!("{report}");
}
