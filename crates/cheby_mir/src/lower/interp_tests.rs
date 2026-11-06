//! Tests of interpolation into a leading `String` (D-1055), read from
//! MIR dumps: the leading value is the buffer, owned as `+`'s left
//! operand is, so counting moves it at its last use and duplicates it
//! otherwise (D-577, D-849); the parameter it comes from is no longer
//! borrowed (D-806); every other interpolation lowers as before; and a
//! split literal (D-780) carries the buffer into its continuations.

use crate::test_support::{Lowered, lower_source, lower_source_split, lower_source_uncounted};
use crate::validate::{Stage, validate};

/// Interpolations that start from a `String` value.
const LEADING: &str = r#"//! Interpolations that start from a `String` value.

import std::io

pub type Person {
  name: String,
  age: Int,
}

pub type Point {
  x: Int,
  y: Int,
}

pub fn show(p: Point) -> String {
  "({p.x}, {p.y})"
}

fn moved(acc: String, i: Int) -> String {
  "{acc}x{i},"
}

fn kept(acc: String, i: Int) -> String {
  let s = "{acc}x{i},"
  s + acc
}

fn twice(s: String) -> String {
  "{s}-{s}"
}

fn twice_local(n: Int) -> String {
  let s = "{n}"
  "{s}-{s}"
}

fn named(name: String, n: Int) -> String {
  "{name}: {n}, {name}"
}

fn field_first(p: Person) -> String {
  "{p.name} is {p.age}"
}

fn ordered(p: Person, q: Person, pt: Point) -> String {
  "{p.name}{q.name}{pt}{p.age}"
}

fn literal_first(name: String) -> String {
  let hello = "hello"
  "{hello}, {name}"
}

fn main() {
  let p = Person { name: "Ada", age: 36 }
  io::println(moved("a", 1))
  io::println(kept("b", 2))
  io::println(twice("c"))
  io::println(twice_local(3))
  io::println(named("d", 4))
  io::println(field_first(p))
  io::println(ordered(p, p, Point { x: 1, y: 2 }))
  io::println(literal_first("e"))
}
"#;

/// Interpolations that do not start from a `String` value, as captured
/// at `d13dbbf` (before D-1055's change) with the dumps below.
const OTHERS: &str = r#"//! Interpolations that start from a `String` value, and others.

import std::io

pub type Person {
  name: String,
  age: Int,
}

fn text_first(acc: String) -> String {
  "x{acc}"
}

fn int_first(n: Int, acc: String) -> String {
  "{n}{acc}"
}

fn debug_first(acc: String) -> String {
  "{acc:?}x"
}

fn single(acc: String) -> String {
  "{acc}"
}

fn empty() -> String {
  ""
}

fn main() {
  io::println(text_first("a"))
  io::println(int_first(1, "b"))
  io::println(debug_first("c"))
  io::println(single("d"))
  io::println(empty())
}
"#;

/// `OTHERS`' functions as `d13dbbf` lowered them (`lower_source`).
const OTHERS_AT_D13DBBF: [(&str, &str); 5] = [
    (
        "app::text_first",
        "fn app::text_first(borrow _0: value) -> value check str#0 {
  entry unwind bb1
  str#0 = \"src/main.cheby:10:4\"
  str#1 = \"x\"
bb0:
  _1: value = call_runtime str_with_capacity(borrow const int 17) ; unwind bb1
  _2: value = call_runtime str_append(_1, borrow const str#1) ; unwind bb1
  _3: value = call_runtime str_append(_2, borrow _0) ; unwind bb1
  return _3
bb1:
  resume
}
",
    ),
    (
        "app::int_first",
        "fn app::int_first(_0: int, borrow _1: value) -> value check str#0 {
  entry unwind bb1
  str#0 = \"src/main.cheby:14:4\"
bb0:
  _2: value = call_runtime str_with_capacity(borrow const int 32) ; unwind bb1
  _3: value = call std::int::show(_0) ; unwind bb2
  _4: value = call_runtime str_append(_2, borrow _3) ; unwind bb3
  drop _3
  _5: value = call_runtime str_append(_4, borrow _1) ; unwind bb1
  return _5
bb1:
  resume
bb2:
  drop _2
  goto bb1()
bb3:
  drop _3
  goto bb1()
}
",
    ),
    (
        "app::debug_first",
        "fn app::debug_first(borrow _0: value) -> value check str#0 {
  entry unwind bb1
  str#0 = \"src/main.cheby:18:4\"
  str#1 = \"x\"
bb0:
  _1: value = call_runtime str_with_capacity(borrow const int 17) ; unwind bb1
  _2: value = call_runtime debug(borrow _0) ; unwind bb2
  _3: value = call_runtime str_append(_1, borrow _2) ; unwind bb3
  drop _2
  _4: value = call_runtime str_append(_3, borrow const str#1) ; unwind bb1
  return _4
bb1:
  resume
bb2:
  drop _1
  goto bb1()
bb3:
  drop _2
  goto bb1()
}
",
    ),
    (
        "app::single",
        "fn app::single(_0: value) -> value check str#0 {
  entry unwind bb2
  str#0 = \"src/main.cheby:22:4\"
bb0:
  return _0
bb1:
  resume
bb2:
  drop _0
  goto bb1()
}
",
    ),
    (
        "app::empty",
        "fn app::empty() -> value check str#0 {
  entry unwind bb1
  str#0 = \"src/main.cheby:26:4\"
  str#1 = \"\"
bb0:
  return const str#1
bb1:
  resume
}
",
    ),
];

