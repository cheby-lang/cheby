//! Targeted tests per construct: tree shapes, newline decisions, grammar
//! errors and their codes, and partial trees after errors.

use super::{codes_of, errors_of, parse_text};
use crate::SyntaxKind::{
    self, ALIAS_DECL, AS_PAT, BIN_EXPR, BLOCK, CALL_EXPR, CASE_ARM, CASE_BODY, COND_ARM,
    CONST_DECL, CTOR_PAT, EQ, ERROR, FIELD_EXPR, FN_DECL, GT, HOLE, IMPORT_DECL, INNER_ATTR,
    INTERFACE_DECL, INTERFACE_FN, LET_STMT, LITERAL_PAT, LOCAL_FN, OR_PAT, PAREN_PAT, PATH_EXPR,
    PREFIX_EXPR, RECORD_EXPR, TEST_DECL, TUPLE_EXPR, TYPE_DECL, UNDERSCORE,
};

/// The kinds of all nodes, in preorder.
fn nodes(text: &str) -> Vec<SyntaxKind> {
    let (_, tree, _) = parse_text("test.cheby", text);
    tree.root()
        .descendants()
        .map(crate::SyntaxNode::kind)
        .collect()
}

fn count(text: &str, kind: SyntaxKind) -> usize {
    nodes(text).into_iter().filter(|&k| k == kind).count()
}

/// The kinds of the significant tokens, without trivia and line breaks.
fn tokens(text: &str) -> Vec<SyntaxKind> {
    let (_, tree, _) = parse_text("test.cheby", text);
    tree.root()
        .tokens()
        .map(crate::SyntaxToken::kind)
        .filter(|k| !k.is_trivia() && *k != SyntaxKind::NEWLINE)
        .collect()
}

/// The children kinds of the first node of `kind`.
fn children_of(text: &str, kind: SyntaxKind) -> Vec<SyntaxKind> {
    let (_, tree, _) = parse_text("test.cheby", text);
    let node = tree
        .root()
        .descendants()
        .find(|n| n.kind() == kind)
        .unwrap();
    node.children().map(crate::SyntaxNode::kind).collect()
}

fn in_fn(body: &str) -> String {
    format!("fn f() {{\n{body}\n}}\n")
}

fn assert_ok(text: &str) {
    let errors = errors_of(text);
    assert!(errors.is_empty(), "{text}\n{errors}");
}

// ---- newlines (§2.8, D-231) ------------------------------------------------

#[test]
fn statements_split_at_line_breaks() {
    let text = in_fn("  let a = b\n  -c");
    assert_ok(&text);
    assert_eq!(count(&text, LET_STMT), 1);
    assert_eq!(count(&text, PREFIX_EXPR), 1);
    assert_eq!(count(&text, BIN_EXPR), 0);
}

#[test]
fn trailing_continuation_tokens() {
    for (text, kind) in [
        ("  let a = b +\n    c", BIN_EXPR),
        ("  let a = b ||\n    c", BIN_EXPR),
        ("  let a = b ==\n    c", BIN_EXPR),
        ("  let a = b |>\n    c", BIN_EXPR),
        ("  let a =\n    b", LET_STMT),
        ("  f(a,\n    b)", CALL_EXPR),
        ("  use x <-\n    f()", PATH_EXPR),
        ("  case x {\n    _ =>\n      1\n  }", CASE_ARM),
    ] {
        let text = in_fn(text);
        assert_ok(&text);
        assert!(nodes(&text).contains(&kind), "{text}");
    }
    assert_ok("fn f() ->\n  Int {\n  1\n}\n");
}

#[test]
fn leading_continuation_tokens() {
    for op in ["|>", "&&", "||"] {
        let text = in_fn(&format!("  a\n\n  // comment\n  {op} b"));
        assert_ok(&text);
        assert_eq!(count(&text, BIN_EXPR), 1, "{op}");
    }
    // Other leading operators start a new statement.
    for op in ["-", "!"] {
        let text = in_fn(&format!("  a\n  {op}b"));
        assert_ok(&text);
        assert_eq!(count(&text, BIN_EXPR), 0, "{op}");
    }
}

#[test]
fn line_breaks_inside_brackets_are_ignored() {
    assert_ok(&in_fn("  f(\n    a\n    ,\n    b\n  )"));
    assert_ok(&in_fn("  [\n    a\n    ,\n    b\n  ]"));
    assert_ok("type T = Result<\n  Int\n  ,\n  String\n>\n");
    // Inside `{` of a closure argument, line breaks separate again.
    let text = in_fn("  f(fn(x) {\n    let y = x\n    y\n  })");
    assert_ok(&text);
    assert_eq!(count(&text, LET_STMT), 1);
}

