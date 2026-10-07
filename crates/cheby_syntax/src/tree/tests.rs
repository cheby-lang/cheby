//! Tree builder and navigation tests.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{FileId, TextRange};

use super::{NodeId, SyntaxTree, TokenId, build};
use crate::SyntaxKind::{self, *};
use crate::event::Event;
use crate::lexer::lex;

fn start(kind: SyntaxKind) -> Event {
    Event::Start {
        kind,
        forward_parent: None,
    }
}

fn tok(raw: u32) -> Event {
    Event::Token { raw }
}

fn build_tree(text: &str, events: Vec<Event>) -> SyntaxTree {
    let lexed = lex(FileId::new(7), text);
    let tree = build(FileId::new(7), Arc::from(text), &lexed, events);
    check_invariants(&tree);
    tree
}

/// Lossless text, contiguous tokens, consistent parents and subtree ends.
fn check_invariants(tree: &SyntaxTree) {
    let root = tree.root();
    assert_eq!(root.kind(), SOURCE_FILE);
    assert_eq!(root.parent(), None);
    assert_eq!(root.text(), tree.text());
    let joined: String = root.tokens().map(super::SyntaxToken::text).collect();
    assert_eq!(joined, tree.text());
    let mut offset = 0;
    for token in root.tokens() {
        assert_eq!(token.text_range().start(), offset);
        offset = token.text_range().end();
        assert!(token.kind().is_token());
        assert_ne!(token.kind(), EOF);
    }
    for node in root.descendants() {
        let joined: String = node.tokens().map(super::SyntaxToken::text).collect();
        assert_eq!(joined, node.text());
        for child in node.children_with_tokens() {
            assert_eq!(child.parent(), Some(node));
            assert!(node.text_range().contains_range(child.text_range()));
        }
    }
    let dump_lines = tree.debug_dump().lines().count();
    assert_eq!(dump_lines, tree.elements.len());
    let stats = tree.stats();
    assert_eq!(
        stats.nodes.checked_add(stats.tokens),
        Some(tree.elements.len())
    );
}

#[test]
fn empty_text() {
    let tree = build_tree("", vec![start(SOURCE_FILE), Event::Finish]);
    assert_eq!(tree.debug_dump(), "SOURCE_FILE@0..0\n");
    let tree = super::super::parse(FileId::new(0), Arc::from(""))
        .into_parts()
        .0;
    assert_eq!(tree.debug_dump(), "SOURCE_FILE@0..0\n");
}

