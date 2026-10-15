//! The module-local warnings (W0002-W0004, W0006, W0009, W0011-W0013)
//! through the whole front end. The package pass (W0005) needs every
//! module's resolution, which the test builds do not keep; the driver
//! tests it.

use crate::test_support::{Build, build, build_with_std};

/// A root module with a `//!` doc line and `body`.
fn module(body: &str) -> String {
    format!("//! Test.\n\n{body}")
}

fn one(body: &str) -> Build {
    build(&[("", &module(body))])
}

/// `message [labels]` of every diagnostic with code `code`, sorted.
fn with_code(b: &Build, code: &str) -> Vec<String> {
    b.messages()
        .into_iter()
        .filter_map(|m| m.strip_prefix(&format!("{code}: ")).map(str::to_owned))
        .collect()
}

/// The codes of the errors only.
fn errors(b: &Build) -> Vec<String> {
    b.codes()
        .into_iter()
        .filter(|c| c.starts_with('E'))
        .collect()
}

// W0002

#[test]
fn unused_bindings_of_every_kind_warn() {
    let b = one("pub fn f(used: Int, unused: Int) -> Int {\n  \
         let a = used\n  \
         let (b, _) = (1, 2)\n  \
         let g = fn(p: Int, q: Int) { q }\n  \
         fn h(r: Int) -> Int {\n    r\n  }\n  \
         case Some(a) {\n    Some(c) => g(h(1), 2)\n    None => 0\n  }\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0002"),
        [
            "unused binding `b` [\"never used\"]",
            "unused binding `c` [\"never used\"]",
            "unused parameter `p` [\"never used\"]",
            "unused parameter `unused` [\"never used\"]",
        ]
    );
}

#[test]
fn underscore_names_never_warn() {
    let b = one("pub fn f(_unused: Int) -> Int {\n  \
         let _x = 1\n  \
         let _ = 2\n  \
         let g = fn(_p: Int) { 3 }\n  \
         g(4)\n}\n");
    assert_eq!(b.codes(), Vec::<String>::new(), "{}", b.rendered());
}

#[test]
fn use_binders_and_local_functions_count() {
    let b = one("fn wrap(k: fn(Int) -> Int) -> Int {\n  k(1)\n}\n\n\
         pub fn f() -> Int {\n  \
         fn again(n: Int) -> Int {\n    again(n)\n  }\n  \
         fn called(n: Int) -> Int {\n    n\n  }\n  \
         use x <- wrap\n  \
         called(2)\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0002"),
        [
            "unused binding `x` [\"never used\"]",
            "unused local function `again` [\"used only inside its own body\"]",
        ]
    );
}

#[test]
fn a_punned_field_suggests_ignoring_it() {
    let b = one("pub type Point {\n  x: Int,\n  y: Int,\n}\n\n\
         pub fn f(p: Point) -> Int {\n  \
         let Point { x, y } = p\n  \
         y\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert!(
        b.rendered()
            .contains("help: if this is intentional, ignore the field: `x: _`"),
        "{}",
        b.rendered()
    );
}

#[test]
fn bodiless_functions_have_no_locals() {
    let b = one("@external(native, \"cheby_test_ext\")\npub fn ext(n: Int) -> Int\n");
    assert_eq!(with_code(&b, "W0002"), Vec::<String>::new());
}

// W0003

#[test]
fn unused_imports_warn() {
    let int = "//! Ints.\n\npub fn show(n: Int) -> String {\n  case n {\n    _ => \"n\"\n  }\n}\n\n\
               pub fn double(n: Int) -> Int {\n  n\n}\n\npub fn half(n: Int) -> Int {\n  n\n}\n";
    let b = build_with_std(
        &[("int", int)],
        &[
            (
                "",
                &module(
                    "import app::util\nimport std::int::{double, half as halve}\n\n\
                     pub fn f(n: Int) -> Int {\n  double(n)\n}\n",
                ),
            ),
            ("util", "//! Util.\n\npub fn g() -> Int {\n  1\n}\n"),
        ],
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0003"),
        [
            "unused import `app::util` [\"nothing in this module uses it\"]",
            "unused import `half as halve` from `std::int` [\"nothing in this module uses it\"]",
        ]
    );
}