#[test]
fn closures_and_keywords_at_line_start_inside_brackets() {
    let text = in_fn("  list::map(\n    xs,\n    fn(x) { x },\n  )\n  [\n    fn(y) { y },\n  ]");
    assert_ok(&text);
    assert_eq!(count(&text, SyntaxKind::CLOSURE), 2);
}

#[test]
fn closing_type_bracket_ends_the_line() {
    let text = "type A = Option<Int>\ntype B = List<List<Int>>\nconst c: Int = 1\n";
    assert_ok(text);
    assert_eq!(count(text, ALIAS_DECL), 2);
    assert_eq!(count(text, CONST_DECL), 1);
}

#[test]
fn shift_and_ge_split_in_types() {
    let text = "const x: List<List<Int>>= []\n";
    assert_ok(text);
    let toks = tokens(text);
    let closers: Vec<_> = toks
        .iter()
        .copied()
        .filter(|k| matches!(k, GT | EQ))
        .collect();
    assert_eq!(closers, [GT, GT, EQ]);
    // In expressions `>>` and `>=` are operators.
    let text = in_fn("  a >> b >= c");
    assert_ok(&text);
    assert_eq!(count(&text, BIN_EXPR), 2);
}

#[test]
fn unexpected_line_breaks_are_e0014() {
    for text in [
        "  let x\n    = 1",
        "  let x = -\n    1",
        "  case x {\n    A |\n    B => 1\n  }",
        "  let p = Point { x: a\n    , y: b }",
    ] {
        let text = in_fn(text);
        let codes = codes_of(&text);
        assert!(codes.contains(&"E0014".to_owned()), "{text}: {codes:?}");
    }
    // A body on the next line is not the function's body (spec, unlike
    // tree-sitter).
    assert_eq!(codes_of("fn f()\n{\n}\n"), ["E0013"]);
}

#[test]
fn line_after_e0014_does_not_cascade() {
    // The example of docs/diagnostics/E0014.md.
    let text = "fn total(price: Int, tax: Int) -> Int {\n  let sum\n    = price + tax\n  sum\n}\n";
    assert_eq!(codes_of(text), ["E0014"]);
    // A statement keyword on the next line is kept and parsed.
    let text = in_fn("  let a\n  let b = 1\n  let c = 2");
    assert_eq!(codes_of(&text), ["E0014"]);
    assert_eq!(count(&text, LET_STMT), 3);
    // A `(` list stops at a statement keyword with one E0015.
    let text = in_fn("  let x = add(1, 2\n  assert x == 3");
    assert_eq!(codes_of(&text), ["E0015"]);
}

#[test]
fn files_need_no_final_line_break() {
    assert_ok("fn f() {}");
    assert_ok("\n\n\nfn f() {}\n\n\n");
    assert_ok("fn f() {}\r\nfn g() {}\r\n");
    assert_ok("");
}

// ---- items -----------------------------------------------------------------

#[test]
fn attributes_belong_to_their_item() {
    let text = "@external(native, \"x\")\n@blocking\npub fn f() -> Int\n";
    assert_ok(text);
    assert_eq!(
        children_of(text, FN_DECL),
        [
            SyntaxKind::ATTR,
            SyntaxKind::ATTR,
            SyntaxKind::VISIBILITY,
            SyntaxKind::PARAM_LIST,
            SyntaxKind::RET_TYPE
        ]
    );
    // Attributes that no item follows are an ERROR node without a grammar
    // error (E0019 is reported by validation).
    let text = "@external(js)\n";
    assert_ok(text);
    assert_eq!(count(text, ERROR), 1);
    assert_eq!(
        codes_of("@inline fn f() {}\n"),
        ["E0013"],
        "an attribute needs its own line"
    );
}

#[test]
fn module_header() {
    let text = "//! Docs.\n@!target(native)\nimport a::b::{C, d as e}\nimport f as g\n";
    assert_ok(text);
    assert_eq!(count(text, INNER_ATTR), 1);
    assert_eq!(count(text, IMPORT_DECL), 2);
    assert_eq!(codes_of("import a as B\n"), ["E0013"]);
    assert_eq!(codes_of("import a::{}\n"), ["E0013"]);
    assert_eq!(codes_of("import a::B\n"), ["E0013"]);
}

