//! Tests of validation: each rule with inputs that must and must not
//! trigger it, the examples and UI files, and a golden of the
//! permissive-shapes file.
//!
//! The golden lives in `src/validate/tests/data/`; run with
//! `CHEBY_BLESS=1` to write it.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use cheby_diag::{Diagnostic, SourceMap, render_all};

use crate::ast::tests::cheby_files;

/// Parses and validates `text`. The text must have no grammar errors.
fn check(text: &str) -> (SourceMap, Vec<Diagnostic>) {
    let mut sources = SourceMap::new();
    let file = sources.add_file("test.cheby", text).unwrap();
    let (tree, errors) = crate::parse(file, Arc::from(text)).into_parts();
    assert!(
        errors.is_empty(),
        "grammar errors:\n{}",
        render_all(&errors, &sources)
    );
    let diags = crate::validate(&tree);
    (sources, diags)
}

/// The code and primary text of each diagnostic of `text`.
fn diags(text: &str) -> Vec<(String, String)> {
    let (_, diags) = check(text);
    diags
        .iter()
        .map(|d| {
            let span = d.primary_span().unwrap();
            let snippet = span.range.slice(text).unwrap().to_owned();
            (d.code.to_string(), snippet)
        })
        .collect()
}

/// Wraps `body` in a function.
fn in_fn(body: &str) -> String {
    format!("fn f() {{\n{body}\n}}\n")
}

/// Asserts that `text` validates cleanly.
#[track_caller]
fn ok(text: &str) {
    let (sources, diags) = check(text);
    assert!(diags.is_empty(), "{}", render_all(&diags, &sources));
}

/// Asserts that `text` reports exactly `expected` (code, primary text).
#[track_caller]
fn reports(text: &str, expected: &[(&str, &str)]) {
    let got = diags(text);
    let got: Vec<(&str, &str)> = got.iter().map(|(c, s)| (c.as_str(), s.as_str())).collect();
    assert_eq!(got, expected, "{text}");
}

/// The rendered diagnostics of `text`.
fn rendered(text: &str) -> String {
    let (sources, diags) = check(text);
    render_all(&diags, &sources)
}

// ---- E0017 doc comments ----

#[test]
fn doc_comments_must_document_something() {
    ok(
        "/// Doc.\n@inline\nfn f() {}\n/// T.\ntype T {\n  /// A.\n  A,\n}\ntype R {\n  /// Field.\n  x: Int,\n}\ninterface I {\n  /// F.\n  fn f(Self)\n}\n/// C.\nconst c: Int = 1\n/// Test.\ntest \"t\" {}\n",
    );
    reports(
        &in_fn("  /// Not here.\n  1"),
        &[("E0017", "/// Not here.")],
    );
    reports("fn f() {}\n/// Trailing.\n", &[("E0017", "/// Trailing.")]);
    reports(
        "@inline\n/// After the attribute.\nfn f() {}\n",
        &[("E0017", "/// After the attribute.")],
    );
    reports(
        "type T {\n  A,\n  /// Dangling.\n}\n",
        &[("E0017", "/// Dangling.")],
    );
    // A plain comment between the doc comment and the item detaches it.
    reports("/// Doc.\n// Plain.\nfn f() {}\n", &[("E0017", "/// Doc.")]);
    // `////` is an ordinary comment.
    ok("//// Not a doc comment.\nfn f() {}\n");
}

#[test]
fn long_doc_comment_runs_validate_in_linear_time() {
    // Each `///` used to re-walk all the doc comments of its item, which
    // took seconds for some ten thousand lines. The bound is far above the
    // linear cost, even in a debug build.
    let lines = 30_000;
    let item = format!("{}fn f() {{}}\n", "/// d\n".repeat(lines));
    let variant = format!("type T {{\n{}  A,\n}}\n", "  /// d\n".repeat(lines));
    for text in [item, variant] {
        let start = std::time::Instant::now();
        ok(&text);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs() < 5, "took {elapsed:?}");
    }
}

