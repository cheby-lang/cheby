//! Tests of the lookup pass: a `list::get` or `map::get` whose `Option`
//! is taken apart at once reads the element without the `Some`, and every
//! other use keeps the kernel's call.

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
const KEPT: &str = "import std::io
import std::list
import std::map

fn returned(xs: List<Int>, i: Int) -> Option<Int> {
  list::get(xs, i)
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
  io::println(\"{a:?} {b:?} {c} {d}\")
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
    ] {
        let f = lowered.function("app", &format!("app::{name}"));
        assert!(f.contains(&format!("call_foreign \"{symbol}\"")), "{f}");
        assert!(!f.contains("list_length"), "{f}");
        assert!(!f.contains("map_find"), "{f}");
    }
}
