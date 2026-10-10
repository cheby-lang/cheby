//! Lowering tests of W2-A's files (`scalar.rs`, `panic.rs`, `entry.rs`):
//! literals, operators, `panic`, `todo`, `assert`, and the entry thunks of
//! `main` and tests, from source through `test_support`. Whole units are
//! pinned by goldens under `tests/golden/` (`CHEBY_BLESS=1` rewrites
//! them), and the decisive shapes by string checks.

use crate::MirModule;
use crate::golden_tests::check_golden;
use crate::test_support::{Lowered, lower_source, lower_source_with_tests};

fn text(unit: &MirModule) -> String {
    crate::dump(unit).unwrap()
}

const SCALARS: &str = r#"//! Literals and operators (D2, §5.4).

type Money {
  Money(Int),
}

fn add(a: Money, b: Money) -> Money {
  todo
}

fn compare(a: Money, b: Money) -> Order {
  todo
}

fn neg(m: Money) -> Money {
  todo
}

fn money() -> Money {
  todo
}

fn numbers() -> List<Int> {
  todo
}

fn even(n: Int) -> Bool {
  n % 2 == 0
}

fn literals() {
  let a = 42
  let b = -9007199254740991
  let c: I8 = -128
  let d: I16 = 32767
  let e: I32 = -1
  let f: I64 = 9223372036854775807
  let g: U8 = 255
  let h: U16 = 65535
  let i: U32 = 4294967295
  let j: U64 = 18446744073709551615
  let k = 1.5
  let l: F32 = 0.1
  let m = -0.0
}

fn ints(a: Int, b: Int) -> Int {
  a + b * a - b / a % b
}

fn shifts(a: Int, b: Int) -> Int {
  a << b >> 1
}

fn bits(a: U8, b: U8) -> U8 {
  a & b | a ^ b
}

fn sized(a: I16, b: U32) -> U32 {
  let c = a - a * 2
  b + b
}

fn floats(x: Float, y: Float) -> Float {
  x * y + x / y - x % y
}

fn f32s(x: F32) -> F32 {
  x - 1.5
}

fn lt_ints(a: Int, b: Int) -> Bool {
  a < b
}

fn ge_floats(x: Float, y: Float) -> Bool {
  x >= y
}

fn le_u64(a: U64, b: U64) -> Bool {
  a <= b
}

fn eq_ints(a: Int, b: Int) -> Bool {
  a == b
}

fn ne_floats(x: Float, y: Float) -> Bool {
  x != y
}

fn ne_bools(a: Bool, b: Bool) -> Bool {
  a != b
}

fn eq_strings(a: String, b: String) -> Bool {
  a == b
}

fn ne_strings(a: String, b: String) -> Bool {
  a != b
}

fn eq_lists(a: List<Int>, b: List<Int>) -> Bool {
  a == b
}

fn ne_options(a: Option<Int>, b: Option<Int>) -> Bool {
  a != b
}

fn lt_strings(a: String, b: String) -> Bool {
  a < b
}

fn gt_money(a: Money, b: Money) -> Bool {
  a > b
}

fn le_money(a: Money, b: Money) -> Bool {
  a <= b
}

fn concat(a: String, b: String) -> String {
  a + b
}

fn add_money(a: Money, b: Money) -> Money {
  let c = a + b
  c + b
}

fn and_tail(a: Bool, n: Int) -> Bool {
  a && even(n)
}

fn or_value(a: Bool, b: Bool) -> Bool {
  a || b
}

fn and_not_tail(a: Bool, n: Int) -> Bool {
  let r = a && even(n)
  !r
}

fn neg_int(a: Int) -> Int {
  -a
}

fn neg_i8(a: I8) -> I8 {
  -a
}

fn neg_float(x: Float) -> Float {
  -x
}

fn neg_f32(x: F32) -> F32 {
  -x
}

fn neg_money(m: Money) -> Money {
  -m
}

fn main() {
  literals()
  let _ = ints(1, 2)
  let _ = shifts(1, 2)
  let _ = bits(1, 2)
  let _ = sized(1, 2)
  let _ = floats(1.0, 2.0)
  let _ = f32s(1.0)
  let _ = lt_ints(1, 2)
  let _ = ge_floats(1.0, 2.0)
  let _ = le_u64(1, 2)
  let _ = eq_ints(1, 2)
  let _ = ne_floats(1.0, 2.0)
  let _ = ne_bools(True, False)
  let _ = eq_strings("a", "b")
  let _ = ne_strings("a", "b")
  let _ = eq_lists(numbers(), numbers())
  let _ = ne_options(None, None)
  let _ = lt_strings("a", "b")
  let _ = gt_money(money(), money())
  let _ = le_money(money(), money())
  let _ = concat("a", "b")
  let _ = add_money(money(), money())
  let _ = and_tail(True, 2)
  let _ = or_value(True, False)
  let _ = and_not_tail(True, 2)
  let _ = neg_int(1)
  let _ = neg_i8(1)
  let _ = neg_float(1.0)
  let _ = neg_f32(1.0)
  let _ = neg_money(money())
}
"#;

