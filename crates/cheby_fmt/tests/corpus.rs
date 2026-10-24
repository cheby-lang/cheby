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
use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxTree, parse};
use cheby_test_support::{display_relative, example, files_with_extension, repo_root};

/// The widths of the CP2 measurement (D-1030).
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

/// Prints, as a Markdown table, how many nodes of each kind the printer
/// laid out and how many it printed verbatim over the corpus at the
/// provisional width. Run with `--nocapture` to read it.
#[test]
fn fmt_corpus_coverage() {
    let root = repo_root();
    let files = corpus(&root);
    let mut total = Coverage::default();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        total.add(&coverage(&text, cheby_fmt::PROVISIONAL_WIDTH).unwrap());
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
        if let Ok(c) = coverage(&text, cheby_fmt::PROVISIONAL_WIDTH) {
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
    let c = coverage(text, cheby_fmt::PROVISIONAL_WIDTH).map_err(|e| e.to_string())?;
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
/// informative measure behind the width's choice at CP2 (D-1030). Run with
/// `--ignored --nocapture`.
#[test]
#[ignore = "a measurement, printed on demand"]
fn fmt_corpus_change_table() {
    let root = repo_root();
    let files = corpus(&root);
    println!(
        "| Width | Files that change (of {}) | Source lines that change |",
        files.len()
    );
    println!("| ----: | ------------------------: | -----------------------: |");
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
        println!("| {width:>5} | {changed_files:>25} | {lines:>24} |");
    }
}
