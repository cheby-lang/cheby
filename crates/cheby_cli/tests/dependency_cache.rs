//! The cache and dependencies (D-1896 to D-1898): after each of many edits,
//! every command on a cache that has seen every earlier state gives what it
//! gives on a cache that has seen none of the packages. The program ends in a
//! panic inside a dependency, so a stale location shows too.
//!
//! The reference cache of each step is a copy of one that holds `std` only,
//! so `std` is compiled once per test.
//!
//! Programs do not run on Windows until M6, so this file is not built there
//! (D-545).
#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::git_fixture;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(120);

/// Every command whose result a cache may change, the executable's run last.
const COMMANDS: &[&[&str]] = &[
    &["check"],
    &["run"],
    &["run", "--release"],
    &["test"],
    &["build"],
];

/// The same in a library, whose own build (D-1852) shares records with
/// `cheby run` of one of its modules (D-1899).
const LIBRARY_COMMANDS: &[&[&str]] = &[
    &["check"],
    &["build"],
    &["run", "geometry::demo"],
    &["build", "geometry::demo", "-o", "../demo"],
    &["test"],
    &["build", "--release"],
];

struct World {
    dir: PathBuf,
    /// The directory of the package that commands run in.
    root: &'static str,
    /// The cache that keeps its entries from step to step.
    warm: FreshDir,
    /// A cache with `std` alone.
    primed: FreshDir,
}

#[expect(clippy::unwrap_used, reason = "test helpers")]
impl World {
    fn new(name: &str) -> Self {
        Self::rooted(name, "app")
    }

    fn rooted(name: &str, root: &'static str) -> Self {
        let dir = Path::new(TMP).join("dependency_cache").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("repos")).unwrap();
        let world = Self {
            dir,
            root,
            warm: FreshDir::create("cheby-dep-warm").unwrap(),
            primed: FreshDir::create("cheby-dep-primed").unwrap(),
        };
        world.write(
            "gitconfig",
            &format!(
                "[url \"file://{}/repos/\"]\n\tinsteadOf = https://packages.example/\n[user]\n\tname = T\n\temail = t@packages.example\n[init]\n\tdefaultBranch = main\n",
                world.dir.display()
            ),
        );
        // Every std module the packages below use, in both profiles.
        world.write(
            "prime/cheby.toml",
            "name = \"prime\"\nversion = \"0.1.0\"\n",
        );
        world.write(
            "prime/src/main.cheby",
            "//! P.\n\nimport std::int\nimport std::io\nimport std::list\n\nfn main() {\n  io::println(int::show(list::length([1])))\n}\n\ntest \"t\" {\n  assert 1 == 1\n}\n",
        );
        for &args in COMMANDS {
            let out = world.cheby("prime", args, world.primed.path());
            assert_eq!(out.status, Some(0), "{}", out.stderr);
        }
        world
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir.join(path)).unwrap()
    }

    /// Replaces `from`, which must be there, with `to` in a file.
    fn edit(&self, path: &str, from: &str, to: &str) {
        let text = self.read(path);
        assert!(text.contains(from), "{path} has no {from:?}:\n{text}");
        self.write(path, &text.replace(from, to));
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("NO_COLOR", "1");
        git_fixture::isolate(cmd, &self.dir.join("gitconfig"));
    }

    fn cheby(&self, package: &str, args: &[&str], cache: &Path) -> RunOutput {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(self.dir.join(package))
            .env(CACHE_ENV, cache);
        self.env(&mut cmd);
        process::run(cmd, None, TIMEOUT).unwrap()
    }

    /// The lookups and parses of package modules, the root's left out in
    /// `app`, of a command on the warm cache.
    fn log(&self, args: &[&str], off: bool) -> Vec<String> {
        let path = self.dir.join("log.txt");
        let mut cmd = Command::new(CHEBY);
        cmd.args(args)
            .current_dir(self.dir.join(self.root))
            .env(CACHE_ENV, self.warm.path())
            .env("CHEBY_CACHE_LOG", &path);
        if off {
            cmd.env("CHEBY_DEP_CACHE", "off");
        }
        self.env(&mut cmd);
        process::run(cmd, None, TIMEOUT).unwrap();
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|line| {
                !line.starts_with("key ")
                    && !line.contains(" std")
                    && (self.root != "app" || line.contains('#'))
            })
            .map(str::to_owned)
            .collect()
    }

    /// What every command gives on `cache`, and the executable's run. With
    /// `reversed`, the last command runs first.
    fn results(&self, cache: &Path, reversed: bool) -> Vec<(String, RunOutput)> {
        let (commands, exe) = if self.root == "app" {
            (COMMANDS, "app/build/native/app")
        } else {
            (LIBRARY_COMMANDS, "demo")
        };
        let exe = self.dir.join(exe);
        let _ = std::fs::remove_file(&exe);
        let run = |args: &&[&str]| (args.join(" "), self.cheby(self.root, args, cache));
        let mut out: Vec<(String, RunOutput)> = if reversed {
            let mut out: Vec<_> = commands.iter().rev().map(run).collect();
            out.reverse();
            out
        } else {
            commands.iter().map(run).collect()
        };
        if exe.is_file() {
            let run = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
            out.push(("the executable".to_owned(), run));
        }
        out
    }

    /// The two caches agree after the edit that `step` names. Returns what
    /// `cheby run` gave. On the cold cache the commands run in the opposite
    /// order, so no record is written and read by the same two.
    fn agree(&self, step: &str) -> RunOutput {
        let cold = FreshDir::create("cheby-dep-cold").unwrap();
        copy(self.primed.path(), cold.path());
        let fetched = self.warm.path().join("pkg");
        if fetched.is_dir() {
            copy(&fetched, &cold.path().join("pkg"));
        }
        let expected = self.results(cold.path(), true);
        // Twice, so a result of entries that this step wrote is seen too.
        for pass in ["first", "second"] {
            let found = self.results(self.warm.path(), false);
            assert_eq!(found.len(), expected.len(), "{step}: {found:?}");
            for ((name, expected), (_, found)) in expected.iter().zip(found) {
                assert_eq!(
                    (&found.stdout, &found.stderr, found.status),
                    (&expected.stdout, &expected.stderr, expected.status),
                    "{step}: {name}, {pass} time warm (left) against cold (right)"
                );
            }
        }
        let mut runs = expected
            .into_iter()
            .filter(|(name, _)| name.starts_with("run"));
        runs.next().unwrap().1
    }

    /// [`Self::agree`], and nothing runs: standard error holds `text`.
    fn fails(&self, step: &str, text: &str) {
        let run = self.agree(step);
        assert_eq!((run.stdout.as_str(), run.status), ("", Some(1)), "{step}");
        assert!(run.stderr.contains(text), "{step}: {}", run.stderr);
    }

    fn commit(&self, repo: &str, tag: &str, files: &[(&str, &str)]) {
        let dir = self.dir.join("repos").join(repo);
        for (path, text) in files {
            self.write(&format!("repos/{repo}/{path}"), text);
        }
        if !dir.join(".git").exists() {
            self.git(repo, &["init", "--quiet"]);
        }
        self.git(repo, &["add", "--all"]);
        self.git(repo, &["commit", "--quiet", "--allow-empty", "-m", tag]);
        self.git(repo, &["tag", "--force", tag]);
    }

    fn git(&self, repo: &str, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(self.dir.join("repos").join(repo));
        self.env(&mut cmd);
        let out = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(out.status, Some(0), "git {args:?}: {}", out.stderr);
    }
}