#[test]
fn type_bodies() {
    let text = "type A { X, Y(Int), Z { a: Int } }\ntype B { a: Int }\ntype C\n";
    assert_ok(text);
    assert_eq!(count(text, TYPE_DECL), 3);
    assert_eq!(count(text, SyntaxKind::VARIANT), 3);
    assert_eq!(count(text, SyntaxKind::RECORD_FIELD), 2);
    // Doc comments before the first variant do not hide it.
    let text = "type A {\n  /// doc\n  X,\n}\n";
    assert_ok(text);
    assert_eq!(count(text, SyntaxKind::VARIANT_LIST), 1);
}

#[test]
fn exposed_on_other_items_is_e0022() {
    assert_eq!(codes_of("pub exposed fn f() {}\n"), ["E0022"]);
    assert_eq!(codes_of("pub exposed type A = Int\n"), ["E0022"]);
    assert_eq!(codes_of("pub exposed interface I\n"), ["E0022"]);
    assert_ok("exposed type A { x: Int }\n");
    assert_ok("pub exposed type A { x: Int }\n");
}

#[test]
fn tests_have_no_visibility() {
    assert_eq!(codes_of("pub test \"t\" {}\n"), ["E0013"]);
    assert_eq!(count("test \"t\" {}\n", TEST_DECL), 1);
    assert_eq!(codes_of("test r\"t\" {}\n"), ["E0013"]);
}

// ---- expressions -----------------------------------------------------------

#[test]
fn holes_only_as_whole_arguments() {
    let text = in_fn("  f(_, a)\n  g(a, _)\n  h(_)");
    assert_ok(&text);
    assert_eq!(count(&text, HOLE), 3);
    for text in ["  f(_ + 1)", "  let x = _", "  [_]", "  (_, 1)"] {
        assert_eq!(codes_of(&in_fn(text)), ["E0025"], "{text}");
    }
    // In a subjectless case, `_ =>` is the default arm, not a hole.
    let text = in_fn("  case {\n    a => 1\n    _ => 2\n  }");
    assert_ok(&text);
    assert_eq!(count(&text, HOLE), 0);
    assert_eq!(count(&text, COND_ARM), 2);
    assert!(tokens(&text).contains(&UNDERSCORE));
}

#[test]
fn tuple_index_is_e0024() {
    assert_eq!(codes_of(&in_fn("  pair.0")), ["E0024"]);
    assert_eq!(codes_of(&in_fn("  pair.0.1")), ["E0024"]);
    assert_eq!(count(&in_fn("  pair.0"), FIELD_EXPR), 1);
}

#[test]
fn empty_tuple_is_e0030() {
    assert_eq!(codes_of(&in_fn("  ()")), ["E0030"]);
    assert_eq!(count(&in_fn("  ()"), TUPLE_EXPR), 1);
    // `(e,)` and `()` types and patterns are left to validation.
    assert_ok(&in_fn("  (e,)"));
    assert_ok("type T = ()\n");
    assert_ok(&in_fn("  let () = x"));
}

