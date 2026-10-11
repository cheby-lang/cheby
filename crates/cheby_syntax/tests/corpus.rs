//! Corpus test (A6, A8): every example and every UI test, through the
//! public API.
//!
//! - Examples parse with no grammar error and validate with no diagnostic.
//! - UI tests parse with no grammar error, except `parse-fail` tests,
//!   which must have at least one (D-424: `check-fail` tests hold only
//!   validation or later errors). Every file of a package UI test must
//!   parse, since a package test cannot be `parse-fail`.
//! - Every tree prints back exactly its input.
//!
//! The tree shapes themselves are pinned by the parser's goldens in
//! `src/parser/tests/data/`.

use std::path::Path;
use std::sync::Arc;

use cheby_diag::{SourceMap, render_all};
use cheby_syntax::{SyntaxToken, SyntaxTree, parse, validate};
use cheby_test_support::{display_relative, example, repo_root, ui};

/// The tree's text, rebuilt from its tokens.
fn printed(tree: &SyntaxTree) -> String {
    tree.root().tokens().map(SyntaxToken::text).collect()
}

/// What checking one file found.
struct Checked {
    /// The tree prints back exactly the input.
    lossless: bool,
    /// The rendered grammar errors.
    errors: String,
    /// The rendered validation diagnostics.
    validation: String,
}

/// Parses and validates `path`.
fn check_file(path: &Path) -> Result<Checked, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut sources = SourceMap::new();
    let file = sources
        .add_file(display_relative(path, &repo_root()), text.as_str())
        .map_err(|e| e.to_string())?;
    let parse = parse(file, Arc::from(text.as_str()));
    let validation = validate(parse.tree());
    Ok(Checked {
        lossless: printed(parse.tree()) == text && parse.tree().text() == text,
        errors: render_all(parse.errors(), &sources),
        validation: render_all(&validation, &sources),
    })
}

#[test]
fn examples_parse_and_validate_cleanly() {
    let root = repo_root();
    let mut count: usize = 0;
    for ex in example::all(&root).unwrap() {
        for path in ex.sources().unwrap() {
            let Checked {
                lossless,
                errors,
                validation,
            } = check_file(&path).unwrap();
            assert!(lossless, "{}: not lossless", path.display());
            assert_eq!(errors, "", "{}", path.display());
            assert_eq!(validation, "", "{}", display_relative(&path, &root));
            count += 1;
        }
    }
    assert!(count >= 90, "only {count} example files found");
}

#[test]
fn ui_tests_follow_their_mode() {
    let root = repo_root();
    let tests = ui::all(&root).unwrap();
    assert!(!tests.is_empty());
    for test in tests {
        for path in test.sources().unwrap() {
            let shown = display_relative(&path, &root);
            let Checked {
                lossless, errors, ..
            } = check_file(&path).unwrap();
            assert!(lossless, "{shown}: not lossless");
            if test.header.mode.is_syntax_error() {
                assert_ne!(
                    errors, "",
                    "{shown}: `parse-fail`, but the parser reports no grammar error"
                );
            } else {
                assert_eq!(
                    errors, "",
                    "{shown}: grammar errors in a file that must parse"
                );
            }
        }
    }
}