#[expect(clippy::unwrap_used, reason = "a test helper")]
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

const APP: &str = "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n[dependencies]\ngeo = { path = \"../geometry\" }\n";

const MAIN: &str = r#"//! App.

import std::io

import geo
import geo::shape

fn main() {
  let a = shape::area(shape::square(2))
  let b = geo::unit()
  let c = shape::origin.x
  let d = shape::apply(5, fn(n) { n + 1 })
  io::println("{a} {b} {c} {d}")
  let opaque = shape::square(3)
  io::println("{opaque:?}")
  shape::boom()
}

test "app" {
  assert geo::unit() >= 0
}
"#;

const GEOMETRY: &str = "name = \"geometry\"\nversion = \"0.1.0\"\nkind = \"library\"\ntargets = [\"native\"]\n\n[dependencies]\nu = { path = \"../units\" }\n";

const GEO_MAIN: &str = "//! Geometry.

import geometry::shape

/// The area of a unit square.
pub fn unit() -> Int {
  shape::area(shape::square(1))
}
";

const SHAPE: &str = r#"//! Shapes.

import std::list

import u
import u::parts

/// A shape.
pub type Shape {
  Square(Int),
}

/// A point.
pub exposed type Point {
  Point { x: Int, y: Int },
}

/// The origin.
pub const origin: Point = Point { x: u::base + 2, y: list::length([1, 2]) }

/// A square.
pub fn square(side: Int) -> Shape {
  Square(side)
}

/// The area.
pub fn area(s: Shape) -> Int {
  case s {
    Square(side) => u::scale(helper(side))
  }
}

