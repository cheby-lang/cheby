//! Clones for known dictionaries and functions (design T12, T13): a call of
//! a generic function with a static dictionary, a function value or a
//! closure gets a clone in the caller's unit that knows it, which the
//! clean-up then calls directly and the inliner copies; a clone of a loop
//! calls itself by key; one clone serves every call with the same shapes;
//! a clone brings the `priv` functions of its module that it calls. Every
//! changed program is checked as lowered and counted.

use crate::dump_function;
use crate::ir::{FnKey, Function, MirModule};
use crate::test_support::lower_source_release;

/// The clones in `unit` made from the function whose path is `origin`,
/// with their dumps.
fn clones_of(unit: &MirModule, origin: &str) -> Vec<(FnKey, String)> {
    unit.functions
        .iter()
        .filter(|f| {
            matches!(f.key, FnKey::Specialized { origin: o, .. }
                if unit.name(o).is_ok_and(|n| n.path() == origin))
        })
        .map(|f: &Function| (f.key, dump_function(unit, f).unwrap()))
        .collect()
}

/// Whether the dump `text` makes an indirect call, `call _N(` or
/// `tail_call _N(`.
fn calls_indirectly(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        let rest = l
            .split_once("= call _")
            .map(|(_, r)| r)
            .or_else(|| l.strip_prefix("tail_call _"));
        rest.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    })
}

const SORTED_MAP: &str = "//! Inserts keys into a sorted map in a loop.

import std::int
import std::io
import std::sorted_map
import std::sorted_map::{SortedMap}

fn build(m: SortedMap<Int, Int>, i: Int, n: Int) -> SortedMap<Int, Int> {
  case i == n {
    True => m
    False => build(sorted_map::insert(m, i * 7 % n, i), i + 1, n)
  }
}

fn main() {
  let m = build(sorted_map::new(), 0, 1000)
  io::println(int::show(sorted_map::size(m)))
}
";

/// `sorted_map::insert` called with `Int`'s static `Compare` dictionary
/// gets a clone without the dictionary, whose comparison is `int::compare`
/// inlined: no indirect call is left, and its recursive calls call the
/// clone. The `priv` helpers it calls come along as clones too.
#[test]
fn a_dictionary_clone_compares_directly() {
    let lowered = lower_source_release(&[("", SORTED_MAP)], false);
    let unit = lowered.unit("app");
    let clones = clones_of(unit, "std::sorted_map::insert");
    let insert = clones
        .iter()
        .find(|(_, text)| text.contains("std/sorted_map.cheby:350"))
        .unwrap_or_else(|| panic!("no clone of insert:\n{}", lowered.dump()));
    assert!(!calls_indirectly(&insert.1), "{}", insert.1);
    assert!(!insert.1.contains("std::int::compare"), "{}", insert.1);
    let FnKey::Specialized { index, .. } = insert.0 else {
        panic!("{:?} is no clone", insert.0)
    };
    assert!(
        insert
            .1
            .contains(&format!("#{index} of std::sorted_map::insert>(")),
        "{}",
        insert.1
    );
    assert!(clones.len() > 1, "the helpers came along: {clones:?}");
    let build = lowered.function("app", "app::build");
    assert!(!build.contains("std::sorted_map::insert("), "{build}");
    // Counted, with the full checks, the reuse of the path's nodes stays.
    let counted = lower_source_release(&[("", SORTED_MAP)], true);
    let unit = counted.unit("app");
    let insert = clones_of(unit, "std::sorted_map::insert")
        .into_iter()
        .find(|(_, text)| text.contains("std/sorted_map.cheby:350"))
        .unwrap();
    assert!(insert.1.contains("alloc_reuse"), "{}", insert.1);
}

/// How many calls of clones `main`'s dump makes.
fn main_calls(main: &str) -> usize {
    main.matches("= call <spec ").count()
}

const FOLDS: &str = "//! Folds lists with literals, names and closures that capture.

import std::float
import std::int
import std::io
import std::list

fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() {
  let xs = list::range(0, 10)
  let a = list::fold(xs, 0, fn(acc, x) { acc + x })
  let b = list::fold(xs, 0, add)
  let c = list::fold(xs, 0, add)
  let k = 3
  let d = list::fold(xs, 0, fn(acc, x) { acc + x * k })
  let keep = fn(acc, x) { acc - x }
  let e = list::fold(xs, 0, keep)
  let f = list::length([keep])
  let s = list::fold([1.5, 2.5], 0.0, fn(acc, x) { acc + x })
  io::println(\"{a} {b} {c} {d} {e} {f} {s}\")
}
";

/// `list::fold` with a closure literal, a function name, a closure that
/// captures and a closure that also escapes: each gets a clone whose loop
/// calls itself by key and calls the callback's code directly, inlined.
/// The two folds with `add` share one clone.
#[test]
fn fold_clones_call_their_callbacks_directly() {
    let lowered = lower_source_release(&[("", FOLDS)], false);
    let unit = lowered.unit("app");
    let clones = clones_of(unit, "std::list::fold");
    assert_eq!(clones.len(), 5, "{}", lowered.dump());
    assert_eq!(main_calls(&lowered.function("app", "app::main")), 6);
    for (key, text) in &clones {
        assert!(!calls_indirectly(text), "{text}");
        let FnKey::Specialized { index, .. } = key else {
            panic!("{key:?} is no clone")
        };
        assert!(
            text.contains(&format!(
                "tail_call <spec app::main#{index} of std::list::fold>("
            )),
            "{text}"
        );
    }
    let main = lowered.function("app", "app::main");
    assert!(!main.contains("std::list::fold("), "{main}");
    assert_ne!(lower_source_release(&[("", FOLDS)], true).dump(), "");
}

/// D-264: the clones of a build are the same twice, numbered in creation
/// order.
#[test]
fn clones_are_deterministic() {
    assert_eq!(
        lower_source_release(&[("", FOLDS)], true).dump(),
        lower_source_release(&[("", FOLDS)], true).dump()
    );
    assert_eq!(
        lower_source_release(&[("", SORTED_MAP)], true).dump(),
        lower_source_release(&[("", SORTED_MAP)], true).dump()
    );
}