#[test]
fn every_kind_of_path_uses_an_import() {
    let util = "//! Util.\n\npub type Shape {\n  Dot,\n}\n\npub interface Named {\n  fn name(Self) -> String\n}\n\n\
                pub fn g() -> Int {\n  1\n}\n\npub const k: Int = 2\n";
    let b = build(&[
        (
            "",
            &module(
                "import app::util\nimport app::util as u\nimport app::util::{Shape}\nimport app::util::{k}\n\
                 import app::util::{Named}\n\n\
                 pub fn f(s: util::Shape) -> Int {\n  u::g()\n}\n\n\
                 pub fn h(s: Shape) -> String {\n  \"{k:?}\"\n}\n\n\
                 pub fn n<T: Named>(t: T) -> String {\n  Named::name(t)\n}\n",
            ),
        ),
        ("util", util),
    ]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(with_code(&b, "W0003"), Vec::<String>::new());
}

#[test]
fn satisfaction_does_not_use_an_import() {
    // `Int` gets its `show` from its home module without an import
    // (D-447, §7.6), so an import of `std::int` that nothing names is
    // unused although the interpolation calls `int::show`. D-762 leaves a
    // module of the local package so.
    let int = "//! Ints.\n\npub fn show(n: Int) -> String {\n  case n {\n    _ => \"n\"\n  }\n}\n";
    let b = build_with_std(
        &[("int", int)],
        &[(
            "",
            &module("import std::int\n\npub fn f(n: Int) -> String {\n  \"{n}\"\n}\n"),
        )],
    );
    assert_eq!(
        with_code(&b, "W0003"),
        ["unused import `std::int` [\"nothing in this module uses it\"]"],
        "{}",
        b.rendered()
    );
}

/// A fake `std::int` with what satisfaction takes from it: `show` for
/// interpolation, `compare` for `Compare`, and `double` to name.
const STD_INT: &str = "//! Ints.\n\npub fn show(n: Int) -> String {\n  case n {\n    _ => \"n\"\n  }\n}\n\n\
                       pub fn compare(a: Int, b: Int) -> Order {\n  case a == b {\n    _ => Equal\n  }\n}\n\n\
                       pub fn double(n: Int) -> Int {\n  n\n}\n";

/// A fake `std::ops` with `Compare`, and a fake `std::list` with a `sort`
/// bounded by it.
const STD_OPS: &str =
    "//! Operators.\n\npub interface Compare {\n  fn compare(Self, Self) -> Order\n}\n";
const STD_LIST: &str = "//! Lists.\n\nimport std::ops::{Compare}\n\n\
                        pub fn sort<T: Compare>(xs: List<T>) -> List<T> {\n  xs\n}\n";

/// Builds the fake std modules with `std::shown`, whose source is
/// `shown`, and a local root module that calls `shown::label`.
fn std_module(shown: &str) -> Build {
    build_with_std(
        &[
            ("int", STD_INT),
            ("ops", STD_OPS),
            ("list", STD_LIST),
            ("shown", &format!("//! Shown.\n\n{shown}")),
        ],
        &[(
            "",
            &module("import std::shown\n\npub fn f() -> String {\n  shown::label(1)\n}\n"),
        )],
    )
}

/// `label` interpolating an `Int`, the only use of `std::int`.
const INTERPOLATES: &str = "pub fn label(n: Int) -> String {\n  \"{n}\"\n}\n";

#[test]
fn satisfaction_uses_a_home_module_import_in_std() {
    // A std module depends only on what it imports (D-447): without
    // `import std::int` the interpolation is E0063 (D-489), so with it the
    // import is used (D-762), aliased or not.
    let without = std_module(INTERPOLATES);
    assert_eq!(errors(&without), ["E0063"], "{}", without.rendered());
    for import in ["import std::int", "import std::int as ints"] {
        let b = std_module(&format!("{import}\n\n{INTERPOLATES}"));
        assert_eq!(
            b.codes(),
            Vec::<String>::new(),
            "{import}: {}",
            b.rendered()
        );
    }
}

#[test]
fn every_satisfaction_uses_a_home_module_import_in_std() {
    // `Compare` for `list::sort` over `Int` takes `int::compare` (D-762:
    // every satisfaction, not only interpolation).
    let body = "import std::int\nimport std::list\n\n\
                pub fn label(n: Int) -> String {\n  case list::sort([n, 1]) {\n    _ => \"sorted\"\n  }\n}\n";
    let b = std_module(body);
    assert_eq!(b.codes(), Vec::<String>::new(), "{}", b.rendered());
    let without = std_module(&body.replacen("import std::int\n", "", 1));
    assert_eq!(errors(&without), ["E0063"], "{}", without.rendered());
}

#[test]
fn satisfaction_uses_an_item_import_of_a_home_module_in_std() {
    // An item import makes its module a dependency like a module import
    // (the driver adds a graph edge for every import's module), so
    // removing `import std::int::{double}` would give E0063 (D-489): it is
    // used when none of its names is written
    // (GAP(satisfaction-item-imports), D-762). With a written name, the
    // module stays a dependency without the other names, which stay unused.
    let b = std_module(&format!("import std::int::{{double}}\n\n{INTERPOLATES}"));
    assert_eq!(b.codes(), Vec::<String>::new(), "{}", b.rendered());
    let b = std_module(
        "import std::int::{double, show}\n\n\
         pub fn label(n: Int) -> String {\n  let m = double(n)\n  \"{m}\"\n}\n",
    );
    assert_eq!(
        with_code(&b, "W0003"),
        ["unused import `show` from `std::int` [\"nothing in this module uses it\"]"],
        "{}",
        b.rendered()
    );
}

#[test]
fn an_unrelated_std_import_stays_unused() {
    // Only an import of a home module that satisfaction takes a function
    // from is used: `std::list` here serves nothing (D-762, D-515).
    let b = std_module(&format!(
        "import std::int\nimport std::list\n\n{INTERPOLATES}"
    ));
    assert_eq!(
        with_code(&b, "W0003"),
        ["unused import `std::list` [\"nothing in this module uses it\"]"],
        "{}",
        b.rendered()
    );
}

#[test]
fn an_item_import_with_nothing_used_is_one_warning() {
    let b = build(&[
        (
            "",
            &module("import app::util::{g, k}\n\npub fn f() -> Int {\n  1\n}\n"),
        ),
        (
            "util",
            "//! Util.\n\npub fn g() -> Int {\n  1\n}\n\npub const k: Int = 2\n",
        ),
    ]);
    assert_eq!(
        with_code(&b, "W0003"),
        ["unused import `app::util::{g, k}` [\"nothing in this module uses it\"]"]
    );
}

// W0004

#[test]
fn unused_private_items_warn() {
    let b = one("priv fn twice(n: Int) -> Int {\n  twice(n) * 2\n}\n\n\
         priv fn used(n: Int) -> Int {\n  n\n}\n\n\
         priv fn by_test() -> Int {\n  1\n}\n\n\
         priv const limit: Int = 3\n\n\
         priv type Color {\n  Red,\n  Green,\n}\n\n\
         priv type Unused {\n  Unused,\n}\n\n\
         priv type Alias = Int\n\n\
         priv interface Named {\n  fn name(Self) -> String\n}\n\n\
         pub fn f() -> Int {\n  \
         let _ = Red\n  \
         used(1)\n}\n\n\
         test \"by test\" {\n  assert by_test() == 1\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0004"),
        [
            "the `priv` constant `limit` is never used [\"never used in this module\"]",
            "the `priv` function `twice` is never used [\"never used in this module\"]",
            "the `priv` interface `Named` is never used [\"never used in this module\"]",
            "the `priv` type `Unused` is never used [\"never used in this module\"]",
            "the `priv` type alias `Alias` is never used [\"never used in this module\"]",
        ]
    );
}