fn helper(side: Int) -> Int {
  side * side
}

/// Applies `f`.
pub fn apply<T>(value: T, f: fn(T) -> T) -> T {
  f(parts::same(value))
}

/// Ends the program.
pub fn boom() {
  panic as "boom"
}

test "shapes" {
  assert area(square(1)) > 0
}
"#;

const UNITS: &str =
    "name = \"units\"\nversion = \"1.2.3\"\nkind = \"library\"\ntargets = [\"native\"]\n";

const UNITS_MAIN: &str = "//! Units.

import units::parts

/// A base.
pub const base: Int = 1 + 4

/// Scales.
pub fn scale(x: Int) -> Int {
  x * 3 + parts::twice([0])
}
";

const PARTS: &str = "//! Parts.

/// Its argument.
pub fn same<T>(value: T) -> T {
  value
}

fn twice(xs: List<Int>) -> Int {
  case xs {
    [first, ..] => first * 2
    [] => 0
  }
}

/// Twice the first.
pub fn doubled(n: Int) -> Int {
  twice([n])
}
";

/// The lines of a cache log that are not hits, in order.
fn misses(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .filter(|line| !line.ends_with(" hit"))
        .cloned()
        .collect();
    out.sort();
    out
}

/// `app` over `geometry` over `units`, all by path.
fn by_path(name: &str) -> World {
    packages_by_path(World::new(name))
}

fn packages_by_path(world: World) -> World {
    for (path, text) in [
        ("app/cheby.toml", APP),
        ("app/src/main.cheby", MAIN),
        ("geometry/cheby.toml", GEOMETRY),
        ("geometry/src/main.cheby", GEO_MAIN),
        ("geometry/src/shape.cheby", SHAPE),
        ("units/cheby.toml", UNITS),
        ("units/src/main.cheby", UNITS_MAIN),
        ("units/src/parts.cheby", PARTS),
    ] {
        world.write(path, text);
    }
    world
}

/// The first line is what matters below: the second shows how an opaque value
/// prints.
fn line(run: &RunOutput) -> &str {
    run.stdout.lines().next().unwrap_or("")
}

/// A warm command takes every dependency module from its records, and an edit
/// misses only the module edited, its readers included when its interface
/// changed (D-1896). Without this, the tests below hold for a compiler that
/// caches no dependency.
#[test]
fn warm_commands_reuse_dependencies() {
    let world = by_path("reuse");
    let log = |args: &[&str], off: bool| world.log(args, off);
    for args in [&["check"][..], &["run"], &["test"], &["build"]] {
        log(args, false);
        let warm = log(args, false);
        assert!(
            warm.iter().any(|l| l == "front geometry#0.1::shape hit"),
            "{args:?}: {warm:?}"
        );
        assert_eq!(misses(&warm), [""; 0], "{args:?}");
    }
    let code = log(&["run"], false);
    for module in [
        "geometry#0.1",
        "geometry#0.1::shape",
        "units#1",
        "units#1::parts",
    ] {
        assert!(code.contains(&format!("code.run {module} hit")), "{code:?}");
    }
    world.edit("units/src/parts.cheby", "first * 2", "first * 2 + 0");
    assert_eq!(
        misses(&log(&["run"], false)),
        [
            "code.run units#1::parts miss",
            "facts units#1::parts miss",
            "front units#1::parts miss",
            "masks units#1::parts miss",
            "parse units#1::parts",
        ]
    );
    world.edit("units/cheby.toml", "1.2.3", "1.2.4");
    let bumped = misses(&log(&["run"], false));
    // The index vouches for sources, not versions, so this command starts
    // again with every module parsed (D-1256).
    for module in ["units#1", "units#1::parts"] {
        assert!(
            bumped.contains(&format!("code.run {module} miss")),
            "{bumped:?}"
        );
    }
    assert!(
        !bumped
            .iter()
            .any(|l| l.contains("geometry#0.1::shape") && l.starts_with("code")),
        "a package's version is in the code of its own modules only: {bumped:?}"
    );
    let off = log(&["run"], true);
    // A constant's entry is keyed by the sources it reaches, whatever the
    // package (D-977), and stays.
    assert!(
        !off.iter()
            .any(|l| l.ends_with(" hit") && !l.starts_with("const ")),
        "{off:?}"
    );
}

const SHAPE_FILE: &str = "geometry/src/shape.cheby";
const UNITS_FILE: &str = "units/src/main.cheby";
const PARTS_FILE: &str = "units/src/parts.cheby";

/// An edit: what it is, the file, the text replaced, its replacement and the
/// first line the program then prints, `!` when it does not build.
type Edit = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

