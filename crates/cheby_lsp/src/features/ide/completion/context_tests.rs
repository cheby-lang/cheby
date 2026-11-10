//! Context detection on every broken form of the syntax map's §2.2 that
//! X §4.1's tests list, and on whole text. Syntax only, so no test runs
//! the front end.

use cheby_driver::analysis::TextRange;
use cheby_syntax::{SyntaxKind as K, SyntaxTree};

use super::{Context, ExprSite, Found, PathNs, Receiver, Subject, detect};
use crate::features::handlers::symbols::tests::doc;

/// The tree of `text` with its `‸` removed, and the offset of the `‸`.
fn parse(text: &str) -> (SyntaxTree, u32) {
    let at = text.find('‸').expect("a cursor");
    let tree = doc(&text.replacen('‸', "", 1)).tree;
    (tree, u32::try_from(at).expect("a small text"))
}

/// What a context is, without its nodes, so tests can compare it.
#[derive(Debug, PartialEq, Eq)]
enum Shape {
    None,
    ImportPath(Vec<&'static str>),
    ImportItems(Vec<&'static str>, Vec<&'static str>),
    Attribute(bool),
    Qualified(Vec<&'static str>, PathNs, bool),
    Dot(&'static str),
    DotInterp(Vec<&'static str>),
    Pipe(&'static str),
    Type,
    Pattern(&'static str),
    RecordField(Vec<&'static str>, bool),
    Interpolation,
    Expr(bool, &'static str),
    Item(bool),
}

/// `found`'s shape. Texts are leaked so the shape owns nothing.
fn shape(found: &Found<'_>) -> Shape {
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    let leak_all = |v: &[String]| v.iter().map(|s| leak(s)).collect::<Vec<_>>();
    match &found.context {
        Context::None => Shape::None,
        Context::ImportPath { prefix } => Shape::ImportPath(leak_all(prefix)),
        Context::ImportItems { module, written } => {
            Shape::ImportItems(leak_all(module), leak_all(written))
        }
        Context::Attribute { inner } => Shape::Attribute(*inner),
        Context::Qualified {
            qualifier,
            ns,
            piped,
        } => Shape::Qualified(leak_all(qualifier), *ns, piped.is_some()),
        Context::Dot {
            receiver: Receiver::Expr(node),
            ..
        } => Shape::Dot(leak(node.text())),
        Context::Dot {
            receiver: Receiver::Interpolation { names, .. },
            ..
        } => Shape::DotInterp(names.iter().map(|t| leak(t.text())).collect()),
        Context::Pipe { lhs, .. } => Shape::Pipe(leak(lhs.text())),
        Context::Type => Shape::Type,
        Context::Pattern { subject } => Shape::Pattern(match subject {
            Subject::Case(_) => "case",
            Subject::Let(_) => "let",
            Subject::Unknown => "unknown",
        }),
        Context::RecordField { written, empty, .. } => {
            Shape::RecordField(leak_all(written), *empty)
        }
        Context::Interpolation => Shape::Interpolation,
        Context::Expr { statement, site } => Shape::Expr(
            *statement,
            match site {
                ExprSite::Path(_) => "path",
                ExprSite::Arg { index: 0, .. } => "arg 0",
                ExprSite::Arg { index: 1, .. } => "arg 1",
                ExprSite::Arg { .. } => "arg n",
                ExprSite::LetValue(_) => "let",
                ExprSite::ArmBody(_) => "arm",
                ExprSite::FieldValue(_) => "field",
                ExprSite::Unknown => "unknown",
            },
        ),
        Context::Item { after_visibility } => Shape::Item(*after_visibility),
    }
}

/// The shape and the partial's text at the `‸` of `text`.
fn at(text: &str) -> (Shape, String) {
    let (tree, offset) = parse(text);
    let found = detect(tree.root(), offset);
    let plain = text.replacen('‸', "", 1);
    let partial = plain
        .get(
            usize::try_from(found.partial.start()).unwrap()
                ..usize::try_from(found.partial.end()).unwrap(),
        )
        .unwrap()
        .to_owned();
    (shape(&found), partial)
}

/// Checks each `(text, shape, partial)`.
fn check(cases: &[(&str, Shape, &str)]) {
    for (text, want, partial) in cases {
        let (got, got_partial) = at(text);
        assert_eq!(got, *want, "context of {text:?}");
        assert_eq!(got_partial, *partial, "partial of {text:?}");
    }
}

#[test]
fn qualified_paths_broken_and_whole() {
    let value = |q: &'static str| Shape::Qualified(vec![q], PathNs::Value, false);
    check(&[
        ("fn f() {\n  list::‸\n}\n", value("list"), ""),
        ("fn f() {\n  let a = list::‸\n}\n", value("list"), ""),
        ("fn f() {\n  g(list::‸)\n}\n", value("list"), ""),
        ("fn f() {\n  list::ma‸\n}\n", value("list"), "ma"),
        ("fn f() {\n  list::m‸ap(xs)\n}\n", value("list"), "map"),
        ("fn f() {\n  Show::‸\n}\n", value("Show"), ""),
        (
            "fn f(x: shapes::Sh‸) {}\n",
            Shape::Qualified(vec!["shapes"], PathNs::Type, false),
            "Sh",
        ),
        (
            "fn f() {\n  case x {\n    shapes::Ci‸ => 1\n  }\n}\n",
            Shape::Qualified(vec!["shapes"], PathNs::Pattern, false),
            "Ci",
        ),
    ]);
}

#[test]
fn package_b_contexts_are_found() {
    check(&[
        (
            "fn f() {\n  xs |> list::‸\n}\n",
            Shape::Qualified(vec!["list"], PathNs::Value, true),
            "",
        ),
        (
            "fn f() {\n  xs |> list::ma‸\n}\n",
            Shape::Qualified(vec!["list"], PathNs::Value, true),
            "ma",
        ),
        ("fn f() {\n  x.‸\n}\n", Shape::Dot("x"), ""),
        ("fn f() {\n  x.fo‸\n}\n", Shape::Dot("x"), "fo"),
        ("fn f() {\n  a.b.‸\n}\n", Shape::Dot("a.b"), ""),
        ("fn f() {\n  xs |> ‸\n}\n", Shape::Pipe("xs"), ""),
        ("fn f() {\n  xs |> ma‸\n}\n", Shape::Pipe("xs"), "ma"),
        (
            "fn f() {\n  \"{p.‸}\"\n}\n",
            Shape::DotInterp(vec!["p"]),
            "",
        ),
        (
            "fn f() {\n  \"{p.x‸}\"\n}\n",
            Shape::DotInterp(vec!["p"]),
            "x",
        ),
    ]);
}

#[test]
fn calls_and_values() {
    check(&[
        ("fn f() {\n  foo(‸\n}\n", Shape::Expr(false, "arg 0"), ""),
        (
            "fn f() {\n  foo(1, ‸\n  let z = 2\n}\n",
            Shape::Expr(false, "arg 1"),
            "",
        ),
        (
            "fn f() {\n  foo(1, ‸)\n}\n",
            Shape::Expr(false, "arg 1"),
            "",
        ),
        (
            "fn f() {\n  foo(1, ba‸)\n}\n",
            Shape::Expr(false, "arg 1"),
            "ba",
        ),
        ("fn f() {\n  let y = ‸\n}\n", Shape::Expr(false, "let"), ""),
        (
            "fn f() {\n  let y = fo‸\n}\n",
            Shape::Expr(false, "let"),
            "fo",
        ),
        ("fn f() {\n  1 + ‸\n}\n", Shape::Expr(false, "unknown"), ""),
        (
            "fn f() {\n  use x <- ‸\n}\n",
            Shape::Expr(false, "unknown"),
            "",
        ),
        ("const A: Int = ‸\n", Shape::Expr(false, "unknown"), ""),
    ]);
}

#[test]
fn statements_start_lines_in_blocks() {
    check(&[
        ("fn f() {\n  ‸\n}\n", Shape::Expr(true, "unknown"), ""),
        (
            "fn f() {\n  let x = 1\n  ‸\n}\n",
            Shape::Expr(true, "unknown"),
            "",
        ),
        (
            "fn f() {\n  let x = 1\n  le‸\n}\n",
            Shape::Expr(true, "unknown"),
            "le",
        ),
        (
            "fn f() {\n  let x = 1\n  case‸\n}\n",
            Shape::Expr(true, "unknown"),
            "case",
        ),
        ("fn f() { ‸}\n", Shape::Expr(true, "unknown"), ""),
        (
            "fn f() {\n  let x = 1\n  fn‸\n}\n",
            Shape::Expr(true, "unknown"),
            "fn",
        ),
        ("fn f() {\n  let‸\n}\n", Shape::Expr(true, "unknown"), "let"),
        // The rest of a complete line takes nothing.
        ("fn f() {\n  foo ‸\n}\n", Shape::None, ""),
        ("fn f() {\n  let x ‸\n}\n", Shape::None, ""),
        // An unclosed body at the end of the file.
        (
            "fn f() {\n  let x = 1\n  ‸",
            Shape::Expr(true, "unknown"),
            "",
        ),
    ]);
}

#[test]
fn case_arms() {
    check(&[
        ("fn f() {\n  case x {‸\n}\n", Shape::Pattern("case"), ""),
        (
            "fn f() {\n  case x {\n    ‸\n  }\n}\n",
            Shape::Pattern("case"),
            "",
        ),
        (
            "fn f() {\n  case x {\n    S‸\n  }\n}\n",
            Shape::Pattern("case"),
            "S",
        ),
        (
            "fn f() {\n  case x {\n    1 => 2\n    ‸\n  }\n}\n",
            Shape::Pattern("case"),
            "",
        ),
        (
            "fn f() {\n  case x {\n    1 =>‸\n  }\n}\n",
            Shape::Expr(false, "arm"),
            "",
        ),
        (
            "fn f() {\n  case x {\n    1 => ‸\n  }\n}\n",
            Shape::Expr(false, "arm"),
            "",
        ),
        (
            "fn f() {\n  case {\n    a => ‸\n  }\n}\n",
            Shape::Expr(false, "arm"),
            "",
        ),
        (
            "fn f() {\n  case x {\n    Some(‸\n  }\n}\n",
            Shape::Pattern("unknown"),
            "",
        ),
        ("fn f() {\n  case ‸\n}\n", Shape::Expr(false, "unknown"), ""),
    ]);
}

#[test]
fn patterns_after_let_use_and_fn() {
    check(&[
        ("fn f() {\n  let ‸\n}\n", Shape::Pattern("let"), ""),
        ("fn f() {\n  let So‸ = x\n}\n", Shape::Pattern("let"), "So"),
        ("fn f() {\n  use ‸\n}\n", Shape::Pattern("unknown"), ""),
        (
            "fn f() {\n  let g = fn(‸\n}\n",
            Shape::Pattern("unknown"),
            "",
        ),
    ]);
}

#[test]
fn record_fields() {
    check(&[
        (
            "fn f() {\n  P { x: 1, ‸\n}\n",
            Shape::RecordField(vec!["x"], false),
            "",
        ),
        (
            "fn f() {\n  P { ‸}\n}\n",
            Shape::RecordField(vec![], true),
            "",
        ),
        (
            "fn f() {\n  P { x: 1, y‸ }\n}\n",
            Shape::RecordField(vec!["x"], false),
            "y",
        ),
        (
            "fn f() {\n  P {\n    x: 1,\n    ‸\n  }\n}\n",
            Shape::RecordField(vec!["x"], false),
            "",
        ),
        ("fn f() {\n  P { x: ‸\n}\n", Shape::Expr(false, "field"), ""),
    ]);
}

#[test]
fn types() {
    check(&[
        ("fn f(x: ‸) {}\n", Shape::Type, ""),
        ("fn f(x: ‸\n", Shape::Type, ""),
        ("fn f(x: In‸) {}\n", Shape::Type, "In"),
        ("fn f() -> ‸\n", Shape::Type, ""),
        ("fn f(x: List<‸) {}\n", Shape::Type, ""),
        ("fn f(x: Map<Int, ‸>) {}\n", Shape::Type, ""),
        ("fn f() {\n  let x: ‸\n}\n", Shape::Type, ""),
        ("type A = ‸\n", Shape::Type, ""),
        ("type P {\n  x: ‸\n}\n", Shape::Type, ""),
        ("fn f(x: dyn ‸) {}\n", Shape::Type, ""),
    ]);
}

#[test]
fn imports() {
    check(&[
        ("import ‸\n", Shape::ImportPath(vec![]), ""),
        ("import std::‸\n", Shape::ImportPath(vec!["std"]), ""),
        ("import std::li‸\n", Shape::ImportPath(vec!["std"]), "li"),
        (
            "import std::json::d‸\n",
            Shape::ImportPath(vec!["std", "json"]),
            "d",
        ),
        (
            "import std::list::{‸\n",
            Shape::ImportItems(vec!["std", "list"], vec![]),
            "",
        ),
        (
            "import std::list::{map, ‸\n",
            Shape::ImportItems(vec!["std", "list"], vec!["map"]),
            "",
        ),
        (
            "import std::list::{map, fi‸}\n",
            Shape::ImportItems(vec!["std", "list"], vec!["map"]),
            "fi",
        ),
        ("import std::list as ‸\n", Shape::None, ""),
    ]);
}

#[test]
fn attributes_and_items() {
    check(&[
        ("@‸\nfn f() {}\n", Shape::Attribute(false), ""),
        ("@dep‸\nfn f() {}\n", Shape::Attribute(false), "dep"),
        ("@!‸\n", Shape::Attribute(true), ""),
        ("‸", Shape::Item(false), ""),
        ("fn f() {}\n‸", Shape::Item(false), ""),
        ("fn f() {}\n\npu‸\n", Shape::Item(false), "pu"),
        ("pub ‸\n", Shape::Item(true), ""),
        ("pub fn f() {}\n\n‸\n", Shape::Item(false), ""),
        // A whole keyword is a partial word too.
        ("fn f() {}\n\nfn‸\n", Shape::Item(false), "fn"),
        ("fn f() {}\n\npub fn‸\n", Shape::Item(true), "fn"),
    ]);
}

#[test]
fn nothing_in_text_comments_numbers_and_declared_names() {
    check(&[
        ("// a comm‸ent\n", Shape::None, ""),
        ("fn f() {\n  // x‸\n}\n", Shape::None, ""),
        ("fn f() {\n  \"ab‸c\"\n}\n", Shape::None, ""),
        ("fn f() {\n  \"‸\"\n}\n", Shape::None, ""),
        ("fn f() {\n  12‸\n}\n", Shape::None, ""),
        ("fn fo‸() {}\n", Shape::None, "fo"),
        ("type Sh‸ {}\n", Shape::None, "Sh"),
        ("fn f() {\n  fn lo‸() {}\n}\n", Shape::None, "lo"),
        ("fn ‸\n", Shape::None, ""),
    ]);
}

#[test]
fn interpolations() {
    check(&[
        ("fn f() {\n  \"a{‸}\"\n}\n", Shape::Interpolation, ""),
        ("fn f() {\n  \"a{to‸}\"\n}\n", Shape::Interpolation, "to"),
        ("fn f() {\n  \"a{‸\"\n}\n", Shape::Interpolation, ""),
    ]);
}

#[test]
fn the_partial_is_the_whole_word() {
    let (tree, offset) = parse("fn f() {\n  let y = fo‸obar\n}\n");
    let found = detect(tree.root(), offset);
    assert_eq!(found.partial, TextRange::new(19, 25));
    assert_eq!(
        found.anchor.map(cheby_syntax::SyntaxToken::kind),
        Some(K::EQ)
    );
}