#[test]
fn doc_comment_suggests_ordinary_comment() {
    let out = rendered(&in_fn("  /// Note.\n  1"));
    assert!(
        out.contains("help: use `//` for an ordinary comment"),
        "{out}"
    );
}

#[test]
fn module_docs_come_first() {
    ok("//! Module.\n//! More.\n\n@!target(js)\nimport std::io\nfn f() {}\n");
    reports(
        "import std::io\n//! Late.\nfn f() {}\n",
        &[("E0017", "//! Late.")],
    );
    reports(&in_fn("  //! Inside.\n  1"), &[("E0017", "//! Inside.")]);
}

// ---- E0018 module order ----

#[test]
fn module_header_order() {
    ok("@!target(js)\nimport a\nimport b\nfn f() {}\n");
    reports(
        "fn f() {}\nimport std::io\n",
        &[("E0018", "import std::io")],
    );
    reports(
        "import std::io\n@!target(js)\nfn f() {}\n",
        &[("E0018", "@!target(js)")],
    );
    reports(
        "fn f() {}\n@!target(js)\nimport a\nimport b\n",
        &[
            ("E0018", "@!target(js)"),
            ("E0018", "import a"),
            ("E0018", "import b"),
        ],
    );
}

// ---- E0019 attributes ----

#[test]
fn attributes_need_an_item() {
    ok("@external(js, \"f\")\n@async\nfn f() -> Int\n");
    reports("fn f() {}\n@inline\n", &[("E0019", "@inline")]);
    reports(
        "@a\n@b\nimport std::io\n",
        &[("E0019", "@a"), ("E0019", "@b")],
    );
    reports("@ inline\nfn f() {}\n", &[("E0019", " ")]);
    reports("@! target(js)\nfn f() {}\n", &[("E0019", " ")]);
}

#[test]
fn names_are_checked_by_resolution() {
    // `async` is reserved (D-128) but `@async` is an attribute (§12.5.2).
    // Reserved words and naming (E0006, E0007) are checked where a name is
    // bound, by name resolution, so validation reports neither.
    ok("@external(js, \"f\")\n@async\nfn f() -> Int\n");
    ok(&in_fn("  let async = 1\n  let myValue = async"));
}

// ---- E0020, E0021 signatures ----

#[test]
fn top_level_signatures() {
    ok("fn f(a: Int, b: List<Int>) -> Int { a }\nconst c: Int = 1\n");
    reports("fn f(a) {}\n", &[("E0020", "a")]);
    reports("const c = 1\n", &[("E0020", "c")]);
    reports("fn f((a, b): (Int, Int)) {}\n", &[("E0021", "(a, b)")]);
    reports("fn f(_: Int) {}\n", &[("E0021", "_")]);
    reports(
        "fn f((a, b)) {}\n",
        &[("E0021", "(a, b)"), ("E0020", "(a, b)")],
    );
    // Closures and local functions may take patterns and omit types.
    ok(&in_fn("  fn g((a, b), c) { a }\n  fn((x, y)) { x }"));
}

// ---- E0022, E0023 ----

#[test]
fn exposed_requires_pub() {
    ok("pub exposed type T { x: Int }\n");
    reports("exposed type T { x: Int }\n", &[("E0022", "exposed")]);
    reports("priv exposed type T { x: Int }\n", &[("E0022", "exposed")]);
    let out = rendered("exposed type T { x: Int }\n");
    assert!(out.contains("help: make the type `pub` as well"), "{out}");
}

#[test]
fn bounds_where_none_are_allowed() {
    ok("fn f<T: Show + Eq>(x: T) {}\ninterface I: Show {}\ntype B = dyn Show\n");
    reports("type T<A: Show> { x: A }\n", &[("E0023", "Show")]);
    reports(
        "type T<A: Show + Eq> = List<A>\n",
        &[("E0023", "Show + Eq")],
    );
    reports("type B = dyn Into<Int>\n", &[("E0023", "<Int>")]);
    reports("fn f<T: From<Int>>(x: T) {}\n", &[("E0023", "<Int>")]);
}

// ---- E0013 shapes ----

