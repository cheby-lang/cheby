//! Unit tests of the formatter on small sources.

use super::{WIDTH, format_source};

#[track_caller]
fn fmt(text: &str) -> String {
    fmt_at(text, WIDTH)
}

#[track_caller]
fn fmt_at(text: &str, width: usize) -> String {
    let once = format_source(text, width).unwrap();
    let twice = format_source(&once, width).unwrap();
    assert_eq!(once, twice, "not idempotent");
    once
}

#[test]
fn fmt_rejects_grammar_errors() {
    let err = format_source("fn f( {\n", 100).unwrap_err();
    assert_ne!(err.errors(), []);
    assert!(err.to_string().contains("syntax error"));
}

#[test]
fn fmt_empty_file() {
    assert_eq!(fmt(""), "");
    assert_eq!(fmt("\n\n  \n"), "");
}

#[test]
fn fmt_trivia_one_final_newline_and_lf() {
    assert_eq!(fmt("fn main() {\n}"), "fn main() {}\n");
    assert_eq!(fmt("fn main() {\n  x\n}\n\n\n"), "fn main() {\n  x\n}\n");
    assert_eq!(fmt("fn main() {\r\n  x\r\n}\r\n"), "fn main() {\n  x\n}\n");
}

#[test]
fn fmt_trivia_blank_lines_collapse() {
    let src = "\n\nimport std::io\n\n\n\nfn a() {}\n\n\n\nfn b() {\n  x\n\n\n\n  y\n}\n";
    assert_eq!(
        fmt(src),
        "import std::io\n\nfn a() {}\n\nfn b() {\n  x\n\n  y\n}\n"
    );
}

#[test]
fn fmt_trivia_no_trailing_whitespace() {
    assert_eq!(
        fmt("fn a() {   \n  x  \n}   // c   \n"),
        "fn a() {\n  x\n} // c\n"
    );
}

#[test]
fn fmt_trivia_comments_stay() {
    let src = "//! Module.\n\n// Before.\nimport std::io // why\n\n/// Doc.\n/// More.\n@external(js, \"a\")\nfn f() -> Int // trailing\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn fmt_trivia_comment_in_header_is_laid_out() {
    // Wave 1 kept this item verbatim; wave 3 lays it out the same way.
    let src = "const k: Int = // the answer\n  42\n";
    assert_eq!(fmt(src), src);
    assert_eq!(fmt("const   k: Int = // the answer\n      42\n"), src);
}

#[test]
fn fmt_trivia_strings_never_change() {
    let src = "fn f() {\n  let s = \"a  \n    b  \n\n\n  c\"\n  s\n}\n";
    assert_eq!(fmt(src), src);
    let raw = "test \"a\" {\n  assert r#\"x \n y\"# == \"\"\n}\n";
    assert_eq!(fmt(raw), raw);
}

#[test]
fn fmt_trivia_long_string_not_split() {
    // A string that does not fit moves to the next line, whole.
    let long = "x".repeat(150);
    let src = format!("const s: String = \"{long}\"\n");
    assert_eq!(
        fmt_at(&src, 80),
        format!("const s: String =\n  \"{long}\"\n")
    );
    let src = format!("fn f() {{\n  g(\"{long}\")\n}}\n");
    assert_eq!(
        fmt_at(&src, 80),
        format!("fn f() {{\n  g(\n    \"{long}\",\n  )\n}}\n")
    );
}

#[test]
fn fmt_trivia_list_comments() {
    let src = "type P {\n  // first\n  x: Int, // after x\n\n  /// Doc of y.\n  y: Int // after y\n  // dangling\n}\n";
    assert_eq!(
        fmt(src),
        "type P {\n  // first\n  x: Int, // after x\n\n  /// Doc of y.\n  y: Int, // after y\n  // dangling\n}\n"
    );
}

#[test]
fn fmt_trivia_comment_only_file() {
    assert_eq!(fmt("// a\n\n\n// b   \n"), "// a\n\n// b\n");
    assert_eq!(fmt("//! Doc."), "//! Doc.\n");
}

#[test]
fn fmt_trivia_blank_line_after_leading_comment_kept() {
    let src = "type T {\n  // Section.\n\n  a: Int,\n  b: Int,\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn fmt_trivia_odd_comment_positions_are_laid_out() {
    // A comment between an element and its comma: the comma goes before
    // it (wave 3; wave 1 kept the item verbatim).
    let src = "fn f(\n  a: Int // a\n  , b: Int,\n) {}\n";
    assert_eq!(fmt(src), "fn f(\n  a: Int, // a\n  b: Int,\n) {}\n");
    // A comment inside an element keeps its place, the rest one level
    // deeper.
    let src = "fn f(\n  a: // the a\n    Int,\n  b: Int,\n) {}\n";
    assert_eq!(fmt(src), src);
    // A comment on the line of `{`.
    let src = "interface I { // note\n  fn f(Self)\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn fmt_trivia_crlf_inside_strings_and_comments() {
    assert_eq!(
        fmt("// c\r\nconst s: String = \"a\r\nb\"\r\n"),
        "// c\nconst s: String = \"a\nb\"\n"
    );
}

