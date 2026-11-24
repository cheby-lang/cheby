//! A direct call of `map::upsert` in steps (`upsert.rs`): no kernel calls
//! the callback, a known function is called directly and any other
//! callback through the uniform convention, and in release the `Some` the
//! steps build and the callback's closure fold away once the callback is
//! inlined.

use crate::golden_tests::check_golden;
use crate::test_support::{lower_source, lower_source_release, lower_source_uncounted};

const SOURCE: &str = "import std::io
import std::list
import std::map

fn bump(found: Option<Int>) -> Int {
  case found {
    Some(n) => n + 1
    None => 1
  }
}

fn count(counts: map::Map<String, Int>, word: String) -> map::Map<String, Int> {
  let next = map::upsert(counts, word, bump)
  next
}

fn add(counts: map::Map<String, Int>, word: String, k: Int) -> map::Map<String, Int> {
  map::upsert(counts, word, fn(found) {
    case found {
      Some(n) => n + k
      None => k
    }
  })
}

fn push(lists: map::Map<Int, List<String>>, key: Int, item: String) -> map::Map<Int, List<String>> {
  map::upsert(lists, key, fn(found) {
    case found {
      Some(items) => list::push(items, item)
      None => [item]
    }
  })
}

fn as_value(counts: map::Map<String, Int>, word: String) -> map::Map<String, Int> {
  let up = map::upsert
  up(counts, word, bump)
}

fn main() {
  let counts = count(count(map::new(), \"a\"), \"a\")
  let more = add(counts, \"b\", 2)
  let lists = push(push(map::new(), 1, \"x\"), 1, \"y\")
  let again = as_value(more, \"b\")
  let a = map::get(again, \"b\")
  let l = map::get(lists, 1)
  io::println(\"{a:?} {l:?}\")
}
";

/// The steps of a direct call, in one block: one `map_upsert`, the address
/// it kept, the callback's `Option`, one call of the callback and the store
/// of its result; no foreign call of the kernel and no branch.
#[test]
fn a_direct_upsert_calls_its_callback_from_generated_code() {
    let lowered = lower_source(&[("", SOURCE)]);
    let count = lowered.function("app", "app::count");
    check_golden("upsert/direct_debug", &count);
    assert!(!count.contains("cheby_map_upsert"), "{count}");
    assert_eq!(
        count.matches("call_runtime map_upsert(").count(),
        1,
        "{count}"
    );
    assert_eq!(count.matches("call app::bump(").count(), 1, "{count}");
    for step in ["map_upsert_address", "map_upsert_found", "map_upsert_store"] {
        assert!(count.contains(step), "{step}:\n{count}");
    }
    for gone in ["branch", "switch", "map_slot_take"] {
        assert!(!count.contains(gone), "{gone}:\n{count}");
    }
}

/// A closure is called through the uniform convention in debug.
#[test]
fn a_closure_callback_is_an_indirect_call_in_debug() {
    let lowered = lower_source_uncounted(&[("", SOURCE)]);
    let add = lowered.function("app", "app::add");
    assert!(!add.contains("cheby_map_upsert"), "{add}");
    assert_eq!(add.matches("call _").count(), 1, "{add}");
}

/// `map::upsert` as a value is the wrapper, whose kernel calls back.
#[test]
fn upsert_as_a_value_keeps_the_kernel() {
    let lowered = lower_source_uncounted(&[("", SOURCE)]);
    let as_value = lowered.function("app", "app::as_value");
    assert!(!as_value.contains("map_upsert("), "{as_value}");
    let wrapper = lowered.any_function("std::map::upsert");
    assert!(
        wrapper.contains("call_foreign \"cheby_map_upsert\""),
        "{wrapper}"
    );
}

/// In release, the callback is inlined and only takes its `Option` apart,
/// so `map_upsert_found` goes: the arm is chosen by the address, the `Some`
/// arm moves the stored value out with `map_slot_take`, and the closure
/// the callback read its capture from is taken apart too. Nothing is
/// allocated, read as a field or called but the steps themselves.
#[test]
fn a_release_upsert_folds_the_some_and_the_closure() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let mut text = String::new();
    for name in ["count", "add", "push"] {
        let f = lowered.function("app", &format!("app::{name}"));
        for gone in [
            "alloc ctor Some",
            "alloc closure",
            "field ",
            "read_tag",
            "call app::",
        ] {
            assert!(!f.contains(gone), "{name}: {gone}:\n{f}");
        }
        assert!(!f.contains("call _"), "{name}:\n{f}");
        text.push_str(&f);
        text.push('\n');
    }
    check_golden("upsert/release_folded", &text);
    let counted = lower_source_release(&[("", SOURCE)], true);
    let count = counted.function("app", "app::count");
    assert!(!count.contains("alloc"), "{count}");
}
