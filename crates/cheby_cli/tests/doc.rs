//! `cheby doc <path>` end to end (§13.10, D-1947): the built binary looks
//! modules and items up in std from any directory, and in a package and
//! its path dependency by the paths the package imports them with.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};
use serde_json::Value;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

/// A library with an opaque type, an exposed one, documented functions and
/// a package-visible helper.
const GEOMETRY: &str = "//! Shapes and their areas.

/// A shape. Its variants are hidden.
pub type Shape {
  Square(Int),
  Circle(Int),
}

/// A point, with its fields in view.
pub exposed type Point {
  x: Int,
  y: Int,
}

/// Makes a square with sides of `side`.
pub fn square(side: Int) -> Shape {
  Square(side)
}

/// The area of `shape`. Circles are rounded down.
pub fn area(shape: Shape) -> Int {
  case shape {
    Square(side) => side * side
    Circle(radius) => scaled(radius)
  }
}

fn scaled(radius: Int) -> Int {
  radius * radius * 3
}
";

const MAIN: &str = "//! The program.

import app::util
import geo

fn main() {
  assert util::twice(geo::area(geo::square(2))) == 8
}
";

const UTIL: &str = "//! Small helpers.

/// Twice `n`.
fn twice(n: Int) -> Int {
  doubled(n)
}

priv fn doubled(n: Int) -> Int {
  n + n
}
";

/// A directory with the package `app`, which depends on the library
/// `geometry` next to it under the key `geo`, and a build cache.
struct Packages {
    dir: FreshDir,
    cache: FreshDir,
}

impl Packages {
    fn new() -> Self {
        let packages = Self {
            dir: FreshDir::create("cli-doc").unwrap(),
            cache: FreshDir::create("cli-doc-cache").unwrap(),
        };
        for (file, text) in [
            (
                "geometry/cheby.toml",
                "name = \"geometry\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
            ),
            ("geometry/src/main.cheby", GEOMETRY),
            (
                "app/cheby.toml",
                "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ngeo = { path = \"../geometry\" }\n",
            ),
            ("app/src/main.cheby", MAIN),
            ("app/src/util.cheby", UTIL),
        ] {
            let path = packages.dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        packages
    }

    /// `cheby doc` with `args` in the package `package`.
    fn doc(&self, package: &str, args: &[&str]) -> RunOutput {
        doc_in(&self.dir.path().join(package), self.cache.path(), args)
    }
}

fn doc_in(dir: &Path, cache: &Path, args: &[&str]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "doc"])
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// `cheby doc` outside any package.
fn std_doc(args: &[&str]) -> RunOutput {
    let dir = FreshDir::create("cli-doc-std").unwrap();
    let cache = FreshDir::create("cli-doc-cache").unwrap();
    doc_in(dir.path(), cache.path(), args)
}

/// Std answers from any directory: an item with its whole doc comment, by
/// its full path or without `std::`.
#[test]
fn a_std_item_in_full() {
    let expected = "std::list::map

pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U>

  Returns the results of calling `f` on each element, in order.
";
    for path in ["std::list::map", "list::map"] {
        let out = std_doc(&[path]);
        assert_eq!(out.status, Some(0), "{}", out.stderr);
        assert_eq!(out.stderr, "");
        assert_eq!(out.stdout, expected, "{path}");
    }
}

