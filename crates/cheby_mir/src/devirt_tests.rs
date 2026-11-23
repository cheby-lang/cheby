//! The clean-up's devirtualization (design T11): a call through a static
//! dictionary, a function value and a closure allocated in the same
//! function becomes a direct call or a call of the closure's code, which
//! the inliner then copies; a tag read of a static constructor is its
//! tag. Every changed program is checked as lowered and then counted.

use crate::golden_tests::check_golden;
use crate::test_support::lower_source_release;

const SOURCE: &str = "//! Calls through a dictionary, a function value and a closure.

import std::int
import std::io
import std::ops

fn smaller<T: ops::Compare>(a: T, b: T) -> T {
  case ops::Compare::compare(a, b) {
    Greater => b
    _ => a
  }
}

fn pick(a: Int, b: Int) -> Int {
  smaller(a, b)
}

fn apply(f: fn(Int) -> Int, x: Int) -> Int {
  f(x)
}

fn double(x: Int) -> Int {
  x * 2
}

fn twice(x: Int) -> Int {
  apply(double, x)
}

fn scaled(k: Int, x: Int) -> Int {
  let f = fn(y) { y * k }
  f(x)
}

fn main() {
  let a = pick(3, 4)
  let b = twice(a)
  let c = scaled(b, 5)
  io::println(int::show(c))
}
";

/// A call of `Compare::compare` through `Int`'s static dictionary is a
/// direct call of `int::compare`, which is inlined, and the `Order` it
/// gives goes to its arm: no call, no field read and no tag read is left.
#[test]
fn a_static_dictionary_call_is_direct() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let pick = lowered.function("app", "app::pick");
    check_golden("devirt/static_dictionary", &pick);
    for gone in ["call", "field", "read_tag"] {
        assert!(!pick.contains(gone), "{gone}:\n{pick}");
    }
    let counted = lower_source_release(&[("", SOURCE)], true);
    assert!(!counted.function("app", "app::pick").contains("call"));
}

/// A call of a function value is a direct call of its function, with the
/// unboxes its value shim makes, and then inlined.
#[test]
fn a_function_value_call_is_direct() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let twice = lowered.function("app", "app::twice");
    check_golden("devirt/function_value", &twice);
    assert!(!twice.contains("call"), "{twice}");
    assert!(twice.contains("checked mul"), "{twice}");
}

/// A call of a closure allocated in the same function calls its code with
/// the closure as `self`, and the code is inlined: its capture is read
/// from the operand that was captured.
#[test]
fn a_local_closure_call_is_its_code() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let scaled = lowered.function("app", "app::scaled");
    check_golden("devirt/local_closure", &scaled);
    for gone in ["call", "alloc", "field"] {
        assert!(!scaled.contains(gone), "{gone}:\n{scaled}");
    }
    let counted = lower_source_release(&[("", SOURCE)], true);
    assert!(!counted.function("app", "app::scaled").contains("alloc"));
}

/// A constructor whose only uses read its tag and its fields is taken
/// apart (`scalar_ctors`): in `unwrapped`, the inlined `flatten` reads
/// the `Some` built here, which goes. A field that is a constant, here
/// the inner `None`, stays a definition, so a read of its fields in a
/// case that cannot run still names a defined local.
#[test]
fn a_constructor_only_read_is_taken_apart() {
    let source = "import std::io

pub fn flatten<T>(o: Option<Option<T>>) -> Option<T> {
  case o {
    Some(inner) => inner
    None => None
  }
}

fn unwrapped(x: Int) -> Option<Int> {
  flatten(Some(Some(x)))
}

fn main() {
  let inner_none: Option<Option<Int>> = Some(None)
  assert flatten(inner_none) == None
  let v = unwrapped(4)
  io::println(\"{v:?}\")
}
";
    let lowered = lower_source_release(&[("", source)], false);
    let unwrapped = lowered.function("app", "app::unwrapped");
    assert_eq!(
        unwrapped.matches("alloc ctor Some").count(),
        1,
        "{unwrapped}"
    );
    for gone in ["field", "read_tag", "call"] {
        assert!(!unwrapped.contains(gone), "{gone}:\n{unwrapped}");
    }
    let counted = lower_source_release(&[("", source)], true);
    assert!(counted.function("app", "app::main").contains("println"));
}

/// Folding settles the whole program: `main` computes no product at run
/// time.
#[test]
fn constants_fold_through_the_calls() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let main = lowered.function("app", "app::main");
    assert!(!main.contains("checked"), "{main}");
    assert!(main.contains("const int 30"), "{main}");
}
