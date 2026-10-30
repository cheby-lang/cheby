//! Per-unit borrowing facts (D-969): each unit's facts by name, their
//! union, their bytes, and the facts about one module's functions
//! ([`crate::borrow::unit_facts`], [`crate::borrow::BuildFacts::union`]).

use crate::borrow::{BorrowMask, UnitFacts};
use crate::test_support::{Lowered, lower_package_inferred, lower_std_inferred};
use crate::{MirModule, UnitKind};

/// `app` uses `size` as a value, calls `util::check` from its code and
/// tail-calls it with a non-constant from its test.
const MAIN: &str = "import std::io
import std::list
import app::util

fn main() {
  let lengths = list::map([\"a\", \"bb\"], size)
  util::check(\"a\")
  io::println(\"{lengths}\")
}

priv fn size(s: String) -> Int {
  case s == \"a\" {
    True => 1
    False => 2
  }
}

test \"checks\" {
  let one = 1
  util::check(\"{one}\")
}
";

const UTIL: &str = "fn check(s: String) -> Nil {
  assert s != \"\"
}
";

/// A module that sorts before the others, so every other module's id
/// shifts by one.
const FIRST: &str = "pub fn unused(n: Int) -> Int {
  n + 1
}
";

/// The `cheby test` build of `app` with `MAIN` and `UTIL`, after `extra`.
fn build(extra: &[(&str, &str)]) -> Lowered {
    let mut files: Vec<(&str, &str)> = extra.to_vec();
    files.extend([("", MAIN), ("util", UTIL)]);
    lower_package_inferred("app", &files, true).unwrap()
}

/// The unit of module `path` of kind `kind` (`code` or `tests`), with its
/// facts.
fn part<'l>(lowered: &'l Lowered, path: &str, kind: &str) -> (&'l MirModule, &'l UnitFacts) {
    let facts = lowered.facts.as_ref().unwrap();
    lowered
        .units
        .iter()
        .zip(&facts.parts)
        .find(|(u, _)| {
            &*u.path == path
                && matches!(
                    (kind, u.unit),
                    ("code", UnitKind::Code(_)) | ("tests", UnitKind::Tests(_))
                )
        })
        .unwrap_or_else(|| panic!("no {kind} unit of `{path}`"))
}

#[test]
fn unit_facts_union_equals_collect_facts() {
    let lowered = build(&[]);
    let facts = lowered.facts.as_ref().unwrap();
    assert_eq!(facts.parts.len(), lowered.units.len());
    assert_eq!(facts.union, facts.collected);
    // Each kind of fact is there, from the unit that holds it.
    let (_, code) = part(&lowered, "app", "code");
    assert!(code.values().contains(&"app::size".to_owned()), "{code:?}");
    assert!(
        code.callees_elsewhere()
            .contains(&"app::util::check".to_owned()),
        "{code:?}"
    );
    let (_, tests) = part(&lowered, "app", "tests");
    assert_eq!(
        tests.tail_owned(),
        [("app::util::check".to_owned(), BorrowMask(1))],
        "{tests:?}"
    );
    // The test's tail call keeps `check`'s parameter owned.
    assert_eq!(
        lowered.masks.get("app::util::check").copied(),
        None,
        "{:?}",
        lowered.masks
    );
}

/// `UTIL` with a test named like its function: `test "check"`.
const UTIL_NAMED: &str = "fn check(s: String) -> Nil {
  assert s != \"\"
}

test \"check\" {
  check(\"a\")
}
";

/// A test that shares a function's text never stands for it: the facts
/// that name `app::util::check` find the function (RB-2, D-969).
#[test]
fn unit_facts_union_skips_a_test_named_like_a_function() {
    let lowered = lower_package_inferred("app", &[("", MAIN), ("util", UTIL_NAMED)], true).unwrap();
    let facts = lowered.facts.as_ref().unwrap();
    assert_eq!(facts.union, facts.collected);
    let (_, code) = part(&lowered, "app", "code");
    assert!(
        code.callees_elsewhere()
            .contains(&"app::util::check".to_owned()),
        "{code:?}"
    );
}

#[test]
fn unit_facts_union_over_std() {
    let lowered = lower_std_inferred().unwrap();
    let facts = lowered.facts.as_ref().unwrap();
    assert_eq!(facts.union, facts.collected);
    assert!(
        facts.parts.iter().any(|p| !p.values().is_empty()),
        "some std unit uses a function as a value"
    );
}

#[test]
fn unit_facts_round_trip_and_reject_damage() {
    let lowered = build(&[]);
    for part in &lowered.facts.as_ref().unwrap().parts {
        let bytes = part.encode();
        assert_eq!(&UnitFacts::decode(&bytes).unwrap(), part);
        let (_, short) = bytes.split_last().unwrap();
        assert!(UnitFacts::decode(short).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(UnitFacts::decode(&long).is_err());
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert!(UnitFacts::decode(&magic).is_err());
    }
    assert_eq!(
        UnitFacts::decode(&UnitFacts::default().encode()).unwrap(),
        UnitFacts::default()
    );
}

/// The facts name functions by path and item, never by `DefId`: shifting
/// every module's id changes no unit's bytes, nor the bytes of the facts
/// about a module's functions.
#[test]
fn unit_facts_do_not_depend_on_module_ids() {
    let plain = build(&[]);
    let shifted = build(&[("aaa", FIRST)]);
    for (path, kind) in [("app", "code"), ("app", "tests"), ("app::util", "code")] {
        let (a_unit, a) = part(&plain, path, kind);
        let (b_unit, b) = part(&shifted, path, kind);
        assert_ne!(a_unit.module(), b_unit.module(), "the ids shifted");
        assert_eq!(a.encode(), b.encode(), "{kind} unit of {path}");
    }
    let about = |lowered: &Lowered| lowered.facts.as_ref().unwrap().about.clone();
    assert_eq!(about(&plain), about(&shifted));
}

/// The facts about a module's functions hold what its masks depend on:
/// `app`'s say `size` is a value, `app::util`'s that the test's tail call
/// keeps `check`'s parameter owned; a build without tests drops that.
#[test]
fn unit_facts_about_module_holds_values_and_tail_owned_positions() {
    let with_tests = build(&[]);
    let about = &with_tests.facts.as_ref().unwrap().about;
    let has = |bytes: &[u8], text: &str| bytes.windows(text.len()).any(|w| w == text.as_bytes());
    assert!(has(&about["app"], "size"), "{about:?}");
    assert!(has(&about["app::util"], "check"), "{about:?}");
    let files = [("", MAIN), ("util", UTIL)];
    let run = lower_package_inferred("app", &files, false).unwrap();
    let run_about = &run.facts.as_ref().unwrap().about;
    assert_eq!(run_about["app"], about["app"]);
    assert_ne!(run_about["app::util"], about["app::util"]);
    assert!(!has(&run_about["app::util"], "check"), "{run_about:?}");
}