/// The lines of `f`'s dump that start with `prefix` after their indent.
fn lines<'a>(f: &'a str, prefix: &str) -> Vec<&'a str> {
    f.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with(prefix))
        .collect()
}

/// The statements of `f`'s dump, without the indent.
fn stmts(f: &str) -> Vec<&str> {
    f.lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("  str#") && !l.starts_with("  entry"))
        .map(str::trim_start)
        .collect()
}

/// The position in `f` of the first line containing `needle`.
fn at(f: &str, needle: &str) -> usize {
    f.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{f}"))
}

/// Every unit of `lowered` is valid at `stage`.
fn assert_valid(lowered: &Lowered, stage: Stage) {
    for unit in &lowered.units {
        if let Err(errors) = validate(unit, stage) {
            panic!("{}:\n{}", unit.path, errors.join("\n"));
        }
    }
}

/// `"{acc}x{i},"`: no fresh buffer; `acc`'s value is the first operand of
/// the first `str_append`, moved at its last use (no `dup`, and the
/// parameter is consumed, so no longer borrowed, D-806) and duplicated
/// when `acc` is used again after the literal.
#[test]
fn interp_leading_string_appends_to_the_value() {
    let lowered = lower_source(&[("", LEADING)]);
    assert_valid(&lowered, Stage::Unwound);

    let moved = lowered.function("app", "app::moved");
    assert!(
        moved.starts_with("fn app::moved(_0: value, _1: int)"),
        "{moved}"
    );
    assert!(!moved.contains("str_with_capacity"), "{moved}");
    let body = stmts(&moved);
    assert!(
        body.first()
            .is_some_and(|s| s.contains("= call_runtime str_append(_0, borrow const str#1)")),
        "{moved}"
    );
    assert!(lines(&moved, "dup ").is_empty(), "{moved}");
    assert_eq!(
        lines(&moved, "_")
            .iter()
            .filter(|l| l.contains("str_append"))
            .count(),
        3
    );

    let kept = lowered.function("app", "app::kept");
    assert!(
        kept.starts_with("fn app::kept(_0: value, _1: int)"),
        "{kept}"
    );
    assert!(!kept.contains("str_with_capacity"), "{kept}");
    let body = stmts(&kept);
    assert_eq!(body.first().copied(), Some("dup _0"), "{kept}");
    assert!(
        body.get(1)
            .is_some_and(|s| s.contains("= call_runtime str_append(_0, borrow const str#1)")),
        "{kept}"
    );
    assert!(
        kept.contains("tail_call std::string::add(_5, _0)"),
        "{kept}"
    );
}

/// A `String` read again later in the same literal is duplicated before
/// it becomes the buffer, so the later part reads the unchanged text:
/// a parameter (`twice`, `named`) and a local (`twice_local`).
#[test]
fn interp_leading_string_reused_in_literal() {
    let lowered = lower_source(&[("", LEADING)]);
    for (key, value) in [
        ("app::twice", "_0"),
        ("app::twice_local", "_1"),
        ("app::named", "_0"),
    ] {
        let f = lowered.function("app", key);
        assert!(!f.contains("str_with_capacity"), "{f}");
        let dup = at(&f, &format!("  dup {value}"));
        let buffer = at(&f, &format!("str_append({value}, borrow const str#1)"));
        let again = at(&f, &format!("borrow {value}) ; unwind"));
        assert!(dup < buffer && buffer < again, "{f}");
        assert_eq!(
            lines(&f, &format!("dup {value}")).len(),
            1,
            "one `dup`, for the later part: {f}"
        );
    }
}

/// A field path of type `String` first is the buffer: the field's value,
/// read out of the record, is `str_append`'s first operand.
#[test]
fn interp_leading_string_field_path() {
    let lowered = lower_source(&[("", LEADING)]);
    let f = lowered.function("app", "app::field_first");
    assert!(!f.contains("str_with_capacity"), "{f}");
    assert!(
        f.contains("_1: value = field _0.0 of ctor app::Person"),
        "{f}"
    );
    assert!(
        f.contains("_2: value = call_runtime str_append(_1, borrow const str#1)"),
        "{f}"
    );
}

