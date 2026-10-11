//! Property tests (testing.md §3, A2, A3, A8): for any input, lexing and
//! parsing never panic, terminate, and the tree prints back exactly the
//! input. Validation must not panic on the trees either.
//!
//! Inputs are arbitrary strings, strings over Cheby's punctuation, and
//! example sources mutated by deleting, inserting and duplicating tokens.

use std::sync::{Arc, LazyLock};

use cheby_diag::FileId;
use cheby_syntax::lexer::lex;
use cheby_syntax::{SyntaxToken, parse, validate};
use cheby_test_support::{example, repo_root};
use proptest::prelude::*;

/// Checks every property on `text`.
fn check(text: &str) -> Result<(), TestCaseError> {
    let file = FileId::new(0);

    let lexed = lex(file, text);
    let mut end = 0;
    for i in 0..lexed.len() {
        let range = lexed.range(i);
        prop_assert_eq!(range.start(), end, "token {} is not contiguous", i);
        prop_assert!(
            range.slice(text).is_some(),
            "token {} is not on char boundaries",
            i
        );
        end = range.end();
    }
    prop_assert_eq!(usize::try_from(end).ok(), Some(text.len()));

    let parse = parse(file, Arc::from(text));
    let tree = parse.tree();
    prop_assert_eq!(tree.text(), text);
    let printed: String = tree.root().tokens().map(SyntaxToken::text).collect();
    prop_assert_eq!(printed.as_str(), text);
    let root = tree.root().text_range();
    prop_assert_eq!(root.start(), 0);
    prop_assert_eq!(usize::try_from(root.end()).ok(), Some(text.len()));
    for node in tree.root().descendants() {
        prop_assert!(root.contains_range(node.text_range()));
    }
    let _ = validate(tree);
    let _ = tree.debug_dump();
    Ok(())
}

/// The sources of every example, read once. `every_example_is_lossless`
/// checks that there are some.
static EXAMPLES: LazyLock<Vec<String>> = LazyLock::new(|| {
    example::all(&repo_root())
        .unwrap_or_default()
        .iter()
        .flat_map(|ex| ex.sources().unwrap_or_default())
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect()
});

/// Example `index`, modulo the number of examples.
fn example_source(index: usize) -> &'static str {
    index
        .checked_rem(EXAMPLES.len())
        .and_then(|i| EXAMPLES.get(i))
        .map_or("", String::as_str)
}

/// `at` modulo `len`, or 0 for an empty range.
fn wrap(at: usize, len: usize) -> usize {
    at.checked_rem(len).unwrap_or(0)
}

/// Fragments inserted by mutations: tokens, partial tokens and the
/// characters that start strings, comments and interpolations.
const FRAGMENTS: &[&str] = &[
    "fn", "type", "import", "case", "let", "use", "assert", "as", "when", "pub", "test", "(", ")",
    "{", "}", "[", "]", "<", ">", ">>", ">=", "=", "=>", "->", "<-", "|>", "|", ",", ":", "::",
    ".", "..", "_", "-", "!", "&&", "||", "@", "@!", "\n", " ", "\t", "\r", "\"", "\"{", "}\"",
    "\\", "//", "///", "//!", "/*", "*/", "r#\"", "\"#", "0x", "1.5e", "0.0", "x.0", "$", "é", ";",
    "Some", "f(_)", "{x:?}",
];

#[derive(Clone, Debug)]
enum Edit {
    Delete(usize),
    Insert(usize, usize),
    Duplicate(usize),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        any::<usize>().prop_map(Edit::Delete),
        (any::<usize>(), 0..FRAGMENTS.len()).prop_map(|(at, f)| Edit::Insert(at, f)),
        any::<usize>().prop_map(Edit::Duplicate),
    ]
}

/// Applies `edits` to the tokens of example `index`.
fn mutate(index: usize, edits: &[Edit]) -> String {
    let source = example_source(index);
    let lexed = lex(FileId::new(0), source);
    let mut tokens: Vec<String> = (0..lexed.len())
        .map(|i| lexed.text(source, i).to_owned())
        .collect();
    for edit in edits {
        let len = tokens.len();
        match *edit {
            Edit::Delete(at) => {
                if len > 0 {
                    tokens.remove(wrap(at, len));
                }
            }
            Edit::Insert(at, fragment) => {
                let text = FRAGMENTS.get(fragment).copied().unwrap_or("");
                tokens.insert(wrap(at, len.saturating_add(1)), text.to_owned());
            }
            Edit::Duplicate(at) => {
                let at = wrap(at, len);
                if let Some(token) = tokens.get(at).cloned() {
                    tokens.insert(at, token);
                }
            }
        }
    }
    tokens.concat()
}

/// Example `index`, cut at a character boundary at or before `cut`.
fn truncate(index: usize, cut: usize) -> &'static str {
    let source = example_source(index);
    let mut at = wrap(cut, source.len().saturating_add(1));
    while !source.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    source.get(..at).unwrap_or("")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_strings(text in any::<String>()) {
        check(&text)?;
    }

    #[test]
    fn punctuation_soup(text in r#"[a-zA-Z0-9_ \t\n\r"{}()\[\]<>=|&!+*/%^:.,@#\\'$é-]{0,300}"#) {
        check(&text)?;
    }

    #[test]
    fn mutated_examples(index in any::<usize>(), edits in prop::collection::vec(edit(), 1..12)) {
        check(&mutate(index, &edits))?;
    }

    /// Truncated examples end inside every kind of construct.
    #[test]
    fn truncated_examples(index in any::<usize>(), cut in any::<usize>()) {
        check(truncate(index, cut))?;
    }
}

#[test]
fn every_example_is_lossless() {
    assert!(EXAMPLES.len() >= 90, "examples not found");
    for source in EXAMPLES.iter() {
        check(source).unwrap();
    }
}