const BODY_EDITS: &[Edit] = &[
    (
        "a private body",
        SHAPE_FILE,
        "side * side",
        "side * side + 1",
        "15 6 7 6",
    ),
    (
        "a body two packages away",
        UNITS_FILE,
        "x * 3",
        "x * 5",
        "20 5 7 6",
    ),
    (
        "a constant",
        SHAPE_FILE,
        "u::base + 2",
        "u::base + 3",
        "12 3 8 6",
    ),
    (
        "a constant of another package",
        UNITS_FILE,
        "1 + 4",
        "1 + 5",
        "12 3 8 6",
    ),
    (
        "a constant made a literal",
        UNITS_FILE,
        "1 + 4",
        "40",
        "12 3 42 6",
    ),
    (
        "a generic body",
        SHAPE_FILE,
        "f(parts::same(value))",
        "f(f(parts::same(value)))",
        "12 3 7 7",
    ),
    (
        "a comment above",
        SHAPE_FILE,
        "/// The area.",
        "// A comment.\n/// The area.",
        "12 3 7 6",
    ),
    (
        "a function added above",
        SHAPE_FILE,
        "/// The area.",
        "fn zero() -> Int {\n  0\n}\n\n/// The area.",
        "12 3 7 6",
    ),
    (
        "a broken test",
        SHAPE_FILE,
        "area(square(1)) > 0",
        "area(1) == \"x\"",
        "12 3 7 6",
    ),
    (
        "an unused import",
        SHAPE_FILE,
        "import std::list\n",
        "import std::int\nimport std::list\n",
        "12 3 7 6",
    ),
    (
        "a panic message",
        SHAPE_FILE,
        "\"boom\"",
        "\"bang\"",
        "12 3 7 6",
    ),
    (
        "a call of a function that borrows",
        SHAPE_FILE,
        "helper(side))",
        "helper(side)) + parts::doubled(1)",
        "14 5 7 6",
    ),
    (
        "a body that consumes instead",
        PARTS_FILE,
        "  case xs {\n    [first, ..] => first * 2",
        "  let held = [xs, [1]]\n  case held {\n    [[first, ..], ..] => first * 2\n    [_, ..] => 0",
        "12 3 7 6",
    ),
];

const INTERFACE_EDITS: &[Edit] = &[
    (
        "a type's representation",
        SHAPE_FILE,
        "Square(Int),",
        "Circle(Int),\n  Square(Int),",
        "!",
    ),
    (
        "a field added",
        SHAPE_FILE,
        "Point { x: Int, y: Int },",
        "Point { w: Int, x: Int, y: Int },",
        "!",
    ),
    (
        "a signature",
        UNITS_FILE,
        "scale(x: Int)",
        "scale(x: String)",
        "!",
    ),
    (
        "an item made package-visible",
        SHAPE_FILE,
        "pub fn boom",
        "fn boom",
        "!",
    ),
    (
        "an item removed",
        SHAPE_FILE,
        "pub fn apply<T>",
        "pub fn applied<T>",
        "!",
    ),
    (
        "an opaque type exposed",
        SHAPE_FILE,
        "pub type Shape",
        "pub exposed type Shape",
        "12 3 7 6",
    ),
    (
        "a bound added",
        PARTS_FILE,
        "same<T>(",
        "same<T: Show>(",
        "!",
    ),
    (
        "a syntax error",
        PARTS_FILE,
        "pub fn same",
        "pub fn fn same",
        "!",
    ),
    (
        "an import that is missing",
        SHAPE_FILE,
        "import u::parts",
        "import u::missing",
        "!",
    ),
    (
        "an import cycle",
        PARTS_FILE,
        "//! Parts.\n",
        "//! Parts.\n\nimport units\n",
        "!",
    ),
];

/// Makes each of `edits`, and undoes it before the next.
fn check_edits(world: &World, edits: &[Edit]) {
    let first = world.agree("the first build");
    assert_eq!(line(&first), "12 3 7 6", "{}", first.stderr);
    for &(step, path, from, to, expected) in edits {
        let text = world.read(path);
        world.edit(path, from, to);
        let run = world.agree(step);
        if expected == "!" {
            assert_eq!((run.stdout.as_str(), run.status), ("", Some(1)), "{step}");
        } else {
            assert_eq!(line(&run), expected, "{step}: {}", run.stderr);
        }
        world.write(path, &text);
        let back = world.agree(&format!("{step}, undone"));
        assert_eq!(back, first, "{step}, undone");
    }
}

/// Edits of bodies, constants and positions in dependencies.
#[test]
fn edits_of_bodies() {
    check_edits(&by_path("bodies"), BODY_EDITS);
}