#[test]
fn records_and_case_subjects() {
    let text = in_fn("  Point { x: 1 }\n  m::Point { ..p, x }");
    assert_ok(&text);
    assert_eq!(count(&text, RECORD_EXPR), 2);
    // In a subject, `Name {` opens the arms.
    let text = in_fn("  case Red {\n    _ => 1\n  }");
    assert_ok(&text);
    assert_eq!(count(&text, RECORD_EXPR), 0);
    assert_eq!(count(&text, CASE_BODY), 1);
    // Parentheses lift the restriction.
    let text = in_fn("  case (P { x: 1 }), f(Q { y }) {\n    _ => 1\n  }");
    assert_ok(&text);
    assert_eq!(count(&text, RECORD_EXPR), 2);
    // A lower-case name followed by `{` is never a record.
    let text = in_fn("  case x {\n    _ => 1\n  }");
    assert_eq!(count(&text, RECORD_EXPR), 0);
    // A record or a block as a subject must be parenthesized (§5.9.6). The
    // restriction reaches operands, calls and field accesses outside
    // brackets, as in tree-sitter-cheby.
    for subject in [
        "P { x: 1 }",
        "a + P { x: 1 }",
        "-P { x: 1 }",
        "P { x: 1 }.x",
        "{ 1 }",
    ] {
        let text = in_fn(&format!("  case {subject} {{\n    _ => 1\n  }}"));
        let codes = codes_of(&text);
        assert!(!codes.is_empty(), "{subject}");
        assert!(codes.iter().all(|c| c == "E0013"), "{subject}: {codes:?}");
    }
    // No subject may start with `{`, not only the first (A.5).
    let text = in_fn("  case a, { 1 } {\n    _, _ => 1\n  }");
    assert_eq!(codes_of(&text), ["E0013"]);
    assert_eq!(count(&text, CASE_BODY), 1);
    // A block subject may span lines; the `{` after its `}` opens the arms.
    let text = in_fn("  case a, {\n    1\n  } {\n    _, _ => 1\n  }");
    assert_eq!(codes_of(&text), ["E0013"]);
    assert_eq!(count(&text, CASE_BODY), 1);
    // A trailing comma after the last subject: the `{` opens the arms, and
    // the one error says to remove the comma (A.5 `subjects`).
    for text in [
        in_fn("  case a, b, {\n    _, _ => 1\n  }"),
        in_fn("  case a, {\n    _ => 1\n  }"),
        in_fn("  let x = case a, b, { _, _ => 1 }\n  x"),
    ] {
        assert_eq!(codes_of(&text), ["E0013"], "{text}");
        assert_eq!(count(&text, CASE_BODY), 1, "{text}");
        // Only the function body is a block.
        assert_eq!(count(&text, BLOCK), 1, "{text}");
        let errors = errors_of(&text);
        assert!(errors.contains("remove the trailing comma"), "{errors}");
    }
    // A block later in a subject is allowed.
    let text = in_fn("  case a, b + { 1 }, ({ 2 }) {\n    _, _, _ => 1\n  }");
    assert_ok(&text);
}

#[test]
fn local_functions_and_closures() {
    let text = in_fn("  fn g(x) { x }\n  let h = fn(x) { x }\n  fn(y) { y }(1)");
    assert_ok(&text);
    assert_eq!(count(&text, LOCAL_FN), 1);
    assert_eq!(count(&text, SyntaxKind::CLOSURE), 2);
    assert_eq!(codes_of(&in_fn("  let f = fn(x)")), ["E0013"]);
}

#[test]
fn paths() {
    for text in [
        "  x",
        "  Some",
        "  io::println",
        "  board::Won",
        "  Show::show",
        "  shape::Shape::area",
        "  channel::new::<Int>",
        "  Box::<Int>",
    ] {
        assert_ok(&in_fn(text));
    }
    assert_eq!(codes_of(&in_fn("  a::b::c")), ["E0013"]);
}

#[test]
fn chained_comparisons_parse() {
    // Rejected by validation (E0029), not by the parser.
    let text = in_fn("  a < b < c");
    assert_ok(&text);
    assert_eq!(count(&text, BIN_EXPR), 2);
}

#[test]
fn left_arrow_is_one_token() {
    // `a<-1` is `a <- 1` by longest match (§2.6), unlike tree-sitter's
    // context-aware lexer.
    assert_ne!(codes_of(&in_fn("  let x = a<-1")).len(), 0);
}

#[test]
fn interpolation_errors_are_e0012() {
    for text in [
        "\"{a + b}\"",
        "\"{}\"",
        "\"{x.0}\"",
        "\"{x\"",
        "\"{X}\"",
        "\"{x:?y}\"",
    ] {
        assert_eq!(codes_of(&in_fn(&format!("  {text}"))), ["E0012"], "{text}");
    }
    assert_ok(&in_fn("  \"{a} {a.b.c} {d:?} \\{e}\""));
}

#[test]
fn line_break_in_interpolation_names_the_line_break() {
    let errors = errors_of(&in_fn("  \"{x\n}\""));
    assert!(
        errors.contains("line break inside a string interpolation"),
        "{errors}"
    );
    assert!(!errors.contains("never closed"), "{errors}");
}

#[test]
fn unterminated_string_does_not_cascade() {
    // After a missing `"`, the rest of the file is string text; its `{`
    // at line ends must not each give an E0012.
    let later = "fn g(a: Int) -> Int {\n  case a {\n    _ => a\n  }\n}\n".repeat(3);
    let text = format!("fn main() {{\n  f(\"hello)\n}}\n\n{later}");
    assert_eq!(codes_of(&text), ["E0010"]);
    // An interpolation error on the string's first line is still reported.
    let text = format!("fn main() {{\n  f(\"hello {{a + b}}\n}}\n\n{later}");
    assert_eq!(codes_of(&text), ["E0010", "E0012"]);
}