/// A module is its doc comment, then each item's signature with the first
/// sentence of its doc comment, and `std` lists std's modules.
#[test]
fn a_std_module_in_short() {
    let out = std_doc(&["std::list"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert!(
        out.stdout
            .starts_with("std::list\n\n  Functions for the built-in `List<T>`"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains(
            "\npub fn length<T>(xs: List<T>) -> Int\n    Returns the number of elements in `xs` (D-301).\n\n"
        ),
        "{}",
        out.stdout
    );
    // The builtin type that is homed in the module is listed last, without
    // the module's doc comment a second time.
    assert!(out.stdout.ends_with("\n\ntype List<T>\n"), "{}", out.stdout);
    let modules = std_doc(&["std"]);
    assert!(
        modules
            .stdout
            .contains("std::list\n    Functions for the built-in `List<T>`")
    );
    assert!(modules.stdout.contains("\nstd::option\n"));
    assert!(
        modules
            .stdout
            .lines()
            .filter(|line| line.starts_with("std::"))
            .count()
            > 50
    );
}

/// A prelude name, another language's name for a std function, a name
/// that several modules have, a part of a name, and a name nothing has.
#[test]
fn names_are_searched() {
    let option = std_doc(&["Option"]);
    assert!(
        option.stdout.starts_with(
            "std::option::Option\n\npub exposed type Option<T> {\n  Some(T),\n  None,\n}\n"
        ),
        "{}",
        option.stdout
    );
    let alias = std_doc(&["list::reduce"]);
    assert!(
        alias.stdout.starts_with(
            "std has no `list::reduce`; Cheby's name for it is `list::fold`\n\nstd::list::fold\n\npub fn fold<T, A>("
        ),
        "{}",
        alias.stdout
    );
    let several = std_doc(&["parse"]);
    assert!(
        several.stdout.contains(
            "std::int::parse\n    pub fn parse(text: String) -> Result<Int, Nil>\n        "
        ),
        "{}",
        several.stdout
    );
    assert!(several.stdout.contains("std::float::parse\n"));
    let part = std_doc(&["starts_wi"]);
    assert!(
        part.stdout.contains("std::string::starts_with\n"),
        "{}",
        part.stdout
    );
    let nothing = std_doc(&["no_such_thing_at_all"]);
    assert_eq!(nothing.status, Some(1));
    assert_eq!(nothing.stdout, "");
    assert_eq!(
        nothing.stderr,
        "error: nothing is named `no_such_thing_at_all`\n\
         help: `cheby doc std` lists the modules of the standard library\n"
    );
}

/// A module of the package shows every item, its private ones included.
#[test]
fn the_package_shows_all_its_items() {
    let packages = Packages::new();
    let out = packages.doc("app", &["app::util"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert_eq!(
        out.stdout,
        "app::util

  Small helpers.

fn twice(n: Int) -> Int
    Twice `n`.

priv fn doubled(n: Int) -> Int
"
    );
    let by_name = packages.doc("app", &["twice"]);
    assert_eq!(
        by_name.stdout,
        "app::util::twice\n\nfn twice(n: Int) -> Int\n\n  Twice `n`.\n"
    );
}

/// A dependency is named by its key in the manifest, and shows what the
/// package may name: `pub` items, an opaque type without its variants and
/// an exposed one with its fields. Inside the library itself, every item
/// shows.
#[test]
fn a_dependency_shows_what_the_package_may_name() {
    let packages = Packages::new();
    let out = packages.doc("app", &["geo"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "geo

  Shapes and their areas.

pub type Shape
    A shape.

pub exposed type Point {
  x: Int,
  y: Int,
}
    A point, with its fields in view.

pub fn square(side: Int) -> Shape
    Makes a square with sides of `side`.

pub fn area(shape: Shape) -> Int
    The area of `shape`.
"
    );
    let item = packages.doc("app", &["geo::area"]);
    assert_eq!(
        item.stdout,
        "geo::area\n\npub fn area(shape: Shape) -> Int\n\n  The area of `shape`. Circles are rounded down.\n"
    );
    let hidden = packages.doc("app", &["geo::scaled"]);
    assert_eq!(hidden.status, Some(1), "{}", hidden.stdout);
    let inside = packages.doc("geometry", &["scaled"]);
    assert_eq!(
        inside.stdout,
        "geometry::scaled\n\nfn scaled(radius: Int) -> Int\n"
    );
}

/// `--format json` gives a line for a module and one for each item, with
/// the whole doc comment.
#[test]
fn json_lines() {
    let packages = Packages::new();
    let out = packages.doc("app", &["geo", "--format", "json"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let lines: Vec<Value> = out
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 5, "{}", out.stdout);
    assert_eq!(lines[0]["type"], "module");
    assert_eq!(lines[0]["path"], "geo");
    assert_eq!(lines[0]["origin"], "dependency");
    assert_eq!(lines[0]["doc"], "Shapes and their areas.");
    let area = &lines[4];
    assert_eq!(area["type"], "item");
    assert_eq!(area["module"], "geo");
    assert_eq!(area["name"], "area");
    assert_eq!(area["kind"], "fn");
    assert_eq!(area["signature"], "pub fn area(shape: Shape) -> Int");
    assert_eq!(
        area["doc"],
        "The area of `shape`. Circles are rounded down."
    );
    assert_eq!(area["targets"], serde_json::json!(["native", "js"]));
    assert!(area.get("deprecated").is_none());
    assert_eq!(lines[1]["kind"], "type");
}

/// A package that cannot be read costs a note, and std still answers.
/// Without a path, `cheby doc` is the HTML generator, which M5 brings.
#[test]
fn an_unreadable_package_leaves_std() {
    let dir = FreshDir::create("cli-doc-broken").unwrap();
    let cache = FreshDir::create("cli-doc-cache").unwrap();
    std::fs::write(dir.path().join("cheby.toml"), "name = \n").unwrap();
    let out = doc_in(dir.path(), cache.path(), &["list::length"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert!(
        out.stderr.starts_with(
            "note: the package was not read, so only the standard library is searched: "
        ),
        "{}",
        out.stderr
    );
    assert!(out.stdout.starts_with("std::list::length\n"));
    let html = doc_in(dir.path(), cache.path(), &[]);
    assert_eq!(html.status, Some(1));
    assert_eq!(
        html.stderr,
        "error: `cheby doc` is not implemented yet (planned for M5)\n"
    );
}
