//! Tests of stubs, the bound and debug printing of E0063, "Satisfy
//! interface…" and "Make visible" (X §6.6, D-1684, D-1696, D-1706, D-1736),
//! in process over real snapshots and reports. Each runs only the front
//! end, so it runs on Windows too (D-1516).

use cheby_driver::analysis::PlainEdit;

use super::{ADD_ALL, provide, resolve, resolve_make_visible};
use crate::features::ide::actions::{Action, ActionId, ActionKind};
use crate::features::ide::fill_arms::tests::{Fixture, titled, titles};

/// The edits of `action` at `needle` of `file`: the listing's, or a resolve's.
fn edits_of(fixture: &Fixture, file: &str, needle: &str, action: &Action) -> Vec<PlainEdit> {
    if let Some(edits) = &action.edit {
        return edits.clone();
    }
    let id = action.data.as_ref().expect("an edit or data");
    let cx = fixture.cx(file);
    let range = fixture.range(file, needle);
    match id {
        ActionId::MakeVisible { .. } => resolve_make_visible(&cx, range, id),
        ActionId::Fix { .. }
        | ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::OrganizeImports
        | ActionId::FillArms
        | ActionId::Stub { .. }
        | ActionId::Habit { .. }
        | ActionId::Rename { .. } => resolve(&cx, range, id),
    }
    .unwrap_or_else(|refusal| panic!("{}", refusal.0))
}

const MAIN: &str = "//! Stubs.

import app::geo

interface Area {
  fn area(Self) -> Float
  fn scale(Self, Float) -> Self
}

interface Merge {
  fn merge(Self, Self) -> Self
}

interface Size {
  fn size(Self) -> Int
}

type Square {
  side: Float,
}

type Stack<T> {
  items: List<T>,
}

type Point {
  x: Int,
}

fn measure<T: Area>(x: T) -> Float {
  Area::area(x)
}

fn combine<T: Merge>(x: T) -> T {
  Merge::merge(x, x)
}

fn count<T: Size>(x: T) -> Int {
  Size::size(x)
}

fn label<T>(x: T) -> String {
  \"{x}\"
}

fn main() {
  let square = Square { side: 1.0 }
  let a = measure(square)
  let b = combine(square)
  let c = count(Stack { items: [1] })
  let d = measure(geo::Circle { radius: 1.0 })
  let e = measure(1)
  let origin = Point { x: 0 }
  let f = \"{origin}\"
  assert a == 0.0 && b == square && c == 0 && d == 0.0 && e == 0.0 && f == label(1)
}
";

const GEO: &str = "//! Circles.

pub exposed type Circle {
  radius: Float,
}

pub fn radius(circle: Circle) -> Float {
  circle.radius
}
";

fn stubs() -> Fixture {
    Fixture::package(
        "stubs",
        &[("main.cheby", MAIN), ("geo.cheby", GEO)],
        &["main.cheby"],
    )
}

/// On E0063 for a missing function: one stub per function the module
/// lacks, and "Add all missing functions", written after the type's
/// declaration with the type's visibility, `Self` as the type, a builtin
/// parameter named `float_value`, and `-> Nil` left out (X §6.6).
#[test]
fn missing_functions_get_stubs_in_the_same_file() {
    let fixture = stubs();
    let at = "measure(|square)";
    let actions = fixture.offered("main.cheby", at, provide);
    assert_eq!(
        titles(&actions),
        [
            "Add the missing function `area`",
            "Add the missing function `scale`",
            ADD_ALL,
        ]
    );
    for action in &actions {
        assert_eq!(action.kind, ActionKind::QuickFix);
        assert!(
            action.edit.is_some(),
            "a stub in the request's file is listed with its edit"
        );
    }
    let all = titled(&actions, ADD_ALL);
    let after = fixture.applied("main.cheby", &edits_of(&fixture, "main.cheby", at, all));
    let expected = "type Square {
  side: Float,
}

fn area(square: Square) -> Float {
  todo
}

fn scale(square: Square, float_value: Float) -> Square {
  todo
}
";
    assert!(after.contains(expected), "{after}");
}

/// A second `Self` is `other`, and a generic type's stub declares its
/// parameters (X §6.6).
#[test]
fn a_second_self_and_a_generic_type() {
    let fixture = stubs();
    let merge = fixture.offered("main.cheby", "combine(|square)", provide);
    let fix = titled(&merge, "Add the missing function `merge`");
    let after = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", "combine(|square)", fix),
    );
    assert!(
        after.contains("fn merge(square: Square, other: Square) -> Square {\n  todo\n}\n"),
        "{after}"
    );
    let size = fixture.offered("main.cheby", "count(|Stack", provide);
    assert_eq!(titles(&size), ["Add the missing function `size`"]);
    let after = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", "count(|Stack", &size[0]),
    );
    assert!(
        after.contains("type Stack<T> {\n  items: List<T>,\n}\n\nfn size<T>(stack: Stack<T>) -> Int {\n  todo\n}\n"),
        "{after}"
    );
}