#[test]
fn satisfying_functions_are_used_through_dictionaries() {
    let b = one("priv type Angle {\n  Angle(Int),\n}\n\n\
         priv fn show(a: Angle) -> String {\n  \
         let Angle(n) = a\n  \
         case n {\n    _ => \"angle\"\n  }\n}\n\n\
         pub fn f() -> String {\n  let a = Angle(1)\n  \"{a}\"\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(with_code(&b, "W0004"), Vec::<String>::new());
}

#[test]
fn package_and_pub_items_are_not_module_warnings() {
    let b = one(
        "fn package_visible() -> Int {\n  1\n}\n\npub fn public() -> Int {\n  2\n}\n\n\
         fn main() {\n  Nil\n}\n",
    );
    assert_eq!(b.codes(), Vec::<String>::new(), "{}", b.rendered());
}

// W0006

#[test]
fn a_discarded_result_warns() {
    let b = one("fn save(name: String) -> Result<Nil, String> {\n  \
         case name {\n    \"\" => Err(\"empty\")\n    _ => Ok(Nil)\n  }\n}\n\n\
         pub fn f() -> Result<Nil, String> {\n  \
         save(\"a\")\n  \
         let _ = save(\"b\")\n  \
         1\n  \
         save(\"c\")\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0006"),
        ["this `Result` is discarded [\"this `Result<Nil, String>` may hold an error\"]"]
    );
}

// W0009

