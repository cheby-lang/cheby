//! Tests of the lookup pass: a `list::get`, `map::get` or `bytes::get`
//! whose `Option` is taken apart at once reads the element without the
//! `Some`, and every other use keeps the kernel's call.

use crate::golden_tests::check_golden;
use crate::test_support::{lower_source, lower_source_uncounted};

/// Each form that takes the result apart at once.
const TAKEN_APART: &str = "import std::io
import std::list
import std::map

fn assert_at(xs: List<Int>, i: Int) -> Int {
  let assert Some(x) = list::get(xs, i)
  x
}

fn case_at(xs: List<String>, i: Int) -> String {
  case list::get(xs, i) {
    Some(x) => x
    None => \"none\"
  }
}

fn guard_at(xs: List<Int>, i: Int) -> Int {
  let guard Some(x) = list::get(xs, i) else {
    0
  }
  x + 1
}

fn guard_key(m: map::Map<String, Int>, k: String) -> Int {
  let guard Some(x) = map::get(m, k) else {
    0
  }
  x + 1
}

fn case_key(m: map::Map<String, String>, k: String) -> String {
  case map::get(m, k) {
    None => \"none\"
    Some(v) => v
  }
}

fn guarded_arms(xs: List<Int>, i: Int) -> Int {
  case list::get(xs, i) {
    Some(x) when x > 1 => x
    Some(_) => 1
    None => 0
  }
}

fn present(m: map::Map<String, Int>, k: String) -> Bool {
  case map::get(m, k) {
    Some(_) => True
    None => False
  }
}