/// A type of another file gets its stubs on resolve, in that file, `pub`
/// as the type is, after the last function that takes it first.
#[test]
fn a_stub_in_another_file_is_resolved() {
    let fixture = stubs();
    let at = "measure(|geo::Circle";
    let actions = fixture.offered("main.cheby", at, provide);
    let all = titled(&actions, ADD_ALL);
    assert_eq!(all.edit, None);
    assert_eq!(
        all.data,
        Some(ActionId::Stub {
            interface: "app::Area".to_owned(),
            function: None,
        })
    );
    let edits = edits_of(&fixture, "main.cheby", at, all);
    assert!(edits.iter().all(|e| e.path == fixture.path("geo.cheby")));
    let after = fixture.applied("geo.cheby", &edits);
    assert!(
        after.ends_with("  circle.radius\n}\n\npub fn area(circle: Circle) -> Float {\n  todo\n}\n\npub fn scale(circle: Circle, float_value: Float) -> Circle {\n  todo\n}\n"),
        "{after}"
    );
}

/// A type of the binary's std, seen from a package, gets the stub
/// disabled (X §6.6, ADR-0023).
#[test]
fn a_std_type_is_refused() {
    let fixture = stubs();
    let actions = fixture.offered("main.cheby", "measure(|1)", provide);
    assert!(!actions.is_empty(), "the stubs are listed, disabled");
    for action in &actions {
        assert_eq!(
            action.disabled.as_deref(),
            Some("`Int` is in `std`: wrap it in a new type (ADR-0023)"),
            "{}",
            action.title
        );
        assert_eq!(action.edit, None);
        assert_eq!(action.data, None);
    }
}

/// On an interpolation: debug printing, and the stub of `show`; on a type
/// parameter: the bound on the enclosing item's parameter (X §6.6).
#[test]
fn an_interpolation_and_a_type_parameter() {
    let fixture = stubs();
    let at = "\"{|origin}\"";
    let actions = fixture.offered("main.cheby", at, provide);
    assert_eq!(
        titles(&actions),
        [
            "Use debug printing `{origin:?}`",
            "Add the missing function `show`",
        ]
    );
    let debug = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", at, &actions[0]),
    );
    assert!(debug.contains("let f = \"{origin:?}\""), "{debug}");
    let show = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", at, &actions[1]),
    );
    assert!(
        show.contains(
            "type Point {\n  x: Int,\n}\n\nfn show(point: Point) -> String {\n  todo\n}\n"
        ),
        "{show}"
    );
    let at = "\"{|x}\"";
    let actions = fixture.offered("main.cheby", at, provide);
    assert_eq!(
        titles(&actions),
        ["Use debug printing `{x:?}`", "Add bound `T: Show`"]
    );
    let bound = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", at, &actions[1]),
    );
    assert!(
        bound.contains("fn label<T: Show>(x: T) -> String {"),
        "{bound}"
    );
}

const CLASH: &str = "//! Two types in one module.

interface Area {
  fn area(Self) -> Float
}

type Round {
  r: Float,
}

type Flat {
  w: Float,
}

fn area(round: Round) -> Float {
  round.r
}

fn measure<T: Area>(x: T) -> Float {
  Area::area(x)
}

fn main() {
  assert measure(Round { r: 1.0 }) == measure(Flat { w: 1.0 })
}
";

/// A name the module uses for a function of another type gets the stub
/// disabled with E0063's help (D-045).
#[test]
fn a_function_of_another_type_disables_the_stub() {
    let fixture = Fixture::package("clash", &[("main.cheby", CLASH)], &["main.cheby"]);
    let actions = fixture.offered("main.cheby", "measure(|Flat", provide);
    assert_eq!(titles(&actions), ["Add the missing function `area`"]);
    assert_eq!(
        actions[0].disabled.as_deref(),
        Some("move `Flat` and its functions to a module of their own")
    );
}

const SHAPE: &str = "//! Shapes.

priv fn secret() -> String {
  \"s\"
}

type Token {
  Token(String),
}

pub fn reveal(t: Token) -> Token {
  t
}

pub type Badge {
  Badge(Int),
}

fn show(badge: Badge) -> String {
  \"badge\"
}
";

const USES: &str = "//! Uses of items another module hides.

import app::shape

fn main() {
  let s = shape::secret()
  let b = shape::Badge(1)
  assert s == \"{b}\"
}
";