#[test]
fn todo_warns_everywhere() {
    let b = one("pub fn f(n: Int) -> Int {\n  \
         case n {\n    0 => todo\n    _ => todo as \"later\"\n  }\n}\n\n\
         pub const c: Int = todo\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(with_code(&b, "W0009").len(), 3, "{}", b.rendered());
}

// W0011

#[test]
fn equality_on_functions_and_handles_warns() {
    let b = one(
        "pub type Handler {\n  name: String,\n  run: fn(Int) -> Int,\n}\n\n\
         pub type Wrapper<T> {\n  Wrapper(T),\n}\n\n\
         pub type Tree {\n  Leaf,\n  Node(List<Tree>, Int),\n}\n\n\
         @external(native, \"cheby_test_db_close\")\npub type Db\n\n\
         pub fn f(a: Handler, b: Handler, g: fn() -> Nil, t: Tree, d: Db) -> Bool {\n  \
         a == b && g != g && [g] == [g] && Wrapper(g) == Wrapper(g) && t == t && d == d \
         && Wrapper(1) == Wrapper(2) && (1, \"x\") == (1, \"y\")\n}\n\n\
         pub fn generic<T>(x: T, y: T) -> Bool {\n  x == y\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0011"),
        [
            "`!=` on a type that contains a function [\"`fn() -> Nil` is a function type\"]",
            "`==` on a type that contains a function [\"`Handler` contains a function type\"]",
            "`==` on a type that contains a function [\"`List<fn() -> Nil>` contains a function type\"]",
            "`==` on a type that contains a function [\"`Wrapper<fn() -> Nil>` contains a function type\"]",
            "`==` on a type that contains a handle [\"`Db` is an external type\"]",
        ]
    );
}

// W0012

#[test]
fn deprecated_items_warn_in_other_modules() {
    let text = "//! Text.\n\n@deprecated(\"use `shout`\")\npub fn yell(s: String) -> String {\n  s\n}\n\n\
                pub fn shout(s: String) -> String {\n  yell(s)\n}\n\n\
                @deprecated(\"use `Level`\")\npub type Volume {\n  Loud,\n  Quiet,\n}\n";
    let b = build(&[
        (
            "",
            &module(
                "import app::text\nimport app::text::{Loud}\n\n\
                 pub fn f(v: text::Volume) -> String {\n  \
                 let _ = Loud\n  \
                 text::yell(\"a\")\n}\n",
            ),
        ),
        ("text", text),
    ]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0012"),
        [
            "use of deprecated constructor `Loud`: use `Level` [\"deprecated\"]",
            "use of deprecated function `text::yell`: use `shout` [\"deprecated\"]",
            "use of deprecated type `text::Volume`: use `Level` [\"deprecated\"]",
        ]
    );
}

// W0013

const LIST: &str = "//! Lists.\n\n\
                    @std_clamped(count)\npub fn take(items: List<Int>, count: Int) -> List<Int> {\n  \
                    case count {\n    _ => items\n  }\n}\n";

#[test]
fn negative_counts_warn() {
    let b = build_with_std(
        &[("list", LIST)],
        &[(
            "",
            &module(
                "import std::list\n\n\
                 pub fn f(xs: List<Int>, n: Int) -> List<Int> {\n  \
                 let a = list::take(xs, -1)\n  \
                 let b = list::take(xs, 2)\n  \
                 let c = list::take(xs, n)\n  \
                 let d = list::take(_, -2)\n  \
                 let e = xs |> list::take(_, -3)\n  \
                 let _ = (a, b, c, d, e)\n  \
                 list::take(xs, 0 - 1)\n}\n",
            ),
        )],
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    assert_eq!(
        with_code(&b, "W0013"),
        [
            "negative `count` passed to `take` [\"this `count` is negative\"]",
            "negative `count` passed to `take` [\"this `count` is negative\"]",
            "negative `count` passed to `take` [\"this `count` is negative\"]",
        ]
    );
}

// The pass as a whole

#[test]
fn warnings_are_sorted_and_stable() {
    let source = "pub fn f(a: Int, b: Int) -> Int {\n  let c = 1\n  todo\n}\n";
    let first = one(source).rendered();
    let second = one(source).rendered();
    assert_eq!(first, second);
    let a = first.find("`a`").unwrap();
    let c = first.find("`c`").unwrap();
    assert!(a < c, "{first}");
}

#[test]
fn imports_of_planned_modules_are_not_unused() {
    // The import is E0038 (D-451) and paths through it are silent; it is
    // not also reported as unused, but the unused parameter still is.
    // `std::http` stays unshipped until tier 3 (D-729).
    let b = one("import std::http\n\npub fn f(unused: Int) {\n  http::get(\"x\")\n}\n");
    assert_eq!(b.codes(), ["E0038", "W0002"], "{}", b.rendered());
    assert_eq!(with_code(&b, "W0003"), Vec::<String>::new());
}