fn main() {
  let xs = [1, 2, 3]
  let m = map::from_list([(\"a\", 1)])
  let s = map::from_list([(\"a\", \"b\")])
  let a = assert_at(xs, 1)
  let b = case_at([\"p\"], 0)
  let c = guard_at(xs, 2)
  let d = guard_key(m, \"a\")
  let e = case_key(s, \"a\")
  let f = guarded_arms(xs, 0)
  let g = present(m, \"z\")
  io::println(\"{a} {b} {c} {d} {e} {f} {g}\")
}
";

/// The functions of [`TAKEN_APART`].
const TAKEN_APART_FNS: [&str; 7] = [
    "assert_at",
    "case_at",
    "guard_at",
    "guard_key",
    "case_key",
    "guarded_arms",
    "present",
];

/// Results used as values: returned, passed on, bound and shown.
const KEPT: &str = "import std::bytes
import std::io
import std::list
import std::map

fn returned(xs: List<Int>, i: Int) -> Option<Int> {
  list::get(xs, i)
}

fn byte_returned(data: bytes::Bytes, i: Int) -> Option<U8> {
  bytes::get(data, i)
}

fn byte_shown(data: bytes::Bytes, i: Int) -> String {
  case bytes::get(data, i) {
    Some(0x0A) => \"newline\"
    other => \"{other:?}\"
  }
}

fn shared_shown(xs: List<Int>, i: Int) -> String {
  case list::get(xs, i) {
    Some(0) => \"zero\"
    other => \"{other:?}\"
  }
}

fn bound(xs: List<Int>, i: Int) -> Option<Int> {
  let o = list::get(xs, i)
  o
}

fn shown(m: map::Map<String, Int>, k: String) -> String {
  case map::get(m, k) {
    None => \"none\"
    o => \"{o:?}\"
  }
}

fn passed(xs: List<Int>, i: Int) -> Bool {
  option_is_some(list::get(xs, i))
}

fn option_is_some(o: Option<Int>) -> Bool {
  case o {
    Some(_) => True
    None => False
  }
}

fn main() {
  let xs = [1, 2, 3]
  let m = map::from_list([(\"a\", 1)])
  let a = returned(xs, 0)
  let b = bound(xs, 9)
  let c = shown(m, \"a\")
  let d = passed(xs, 1)
  let data = bytes::from_list([10, 32])
  let e = byte_returned(data, 0)
  let f = byte_shown(data, 1)
  let g = shared_shown(xs, 1)
  io::println(\"{a:?} {b:?} {c} {d} {e:?} {f} {g}\")
}
";

#[test]
fn lookups_taken_apart_at_once_build_no_some() {
    let lowered = lower_source(&[("", TAKEN_APART)]);
    let mut text = String::new();
    for name in TAKEN_APART_FNS {
        let f = lowered.function("app", &format!("app::{name}"));
        assert!(!f.contains("cheby_list_get"), "{f}");
        assert!(!f.contains("cheby_map_get"), "{f}");
        assert!(!f.contains("read_tag"), "{f}");
        assert!(!f.contains("ctor Some"), "{f}");
        text.push_str(&f);
        text.push('\n');
    }
    check_golden("lookup/taken_apart", &text);
}

#[test]
fn a_list_lookup_tests_the_bounds_and_reads_the_element() {
    let lowered = lower_source_uncounted(&[("", TAKEN_APART)]);
    let f = lowered.function("app", "app::case_at");
    assert!(f.contains("= call_runtime list_length("), "{f}");
    assert!(f.contains("= compare ge int _1, const int 0"), "{f}");
    assert!(f.contains("= call_runtime list_get("), "{f}");
    assert_eq!(f.matches("call_runtime list_get(").count(), 1, "{f}");
}

#[test]
fn a_map_lookup_finds_the_value_once_and_reads_it_there() {
    let lowered = lower_source_uncounted(&[("", TAKEN_APART)]);
    let f = lowered.function("app", "app::case_key");
    assert_eq!(f.matches("call_runtime map_find(").count(), 1, "{f}");
    assert!(f.contains("= compare ne u64 "), "{f}");
    assert_eq!(f.matches("call_runtime map_value_at(").count(), 1, "{f}");
    // A `Some(_)` arm reads nothing.
    let f = lowered.function("app", "app::present");
    assert!(f.contains("call_runtime map_find("), "{f}");
    assert!(!f.contains("map_value_at"), "{f}");
}

#[test]
fn a_failed_let_assert_still_prints_the_none() {
    let lowered = lower_source_uncounted(&[("", TAKEN_APART)]);
    let f = lowered.function("app", "app::assert_at");
    assert!(f.contains("= const ctor None"), "{f}");
    assert!(f.contains("call_runtime debug("), "{f}");
}

#[test]
fn lookups_used_as_values_keep_the_call() {
    let lowered = lower_source(&[("", KEPT)]);
    for (name, symbol) in [
        ("returned", "cheby_list_get"),
        ("bound", "cheby_list_get"),
        ("shown", "cheby_map_get"),
        ("passed", "cheby_list_get"),
        ("byte_returned", "cheby_bytes_get"),
        // A `_` arm that others enter too and that uses the result.
        ("byte_shown", "cheby_bytes_get"),
        ("shared_shown", "cheby_list_get"),
    ] {
        let f = lowered.function("app", &format!("app::{name}"));
        assert!(f.contains(&format!("call_foreign \"{symbol}\"")), "{f}");
        assert!(!f.contains("list_length"), "{f}");
        assert!(!f.contains("map_find"), "{f}");
        assert!(!f.contains("bytes_in_range"), "{f}");
    }
}

/// The forms of a byte read that a scanner writes: literals, or-patterns
/// and guards with a `_` arm that the `None` shares, a bound byte used as
/// a number and as a value, and `let assert`.
const BYTES: &str = "import std::bytes
import std::int
import std::io
import std::list

fn kind(data: bytes::Bytes, i: Int) -> Int {
  case bytes::get(data, i) {
    Some(0x20) | Some(0x0A) => 0
    Some(0x22) => 1
    Some(byte) when byte >= 0x30 && byte <= 0x39 => int::from_u8(byte)
    _ => -1
  }
}

fn count_lines(data: bytes::Bytes, i: Int, n: Int) -> Int {
  case bytes::get(data, i) {
    Some(0x0A) => count_lines(data, i + 1, n + 1)
    Some(_) => count_lines(data, i + 1, n)
    None => n
  }
}

fn kept(data: bytes::Bytes, i: Int) -> List<U8> {
  case bytes::get(data, i) {
    Some(byte) => [byte]
    None => []
  }
}

fn first(data: bytes::Bytes) -> U8 {
  let assert Some(byte) = bytes::get(data, 0)
  byte
}

fn main() {
  let data = bytes::from_list([0x31, 0x0A, 0x22])
  let a = kind(data, 0)
  let b = count_lines(data, 0, 0)
  let c = kept(data, 2)
  let d = first(data)
  io::println(\"{a} {b} {c:?} {d}\")
}
";

/// The functions of [`BYTES`].
const BYTES_FNS: [&str; 4] = ["kind", "count_lines", "kept", "first"];

#[test]
fn byte_reads_taken_apart_at_once_call_nothing() {
    let lowered = lower_source(&[("", BYTES)]);
    let mut text = String::new();
    for name in BYTES_FNS {
        let f = lowered.function("app", &format!("app::{name}"));
        assert!(!f.contains("cheby_bytes_get"), "{f}");
        assert!(!f.contains("read_tag"), "{f}");
        assert!(!f.contains("ctor Some"), "{f}");
        assert_eq!(f.matches("call_runtime bytes_in_range(").count(), 1, "{f}");
        text.push_str(&f);
        text.push('\n');
    }
    check_golden("lookup/bytes", &text);
}

#[test]
fn a_byte_is_boxed_only_where_a_word_is_used() {
    let lowered = lower_source_uncounted(&[("", BYTES)]);
    // Matched against literals and a guard: the byte, never a word.
    let f = lowered.function("app", "app::kind");
    assert!(!f.contains("box u8"), "{f}");
    assert!(!f.contains("unbox u8"), "{f}");
    assert!(f.contains("call_runtime bytes_byte("), "{f}");
    // Put into a list: one box, of the byte read.
    let f = lowered.function("app", "app::kept");
    assert_eq!(f.matches("call_runtime bytes_byte(").count(), 1, "{f}");
    assert_eq!(f.matches("= box u8 ").count(), 1, "{f}");
    // A `Some(_)` arm reads nothing.
    let f = lowered.function("app", "app::count_lines");
    assert_eq!(f.matches("call_runtime bytes_byte(").count(), 1, "{f}");
}

#[test]
fn a_failed_byte_assert_still_prints_the_none() {
    let lowered = lower_source_uncounted(&[("", BYTES)]);
    let f = lowered.function("app", "app::first");
    assert!(f.contains("= const ctor None"), "{f}");
    assert!(f.contains("call_runtime debug("), "{f}");
}

/// A `_` arm that a `Some` arm's literal also falls to, and that does not
/// use the result, no longer keeps a list's or a map's lookup either.
const SHARED: &str = "import std::io
import std::list
import std::map

fn zero_at(xs: List<Int>, i: Int) -> Bool {
  case list::get(xs, i) {
    Some(0) => True
    _ => False
  }
}

fn positive(m: map::Map<String, Int>, k: String) -> Int {
  case map::get(m, k) {
    Some(n) when n > 0 => n
    _ => 0
  }
}

fn main() {
  let a = zero_at([0, 1], 1)
  let b = positive(map::from_list([(\"a\", 2)]), \"a\")
  io::println(\"{a} {b}\")
}
";

#[test]
fn a_shared_none_arm_that_ignores_the_result_takes_it_apart() {
    let lowered = lower_source_uncounted(&[("", SHARED)]);
    let f = lowered.function("app", "app::zero_at");
    assert!(!f.contains("cheby_list_get"), "{f}");
    assert!(f.contains("call_runtime list_get("), "{f}");
    let f = lowered.function("app", "app::positive");
    assert!(!f.contains("cheby_map_get"), "{f}");
    assert!(f.contains("call_runtime map_find("), "{f}");
}