/// `geometry` as the root package, with a module that has a `main`.
fn library(name: &str) -> World {
    let world = packages_by_path(World::rooted(name, "geometry"));
    let demo = MAIN
        .replace(
            "import geo\nimport geo::shape\n",
            "import geometry\nimport geometry::shape\n",
        )
        .replace("geo::unit", "geometry::unit");
    world.write("geometry/src/demo.cheby", &demo);
    world
}

/// A library's build fills the records that its next build and `cheby run`
/// read (D-1899): they compile nothing.
#[test]
fn library_builds_reuse_records() {
    let world = library("library_reuse");
    let code = |args: &[&str]| -> Vec<String> {
        let lines = world.log(args, false);
        let code = lines.into_iter().filter(|l| l.starts_with("code.run "));
        code.collect()
    };
    let cold = code(&["build"]);
    assert!(
        cold.contains(&"code.run geometry::shape miss".to_owned()),
        "{cold:?}"
    );
    for args in [&["build"][..], &["run", "geometry::demo"], &["build"]] {
        let warm = code(args);
        for module in [
            "geometry::demo",
            "geometry::shape",
            "units#1",
            "units#1::parts",
        ] {
            let hit = format!("code.run {module} hit");
            assert!(warm.contains(&hit), "{args:?}: {warm:?}");
        }
        assert_eq!(misses(&warm), [""; 0], "{args:?}");
    }
    world.edit(SHAPE_FILE, "side * side", "side * side + 1");
    assert_eq!(misses(&code(&["build"])), ["code.run geometry::shape miss"]);
    // The other order: a library's build reads what `cheby run` wrote.
    world.edit(SHAPE_FILE, "side * side + 1", "side * side + 2");
    assert_eq!(
        misses(&code(&["run", "geometry::demo"])),
        ["code.run geometry::shape miss"]
    );
    assert_eq!(misses(&code(&["build"])), [""; 0]);
}

/// Edits of a library's bodies, and what its build, and `cheby run` of a
/// module of it, then give (D-1852, D-1899).
#[test]
fn edits_of_bodies_of_a_library() {
    let world = library("library_bodies");
    // A library's own tests are checked in its build.
    let edits: Vec<Edit> = BODY_EDITS
        .iter()
        .map(|&(step, path, from, to, expected)| {
            let fails = step == "a broken test";
            (step, path, from, to, if fails { "!" } else { expected })
        })
        .collect();
    check_edits(&world, &edits);
    // A symbol that only an unreached function names: an error of the
    // library's build and not of a program (D-943).
    let text = world.read(SHAPE_FILE);
    world.write(
        SHAPE_FILE,
        &format!("{text}\n/// Has no definition.\n@external(native, \"no_such_symbol\")\npub fn missing() -> Int\n"),
    );
    let run = world.agree("a missing symbol");
    assert_eq!(line(&run), "12 3 7 6", "{}", run.stderr);
    for _ in 0..2 {
        let build = world.cheby("geometry", &["build"], world.warm.path());
        assert!(build.stderr.contains("error[E0053]"), "{}", build.stderr);
    }
    world.write(SHAPE_FILE, &text);
    world.agree("the symbol removed");
}

/// Edits of what a library's other modules see.
#[test]
fn edits_of_interfaces_of_a_library() {
    // A root package's modules see package-visible items of each other, so
    // one edit builds here.
    let edits: Vec<Edit> = INTERFACE_EDITS
        .iter()
        .map(|&(step, path, from, to, expected)| {
            let builds = step == "an item made package-visible";
            (
                step,
                path,
                from,
                to,
                if builds { "12 3 7 6" } else { expected },
            )
        })
        .collect();
    check_edits(&library("library_interfaces"), &edits);
}

/// Edits of what other modules see of dependencies, and errors in them.
#[test]
fn edits_of_interfaces() {
    check_edits(&by_path("interfaces"), INTERFACE_EDITS);
}

