//! Tests of the near-miss scan of "Satisfy interface…" (D-1696, F §7),
//! in process. Each runs only the front end, so it runs on Windows too
//! (D-1516).

use super::{NearMiss, near_misses};
use crate::features::ide::fill_arms::tests::Fixture;
use crate::features::ide::stubs::provide;

/// The near misses of type `name` of `file`, as `interface: function` paths.
fn misses(fixture: &Fixture, file: &str, name: &str) -> Vec<String> {
    let snapshot = &fixture.analyzed.snapshot;
    let cx = fixture.cx(file);
    let resolved = cx.view.resolved().unwrap();
    let names = &snapshot.globals().names;
    let (index, _) = resolved
        .items
        .iter_enumerated()
        .find(|(_, item)| names.get(item.name) == name)
        .expect("the type");
    near_misses(snapshot, cx.view, index)
        .into_iter()
        .map(
            |NearMiss {
                 interface,
                 function,
             }| {
                let path = |def: cheby_driver::analysis::DefId| {
                    let module = &snapshot.table().get(def.module).unwrap().path;
                    let item = snapshot
                        .interfaces()
                        .get(def.module)
                        .unwrap()
                        .item(def.index)
                        .unwrap();
                    format!("{module}::{}", names.get(item.name))
                };
                format!(
                    "{}: {}",
                    path(interface),
                    names.get(
                        snapshot
                            .interfaces()
                            .get(function.module)
                            .unwrap()
                            .item(function.index)
                            .unwrap()
                            .name,
                    )
                )
            },
        )
        .collect()
}

const SHAPES: &str = "//! Interfaces of shapes.

pub interface Shape {
  fn area(Self) -> Float
  fn perimeter(Self) -> Float
}

pub interface Named {
  fn name(Self) -> String
  fn tag(Self) -> String
}
";

const MAIN: &str = "//! Types near interfaces.

import app::shapes

interface Solid {
  fn volume(Self) -> Float
  fn mass(Self) -> Float
  fn density(Self) -> Float
}

interface Pair {
  fn left(Self) -> Int
  fn right(Self) -> Int
}

type Cube {
  side: Float,
}

fn area(cube: Cube) -> Float {
  cube.side * cube.side
}

fn volume(cube: Cube) -> Float {
  cube.side
}

fn left(cube: Cube) -> Int {
  1
}

fn right(cube: Cube) -> Int {
  2
}

fn main() {
  assert left(Cube { side: 1.0 }) == right(Cube { side: area(Cube { side: volume(Cube { side: 1.0 }) }) })
}
";

/// A type misses `shapes::Shape`, an interface of a module it imports, by
/// `perimeter` alone; `Solid` by two functions, which is no near miss;
/// `Pair`, which it satisfies, and `Named`, which it has nothing of, neither.
#[test]
fn a_type_misses_an_interface_by_one_function() {
    let fixture = Fixture::package(
        "near",
        &[("main.cheby", MAIN), ("shapes.cheby", SHAPES)],
        &["main.cheby"],
    );
    assert_eq!(
        misses(&fixture, "main.cheby", "Cube"),
        ["app::shapes::Shape: perimeter"]
    );
    let actions = fixture.offered("main.cheby", "type |Cube", provide);
    let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Satisfy interface `shapes::Shape`: add `perimeter`"]
    );
    let after = fixture.applied("main.cheby", actions[0].edit.as_deref().expect("same file"));
    assert!(
        after.contains("fn right(cube: Cube) -> Int {\n  2\n}\n\nfn perimeter(cube: Cube) -> Float {\n  todo\n}\n"),
        "{after}"
    );
}

/// The binary's std is no module of a package's session, so its types have
/// no near misses (D-1700): `std::duration`'s `Duration` has `to_seconds`
/// and lacks `to_fortnights`, so it would miss the package's `Timed` by one
/// function as the package's `Span` does, which gets its near miss (T-6).
#[test]
fn a_std_type_has_no_near_miss() {
    const TIMED: &str = "//! Times.

import std::duration

pub interface Timed {
  fn to_seconds(Self) -> Int
  fn to_fortnights(Self) -> Int
}

type Span {
  n: Int,
}

fn to_seconds(s: Span) -> Int {
  s.n
}

fn main() {
  assert to_seconds(Span { n: duration::to_seconds(duration::seconds(1)) }) == 1
}
";
    let fixture = Fixture::package("near_std", &[("main.cheby", TIMED)], &["main.cheby"]);
    let span = misses(&fixture, "main.cheby", "Span");
    assert!(
        span.iter()
            .any(|miss| miss.ends_with("Timed: to_fortnights")),
        "{span:?}"
    );
    let snapshot = &fixture.analyzed.snapshot;
    let duration = snapshot
        .all_modules()
        .find(|m| m.path().is_some_and(|p| p.to_string() == "std::duration"))
        .expect("std::duration is loaded");
    assert!(!duration.is_own());
    let names = &snapshot.globals().names;
    let iface = duration.interface().unwrap();
    let (ty, _) = iface
        .items
        .iter_enumerated()
        .find(|(_, item)| names.get(item.name) == "Duration")
        .expect("the type `Duration`");
    // A module that is not the session's own has no tree in the snapshot,
    // so the scan finds nothing there even past its first refusal; the
    // package's `Span` above shows that the same interface is found.
    assert_eq!(near_misses(snapshot, duration, ty), []);
}