#[test]
fn local_function_shapes() {
    ok(&in_fn("  fn g(x) { x }\n  g(1)"));
    reports(&in_fn("  pub fn g(x) { x }"), &[("E0013", "pub")]);
    reports(&in_fn("  fn g<T>(x: T) { x }"), &[("E0013", "<T>")]);
    reports(&in_fn("  fn g(x)"), &[("E0013", "g")]);
}

#[test]
fn let_message_needs_assert() {
    ok(&in_fn("  let assert Some(x) = y as \"no value\""));
    reports(&in_fn("  let x = y as \"m\""), &[("E0013", "as \"m\"")]);
    let out = rendered(&in_fn("  let x = y as \"m\""));
    assert!(out.contains("write `let assert`"), "{out}");
}

#[test]
fn use_binders_have_no_trailing_comma() {
    ok(&in_fn("  use a, b <- f()\n  use <- g()\n  a"));
    reports(&in_fn("  use a, <- f()\n  a"), &[("E0013", ",")]);
}

#[test]
fn empty_field_lists() {
    ok("type T { A, B(Int), C { x: Int } }\ntype U\n");
    reports("type T {}\n", &[("E0013", "{}")]);
    reports("type T { A() }\n", &[("E0013", "()")]);
    reports("type T { A {} }\n", &[("E0013", "{}")]);
    reports(
        &in_fn("  case x {\n    Some() => 1\n    _ => 2\n  }"),
        &[("E0013", "()")],
    );
}

#[test]
fn import_shapes() {
    ok("import std::io as sys\nimport std::{list, Option as Opt, map as m}\n");
    reports("import std::{io, list} as both\n", &[("E0013", "as both")]);
    reports("import std::{Option as opt}\n", &[("E0013", "opt")]);
    reports("import std::{list as List}\n", &[("E0013", "List")]);
}

#[test]
fn pattern_shapes() {
    ok(&in_fn(
        "  case x {\n    A | B as y => 1\n    (A as y) | (B as y) => 2\n    _ => 3\n  }",
    ));
    reports(
        &in_fn("  case x {\n    A as y | B => 1\n    _ => 2\n  }"),
        &[("E0013", "A as y")],
    );
    reports(
        &in_fn("  case x {\n    a as b as c => 1\n  }"),
        &[("E0013", "as")],
    );
}

#[test]
fn empty_case_bodies() {
    reports(&in_fn("  case x {}"), &[("E0013", "{}")]);
    reports(&in_fn("  case {}"), &[("E0013", "{}")]);
}

// ---- E0026, E0027 holes ----

#[test]
fn interface_functions_take_no_type_arguments() {
    reports(&in_fn("  Show::show::<Int>(1)"), &[("E0023", "::<Int>")]);
    reports(
        &in_fn("  fmt::Show::show::<Int>(1)"),
        &[("E0023", "::<Int>")],
    );
    ok(&in_fn(
        "  let a = list::empty::<Int>()\n  let b = Box::<Int>\n  Show::show(1)",
    ));
}

#[test]
fn token_checks_skip_interpolations_with_grammar_errors() {
    let text = in_fn("  let s = \"{a + b}\"\n  let t = \"{ c }\"\n  let badName = \"{d\n}\"");
    let mut sources = SourceMap::new();
    let file = sources.add_file("test.cheby", text.as_str()).unwrap();
    let (tree, errors) = crate::parse(file, Arc::from(text.as_str())).into_parts();
    assert_eq!(errors.len(), 2, "{}", render_all(&errors, &sources));
    let found: Vec<(String, String)> = crate::validate_tokens(&tree)
        .iter()
        .map(|d| {
            let span = d.primary_span().unwrap();
            (
                d.code.to_string(),
                span.range.slice(&text).unwrap().to_owned(),
            )
        })
        .collect();
    // Only the whitespace of the valid `{ c }`. The name `badName` is left
    // to name resolution, which a file with grammar errors never reaches.
    let expected = [("E0012", " "), ("E0012", " ")];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(c, t)| ((*c).to_owned(), (*t).to_owned()))
        .collect();
    assert_eq!(found, expected);
}