/// Edits of manifests: versions, keys, names, targets and limits.
#[test]
fn edits_of_manifests() {
    let world = by_path("manifests");
    let first = world.agree("the first build");
    assert!(
        first.stderr.contains("geometry@0.1.0/src/shape.cheby:"),
        "{}",
        first.stderr
    );
    // The exact version is in locations, and the major in the identity.
    for version in ["0.1.1", "0.2.0", "0.1.0+build", "1.0.0", "0.1.0"] {
        let text = world.read("geometry/cheby.toml");
        let (before, after) = text.split_once("version = \"").unwrap();
        let rest = after.split_once('"').unwrap().1;
        world.write(
            "geometry/cheby.toml",
            &format!("{before}version = \"{version}\"{rest}"),
        );
        let run = world.agree(&format!("version {version}"));
        assert!(
            run.stderr
                .contains(&format!("geometry@{version}/src/shape.cheby:")),
            "{}",
            run.stderr
        );
    }
    // Keys, names and targets.
    world.edit("app/cheby.toml", "geo =", "shapes =");
    world.fails("a key renamed in the manifest", "E0039");
    world.edit(
        "app/src/main.cheby",
        "import geo\nimport geo::shape",
        "import shapes as geo\nimport shapes::shape",
    );
    assert_eq!(
        line(&world.agree("a key renamed in the source")),
        "12 3 7 6"
    );
    world.write("app/cheby.toml", APP);
    world.write("app/src/main.cheby", MAIN);
    world.edit("geometry/cheby.toml", "\"geometry\"", "\"geo_metry\"");
    world.fails("a package renamed", "E0039");
    world.write("geometry/cheby.toml", GEOMETRY);
    world.edit("app/cheby.toml", "name = \"app\"", "name = \"geometry\"");
    world.agree("the root named as its dependency");
    world.write("app/cheby.toml", APP);
    world.edit("geometry/cheby.toml", "[\"native\"]", "[\"js\"]");
    world.fails("other targets", "does not target `native`");
    world.write("geometry/cheby.toml", GEOMETRY);
    world.edit("app/cheby.toml", "targets = [\"native\"]\n", "");
    world.fails("more targets in the root", "does not target `js`");
    world.write("app/cheby.toml", APP);
    // Limits of the dependency and of the root.
    let limits = |limit: &str| format!("\n[const-eval]\nmemory-limit = \"{limit}\"\n");
    let wide = SHAPE.replace(
        "list::length([1, 2])",
        "list::length(list::repeat(1, 100_000))",
    );
    world.write("geometry/src/shape.cheby", &wide);
    assert_eq!(line(&world.agree("a larger constant")), "12 3 7 6");
    world.write(
        "geometry/cheby.toml",
        &format!("{GEOMETRY}{}", limits("1KiB")),
    );
    world.fails("a lower limit of the dependency", "memory limit of 1KiB");
    world.write("geometry/cheby.toml", GEOMETRY);
    assert_eq!(line(&world.agree("the limit undone")), "12 3 7 6");
    world.write("app/cheby.toml", &format!("{APP}{}", limits("1KiB")));
    assert_eq!(line(&world.agree("a lower limit of the root")), "12 3 7 6");
    world.write("app/cheby.toml", APP);
    world.write("geometry/src/shape.cheby", SHAPE);
    assert_eq!(world.agree("all undone"), first);
}

/// Modules and packages that come, go and move.
#[test]
fn edits_of_the_set_of_modules() {
    let world = by_path("modules");
    let first = world.agree("the first build");
    world.write(
        "units/src/extra.cheby",
        "//! Extra.\n\n/// N.\npub fn n() -> Int {\n  100\n}\n",
    );
    assert_eq!(
        line(&world.agree("a module that nothing imports")),
        "12 3 7 6"
    );
    world.edit(
        "geometry/src/shape.cheby",
        "import u::parts\n",
        "import u::extra\nimport u::parts\n",
    );
    world.edit(
        "geometry/src/shape.cheby",
        "side * side",
        "side * side + extra::n()",
    );
    assert_eq!(line(&world.agree("a module imported")), "312 303 7 6");
    std::fs::remove_file(world.dir.join("units/src/extra.cheby")).unwrap();
    world.fails("a module removed", "E0039");
    world.write("geometry/src/shape.cheby", SHAPE);
    assert_eq!(world.agree("all undone"), first);
    world.write("units2/cheby.toml", UNITS);
    world.write(
        "units2/src/main.cheby",
        &UNITS_MAIN.replace("x * 3", "x * 7"),
    );
    world.write("units2/src/parts.cheby", PARTS);
    world.edit("geometry/cheby.toml", "../units", "../units2");
    assert_eq!(
        line(&world.agree("another directory of one identity")),
        "28 7 7 6"
    );
    world.write("geometry/cheby.toml", GEOMETRY);
    assert_eq!(world.agree("the first directory again"), first);
    world.edit("units2/cheby.toml", "1.2.3", "2.0.0");
    world.edit(
        "app/cheby.toml",
        "geo =",
        "u = { path = \"../units2\" }\ngeo =",
    );
    world.edit(
        "app/src/main.cheby",
        "import std::io\n",
        "import std::io\n\nimport u\n",
    );
    world.edit(
        "app/src/main.cheby",
        "let b = geo::unit()",
        "let b = geo::unit() + u::scale(1)",
    );
    assert_eq!(line(&world.agree("two majors of one package")), "12 10 7 6");
    world.write("app/cheby.toml", APP);
    world.fails("a dependency removed", "E0039");
    world.write("app/src/main.cheby", MAIN);
    assert_eq!(world.agree("everything undone"), first);
}