/// A static text bound by `let` and leading the literal is the buffer as
/// a constant operand; `str_append` copies it (D-849).
#[test]
fn interp_leading_string_static_text() {
    let lowered = lower_source(&[("", LEADING)]);
    let f = lowered.function("app", "app::literal_first");
    assert!(!f.contains("str_with_capacity"), "{f}");
    assert!(
        f.contains("call_runtime str_append(const str#1, borrow const str#2)"),
        "{f}"
    );
}

/// The parts are evaluated in source order, the leading value first, as
/// lowered (before counting). Reads of a field, which cannot be observed,
/// may move earlier (`p.age`'s here); the `show` calls and the appends
/// keep the source order.
#[test]
fn interp_leading_string_source_order() {
    let lowered = lower_source_uncounted(&[("", LEADING)]);
    assert_valid(&lowered, Stage::Lowered);
    let f = lowered.function("app", "app::ordered");
    assert!(!f.contains("str_with_capacity"), "{f}");
    let p_name = at(&f, "= field _0.0 of ctor app::Person");
    let q_name = at(&f, "= field _1.0 of ctor app::Person");
    let first_append = at(&f, "= call_runtime str_append(");
    let point = at(&f, "= call app::show(");
    let int = at(&f, "= call std::int::show(");
    assert!(
        p_name < q_name && q_name < first_append && first_append < point && point < int,
        "{f}"
    );
    let appends = lines(&f, "_");
    let appends: Vec<&&str> = appends
        .iter()
        .filter(|l| l.contains("str_append"))
        .collect();
    assert_eq!(appends.len(), 3, "{f}");
    let buffer = lines(&f, "_")
        .into_iter()
        .find(|l| l.contains("= field _0.0 of ctor app::Person"))
        .and_then(|l| l.split(':').next())
        .unwrap_or_else(|| panic!("{f}"))
        .to_owned();
    assert!(
        appends
            .first()
            .is_some_and(|l| l.contains(&format!("str_append({buffer}, "))),
        "{f}"
    );
}

/// Every other interpolation lowers exactly as at `d13dbbf`: a leading
/// text, a leading value of another type, a leading `{x:?}`, a single
/// part and an empty literal.
#[test]
fn interp_leading_string_others_unchanged() {
    let lowered = lower_source(&[("", OTHERS)]);
    for (key, expected) in OTHERS_AT_D13DBBF {
        assert_eq!(lowered.function("app", key), expected, "{key}");
    }
}

/// A split literal (D-780) whose first part is a `String` carries the
/// buffer into its continuations at every small size, and every lowered
/// body validates.
#[test]
fn interp_leading_string_split() {
    let src = r#"//! A split interpolation that starts from a `String` value.

import std::io

pub type Person {
  name: String,
  age: Int,
}

fn param(acc: String, n: Int) -> String {
  "{acc}: {n}, {n}, {n}, {n}, {acc}"
}

fn field(p: Person, n: Int) -> String {
  "{p.name}: {n}, {n}, {p.age}, {n}, {p.name}"
}

fn main() {
  io::println(param("a", 1))
  io::println(field(Person { name: "b", age: 2 }, 3))
}
"#;
    for size in 1..=8 {
        let lowered = lower_source_split(&[("", src)], size, false);
        assert_valid(&lowered, Stage::Unwound);
        let unit = lowered.unit("app");
        let dump = crate::dump(unit).unwrap();
        assert!(!dump.contains("str_with_capacity"), "size {size}:\n{dump}");
        let mut cut = false;
        for f in &unit.functions {
            let text = crate::dump_function(unit, f).unwrap();
            if !text.starts_with("fn continuation ") {
                continue;
            }
            cut = true;
            // The continuation's first parameter is the buffer: the first
            // `str_append` appends to it.
            let first = lines(&text, "_")
                .into_iter()
                .find(|l| l.contains("str_append("))
                .unwrap_or_else(|| panic!("size {size}:\n{text}"));
            assert!(first.contains("str_append(_0, "), "size {size}:\n{text}");
        }
        assert!(cut, "size {size}: no continuation:\n{dump}");
    }
    // At size 1 the cut comes right after the leading value: the value
    // itself is passed as the buffer.
    let lowered = lower_source_split(&[("", src)], 1, false);
    let field = lowered.function("app", "app::field");
    let call = lines(&field, "_")
        .into_iter()
        .find(|l| l.contains("call <continuation app::field#0>("))
        .unwrap_or_else(|| panic!("{field}"));
    assert!(
        field.contains("_2: value = field _0.0 of ctor app::Person"),
        "{field}"
    );
    assert!(call.contains(">(_2, _0, _1)"), "{field}");
    assert!(!field.contains("str_append"), "{field}");
}