#[test]
fn holes() {
    ok(&in_fn(
        "  f(a, _)\n  f(_, a)\n  f(a, g(_, b))\n  list::map(xs, int::add(1, _))",
    ));
    reports(&in_fn("  f(_, _)"), &[("E0026", "_")]);
    let three = diags(&in_fn("  f(_, a, _, _)"));
    assert_eq!(three.len(), 1, "one diagnostic per argument list");
    reports(&in_fn("  double(_)"), &[("E0027", "double(_)")]);
    reports(
        &in_fn("  list::map(xs, string::upper(_,))"),
        &[("E0027", "string::upper(_,)")],
    );
    // A sole hole is an error everywhere, also in a pipe (D-245).
    reports(&in_fn("  x |> f(_)"), &[("E0027", "f(_)")]);
    let out = rendered(&in_fn("  double(_)"));
    assert!(out.contains("help: write `double` instead"), "{out}");
}

// ---- E0028 pipes ----

#[test]
fn pipes_need_a_hole() {
    ok(&in_fn(
        "  xs |> list::map(_, f)\n  xs |> list::length\n  xs |> f(a, _)\n  x |> (make(c))\n  x |> fn(y) { y }\n  x |> f(_, g(_, 1))",
    ));
    reports(
        &in_fn("  xs |> list::take(3)"),
        &[("E0028", "list::take(3)")],
    );
    reports(&in_fn("  x |> f()"), &[("E0028", "f()")]);
    reports(
        &in_fn("  x |> f(a, g(_, 1))"),
        &[("E0028", "f(a, g(_, 1))")],
    );
    // `g(_)` is a sole hole on its own (D-245).
    reports(
        &in_fn("  x |> f(a, g(_))"),
        &[("E0028", "f(a, g(_))"), ("E0027", "g(_)")],
    );
    reports(
        &in_fn("  xs |> list::map(_, f) |> list::take(3)"),
        &[("E0028", "list::take(3)")],
    );
    let out = rendered(&in_fn("  x |> f(a, g(_, 1))"));
    assert!(
        out.contains("note: the `_` in `g(…)` belongs to `g` (D-202)"),
        "{out}"
    );
    assert!(
        out.contains("help: add `_` where the piped value goes"),
        "{out}"
    );
    let out = rendered(&in_fn("  x |> f()"));
    assert!(out.contains("write `f`"), "{out}");
}

// ---- E0029 comparisons ----

#[test]
fn comparisons_do_not_chain() {
    ok(&in_fn(
        "  a < b && b < c\n  (a < b) == c\n  a == (b < c)\n  xs |> list::length == 3",
    ));
    reports(&in_fn("  a < b < c"), &[("E0029", "<")]);
    let four = diags(&in_fn("  a < b <= c == d"));
    assert_eq!(four.len(), 2, "one per extra operator");
}

// ---- E0030 tuples ----

#[test]
fn short_tuples() {
    ok("fn f(p: (Int, Int)) -> (Int, Int) {\n  let (a, b) = p\n  (a, b)\n}\n");
    reports(&in_fn("  (1,)"), &[("E0030", "(1,)")]);
    reports("type U = ()\n", &[("E0030", "()")]);
    reports("type O = (Int,)\n", &[("E0030", "(Int,)")]);
    reports("type O = (Int)\n", &[("E0030", "(Int)")]);
    reports(
        &in_fn("  case x {\n    () => 1\n    (p,) => 2\n    (p) => 3\n  }"),
        &[("E0030", "()"), ("E0030", "(p,)")],
    );
}

// ---- E0031 record updates ----

#[test]
fn record_updates() {
    ok(&in_fn(
        "  Point { ..p, x: 1 }\n  m::Point { ..p, x: 1, y: 2 }\n  Point { x: 1, y }",
    ));
    reports(&in_fn("  Point { x: 1, ..p }"), &[("E0031", "..p")]);
    reports(&in_fn("  Point { ..p, x: 1, ..q }"), &[("E0031", "..q")]);
    reports(&in_fn("  Point { ..p }"), &[("E0031", "Point { ..p }")]);
}