/// C sources and headers of dependencies, and symbols that come and go.
#[test]
fn edits_of_native_code() {
    let world = by_path("native_code");
    world.edit(
        "units/cheby.toml",
        "targets = [\"native\"]\n",
        "targets = [\"native\"]\n\n[native]\nsources = [\"src/scale.c\"]\n",
    );
    world.write("units/src/factor.h", "#define FACTOR 3\n");
    world.write("units/src/scale.c", "#include <stdint.h>\n#include \"factor.h\"\nint64_t units_scale(int64_t x) { return x * FACTOR; }\n");
    world.edit(
        "units/src/main.cheby",
        "pub fn scale(x: Int) -> Int {\n  x * 3 + parts::twice([0])\n}",
        "@external(native, \"units_scale\")\npub fn scale(x: Int) -> Int",
    );
    check_edits(
        &world,
        &[
            (
                "a C body",
                "units/src/scale.c",
                "x * FACTOR",
                "x * FACTOR + 1",
                "13 4 7 6",
            ),
            ("a C header", "units/src/factor.h", "3", "6", "24 6 7 6"),
            (
                "a symbol renamed in C",
                "units/src/scale.c",
                "units_scale",
                "units_scaled",
                "!",
            ),
            (
                "a symbol renamed in Cheby",
                "units/src/main.cheby",
                "units_scale",
                "units_other",
                "!",
            ),
            ("a C error", "units/src/scale.c", "return", "returned", "!"),
            (
                "a wrapper in place of C",
                "units/src/main.cheby",
                "@external(native, \"units_scale\")\npub fn scale(x: Int) -> Int",
                "pub fn scale(x: Int) -> Int {\n  x * 9\n}",
                "36 9 7 6",
            ),
        ],
    );
}

/// Versions, moved tags and overrides of git packages.
#[test]
fn changes_of_git_packages() {
    let world = by_path("git");
    let at = |tag: &str| UNITS_MAIN.replace("x * 3", tag);
    for (version, body) in [("1.2.3", "x * 3"), ("1.3.0", "x * 4"), ("2.0.0", "x * 5")] {
        world.commit(
            "units",
            &format!("v{version}"),
            &[
                ("cheby.toml", &UNITS.replace("1.2.3", version)),
                ("src/main.cheby", &at(body)),
                ("src/parts.cheby", PARTS),
            ],
        );
    }
    let needs =
        |tag: &str| format!("u = {{ git = \"https://packages.example/units\", tag = \"{tag}\" }}");
    let manifest = |tag: &str| GEOMETRY.replace("u = { path = \"../units\" }", &needs(tag));
    let update = || {
        let out = world.cheby("app", &["update"], world.warm.path());
        assert_eq!(out.status, Some(0), "{}", out.stderr);
    };
    for (tag, expected) in [
        ("v1.2.3", "12 3 7 6"),
        ("v1.3.0", "16 4 7 6"),
        ("v2.0.0", "20 5 7 6"),
        ("v1.2.3", "12 3 7 6"),
    ] {
        world.write("geometry/cheby.toml", &manifest(tag));
        world.agree(&format!("{tag}, not locked"));
        update();
        assert_eq!(line(&world.agree(tag)), expected, "{tag}");
    }
    world.commit(
        "units",
        "v1.2.3",
        &[("cheby.toml", UNITS), ("src/main.cheby", &at("x * 11"))],
    );
    assert_eq!(line(&world.agree("a tag that moved")), "12 3 7 6");
    update();
    assert_eq!(line(&world.agree("a tag that moved, updated")), "44 11 7 6");
    world.edit("app/cheby.toml", "[dependencies]", "[overrides]\n\"https://packages.example/units\" = { path = \"../units\" }\n\n[dependencies]");
    assert_eq!(line(&world.agree("an override")), "12 3 7 6");
    world.edit("units/src/main.cheby", "x * 3", "x * 8");
    assert_eq!(line(&world.agree("an override edited")), "32 8 7 6");
    world.edit("units/cheby.toml", "1.2.3", "1.9.0");
    world.agree("an override of another version");
    world.write("app/cheby.toml", APP);
    assert_eq!(line(&world.agree("no override")), "44 11 7 6");
}

