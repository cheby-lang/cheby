//! Renderer tests. Each pins the exact text, because UI-test goldens depend
//! on it (D-418).

use super::{render, render_all};
use crate::codes;
use crate::diagnostic::Diagnostic;
use crate::source::SourceMap;
use crate::text::{FileId, Span, TextRange};

fn one_file(text: &str) -> (SourceMap, FileId) {
    let mut map = SourceMap::new();
    let id = map.add_file("src/main.cheby", text).unwrap();
    (map, id)
}

/// The span of the `n`-th (0-based) occurrence of `needle`.
fn find(file: FileId, text: &str, needle: &str, n: usize) -> Span {
    let start = text.match_indices(needle).nth(n).unwrap().0;
    let start = u32::try_from(start).unwrap();
    let len = u32::try_from(needle.len()).unwrap();
    Span::new(file, TextRange::at(start, len))
}

fn at(file: FileId, start: usize, end: usize) -> Span {
    Span::new(
        file,
        TextRange::new(u32::try_from(start).unwrap(), u32::try_from(end).unwrap()),
    )
}

#[test]
fn spec_example() {
    let text = "fn process(s: String) -> Int {\n  let x = 1\n  let trimmed = string::trim(s)\n  let trimmed = string::lowercase(trimmed)\n  0\n}\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::INVALID_INTERPOLATION, "`trimmed` is already bound")
        .with_secondary(find(file, text, "trimmed", 0), "first bound here")
        .with_primary(find(file, text, "trimmed", 1), "bound again here")
        .with_help("use a new name, such as `trimmed_2`");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0012]: `trimmed` is already bound
 --> src/main.cheby:4:7
  |
3 |   let trimmed = string::trim(s)
  |       ------- first bound here
4 |   let trimmed = string::lowercase(trimmed)
  |       ^^^^^^^ bound again here
  |
  = help: use a new name, such as `trimmed_2`
"
    );
}

#[test]
fn warning_without_children() {
    let text = "fn f() -> ((Int, Int), Int) {\n  todo\n}\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::TUPLE_SHAPE, "tuple with a tuple element")
        .with_primary(find(file, text, "((Int, Int), Int)", 0), "");
    assert_eq!(
        render(&diag, &map),
        "\
warning[W0001]: tuple with a tuple element
 --> src/main.cheby:1:11
  |
1 | fn f() -> ((Int, Int), Int) {
  |           ^^^^^^^^^^^^^^^^^
"
    );
}

#[test]
fn several_labels_on_one_line() {
    let text = "let x = f(a, b, c)\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::MULTIPLE_HOLES, "three labels")
        .with_secondary(find(file, text, "a", 0), "first")
        .with_secondary(find(file, text, "b", 0), "second")
        .with_primary(find(file, text, "c", 0), "third");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0026]: three labels
 --> src/main.cheby:1:17
  |
1 | let x = f(a, b, c)
  |           -  -  ^ third
  |           |  |
  |           |  second
  |           |
  |           first
"
    );
}

#[test]
fn overlapping_labels_prefer_primary_markers() {
    let text = "a + bcd\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "overlap")
        .with_secondary(at(file, 0, 7), "whole")
        .with_primary(at(file, 4, 5), "");
    // The secondary label ends last, so it is written inline.
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: overlap
 --> src/main.cheby:1:5
  |
1 | a + bcd
  | ----^-- whole
"
    );
}

#[test]
fn label_left_of_a_longer_one_hangs() {
    let text = "foo(bar)\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "nested")
        .with_primary(at(file, 0, 8), "call")
        .with_secondary(at(file, 4, 7), "argument");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: nested
 --> src/main.cheby:1:1
  |
1 | foo(bar)
  | ^^^^^^^^ call
  |     |
  |     argument
"
    );
}