#[test]
fn dump_format() {
    // fn main() {}
    let text = "fn main() {}\n";
    let events = vec![
        start(SOURCE_FILE),
        start(FN_DECL),
        tok(0),
        tok(2),
        start(PARAM_LIST),
        tok(3),
        tok(4),
        Event::Finish,
        start(BLOCK),
        tok(6),
        tok(7),
        Event::Finish,
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let expected = r#"SOURCE_FILE@0..13
  FN_DECL@0..12
    FN_KW@0..2 "fn"
    WHITESPACE@2..3 " "
    LOWER_IDENT@3..7 "main"
    PARAM_LIST@7..9
      L_PAREN@7..8 "("
      R_PAREN@8..9 ")"
    WHITESPACE@9..10 " "
    BLOCK@10..12
      L_BRACE@10..11 "{"
      R_BRACE@11..12 "}"
  NEWLINE@12..13 "\n"
"#;
    assert_eq!(tree.debug_dump(), expected);
}

#[test]
fn round_trip_of_a_flat_root() {
    let text = "let x = \"a {b}\" // c\r\n\u{feff} $ é";
    let parse = super::super::parse(FileId::new(0), Arc::from(text));
    let tree = parse.tree();
    check_invariants(tree);
    assert_eq!(
        tree.root().tokens().count(),
        lex(FileId::new(0), text).len()
    );
}

#[test]
fn forward_parent_wraps_a_completed_node() {
    // a + b, where `a` is completed first and then preceded by BIN_EXPR.
    let text = "a + b";
    let events = vec![
        start(SOURCE_FILE),
        Event::Start {
            kind: PATH_EXPR,
            forward_parent: NonZeroU32::new(3),
        },
        tok(0),
        Event::Finish,
        start(BIN_EXPR),
        tok(2),
        start(PATH_EXPR),
        tok(4),
        Event::Finish,
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let expected = r#"SOURCE_FILE@0..5
  BIN_EXPR@0..5
    PATH_EXPR@0..1
      LOWER_IDENT@0..1 "a"
    WHITESPACE@1..2 " "
    PLUS@2..3 "+"
    WHITESPACE@3..4 " "
    PATH_EXPR@4..5
      LOWER_IDENT@4..5 "b"
"#;
    assert_eq!(tree.debug_dump(), expected);
}

#[test]
fn forward_parent_chains() {
    let text = "x";
    let events = vec![
        start(SOURCE_FILE),
        Event::Start {
            kind: PATH_EXPR,
            forward_parent: NonZeroU32::new(3),
        },
        tok(0),
        Event::Finish,
        Event::Start {
            kind: CALL_EXPR,
            forward_parent: NonZeroU32::new(2),
        },
        Event::Finish,
        start(FIELD_EXPR),
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let kinds: Vec<_> = tree
        .root()
        .descendants()
        .map(super::SyntaxNode::kind)
        .collect();
    assert_eq!(kinds, [SOURCE_FILE, FIELD_EXPR, CALL_EXPR, PATH_EXPR]);
}

#[test]
fn trivia_goes_to_the_parent_before_a_start() {
    // `x` + spaces + `(` `)`, with the spaces between the nodes.
    let text = "  x  ";
    let events = vec![
        start(SOURCE_FILE),
        start(PATH_EXPR),
        tok(1),
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let expected = r#"SOURCE_FILE@0..5
  WHITESPACE@0..2 "  "
  PATH_EXPR@2..3
    LOWER_IDENT@2..3 "x"
  WHITESPACE@3..5 "  "
"#;
    assert_eq!(tree.debug_dump(), expected);
}

#[test]
fn doc_comments_attach_to_items() {
    let text = "//! m\n/// a\n// c\n/// d\n\n///e\n@inline\nfn f";
    let lexed = lex(FileId::new(0), text);
    let raw = |kind: SyntaxKind| {
        (0..lexed.len())
            .find(|&i| lexed.kind(i) == kind)
            .map(|i| u32::try_from(i).unwrap())
            .unwrap()
    };
    let at = raw(AT);
    let events = vec![
        start(SOURCE_FILE),
        start(FN_DECL),
        start(ATTR),
        tok(at),
        tok(at + 1),
        Event::Finish,
        tok(at + 2),
        tok(raw(FN_KW)),
        tok(raw(FN_KW) + 2),
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let item = tree.root().first_child().unwrap();
    assert_eq!(item.kind(), FN_DECL);
    assert_eq!(item.text(), "/// d\n\n///e\n@inline\nfn f");
    let root_tokens: Vec<_> = tree
        .root()
        .children_with_tokens()
        .filter_map(super::SyntaxElement::as_token)
        .map(super::SyntaxToken::text)
        .collect();
    assert_eq!(root_tokens, ["//! m", "\n", "/// a", "\n", "// c", "\n"]);
    assert_eq!(item.first_significant_token().unwrap().kind(), AT);
}

#[test]
fn doc_comments_do_not_attach_to_other_nodes() {
    let text = "/// a\nx";
    let events = vec![
        start(SOURCE_FILE),
        start(PATH_EXPR),
        tok(2),
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    assert_eq!(tree.root().first_child().unwrap().text(), "x");
}

#[test]
fn split_tokens() {
    // List<List<Int>>=
    let text = "a>>=b";
    let events = vec![
        start(SOURCE_FILE),
        tok(0),
        Event::Split {
            raw: 1,
            kind: GT,
            offset: 0,
            len: 1,
        },
        Event::Split {
            raw: 1,
            kind: GT,
            offset: 1,
            len: 1,
        },
        tok(2),
        tok(3),
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let kinds: Vec<_> = tree.root().tokens().map(|t| (t.kind(), t.text())).collect();
    assert_eq!(
        kinds,
        [
            (LOWER_IDENT, "a"),
            (GT, ">"),
            (GT, ">"),
            (EQ, "="),
            (LOWER_IDENT, "b")
        ]
    );

    let text = "x>=";
    let events = vec![
        start(SOURCE_FILE),
        tok(0),
        Event::Split {
            raw: 1,
            kind: GT,
            offset: 0,
            len: 1,
        },
        Event::Split {
            raw: 1,
            kind: EQ,
            offset: 1,
            len: 1,
        },
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let kinds: Vec<_> = tree.root().tokens().map(super::SyntaxToken::kind).collect();
    assert_eq!(kinds, [LOWER_IDENT, GT, EQ]);
}

#[test]
fn half_split_tokens_stay_lossless() {
    // Only the first half named: the rest becomes an error token.
    let text = "x>> y";
    let events = vec![
        start(SOURCE_FILE),
        tok(0),
        Event::Split {
            raw: 1,
            kind: GT,
            offset: 0,
            len: 1,
        },
        tok(3),
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let kinds: Vec<_> = tree.root().tokens().map(|t| (t.kind(), t.text())).collect();
    assert_eq!(
        kinds,
        [
            (LOWER_IDENT, "x"),
            (GT, ">"),
            (ERROR_TOKEN, ">"),
            (WHITESPACE, " "),
            (LOWER_IDENT, "y")
        ]
    );

    // A gap before the split part, an oversized length and a node kind.
    let events = vec![
        start(SOURCE_FILE),
        Event::Split {
            raw: 1,
            kind: SOURCE_FILE,
            offset: 1,
            len: 200,
        },
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let kinds: Vec<_> = tree.root().tokens().map(|t| (t.kind(), t.text())).collect();
    assert_eq!(
        kinds,
        [
            (LOWER_IDENT, "x"),
            (ERROR_TOKEN, ">"),
            (ERROR_TOKEN, ">"),
            (WHITESPACE, " "),
            (LOWER_IDENT, "y")
        ]
    );

    // A whole-token event after a split part.
    let events = vec![
        start(SOURCE_FILE),
        Event::Split {
            raw: 1,
            kind: GT,
            offset: 0,
            len: 1,
        },
        tok(1),
        Event::Finish,
    ];
    build_tree(text, events);
}

#[test]
fn malformed_events_are_tolerated() {
    let text = "a b c";
    // No events at all.
    let t = build_tree(text, Vec::new());
    assert_eq!(t.root().tokens().count(), 5);
    // Extra finishes, unclosed nodes, tokens out of order and out of range,
    // a token kind as a node, a tombstone and a second root start.
    let events = vec![
        Event::Finish,
        start(PATH_EXPR),
        tok(4),
        tok(0),
        tok(99),
        Event::Tombstone,
        start(COMMA),
        Event::Finish,
        Event::Finish,
        Event::Finish,
        start(SOURCE_FILE),
        Event::Start {
            kind: BLOCK,
            forward_parent: NonZeroU32::new(1000),
        },
    ];
    let t = build_tree(text, events);
    let kinds: Vec<_> = t
        .root()
        .descendants()
        .map(super::SyntaxNode::kind)
        .collect();
    assert_eq!(kinds, [SOURCE_FILE, PATH_EXPR, ERROR, SOURCE_FILE, BLOCK]);
    // Forward parent pointing at a non-start event.
    let events = vec![
        start(SOURCE_FILE),
        Event::Start {
            kind: PATH_EXPR,
            forward_parent: NonZeroU32::new(1),
        },
        tok(0),
        Event::Finish,
        Event::Finish,
    ];
    build_tree(text, events);
}

fn sample() -> SyntaxTree {
    // f(a, b)
    let text = "f(a, b) // c\n";
    let events = vec![
        start(SOURCE_FILE),
        start(CALL_EXPR),
        start(PATH_EXPR),
        tok(0),
        Event::Finish,
        start(ARG_LIST),
        tok(1),
        start(PATH_EXPR),
        tok(2),
        Event::Finish,
        tok(3),
        start(PATH_EXPR),
        tok(5),
        Event::Finish,
        tok(6),
        Event::Finish,
        Event::Finish,
        tok(9),
        Event::Finish,
    ];
    build_tree(text, events)
}

#[test]
fn navigation() {
    let tree = sample();
    let root = tree.root();
    let call = root.first_child().unwrap();
    assert_eq!(call.kind(), CALL_EXPR);
    assert_eq!(call.text(), "f(a, b)");
    assert_eq!(call.span().file, FileId::new(7));
    let children: Vec<_> = call.children().map(super::SyntaxNode::kind).collect();
    assert_eq!(children, [PATH_EXPR, ARG_LIST]);
    let args = call.first_child_of_kind(ARG_LIST).unwrap();
    assert_eq!(args.parent(), Some(call));
    let arg_kinds: Vec<_> = args
        .children_with_tokens()
        .map(super::SyntaxElement::kind)
        .collect();
    assert_eq!(
        arg_kinds,
        [L_PAREN, PATH_EXPR, COMMA, WHITESPACE, PATH_EXPR, R_PAREN]
    );
    let (a, b) = {
        let mut it = args.children();
        (it.next().unwrap(), it.next().unwrap())
    };
    assert_eq!(a.next_sibling(), Some(b));
    assert_eq!(b.prev_sibling(), Some(a));
    assert_eq!(a.prev_sibling(), None);
    assert_eq!(b.next_sibling(), None);
    assert_eq!(a.next_sibling_or_token().unwrap().kind(), COMMA);
    assert_eq!(b.prev_sibling_or_token().unwrap().kind(), WHITESPACE);
    assert_eq!(args.first_token_of_kind(COMMA).unwrap().text(), ",");
    assert_eq!(call.first_child().unwrap().next_sibling(), Some(args));
    assert_eq!(root.first_child().unwrap().next_sibling(), None);

    let ancestors: Vec<_> = b.ancestors().map(super::SyntaxNode::kind).collect();
    assert_eq!(ancestors, [PATH_EXPR, ARG_LIST, CALL_EXPR, SOURCE_FILE]);
    let descendants: Vec<_> = call.descendants().map(super::SyntaxNode::kind).collect();
    assert_eq!(
        descendants,
        [CALL_EXPR, PATH_EXPR, ARG_LIST, PATH_EXPR, PATH_EXPR]
    );
    assert_eq!(call.descendants_with_tokens().count(), 12);

    assert_eq!(root.first_token().unwrap().text(), "f");
    assert_eq!(root.last_token().unwrap().kind(), NEWLINE);
    assert_eq!(args.first_token().unwrap().kind(), L_PAREN);
    assert_eq!(args.last_token().unwrap().kind(), R_PAREN);
    assert_eq!(args.first_significant_token().unwrap().kind(), L_PAREN);

    let comma = args.first_token_of_kind(COMMA).unwrap();
    assert_eq!(comma.parent(), Some(args));
    assert_eq!(comma.next_token().unwrap().kind(), WHITESPACE);
    assert_eq!(comma.prev_token().unwrap().text(), "a");
    assert_eq!(comma.next_sibling().unwrap().kind(), WHITESPACE);
    assert_eq!(comma.prev_sibling().unwrap().kind(), PATH_EXPR);
    assert_eq!(root.first_token().unwrap().prev_token(), None);
    assert_eq!(root.last_token().unwrap().next_token(), None);

    assert_eq!(tree.node(call.id()), Some(call));
    assert_eq!(tree.token(comma.id()), Some(comma));
    assert_eq!(tree.node(NodeId(comma.id().index())), None);
    assert_eq!(tree.token(TokenId(call.id().index())), None);
    assert_eq!(tree.node(NodeId(u32::MAX)), None);
    assert_eq!(tree.token(TokenId(9999)), None);
}

#[test]
fn offsets_and_ranges() {
    let tree = sample();
    let root = tree.root();
    let at = |offset| root.token_at_offset(offset).map(super::SyntaxToken::text);
    assert_eq!(at(0), Some("f"));
    assert_eq!(at(1), Some("("));
    assert_eq!(at(4), Some(" "));
    assert_eq!(at(12), Some("\n"));
    assert_eq!(at(13), Some("\n"), "end of text: the last token");
    assert_eq!(at(14), None);

    let cover = |start, end| {
        root.covering_element(TextRange::new(start, end))
            .map(|e| (e.kind(), e.text()))
    };
    assert_eq!(cover(2, 3), Some((LOWER_IDENT, "a")));
    assert_eq!(cover(2, 6), Some((ARG_LIST, "(a, b)")));
    assert_eq!(cover(0, 7), Some((CALL_EXPR, "f(a, b)")));
    assert_eq!(cover(0, 13), Some((SOURCE_FILE, "f(a, b) // c\n")));
    // An empty range at a boundary: the element on the left.
    assert_eq!(cover(3, 3), Some((LOWER_IDENT, "a")));
    assert_eq!(cover(0, 99), None);
    let args = root
        .first_child()
        .unwrap()
        .first_child_of_kind(ARG_LIST)
        .unwrap();
    assert_eq!(args.token_at_offset(0), None);
}

#[test]
fn empty_nodes() {
    let text = "x";
    let events = vec![
        start(SOURCE_FILE),
        start(ERROR),
        Event::Finish,
        tok(0),
        start(ERROR),
        Event::Finish,
        Event::Finish,
    ];
    let tree = build_tree(text, events);
    let ranges: Vec<_> = tree
        .root()
        .children()
        .map(super::SyntaxNode::text_range)
        .collect();
    assert_eq!(ranges, [TextRange::empty(0), TextRange::empty(1)]);
    assert_eq!(tree.root().token_at_offset(1).unwrap().text(), "x");
    let first = tree.root().first_child().unwrap();
    assert_eq!(first.first_token(), None);
    assert_eq!(first.children_with_tokens().count(), 0);
}

/// Every `.cheby` file under `dir`, sorted.
fn cheby_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn examples_round_trip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = cheby_files(&root.join("docs/examples"));
    files.extend(cheby_files(&root.join("tests/ui")));
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let parse = super::super::parse(FileId::new(0), Arc::from(text.as_str()));
        check_invariants(parse.tree());
        assert_eq!(parse.tree().root().text(), text);
    }
}

#[test]
fn examples_have_no_lexical_diagnostics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in cheby_files(&root.join("docs/examples")) {
        let text = std::fs::read_to_string(&path).unwrap();
        let parse = super::super::parse(FileId::new(0), Arc::from(text.as_str()));
        assert_eq!(parse.errors(), [], "{}", path.display());
        // Only errors are checked here; warnings belong to the corpus test.
        let mut diags = super::super::validate(parse.tree());
        diags.retain(cheby_diag::Diagnostic::is_error);
        assert_eq!(diags, [], "{}", path.display());
    }
}