fn scalars() -> Lowered {
    let lowered = lower_source(&[("", SCALARS)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

/// The whole unit, pinned.
#[test]
fn scalar_unit_golden() {
    check_golden("scalars", &text(scalars().unit("app")));
}

fn f(l: &Lowered, name: &str) -> String {
    l.function("app", &format!("app::{name}"))
}

/// Every numeric type's literal is a constant of its representation;
/// signed sized values are sign-extended, `F32` rounds to nearest.
#[test]
fn literals_of_every_numeric_type() {
    let text = f(&scalars(), "literals");
    for want in [
        "= const int 42",
        "= const int -9007199254740991",
        "= const i8 -128",
        "= const i16 32767",
        "= const i32 -1",
        "= const i64 9223372036854775807",
        "= const u8 255",
        "= const u16 65535",
        "= const u32 4294967295",
        "= const u64 18446744073709551615",
        "= const float 1.5",
        "= const f32 0.1",
        "= const float -0.0",
    ] {
        assert!(text.contains(want), "missing `{want}` in:\n{text}");
    }
}

/// Integer arithmetic and shifts are checked at the operator's location;
/// floats use IEEE arithmetic; `& | ^` are bitwise.
#[test]
fn builtin_arithmetic_kinds() {
    let l = scalars();
    let ints = f(&l, "ints");
    for op in ["add", "sub", "mul", "div", "rem"] {
        assert!(ints.contains(&format!("checked {op} int")), "{ints}");
    }
    let shifts = f(&l, "shifts");
    assert!(shifts.contains("checked shl int"), "{shifts}");
    assert!(shifts.contains("checked shr int"), "{shifts}");
    let bits = f(&l, "bits");
    for op in ["and", "or", "xor"] {
        assert!(bits.contains(&format!("bitwise {op} u8")), "{bits}");
    }
    let sized = f(&l, "sized");
    assert!(sized.contains("checked mul i16"), "{sized}");
    assert!(sized.contains("checked add u32"), "{sized}");
    let floats = f(&l, "floats");
    for op in ["add", "sub", "mul", "div", "rem"] {
        assert!(
            floats.contains(&format!("float_arith {op} float")),
            "{floats}"
        );
    }
    assert!(f(&l, "f32s").contains("float_arith sub f32"));
}

/// Comparisons of numbers and `Bool` are `compare`; `==` on `String` is
/// `str_eq`, on other uniform types `equal` with the location (D-556);
/// `!=` negates.
#[test]
fn equality_and_ordering_by_type() {
    let l = scalars();
    assert!(f(&l, "lt_ints").contains("compare lt int"));
    assert!(f(&l, "ge_floats").contains("compare ge float"));
    assert!(f(&l, "le_u64").contains("compare le u64"));
    assert!(f(&l, "eq_ints").contains("compare eq int"));
    assert!(f(&l, "ne_floats").contains("compare ne float"));
    assert!(f(&l, "ne_bools").contains("compare ne bool"));
    let eq = f(&l, "eq_strings");
    assert!(
        eq.contains("call_runtime str_eq(borrow _0, borrow _1)"),
        "{eq}"
    );
    let ne = f(&l, "ne_strings");
    assert!(
        ne.contains("call_runtime str_eq(borrow _0, borrow _1)"),
        "{ne}"
    );
    assert!(ne.contains("= not _"), "{ne}");
    // The location is the operator expression's (D-556).
    let lists = f(&l, "eq_lists");
    assert!(
        lists.contains("call_runtime equal(borrow _0, borrow _1, borrow const str#1)"),
        "{lists}"
    );
    assert!(
        lists.contains("str#1 = \"src/main.cheby:105:3\""),
        "{lists}"
    );
    let options = f(&l, "ne_options");
    assert!(
        options.contains("call_runtime equal(borrow _0, borrow _1, borrow const str#1)"),
        "{options}"
    );
    assert!(options.contains("= not _"), "{options}");
}

/// `<` and friends on other types call `compare` and test the `Order`
/// tag: `<` is `== Less`, `<=` is `!= Greater`, `>` is `== Greater`,
/// `>=` is `!= Less` (§5.4.4).
#[test]
fn ordering_through_compare() {
    let l = scalars();
    let lt = f(&l, "lt_strings");
    assert!(lt.contains("call std::string::compare(_0, _1)"), "{lt}");
    assert!(lt.contains("read_tag _2 of order"), "{lt}");
    assert!(lt.contains("compare eq int _3, const int 0"), "{lt}");
    let gt = f(&l, "gt_money");
    assert!(gt.contains("call app::compare("), "{gt}");
    assert!(gt.contains("const int 2"), "{gt}");
    assert!(gt.contains("compare eq int"), "{gt}");
    let le = f(&l, "le_money");
    assert!(le.contains("compare ne int"), "{le}");
    assert!(le.contains("const int 2"), "{le}");
}

/// An overloaded arithmetic operator calls the type's function, as a tail
/// call in tail position (D-015).
#[test]
fn operator_calls() {
    let l = scalars();
    let concat = f(&l, "concat");
    assert!(
        concat.contains("tail_call std::string::add(_0, _1)"),
        "{concat}"
    );
    let add = f(&l, "add_money");
    assert!(add.contains("= call app::add("), "{add}");
    assert!(add.contains("tail_call app::add("), "{add}");
    assert!(f(&l, "neg_money").contains("tail_call app::neg(_0)"));
}

/// `&&` and `||` branch and join through a block parameter; the right
/// operand is in tail position when the whole expression is (§5.15.2).
#[test]
fn short_circuit_and_tail_position() {
    let l = scalars();
    let and = f(&l, "and_tail");
    assert!(and.contains("branch _0"), "{and}");
    assert!(and.contains("tail_call app::even("), "{and}");
    assert!(and.contains("const bool false"), "{and}");
    let or = f(&l, "or_value");
    assert!(or.contains("const bool true"), "{or}");
    assert!(or.contains("(_"), "{or}");
    let not_tail = f(&l, "and_not_tail");
    assert!(!not_tail.contains("tail_call"), "{not_tail}");
    assert!(not_tail.contains("= call app::even("), "{not_tail}");
    assert!(not_tail.contains("= not _"), "{not_tail}");
}

/// Unary `-`: checked `0 - x` on integers (panics on the minimum), a sign
/// flip on floats, the type's `neg` otherwise.
#[test]
fn negation_kinds() {
    let l = scalars();
    let int = f(&l, "neg_int");
    assert!(int.contains("checked sub int const int 0, _0"), "{int}");
    let i8 = f(&l, "neg_i8");
    assert!(i8.contains("checked sub i8 const i8 0, _0"), "{i8}");
    assert!(f(&l, "neg_float").contains("float_neg float _0"));
    assert!(f(&l, "neg_f32").contains("float_neg f32 _0"));
}

const PANICS: &str = r#"//! `panic`, `todo` and `assert` (D9, §11.5, §11.6).

import std::list

fn plain() -> Int {
  panic
}

fn with_message(text: String) -> Int {
  panic as text
}

fn unfinished() -> Int {
  todo
}

fn unfinished_as() -> Int {
  todo as "area of a dot"
}

fn check(b: Bool) {
  assert b
}

fn check_ints(items: List<Int>) {
  assert list::length(items) == 3
}

fn check_strings(a: String, b: String) {
  assert a != b as "different " + a
}

fn check_order(a: String, b: String) {
  assert a < b
}

fn main() {
  let _ = plain()
  let _ = with_message("x")
  let _ = unfinished()
  let _ = unfinished_as()
  check(True)
  check_ints(list::new())
  check_strings("a", "b")
  check_order("a", "b")
}
"#;

fn panics() -> Lowered {
    let lowered = lower_source(&[("", PANICS)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

#[test]
fn panic_unit_golden() {
    check_golden("panics_and_asserts", &text(panics().unit("app")));
}

/// `panic` and `todo` end the block with a located panic; a message is a
/// borrowed `String` (the runtime adds `not implemented: `).
#[test]
fn panic_and_todo() {
    let l = panics();
    let plain = f(&l, "plain");
    assert!(plain.contains("panic explicit at str#1"), "{plain}");
    assert!(plain.contains("\"src/main.cheby:6:3\""), "{plain}");
    let with = f(&l, "with_message");
    assert!(with.contains("panic explicit _0 at"), "{with}");
    assert!(f(&l, "unfinished").contains("panic todo at"));
    let todo_as = f(&l, "unfinished_as");
    assert!(todo_as.contains("panic todo _"), "{todo_as}");
    assert!(todo_as.contains("\"area of a dot\""), "{todo_as}");
}

/// A plain `assert` panics with the source text; a comparison evaluates
/// both operands once and prints them with `left:`/`right:` (D-528); the
/// `as` message is evaluated only on failure.
#[test]
fn assert_messages() {
    let l = panics();
    let plain = f(&l, "check");
    assert!(plain.contains("\"assertion failed: b\""), "{plain}");
    assert!(plain.contains("panic assert _"), "{plain}");
    assert!(!plain.contains("left"), "{plain}");
    let ints = f(&l, "check_ints");
    assert_eq!(ints.matches("call std::list::length(").count(), 1, "{ints}");
    assert!(
        ints.contains("\"assertion failed: list::length(items) == 3\""),
        "{ints}"
    );
    assert!(ints.contains("\"\\n  left: \""), "{ints}");
    assert!(ints.contains("\"\\n right: \""), "{ints}");
    assert_eq!(ints.matches("call_runtime debug(").count(), 2, "{ints}");
    let strings = f(&l, "check_strings");
    assert!(strings.contains("\"assertion failed: \""), "{strings}");
    // `"different " + a` is built only in the failing block, after the
    // branch.
    let branch = strings.find("branch").unwrap();
    let add = strings.find("call std::string::add(").unwrap();
    assert!(branch < add, "{strings}");
    let order = f(&l, "check_order");
    assert!(order.contains("call std::string::compare("), "{order}");
    assert!(order.contains("\"\\n right: \""), "{order}");
}

const ENTRIES: &str = r#"//! `main` returning `Result` and tests (D-140, D-182, D-529).

type Shown {
  Shown,
}

type Unshown {
  Unshown,
}

fn show(s: Shown) -> String {
  "shown"
}

fn shown() -> Result<Nil, Shown> {
  Err(Shown)
}

fn unshown() -> Result<Nil, Unshown> {
  Err(Unshown)
}

fn main() -> Result<Nil, Shown> {
  shown()
}

test "returns nil" {
  assert True
}

fn fine() -> Result<Nil, String> {
  Ok(Nil)
}

test "returns ok" {
  fine()
}

test "returns an error without show" {
  unshown()
}
"#;

fn entries() -> Lowered {
    let lowered = lower_source_with_tests(&[("", ENTRIES)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

#[test]
fn entry_unit_golden() {
    let l = entries();
    let unit = l.units.last().unwrap();
    check_golden("entry_results", &text(unit));
}

/// `main` returning `Result`: status 0 on `Ok`; on `Err` the error's
/// `show` text on standard error and status 1 (D-529).
#[test]
fn result_main_prints_errors_by_show() {
    let main = entries().any_function("entry main");
    assert!(
        main.starts_with("fn entry main(app::main) [c] () -> i64"),
        "{main}"
    );
    assert!(main.contains("read_tag _0 of result"), "{main}");
    assert!(main.contains("field _0.0 of ctor Err"), "{main}");
    assert!(main.contains("call app::show(_3)"), "{main}");
    assert!(
        main.contains("call_foreign \"cheby_io_eprintln\"(borrow _4) -> void"),
        "{main}"
    );
    assert!(main.contains("return const i64 1"), "{main}");
    assert!(main.contains("return const i64 0"), "{main}");
}

/// A test that passes returns the static `Nil`; one returning `Result`
/// returns the text of an `Err`, made like `main`'s (by debug text
/// without `Show`), and prints nothing (D-541, GAP(test-err-thunk)).
#[test]
fn test_entries_by_result() {
    let l = entries();
    let dump = text(l.units.last().unwrap());
    let tests: Vec<&str> = dump
        .split("\nfn ")
        .filter(|f| f.starts_with("entry test("))
        .collect();
    assert_eq!(tests.len(), 3, "{dump}");
    assert!(!tests[0].contains("read_tag"), "{}", tests[0]);
    assert!(tests[0].contains("-> value"), "{}", tests[0]);
    assert!(tests[0].contains("return const nil"), "{}", tests[0]);
    assert!(tests[1].contains("read_tag _0 of result"), "{}", tests[1]);
    assert!(tests[1].contains("call std::string::show("), "{}", tests[1]);
    assert!(tests[1].contains("return const nil"), "{}", tests[1]);
    assert!(tests[1].contains("return _4"), "{}", tests[1]);
    assert!(
        tests[2].contains("call_runtime debug(borrow _3)"),
        "{}",
        tests[2]
    );
    assert!(!tests[2].contains("cheby_io_eprintln"), "{}", tests[2]);
    // Every entries unit carries trampolines 0-4 (plan §4.3.4).
    for n in 0..=4 {
        assert!(dump.contains(&format!("fn trampoline {n} [c] (")), "{dump}");
    }
}