#[test]
fn multi_line_span() {
    let text = "fn f() {\n  let x = foo(\n    a,\n  )\n}\n";
    let (map, file) = one_file(text);
    let start = text.find("foo(").unwrap();
    let end = text.find("  )").unwrap() + 3;
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "a call")
        .with_primary(at(file, start, end), "this call");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: a call
 --> src/main.cheby:2:11
  |
2 |     let x = foo(
  |  ___________^
3 | |     a,
4 | |   )
  | |___^ this call
"
    );
}

#[test]
fn long_multi_line_span_is_elided() {
    let text = "{\n  a\n  b\n  c\n  d\n  e\n}\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::UNCLOSED_DELIMITER, "block")
        .with_primary(at(file, 0, text.len() - 1), "here")
        .with_note("a note");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0015]: block
 --> src/main.cheby:1:1
  |
1 |   {
  |  _^
2 | |   a
...
6 | |   e
7 | | }
  | |_^ here
  |
  = note: a note
"
    );
}

#[test]
fn span_ending_with_a_line_break_stays_on_its_line() {
    let text = "ab\ncd\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "line").with_primary(at(file, 0, 3), "");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: line
 --> src/main.cheby:1:1
  |
1 | ab
  | ^^^
"
    );
}

#[test]
fn tabs_display_as_four_spaces() {
    let text = "\tlet\tx = 1\n";
    let (map, file) = one_file(text);
    let diag =
        Diagnostic::new(codes::SYNTAX_ERROR, "tabs").with_primary(find(file, text, "x", 0), "x");
    // The location counts the tab as one column (D-421).
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: tabs
 --> src/main.cheby:1:6
  |
1 |     let    x = 1
  |            ^ x
"
    );
}

#[test]
fn unicode_columns_and_widths() {
    let text = "let s = \"日本\" + é\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "wide")
        .with_secondary(find(file, text, "日本", 0), "wide")
        .with_primary(find(file, text, "é", 0), "narrow");
    // `é` is the 16th code point; `日本` is four columns wide.
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: wide
 --> src/main.cheby:1:16
  |
1 | let s = \"日本\" + é
  |          ----    ^ narrow
  |          |
  |          wide
"
    );
}

#[test]
fn control_characters_are_shown_as_pictures() {
    let text = "a\rb\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::LONE_CARRIAGE_RETURN, "lone carriage return")
        .with_primary(at(file, 1, 2), "");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0002]: lone carriage return
 --> src/main.cheby:1:2
  |
1 | a\u{240d}b
  |  ^
"
    );
}

#[test]
fn crlf_is_not_displayed() {
    let text = "ab\r\ncd\r\n";
    let (map, file) = one_file(text);
    let diag =
        Diagnostic::new(codes::SYNTAX_ERROR, "crlf").with_primary(find(file, text, "cd", 0), "");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: crlf
 --> src/main.cheby:2:1
  |
2 | cd
  | ^^
"
    );
}

#[test]
fn end_of_file_spans() {
    // After a final line break, the end of the file is on the empty last
    // line.
    let text = "fn f() {\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::UNCLOSED_DELIMITER, "eof")
        .with_primary(at(file, text.len(), text.len()), "expected `}`")
        .with_secondary(find(file, text, "{", 0), "unclosed");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0015]: eof
 --> src/main.cheby:2:1
  |
1 | fn f() {
  |        - unclosed
2 |
  | ^ expected `}`
"
    );

    // Without one, it is after the last character.
    let text = "fn f() {";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::UNCLOSED_DELIMITER, "eof")
        .with_primary(at(file, text.len(), 99), "here");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0015]: eof
 --> src/main.cheby:1:9
  |
1 | fn f() {
  |         ^ here
"
    );
}

#[test]
fn gaps_of_one_line_are_shown_and_longer_gaps_elided() {
    let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "gaps")
        .with_primary(find(file, text, "a", 0), "one")
        .with_secondary(find(file, text, "c", 0), "two")
        .with_secondary(find(file, text, "k", 0), "three");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: gaps
  --> src/main.cheby:1:1
   |