// ---- patterns --------------------------------------------------------------

#[test]
fn pattern_shapes() {
    let text = in_fn("  let (a) = x\n  case x {\n    A | B as c => 1\n    -1 => 2\n  }");
    assert_ok(&text);
    assert_eq!(count(&text, PAREN_PAT), 1);
    assert_eq!(count(&text, OR_PAT), 1);
    assert_eq!(count(&text, AS_PAT), 1);
    assert_eq!(count(&text, LITERAL_PAT), 1);
    // `as` binds looser than `|`.
    assert_eq!(children_of(&text, AS_PAT).first().copied(), Some(OR_PAT));
    assert_eq!(count(&in_fn("  let m::A(x) = y"), CTOR_PAT), 1);
}

#[test]
fn named_rest_in_field_patterns_is_e0032() {
    assert_eq!(codes_of(&in_fn("  let P { x, ..r } = p")), ["E0032"]);
    assert_ok(&in_fn("  let [x, ..r] = p"));
}

// ---- recovery (A8) ---------------------------------------------------------

#[test]
fn partial_trees_keep_the_good_parts() {
    let text = "fn a() {\n  let = 1\n  let ok = 2\n}\n%%%\nfn b() {\n  f(1,\n}\nfn c() {}\n";
    let (_, tree, errors) = parse_text("test.cheby", text);
    assert!(errors.len() >= 2, "{errors:?}");
    let fns: Vec<_> = tree
        .root()
        .children()
        .filter(|n| n.kind() == FN_DECL)
        .collect();
    assert_eq!(fns.len(), 3);
    assert_eq!(count(text, LET_STMT), 2);
}

#[test]
fn several_errors_in_one_file() {
    let text =
        "fn a(x: Int y: Int) {}\nfn b() { pair.0 }\nfn c() { f(_ + 1) }\nconst d: Int = ()\n";
    assert_eq!(codes_of(text), ["E0013", "E0024", "E0025", "E0030"]);
}

#[test]
fn one_error_per_token() {
    // The missing `)` and the bad token are the same position.
    let errors = codes_of(&in_fn("  f(a b"));
    assert!(errors.len() <= 2, "{errors:?}");
}

#[test]
fn missing_closing_brace_stops_at_the_next_item() {
    let text = "fn a() {\n  let x = 1\n\ntype T { x: Int }\n";
    assert_eq!(codes_of(text), ["E0015"]);
    assert_eq!(count(text, TYPE_DECL), 1);
    assert_eq!(count(text, BLOCK), 1);
}

/// The kinds of the top-level nodes, without `ERROR` nodes.
fn top_level(text: &str) -> Vec<SyntaxKind> {
    let (_, tree, _) = parse_text("test.cheby", text);
    tree.root()
        .children()
        .map(crate::SyntaxNode::kind)
        .filter(|&kind| kind != ERROR)
        .collect()
}

#[test]
fn unclosed_case_or_interface_body_keeps_later_functions() {
    let case = "fn main(x: Int) -> Int {\n  case x {\n    1 => 2\n    _ => 3\n\nfn helper(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn other() -> Int {\n  1\n}\n";
    assert_eq!(codes_of(case), ["E0015"]);
    assert_eq!(top_level(case), [FN_DECL, FN_DECL, FN_DECL]);
    let interface = "interface Show {\n  fn show(Self) -> String\n\nfn helper(a: Int, b: Int) -> Int {\n  a + b\n}\n";
    assert_eq!(codes_of(interface), ["E0015"]);
    assert_eq!(top_level(interface), [INTERFACE_DECL, FN_DECL]);
    assert_eq!(count(interface, INTERFACE_FN), 1);
}

#[test]
fn unclosed_block_keeps_later_functions() {
    // The `}` of `main` closes the `case`, so `main` is never closed. A
    // function in column 1 then ends it, and the E0015 points at the
    // `case` brace as the likely culprit.
    let text = "fn main() {\n  case x {\n    1 => a()\n    _ => b()\n}\n\nfn fizzbuzz(n: Int) -> String {\n  \"x\"\n}\n\ntest \"t\" {\n  assert 1 == 1\n}\n";
    assert_eq!(codes_of(text), ["E0015"]);
    assert_eq!(top_level(text), [FN_DECL, FN_DECL, TEST_DECL]);
    assert_eq!(count(text, LOCAL_FN), 0);
    let errors = errors_of(text);
    assert!(errors.contains("2 |   case x {"), "{errors}");
    assert!(
        errors.contains("this `{` may be the one missing its `}`"),
        "{errors}"
    );
    let closure = "fn main() {\n  let f = fn(x) { x + 1\n  f(1)\n}\n\nfn g() -> Int {\n  1\n}\n";
    assert_eq!(codes_of(closure), ["E0015"]);
    assert_eq!(top_level(closure), [FN_DECL, FN_DECL]);
}