/// E0043 raises a `priv` item of another file to package-visible on resolve;
/// E0074 raises the type to what its item needs, in the item's own file;
/// E0063 `LessVisible` raises the satisfying function to the type's
/// visibility (D-164, D-636, D-1736).
#[test]
fn make_visible_raises_what_each_use_needs() {
    let fixture = Fixture::package(
        "visible",
        &[("main.cheby", USES), ("shape.cheby", SHAPE)],
        &["main.cheby", "shape.cheby"],
    );
    let at = "shape::|secret()";
    let actions = fixture.offered("main.cheby", at, provide);
    assert_eq!(titles(&actions), ["Make `secret` package-visible"]);
    assert_eq!(
        actions[0].data,
        Some(ActionId::MakeVisible {
            module: "app::shape".to_owned(),
            item: "secret".to_owned(),
        })
    );
    let after = fixture.applied(
        "shape.cheby",
        &edits_of(&fixture, "main.cheby", at, &actions[0]),
    );
    assert!(after.contains("\nfn secret() -> String {"), "{after}");

    let at = "reveal(t: |Token)";
    let actions = fixture.offered("shape.cheby", at, provide);
    assert_eq!(titles(&actions), ["Make `Token` `pub`"]);
    assert!(
        actions[0].edit.is_some(),
        "the type is in the request's file"
    );
    let after = fixture.applied(
        "shape.cheby",
        &edits_of(&fixture, "shape.cheby", at, &actions[0]),
    );
    assert!(after.contains("\npub type Token {"), "{after}");

    let at = "\"{|b}\"";
    let actions = fixture.offered("main.cheby", at, provide);
    let raise = titled(&actions, "Make `show` `pub`");
    let after = fixture.applied("shape.cheby", &edits_of(&fixture, "main.cheby", at, raise));
    assert!(
        after.contains("\npub fn show(badge: Badge) -> String {"),
        "{after}"
    );
}

const ALIASED: &str = "//! Aliases.

type Token {
  Token(String),
}

type Tok = Token

pub fn reveal(t: Tok) -> String {
  let Token(s) = t
  s
}
";

/// D-1745: E0074 on a type named through an alias raises the type the alias
/// stands for, which the checker reports, not the alias.
#[test]
fn an_alias_raises_the_type_it_stands_for() {
    let fixture = Fixture::package("aliased", &[("main.cheby", ALIASED)], &["main.cheby"]);
    let at = "reveal(t: |Tok)";
    let actions = fixture.offered("main.cheby", at, provide);
    let raise = titled(&actions, "Make `Token` `pub`");
    let after = fixture.applied("main.cheby", &edits_of(&fixture, "main.cheby", at, raise));
    assert!(after.contains("\npub type Token {"), "{after}");
    assert!(after.contains("\ntype Tok = Token\n"), "{after}");
    // A generic alias, whose E0074 covers its arguments too (W11's review,
    // S8).
    let generic = "//! Aliases.\n\ntype Secret {\n  Secret(Int),\n}\n\npub type Other {\n  Other(Int),\n}\n\ntype Pair<T> = (T, Secret)\n\npub fn make(o: Other) -> Pair<Other> {\n  (o, Secret(1))\n}\n";
    let fixture = Fixture::package(
        "aliased_generic",
        &[("main.cheby", generic)],
        &["main.cheby"],
    );
    let at = "-> |Pair<Other>";
    let actions = fixture.offered("main.cheby", at, provide);
    let raise = titled(&actions, "Make `Secret` `pub`");
    let after = fixture.applied("main.cheby", &edits_of(&fixture, "main.cheby", at, raise));
    assert!(after.contains("\npub type Secret {"), "{after}");
}

const ELEMENTS: &str = "//! Elements.

type Point {
  x: Int,
}

fn main() {
  let p = Point { x: 1 }
  let pair = (1, p)
  let points = [p]
  assert \"{pair}\" == \"{points}\"
}
";

/// D-1745: an E0063 reached through a tuple's element or a bound of a
/// satisfying function, here `List`'s `show`, stubs the type at the end of
/// the chain, `Point`.
#[test]
fn a_chain_stubs_its_last_type() {
    let fixture = Fixture::package("elements", &[("main.cheby", ELEMENTS)], &["main.cheby"]);
    for at in ["\"{|pair}", "\"{|points}"] {
        let actions = fixture.offered("main.cheby", at, provide);
        let stub = titled(&actions, "Add the missing function `show`");
        let after = fixture.applied("main.cheby", &edits_of(&fixture, "main.cheby", at, stub));
        assert!(
            after.contains("fn show(point: Point) -> String {\n  todo\n}"),
            "{at}: {after}"
        );
    }
}

const UNUSED: &str = "//! A helper nothing uses.

