//! The release inliner across units (design T8, T9): a body of another
//! module of the package, with its closures and static dictionaries, and
//! std's bodies by name ([`crate::export::PortableBody`]); what never
//! crosses (a `priv` helper, a wait that keeps its caller's frame,
//! D-861); positions and shared functions of the copies.

use crate::dump_function;
use crate::golden_tests::check_golden;
use crate::ir::{FnKey, Function, MirModule, StaticKind};
use crate::test_support::{lower_source, lower_source_release};

/// `app::util`: a closure that captures, a static dictionary, a function
/// value, and a public function whose body calls a `priv` helper.
const UTIL: &str = "//! Helpers that other modules inline.

import std::int
import std::list

/// Each element times `k`, through a closure that captures `k`.
fn scale(xs: List<Int>, k: Int) -> List<Int> {
  list::map(xs, fn(x) { x * k })
}

/// The list in order, through `Compare`'s static dictionary of `Int`.
fn sorted(xs: List<Int>) -> List<Int> {
  list::sort(xs)
}

/// Each element as text, through a function value.
fn texts(xs: List<Int>) -> List<String> {
  list::map(xs, int::show)
}

/// `n` counted down to 0, through a `priv` helper that stays a call.
pub fn wrap(n: Int) -> Int {
  helper(n)
}

priv fn helper(n: Int) -> Int {
  case n {
    0 => 0
    _ => helper(n - 1)
  }
}
";

const MAIN: &str = "//! Calls the helpers of `app::util`.

import std::io
import app::util

fn main() {
  let xs = util::scale([3, 1, 2], 10)
  let ys = util::sorted(xs)
  let zs = util::texts(ys)
  let w = util::wrap(41)
  io::println(\"{zs} {w}\")
}
";

/// The item named `item` of `unit`.
fn named<'u>(unit: &'u MirModule, item: &str) -> &'u Function {
    unit.functions
        .iter()
        .find(|f| matches!(f.key, FnKey::Item(d) if unit.name(d).is_ok_and(|n| &*n.item == item)))
        .unwrap_or_else(|| panic!("no {item}"))
}

/// D-918: a body of another module comes with its closures, renumbered
/// as closures of the host, and with the static dictionary and the value
/// shim it names; a body that calls a `priv` helper of its module stays
/// a call. The counted program is valid.
#[test]
fn bodies_of_another_module_come_with_their_closures_and_statics() {
    let lowered = lower_source_release(&[("", MAIN), ("util", UTIL)], false);
    let unit = lowered.unit("app");
    let main = named(unit, "main");
    let text = dump_function(unit, main).unwrap();
    for gone in ["app::util::scale", "app::util::sorted", "app::util::texts"] {
        assert!(!text.contains(gone), "{gone} was not inlined:\n{text}");
    }
    assert!(text.contains("call app::util::wrap("), "{text}");
    let main_def = if let FnKey::Item(d) = main.key {
        Some(d)
    } else {
        None
    }
    .unwrap();
    let closures: Vec<&Function> = unit
        .functions
        .iter()
        .filter(|f| matches!(f.key, FnKey::Closure { owner, .. } if owner == main_def))
        .collect();
    // `scale`'s closure and `list::map`'s, `list::sort`'s and `texts`'s
    // `list::map`'s, each its own.
    assert_eq!(closures.len(), 4, "{}", lowered.dump());
    let mut keys: Vec<FnKey> = closures.iter().map(|f| f.key).collect();
    keys.dedup();
    assert_eq!(keys.len(), 4, "{keys:?}");
    let dict = unit
        .statics
        .iter()
        .find(|s| s.kind == StaticKind::Dict && &*s.module == "app::util");
    assert!(dict.is_some(), "{:?}", unit.statics);
    // The copied value shim is the one its own module's unit defines.
    let util = lowered.unit("app::util");
    let shim_of = |u: &MirModule| {
        u.functions
            .iter()
            .find(|f| {
                matches!(f.key, FnKey::ValueShim(d) if u.name(d).is_ok_and(|n| n.path() == "std::int::show"))
            })
            .map(|f| dump_function(u, f).unwrap())
    };
    assert_eq!(shim_of(unit), shim_of(util));
    assert!(shim_of(unit).is_some());
    let counted = lower_source_release(&[("", MAIN), ("util", UTIL)], true);
    assert!(
        counted
            .function("app", "app::main")
            .contains("call_foreign")
    );
}

/// D-1172: a body from another file takes the call's position, so the
/// host's line table holds no position the debug build's does not.
#[test]
fn a_body_from_another_file_takes_the_calls_position() {
    let debug = lower_source(&[("", MAIN), ("util", UTIL)]);
    let release = lower_source_release(&[("", MAIN), ("util", UTIL)], false);
    let before = &named(debug.unit("app"), "main").positions.table;
    let unit = release.unit("app");
    let after = &named(unit, "main").positions.table;
    assert_ne!(after.as_slice(), []);
    assert!(
        after.iter().all(|p| before.contains(p)),
        "{after:?} {before:?}"
    );
    for f in &unit.functions {
        if matches!(f.key, FnKey::Closure { .. }) {
            assert!(
                f.positions.table.iter().all(|p| before.contains(p)),
                "{f:?}"
            );
        }
    }
}

/// Std's `int::compare` crosses by name into user code, and the clean-up
/// threads its `Order` to the arm, with no tag read and no call left.
#[test]
fn int_compare_inlines_into_user_code() {
    let source = "//! Describes how two numbers compare.

import std::int
import std::io

fn describe(a: Int, b: Int) -> String {
  case int::compare(a, b) {
    Less => \"less\"
    Equal => \"equal\"
    Greater => \"greater\"
  }
}

fn main() {
  io::println(describe(1, 2))
}
";
    let lowered = lower_source_release(&[("", source)], false);
    let describe = lowered.function("app", "app::describe");
    check_golden("inline/int_compare_across", &describe);
    assert!(!describe.contains("std::int::compare"), "{describe}");
    assert!(!describe.contains("read_tag"), "{describe}");
    let counted = lower_source_release(&[("", source)], true);
    assert!(!counted.function("app", "app::describe").contains("call "));
}

/// D-861: a std wait that keeps its caller's frame is never inlined, so a
/// deadlock report still names the caller's call.
#[test]
fn a_frame_keeping_wait_is_never_inlined() {
    let source = "//! Sleeps once.

import std::duration
import std::io
import std::time

fn main() {
  time::sleep(duration::milliseconds(1))
  io::println(\"done\")
}
";
    assert!(crate::waits::keeps_frame("std::time", "sleep"));
    let lowered = lower_source_release(&[("", source)], false);
    let main = lowered.function("app", "app::main");
    assert!(main.contains("std::time::sleep("), "{main}");
}

/// D-264: inlining across units gives the same units twice.
#[test]
fn inlining_across_units_is_deterministic() {
    let files = [("", MAIN), ("util", UTIL)];
    assert_eq!(
        lower_source_release(&files, true).dump(),
        lower_source_release(&files, true).dump()
    );
}
