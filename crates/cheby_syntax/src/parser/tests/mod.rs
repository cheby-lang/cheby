//! Parser tests: examples, UI files, golden CST dumps of the tree-sitter
//! corpus and of targeted cases, error recovery, and robustness.
//!
//! Goldens live in `src/parser/tests/data/{ok,err}/`: each `NAME.cheby` has
//! a `NAME.cst` with the tree dump and the rendered grammar errors. Files in
//! `ok/` must parse without errors, files in `err/` with at least one. Run
//! with `CHEBY_BLESS=1` to write the goldens.

mod constructs;
mod robustness;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, SourceMap, render_all};

use crate::{SyntaxKind, SyntaxTree};

/// Parses `text` as file `path` and checks the tree's invariants.
pub(super) fn parse_text(path: &str, text: &str) -> (SourceMap, SyntaxTree, Vec<Diagnostic>) {
    let mut sources = SourceMap::new();
    let file = sources.add_file(path, text).unwrap();
    let (tree, errors) = crate::parse(file, Arc::from(text)).into_parts();
    check_tree(&tree, text);
    (sources, tree, errors)
}

/// The tree covers the text exactly, tokens are contiguous, and every node
/// lies inside its parent.
pub(super) fn check_tree(tree: &SyntaxTree, text: &str) {
    let root = tree.root();
    assert_eq!(root.kind(), SyntaxKind::SOURCE_FILE);
    let mut offset = 0;
    let mut joined = String::with_capacity(text.len());
    for token in root.tokens() {
        assert_eq!(token.text_range().start(), offset, "tokens are contiguous");
        offset = token.text_range().end();
        assert!(token.kind().is_token());
        joined.push_str(token.text());
    }
    assert_eq!(joined, text, "the tree is lossless");
    for node in root.descendants() {
        if let Some(parent) = node.parent() {
            assert!(parent.text_range().contains_range(node.text_range()));
        }
    }
}

/// The tree dump followed by the rendered errors, as stored in goldens.
pub(super) fn snapshot(path: &str, text: &str) -> (String, usize) {
    let (sources, tree, errors) = parse_text(path, text);
    let mut out = tree.debug_dump();
    if !errors.is_empty() {
        out.push_str("\n// errors\n\n");
        out.push_str(&render_all(&errors, &sources));
    }
    (out, errors.len())
}

/// The rendered errors of `text`, empty if none.
pub(super) fn errors_of(text: &str) -> String {
    let (sources, _, errors) = parse_text("test.cheby", text);
    render_all(&errors, &sources)
}

/// The codes of the errors of `text`, in order.
pub(super) fn codes_of(text: &str) -> Vec<String> {
    let (_, _, errors) = parse_text("test.cheby", text);
    errors.iter().map(|d| d.code.to_string()).collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// All `.cheby` files under `dir`, sorted.
pub(super) fn cheby_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub(super) fn example_files() -> Vec<PathBuf> {
    cheby_files(&repo_root().join("docs/examples"))
}

#[test]
fn examples_parse_without_errors() {
    let files = example_files();
    assert!(files.len() > 30, "examples found");
    let mut report = String::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let rendered = errors_of(&text);
        if !rendered.is_empty() {
            writeln!(report, "== {}\n{rendered}", path.display()).unwrap();
        }
    }
    assert!(report.is_empty(), "{report}");
}

/// The `@mode` of a UI test file.
fn ui_mode(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("// @mode:"))
        .map(str::trim)
}

#[test]
fn ui_files_parse_as_their_mode_says() {
    let files = cheby_files(&repo_root().join("tests/ui"));
    assert_ne!(files.len(), 0);
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let errors = errors_of(&text);
        if ui_mode(&text) == Some("parse-fail") {
            assert!(!errors.is_empty(), "{} must not parse", path.display());
        } else {
            assert!(errors.is_empty(), "{}:\n{errors}", path.display());
        }
    }
}

/// Compares (or blesses) the golden of every file in `data/ok` and
/// `data/err`.
#[test]
fn goldens() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/parser/tests/data");
    let bless = std::env::var_os("CHEBY_BLESS").is_some();
    let mut failures = String::new();
    let mut count = 0;
    for (dir, want_errors) in [("ok", false), ("err", true)] {
        for path in cheby_files(&data.join(dir)) {
            count += 1;
            let text = std::fs::read_to_string(&path).unwrap();
            let name = format!("{dir}/{}", path.file_name().unwrap().to_string_lossy());
            let (actual, errors) = snapshot(&name, &text);
            if want_errors != (errors > 0) {
                writeln!(
                    failures,
                    "{name}: expected errors: {want_errors}, found {errors}"
                )
                .unwrap();
            }
            let golden = path.with_extension("cst");
            if bless {
                std::fs::write(&golden, &actual).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&golden).unwrap_or_default();
            if expected != actual {
                writeln!(
                    failures,
                    "{name}: golden differs (run with CHEBY_BLESS=1)\n--- actual\n{actual}"
                )
                .unwrap();
            }
        }
    }
    assert!(count > 0);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
#[ignore = "debugging aid: CHEBY_DUMP=path cargo test -- --ignored dump"]
fn dump() {
    let path = std::env::var("CHEBY_DUMP").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let (out, _) = snapshot(&path, &text);
    println!("{out}");
}