// ---- E0032 rests ----

#[test]
fn rests() {
    ok(&in_fn(
        "  case xs {\n    [a, ..rest] => 1\n    [..init, last] => 2\n    [..] => 3\n    P { x, .. } => 4\n    P { .. } => 5\n  }",
    ));
    reports(
        &in_fn("  case xs {\n    [..a, b, ..c] => 1\n  }"),
        &[("E0032", "..c")],
    );
    reports(
        &in_fn("  case p {\n    P { .., x } => 1\n  }"),
        &[("E0032", "..")],
    );
    reports(
        &in_fn("  case p {\n    P { x, .., .. } => 1\n  }"),
        &[("E0032", "..")],
    );
    reports(
        &in_fn("  case p {\n    P { .., x, .. } => 1\n  }"),
        &[("E0032", ".."), ("E0032", "..")],
    );
}

// ---- E0033, E0034 literal patterns ----

#[test]
fn literal_patterns() {
    ok(&in_fn(
        "  case x {\n    1 => 1\n    -1 => 2\n    \"a\" => 3\n    r\"b{c}\" => 4\n    _ => 5\n  }",
    ));
    reports(
        &in_fn("  case x {\n    1.5 => 1\n    -0.5 => 2\n  }"),
        &[("E0033", "1.5"), ("E0033", "-0.5")],
    );
    reports(
        &in_fn("  case x {\n    \"hi {name}\" => 1\n  }"),
        &[("E0034", "{name}")],
    );
}

// ---- E0056 negative literals ----

#[test]
fn negative_literals_have_no_space() {
    ok(&in_fn(
        "  let a = -5\n  let b = -0.5\n  let c = -(5)\n  let d = a - 5\n  let e = - a\n  case a {\n    -1 => 1\n    _ => 0\n  }",
    ));
    reports(&in_fn("  let a = - 5"), &[("E0056", " ")]);
    reports(&in_fn("  let a = -\t\t0.5"), &[("E0056", "\t\t")]);
    reports(&in_fn("  let a = 1 + - 5 - 1"), &[("E0056", " ")]);
    reports(
        &in_fn("  case a {\n    -  1 => 1\n    _ => 0\n  }"),
        &[("E0056", "  ")],
    );
    let fix = &rendered(&in_fn("  let a = - 5"));
    assert!(fix.contains("help: remove the space"), "{fix}");
}

// ---- E0035 subjectless case ----

#[test]
fn subjectless_case_ends_with_default() {
    ok(&in_fn(
        "  case {\n    a => 1\n    _ => 2\n  }\n  case x {\n    a => 1\n  }",
    ));
    reports(
        &in_fn("  case {\n    a => 1\n    b => 2\n  }"),
        &[("E0035", "b")],
    );
    reports(
        &in_fn("  case {\n    _ => 1\n    b => 2\n  }"),
        &[("E0035", "_"), ("E0035", "b")],
    );
}

// ---- W0001 tuple shapes ----

#[test]
fn tuple_shape_warning() {
    ok("fn f(p: (Int, Int, Int)) -> List<(Int, String)> { [] }\n");
    reports(
        "fn f(p: (Int, Int, Int, Int)) {}\n",
        &[("W0001", "(Int, Int, Int, Int)")],
    );
    reports(
        "type T = ((Int, Int), Int)\n",
        &[("W0001", "((Int, Int), Int)")],
    );
    // One warning per written type.
    reports(
        "type T = (((A, B), C), D)\n",
        &[("W0001", "(((A, B), C), D)")],
    );
    reports(
        "type R { pairs: List<((A, B), C)> }\n",
        &[("W0001", "((A, B), C)")],
    );
    reports(
        &in_fn("  let x: ((A, B), C) = y\n  fn((a, b): ((A, B), C)) { a }"),
        &[("W0001", "((A, B), C)"), ("W0001", "((A, B), C)")],
    );
    // Values and patterns are not types.
    ok(&in_fn("  let ((a, b), c) = ((1, 2), 3)"));
    let (_, diags) = check("type T = (A, B, C, D)\n");
    assert!(!diags[0].is_error(), "a warning");
}