/// Doubles.
@deprecated(\"use triple\")
fn double(x: Int) -> Int {
  x * 2
}
";

/// W0005's "make `pub`" writes `pub` after the item's docs and attributes.
#[test]
fn an_unused_item_is_made_pub() {
    let fixture = Fixture::package(
        "unused",
        &[
            (
                "main.cheby",
                "//! Root.\n\nfn main() {\n  assert 1 == 1\n}\n",
            ),
            ("helper.cheby", UNUSED),
        ],
        &["main.cheby", "helper.cheby"],
    );
    let at = "fn |double";
    let actions = fixture.offered("helper.cheby", at, provide);
    let raise = titled(&actions, "Make `double` `pub`");
    let after = fixture.applied(
        "helper.cheby",
        &edits_of(&fixture, "helper.cheby", at, raise),
    );
    assert!(
        after.contains("/// Doubles.\n@deprecated(\"use triple\")\npub fn double(x: Int) -> Int {"),
        "{after}"
    );
}

const INNER: &str = "//! A type whose fields this module keeps.

priv type Inner {
  x: Int,
}

type Outer {
  inner: Inner,
}

pub fn make() -> Outer {
  Outer { inner: Inner { x: 1 } }
}
";

/// A step of a field path names a field, not an item, so E0043 there
/// offers no "Make visible" (D-1694).
#[test]
fn a_field_path_step_offers_no_raise() {
    let fixture = Fixture::package(
        "path",
        &[
            (
                "main.cheby",
                "//! Root.\n\nimport app::inner\n\nfn main() {\n  let o = inner::make()\n  let p = inner::Outer { ..o, inner.x: 2 }\n  assert p == o\n}\n",
            ),
            ("inner.cheby", INNER),
        ],
        &["main.cheby"],
    );
    let codes: Vec<String> = fixture
        .diagnostics("main.cheby")
        .iter()
        .map(|d| d.code.to_string())
        .collect();
    assert!(codes.contains(&"E0043".to_owned()), "{codes:?}");
    let actions = fixture.offered("main.cheby", "inner.|x: 2", provide);
    assert!(
        actions.iter().all(|a| !a.title.starts_with("Make ")),
        "{:?}",
        titles(&actions)
    );
}

const PROBE_HIDDEN: &str = "//! A probe: a helper another probe calls, and types to stub.

priv fn secret() -> Int {
  1
}

/// Sized things.
interface Measured {
  fn size(Self) -> Int
}

type Crate {
  n: Int,
}

pub type Parcel {
  n: Int,
}

type Token {
  Token(Int),
}

pub fn reveal(t: Token) -> Token {
  t
}

fn measure<T: Measured>(x: T) -> Int {
  Measured::size(x)
}

test \"measured\" {
  assert measure(Crate { n: 1 }) == measure(Parcel { n: 1 })
}
";

const PROBE_USER: &str = "//! A probe that calls another probe's private helper.

import std::probe_hidden

fn twice() -> Int {
  probe_hidden::secret() * 2
}

test \"twice\" {
  assert twice() == 2
}
";

/// In a std checkout (D-1706): E0043 raises to package-visible on resolve;
/// a package-visible checkout type gets its stub; a stub that would have
/// to be `pub` and a raise to `pub` (E0074) are not offered.
#[test]
fn a_std_checkout_raises_to_package_visible_only() {
    let fixture = Fixture::checkout(
        "fix_vis",
        &[
            ("probe_hidden.cheby", PROBE_HIDDEN),
            ("probe_user.cheby", PROBE_USER),
        ],
    );
    let at = "probe_hidden::|secret()";
    let actions = fixture.offered("probe_user.cheby", at, provide);
    assert_eq!(
        titles(&actions),
        ["Make `secret` package-visible"],
        "{:?}",
        fixture.diagnostics("probe_user.cheby")
    );
    let edits = edits_of(&fixture, "probe_user.cheby", at, &actions[0]);
    let after = fixture.applied("probe_hidden.cheby", &edits);
    assert!(after.contains("\nfn secret() -> Int {"), "{after}");

    let at = "measure(|Crate";
    let actions = fixture.offered("probe_hidden.cheby", at, provide);
    assert_eq!(titles(&actions), ["Add the missing function `size`"]);
    let after = fixture.applied(
        "probe_hidden.cheby",
        &edits_of(&fixture, "probe_hidden.cheby", at, &actions[0]),
    );
    // Package-visible, as the type is: never `pub` (D-1706).
    assert!(
        after.contains("\n\nfn size(crate: Crate) -> Int {\n  todo\n}"),
        "{after}"
    );
    assert!(!after.contains("pub fn size("), "{after}");

    let parcel = fixture.offered("probe_hidden.cheby", "measure(|Parcel", provide);
    assert!(parcel.is_empty(), "a `pub` stub: {:?}", titles(&parcel));
    let token = fixture.offered("probe_hidden.cheby", "reveal(t: |Token)", provide);
    assert!(token.is_empty(), "a raise to `pub`: {:?}", titles(&token));
}
