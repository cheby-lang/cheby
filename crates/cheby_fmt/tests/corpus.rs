//! The corpus test of the formatter (D-1031): every example source
//! (`docs/examples/**/src/**/*.cheby`) and every std source
//! (`std/**/*.cheby`), formatted at 80, 100 and 120 columns.
//!
//! - Each formats without error, and a second pass changes nothing.
//! - Formatting keeps the meaning (GAP(fmt-trivia-definition)): the tokens
//!   without whitespace, line breaks and comments are unchanged, and so are
//!   the comments' texts in order; the formatted text parses with no grammar
//!   error into a tree whose node kinds, trivia aside, equal the original's.
//!   Two changes are the style's own and are compared as such: imports are
//!   sorted within their block (D-1030), so the imports are compared as a
//!   set, and a list the printer lays out gains a trailing comma when it
//!   breaks and loses it when it fits, so that comma is left out.
//!
//! The test only reads the repository.

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
    )
}

fn is_significant(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && kind != SyntaxKind::NEWLINE
}

/// What formatting must keep of one top-level node: its node kinds in
/// preorder and its significant tokens, without a laid-out list's
/// trailing comma.
fn shape(node: SyntaxNode<'_>) -> (Vec<SyntaxKind>, Vec<String>) {
    let kinds = node.descendants().map(SyntaxNode::kind).collect();
    let tokens: Vec<_> = node.tokens().filter(|t| is_significant(t.kind())).collect();
    let mut texts = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let trailing_comma = token.kind() == SyntaxKind::COMMA
            && token.parent().is_some_and(|p| is_laid_out_list(p.kind()))
            && tokens.get(i.saturating_add(1)).is_some_and(|next| {
                matches!(
                    next.kind(),
                    SyntaxKind::R_PAREN | SyntaxKind::GT | SyntaxKind::R_BRACE
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
}