#[test]
fn fmt_items_attributes_break_like_lists() {
    let path = "x".repeat(70);
    let src = format!("@external(js, \"{path}\", \"name\")\nfn f() -> Int\n");
    assert_eq!(
        fmt_at(&src, 80),
        format!("@external(\n  js,\n  \"{path}\",\n  \"name\",\n)\nfn f() -> Int\n")
    );
    assert_eq!(fmt("@!target( native )\n"), "@!target(native)\n");
}

#[test]
fn fmt_imports_sorted_within_blocks() {
    let src = "import std::list\nimport std::io\nimport app::b\n\nimport std::string\n// keep\nimport std::int\nimport std::bytes\n";
    assert_eq!(
        fmt(src),
        "import app::b\nimport std::io\nimport std::list\n\nimport std::string\n// keep\nimport std::bytes\nimport std::int\n"
    );
}

#[test]
fn fmt_imports_layout() {
    assert_eq!(
        fmt("import   std::map::{ Map ,Set as S}\nimport std::io  as  sio\n"),
        "import std::io as sio\nimport std::map::{Map, Set as S}\n"
    );
}

#[test]
fn fmt_items_fn_header_joins_when_it_fits() {
    let src = "pub fn f<T: Show>(\n  a: Int,\n  b: List<T>,\n) -> Int {\n  a\n}\n";
    assert_eq!(
        fmt(src),
        "pub fn f<T: Show>(a: Int, b: List<T>) -> Int {\n  a\n}\n"
    );
}

#[test]
fn fmt_items_fn_header_breaks_when_too_wide() {
    let src = "fn function_with_a_long_name(first_argument: Int, second_argument: String) -> Result<Int, String> {\n  todo\n}\n";
    assert_eq!(
        fmt_at(src, 80),
        "fn function_with_a_long_name(\n  first_argument: Int,\n  second_argument: String,\n) -> Result<Int, String> {\n  todo\n}\n"
    );
}

#[test]
fn fmt_items_type_bodies() {
    // D-1274: a type's own body is broken however
    // short; a record-shaped variant's fields and a payload that fits stay
    // flat.
    assert_eq!(
        fmt("pub   type Color {\n  Red,\n  Green(Int,),\n  Blue { x: Int,  y: Int }\n}\n"),
        "pub type Color {\n  Red,\n  Green(Int),\n  Blue { x: Int, y: Int },\n}\n"
    );
    assert_eq!(
        fmt("type Player { X, O }\n"),
        "type Player {\n  X,\n  O,\n}\n"
    );
    assert_eq!(
        fmt("pub type Duration { seconds: Int, nanoseconds: Int }\n"),
        "pub type Duration {\n  seconds: Int,\n  nanoseconds: Int,\n}\n"
    );
    assert_eq!(
        fmt("type Tree<K, V> { Leaf, Bin(Int, K, V, Tree<K, V>, Tree<K, V>) }\n"),
        "type Tree<K, V> {\n  Leaf,\n  Bin(Int, K, V, Tree<K, V>, Tree<K, V>),\n}\n"
    );
    assert_eq!(fmt("type Unit { Unit }\n"), "type Unit {\n  Unit,\n}\n");
    let src = "type Shape {\n  Circle { radius: Float },\n  Rectangle { width_of_the_shape: Float, height_of_the_shape: Float },\n}\n";
    assert_eq!(
        fmt_at(src, 60),
        "type Shape {\n  Circle { radius: Float },\n  Rectangle {\n    width_of_the_shape: Float,\n    height_of_the_shape: Float,\n  },\n}\n"
    );
}

#[test]
fn fmt_items_interface_and_const() {
    assert_eq!(
        fmt("pub interface Show :Eq { fn show(Self)->String\n\n\n  fn debug(Self) }\n"),
        "pub interface Show: Eq {\n  fn show(Self) -> String\n\n  fn debug(Self)\n}\n"
    );
    assert_eq!(fmt("interface Marker {\n}\n"), "interface Marker {}\n");
    assert_eq!(fmt("const   k :Int=1 + 2\n"), "const k: Int = 1 + 2\n");
    // The value is laid out (wave 2): the line break after `=` goes.
    assert_eq!(
        fmt("const xs: List<Int> =\n  [\n    1,\n  ]\n"),
        "const xs: List<Int> = [1]\n"
    );
}

#[test]
fn fmt_items_bodies_reindented_relative() {
    // Bodies are laid out at the item's indentation; a verbatim node keeps
    // its lines relative to its first one (a comment between `-` and its
    // number is an error the formatter keeps, D-470).
    let src = "  fn f(\n      a: Int) {\n      a\n  }\n";
    assert_eq!(fmt(src), "fn f(a: Int) {\n  a\n}\n");
    let src = "fn f() {\n    g(-  // gap\n        1)\n}\n";
    assert_eq!(
        fmt(src),
        "fn f() {\n  g(\n    -  // gap\n        1,\n  )\n}\n"
    );
}

#[test]
fn fmt_items_test_and_alias() {
    assert_eq!(
        fmt("test   \"adds\"   {\n  assert 1 + 1 == 2\n}\ntype   Pair<A,B> =   (A, B)\n"),
        "test \"adds\" {\n  assert 1 + 1 == 2\n}\ntype Pair<A, B> = (A, B)\n"
    );
}
