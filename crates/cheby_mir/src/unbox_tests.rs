//! Raw parameters and results of clones (design T14): a fold that carries
//! a `Float` takes and returns it raw, and its loop boxes nothing; the call
//! passes the raw value and uses the raw result. The counted program is
//! valid.

use crate::test_support::lower_source_release;

const FLOAT_FOLD: &str = "//! Sums floats through a fold.

import std::float
import std::io
import std::list

fn main() {
  let total = list::fold(list::range(0, 100), 0.0, fn(acc, i) { acc + float::from_int(i) })
  io::println(float::show(total))
}
";

/// The clone of `list::fold` takes the accumulator and returns the result as
/// a `Float`, its self tail call passes the sum raw, and `main` neither boxes
/// `0.0` nor unboxes the result.
#[test]
fn a_float_fold_clone_takes_and_returns_floats() {
    let lowered = lower_source_release(&[("", FOLD_SOURCE)], false);
    let unit = lowered.unit("app");
    let clone = unit
        .functions
        .iter()
        .find(|f| matches!(f.key, crate::FnKey::Specialized { .. }))
        .unwrap_or_else(|| panic!("no clone:\n{}", lowered.dump()));
    let text = crate::dump_function(unit, clone).unwrap();
    assert_eq!(clone.ret, crate::Repr::Float, "{text}");
    assert!(!text.contains("box float"), "{text}");
    let main = lowered.function("app", "app::main");
    assert!(main.contains("float = call <spec"), "{main}");
    assert!(!main.contains("box float const float 0.0"), "{main}");
    assert_ne!(lower_source_release(&[("", FOLD_SOURCE)], true).dump(), "");
}

/// The program of [`a_float_fold_clone_takes_and_returns_floats`].
const FOLD_SOURCE: &str = FLOAT_FOLD;