// ---- E0024 is a grammar error ----

#[test]
fn tuple_index_is_a_grammar_error() {
    let mut sources = SourceMap::new();
    let text = in_fn("  pair.0");
    let file = sources.add_file("test.cheby", text.as_str()).unwrap();
    let parse = crate::parse(file, Arc::from(text.as_str()));
    let codes: Vec<String> = parse.errors().iter().map(|d| d.code.to_string()).collect();
    assert_eq!(codes, ["E0024"]);
    assert_eq!(crate::validate(parse.tree()), []);
}

// ---- Whole files ----

/// Examples have no validation errors. Example 030 has one W0001 (a
/// four-element tuple type), so warnings are not checked here.
#[test]
fn examples_validate_without_errors() {
    let files = cheby_files("docs/examples");
    assert!(files.len() > 30);
    let mut report = String::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let (sources, diags) = check(&text);
        let errors: Vec<Diagnostic> = diags.into_iter().filter(Diagnostic::is_error).collect();
        let out = render_all(&errors, &sources);
        if !out.is_empty() {
            writeln!(report, "== {}\n{out}", path.display()).unwrap();
        }
    }
    assert!(report.is_empty(), "{report}");
}

/// UI files that must pass report no validation error, and the D-243
/// file reports only warnings.
#[test]
fn passing_ui_files_have_no_validation_errors() {
    for path in cheby_files("tests/ui") {
        let text = std::fs::read_to_string(&path).unwrap();
        let mode = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("// @mode:"))
            .map(str::trim);
        if !matches!(mode, Some("check-pass" | "run-pass" | "build-pass")) {
            continue;
        }
        let (sources, diags) = check(&text);
        let errors: Vec<Diagnostic> = diags.into_iter().filter(Diagnostic::is_error).collect();
        assert!(
            errors.is_empty(),
            "{}:\n{}",
            path.display(),
            render_all(&errors, &sources)
        );
    }
}

/// Validation never panics on broken trees.
#[test]
fn validation_survives_broken_trees() {
    let inputs = [
        "fn (",
        "type T {",
        "case",
        "fn f() { x |> }",
        "fn f() { a < }",
        "fn f() { P { .. ",
        "fn f() { case { _ => } }",
        "import a::{",
        "@",
        "@!",
        "fn f(a: (Int,",
        "fn f() { f(_, ",
        "fn f() { let assert = }",
    ];
    for text in inputs {
        let parse = crate::parse(cheby_diag::FileId::new(0), Arc::from(text));
        let _ = crate::validate(parse.tree());
    }
    for path in cheby_files("tests/ui") {
        let text = std::fs::read_to_string(&path).unwrap();
        let parse = crate::parse(cheby_diag::FileId::new(0), Arc::from(text.as_str()));
        let _ = crate::validate(parse.tree());
    }
}

/// The rendered diagnostics of the permissive-shapes file, which the
/// parser accepts without a grammar error.
#[test]
fn permissive_golden() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/validate/tests/data");
    let input = dir.join("permissive.cheby");
    let text = std::fs::read_to_string(&input).unwrap();
    let mut sources = SourceMap::new();
    let file = sources.add_file("permissive.cheby", text.as_str()).unwrap();
    let (tree, errors) = crate::parse(file, Arc::from(text.as_str())).into_parts();
    assert!(errors.is_empty(), "{}", render_all(&errors, &sources));
    let mut diags = crate::validate(&tree);
    sources.sort(&mut diags);
    let actual = render_all(&diags, &sources);
    let golden = dir.join("permissive.diag");
    if std::env::var_os("CHEBY_BLESS").is_some() {
        std::fs::write(&golden, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&golden).unwrap_or_default();
    assert!(
        expected == actual,
        "golden {} differs; rerun with CHEBY_BLESS=1\n{actual}",
        golden.display()
    );
}