1  | a
   | ^ one
2  | b
3  | c
   | - two
...
11 | k
   | - three
"
    );
}

#[test]
fn labels_in_other_files_get_their_own_section() {
    let mut map = SourceMap::new();
    let lib = map
        .add_file("src/lib.cheby", "pub fn helper() {\n}\n")
        .unwrap();
    let main = map
        .add_file("src/main.cheby", "fn main() {\n  helper(1)\n}\n")
        .unwrap();
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "two files")
        .with_secondary(at(lib, 7, 13), "declared here")
        .with_primary(at(main, 14, 23), "called here");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: two files
 --> src/main.cheby:2:3
  |
2 |   helper(1)
  |   ^^^^^^^^^ called here
  |
 ::: src/lib.cheby:1:8
  |
1 | pub fn helper() {
  |        ------ declared here
"
    );
}

#[test]
fn diagnostics_without_labels() {
    let map = SourceMap::new();
    let diag = Diagnostic::new(codes::INVALID_MANIFEST, "no package name")
        .with_note("first line\nsecond line")
        .with_help("add `name = \"my_app\"`");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0036]: no package name
  = note: first line
          second line
  = help: add `name = \"my_app\"`
"
    );
}

#[test]
fn notes_come_before_helps() {
    let text = "x\n";
    let (map, file) = one_file(text);
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "order")
        .with_help("help one")
        .with_primary(at(file, 0, 1), "")
        .with_note("note one")
        .with_suggestion("help two", at(file, 0, 1), "y")
        .with_note("note two");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: order
 --> src/main.cheby:1:1
  |
1 | x
  | ^
  |
  = note: note one
  = note: note two
  = help: help one
  = help: help two
"
    );
}

#[test]
fn render_all_separates_with_blank_lines() {
    let text = "x\n";
    let (map, file) = one_file(text);
    let a = Diagnostic::new(codes::SYNTAX_ERROR, "a").with_primary(at(file, 0, 1), "");
    let b = Diagnostic::new(codes::TUPLE_SHAPE, "b");
    assert_eq!(
        render_all(&[a, b], &map),
        "\
error[E0013]: a
 --> src/main.cheby:1:1
  |
1 | x
  | ^

warning[W0001]: b

"
    );
}

#[test]
fn sorting_is_by_path_then_position() {
    let mut map = SourceMap::new();
    let b = map.add_file("src/b.cheby", "xyz\n").unwrap();
    let a = map.add_file("src/a.cheby", "xyz\n").unwrap();
    let mut diags = vec![
        Diagnostic::new(codes::SYNTAX_ERROR, "b2").with_primary(at(b, 2, 3), ""),
        Diagnostic::new(codes::SYNTAX_ERROR, "a1").with_primary(at(a, 1, 2), ""),
        Diagnostic::new(codes::SYNTAX_ERROR, "b0").with_primary(at(b, 0, 1), ""),
        Diagnostic::new(codes::INVALID_MANIFEST, "none"),
    ];
    map.sort(&mut diags);
    let order: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(order, ["none", "a1", "b0", "b2"]);
}

#[test]
fn nested_multi_line_spans_use_two_rails() {
    let text = "a(\n  b(\n    c\n  )\n)\n";
    let (map, file) = one_file(text);
    let outer = at(file, 0, text.len() - 1);
    let inner_start = text.find("b(").unwrap();
    let inner_end = text.find("  )").unwrap() + 3;
    let diag = Diagnostic::new(codes::SYNTAX_ERROR, "nested")
        .with_primary(outer, "outer")
        .with_secondary(at(file, inner_start, inner_end), "inner");
    assert_eq!(
        render(&diag, &map),
        "\
error[E0013]: nested
 --> src/main.cheby:1:1
  |
1 |     a(
  |  ___^
2 | |     b(
  | |  ___-
3 | | |     c
4 | | |   )
  | | |___- inner
5 | |   )
  | |___^ outer
"
    );
}