#[test]
fn local_functions_in_column_one_stay_local_when_closed() {
    // Indentation is not significant: valid code with a local function in
    // column 1 parses as before.
    let text = "fn main() {\nfn helper() -> Int {\n  1\n}\n  helper()\n}\n";
    assert_ok(text);
    assert_eq!(top_level(text), [FN_DECL]);
    assert_eq!(count(text, LOCAL_FN), 1);
    let interface = "interface Show {\nfn show(Self) -> String\n}\n";
    assert_ok(interface);
    assert_eq!(count(interface, INTERFACE_FN), 1);
}

#[test]
fn stray_closer_ends_the_inner_body() {
    // The `)` closes the argument list, after an E0015 for the closure.
    let text = "fn main() {\n  xs |> list::map(_, fn(x) { x + 1 )\n  helper(1, 2)\n}\n\nfn helper(a: Int, b: Int) -> Int {\n  a + b\n}\n";
    assert_eq!(codes_of(text), ["E0015"]);
    assert_eq!(top_level(text), [FN_DECL, FN_DECL]);
}

#[test]
fn too_deep_construct_keeps_later_items() {
    let text = format!(
        "const x: {}Int{} = []\nfn after() -> Int {{\n  1\n}}\n",
        "List<".repeat(300),
        ">".repeat(300)
    );
    assert_eq!(codes_of(&text), ["E0016"]);
    assert_eq!(top_level(&text), [CONST_DECL, FN_DECL]);
}

#[test]
fn lexer_errors_do_not_cascade() {
    assert_eq!(codes_of(&in_fn("  let a = 1;")), ["E0003"]);
    assert_eq!(codes_of(&in_fn("  let b = $x")), ["E0003"]);
    assert_eq!(codes_of(&in_fn("  /* c */ let c = 1")), ["E0004"]);
    assert_eq!(codes_of("fn f() {\n  g(\"open)\n}\n"), ["E0010"]);
}

/// The helps of the grammar errors of `text`, in order.
fn helps_of(text: &str) -> Vec<String> {
    let (_, _, errors) = parse_text("test.cheby", text);
    errors
        .iter()
        .flat_map(|d| d.helps.iter().map(|h| h.message.clone()))
        .collect()
}

/// A reserved word that causes a grammar error explains it, at the word
/// or right after it on the same line (§2.4, D-128).
#[test]
fn reserved_words_explain_grammar_errors() {
    assert_eq!(
        helps_of(&in_fn("  if n > 0 { 1 } else { 0 }")),
        ["Cheby has no `if`; use `case`"]
    );
    assert_eq!(
        helps_of(&in_fn("  return n")),
        ["Cheby has no `return`; a function returns the value of its body"]
    );
    assert_eq!(
        helps_of(&in_fn("  let mut x = 1")),
        ["values are immutable; bind a new name"]
    );
    assert_eq!(
        helps_of("struct Point {\n  x: Int,\n}\n"),
        ["Cheby has no `struct`; use `type`"]
    );
    // A word on the line before does not explain an error.
    assert_eq!(helps_of(&in_fn("  go\n  )")), Vec::<String>::new());
    // Nor does a word that is not reserved.
    assert_eq!(helps_of(&in_fn("  foo n")), Vec::<String>::new());
    // A reserved word that parses as a name is not a grammar error: name
    // resolution reports it where it is bound (E0007).
    assert_eq!(codes_of(&in_fn("  let go = 1\n  go")), Vec::<String>::new());
}

#[test]
fn generic_interface_is_one_error() {
    // D-165: the type parameters are reported once and the body still
    // parses.
    let text = "interface Into<T> {\n  fn into(Self) -> T\n}\n\nfn main() {}\n";
    assert_eq!(codes_of(text), ["E0013"]);
    assert_eq!(top_level(text), [INTERFACE_DECL, FN_DECL]);
    assert_eq!(count(text, INTERFACE_FN), 1);
}