/// A generator of edits: a multiplicative congruence, so a failure names the
/// step and repeats.
struct Random(u64);

impl Random {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33).checked_rem(n).unwrap_or(0)
    }
}

/// Modules that call each other, in two packages, under edits in any order
/// that are not undone.
#[test]
fn edits_in_any_order() {
    const MODULES: u64 = 4;
    let world = World::new("random");
    let mut random = Random(20_261_125);
    let module = |package: &str, at: u64, random: &mut Random| {
        let own = |n: u64| format!("{package}::m{n}");
        let (mut imports, mut calls) = (Vec::new(), Vec::new());
        if at > 0 {
            imports.push(format!("import {}\n", own(at - 1)));
            calls.push(format!("m{}::f(n)", at - 1));
            // Package-visible, so this module's code depends on whether the
            // other's `keep` borrows its list (D-812).
            calls.push(format!("m{}::keep([n, 3])", at - 1));
        }
        if package == "upper" {
            let lower = random.below(MODULES);
            imports.push(format!("import low::m{lower} as low{lower}\n"));
            calls.push(format!("low{lower}::f(n + 1)"));
            calls.push(format!("low{lower}::limit"));
        }
        calls.push(format!("{}", random.below(50)));
        let imports = imports.concat();
        let blank = if imports.is_empty() { "" } else { "\n" };
        let padding = "// Padding.\n".repeat(usize::try_from(random.below(3)).unwrap());
        let body = match random.below(3) {
            0 => format!("  {}\n", calls.join(" + ")),
            1 => format!("  let sum = {}\n  keep([sum])\n", calls.join(" + ")),
            _ => format!("  keep([n, {}])\n", calls.join(" + ")),
        };
        let keep = match random.below(3) {
            0 => "  case xs {\n    [first, ..] => first\n    [] => 0\n  }\n",
            1 => "  case xs {\n    [first, ..rest] => first + keep(rest)\n    [] => 1\n  }\n",
            _ => {
                "  let held = [xs, [1]]\n  case held {\n    [[first, ..], ..] => first\n    [_, ..] => 2\n    [] => 3\n  }\n"
            }
        };
        format!(
            "//! M{at}.\n\n{imports}{blank}{padding}/// A limit.\npub const limit: Int = {} + {}\n\n/// F.\npub fn f(n: Int) -> Int {{\n{body}}}\n\nfn keep(xs: List<Int>) -> Int {{\n{keep}}}\n",
            random.below(9),
            random.below(9),
        )
    };
    let library = |name: &str, version: u64, rest: &str| {
        format!("name = \"{name}\"\nversion = \"1.{version}.0\"\nkind = \"library\"\n{rest}")
    };
    let needs_lower = "\n[dependencies]\nlow = { path = \"../lower\" }\n";
    world.write("app/cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nup = { path = \"../upper\" }\nlow = { path = \"../lower\" }\n");
    world.write("lower/cheby.toml", &library("lower", 0, ""));
    world.write("upper/cheby.toml", &library("upper", 0, needs_lower));
    for at in 0..MODULES {
        for package in ["lower", "upper"] {
            let text = module(package, at, &mut random);
            world.write(&format!("{package}/src/m{at}.cheby"), &text);
        }
    }
    let main = |random: &mut Random| {
        let (a, b) = (random.below(MODULES), random.below(MODULES));
        format!(
            "//! App.\n\nimport low::m{b} as lower_module\nimport std::io\nimport up::m{a} as upper_module\n\nfn main() {{\n  let a = upper_module::f(2)\n  let b = lower_module::f(3) + lower_module::limit\n  io::println(\"{{a}} {{b}}\")\n}}\n"
        )
    };
    let text = main(&mut random);
    world.write("app/src/main.cheby", &text);
    for step in 0..24 {
        match random.below(7) {
            0 => {
                let text = main(&mut random);
                world.write("app/src/main.cheby", &text);
            }
            1 => {
                let version = random.below(3);
                world.write("lower/cheby.toml", &library("lower", version, ""));
            }
            2 => {
                let version = random.below(3);
                world.write("upper/cheby.toml", &library("upper", version, needs_lower));
            }
            n => {
                let package = if n % 2 == 0 { "lower" } else { "upper" };
                let at = random.below(MODULES);
                let text = module(package, at, &mut random);
                world.write(&format!("{package}/src/m{at}.cheby"), &text);
            }
        }
        let run = world.agree(&format!("step {step}"));
        assert_eq!(run.status, Some(0), "step {step}: {}", run.stderr);
    }
}
