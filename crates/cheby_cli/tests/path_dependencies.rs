//! Path dependencies end to end (D-1856 to D-1871): the built binary on
//! small packages next to each other, each test in a directory of its own.
//!
//! Programs do not run on Windows until M6, so this file is not built
//! there (D-545).
#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(120);

/// Packages next to each other, with one build cache.
struct Packages {
    dir: PathBuf,
    cache: HarnessCache,
}

impl Packages {
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = Path::new(TMP).join("path_dependencies").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        let packages = Self {
            dir,
            cache: HarnessCache::create("cli-path-dependencies").unwrap(),
        };
        for (path, text) in files {
            packages.write(path, text);
        }
        packages
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn cheby(&self, package: &str, args: &[&str]) -> RunOutput {
        self.cheby_with(package, args, None)
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn cheby_with(&self, package: &str, args: &[&str], stdin: Option<&[u8]>) -> RunOutput {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(self.dir.join(package))
            .env("NO_COLOR", "1");
        self.cache.apply(&mut cmd);
        process::run(cmd, stdin, TIMEOUT).unwrap()
    }

    /// The command succeeds with this output and nothing on standard error.
    fn prints(&self, package: &str, args: &[&str], stdout: &str) {
        let out = self.cheby(package, args);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            (stdout, "", Some(0)),
            "{package} {args:?}"
        );
    }

    /// `cheby check` fails with a standard error that starts with `expected`.
    fn check_fails(&self, package: &str, expected: &str) {
        let out = self.cheby(package, &["check"]);
        assert_eq!(out.status, Some(1), "{}", out.stderr);
        assert!(out.stderr.starts_with(expected), "{}", out.stderr);
    }
}

const UNITS: [(&str, &str); 2] = [
    (
        "units/cheby.toml",
        "name = \"units\"\nversion = \"1.2.3\"\nkind = \"library\"\n",
    ),
    (
        "units/src/main.cheby",
        "//! Units.\n\n/// Scales.\npub fn scale(x: Int) -> Int {\n  x * 3\n}\n",
    ),
];

const GEOMETRY: [(&str, &str); 3] = [
    (
        "geometry/cheby.toml",
        "name = \"geometry\"\nversion = \"0.1.0\"\nkind = \"library\"\n\n[dependencies]\nu = { path = \"../units\" }\n",
    ),
    (
        "geometry/src/main.cheby",
        "//! Geometry.\n\nimport geometry::shape\n\n/// The area of a unit square.\npub fn unit() -> Int {\n  shape::area(shape::square(1))\n}\n",
    ),
    (
        "geometry/src/shape.cheby",
        "//! Shapes.\n\nimport u\n\n/// A shape.\npub type Shape {\n  Square(Int),\n}\n\n/// A point.\npub exposed type Point {\n  Point { x: Int, y: Int },\n}\n\n/// The origin.\npub const origin: Point = Point { x: 0, y: 7 }\n\n/// A square.\npub fn square(side: Int) -> Shape {\n  Square(side)\n}\n\n/// The area.\npub fn area(s: Shape) -> Int {\n  case s {\n    Square(side) => u::scale(side * side)\n  }\n}\n\n/// Fails.\npub fn boom() -> Int {\n  panic as \"boom\"\n}\n\nfn unused() -> Int {\n  1\n}\n\ntest \"a library's own test\" {\n  assert area(Square(1)) == 3\n}\n",
    ),
];

const APP: &str =
    "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ngeo = { path = \"../geometry\" }\n";

/// A program over `geometry` whose `main` ends with `extra`.
fn app(extra: &str) -> String {
    format!(
        "//! App.\n\nimport geo\nimport geo::shape\nimport std::io\n\nfn main() {{\n  let a = shape::area(shape::square(2))\n  let u = geo::unit()\n  let o = shape::origin\n  io::println(\"{{a}} {{u}} {{o.y}}\")\n{extra}}}\n\ntest \"app's test\" {{\n  assert geo::unit() == 3\n}}\n"
    )
}

fn with_app(name: &str, extra: &str) -> Packages {
    let packages = Packages::new(name, &[UNITS.as_slice(), &GEOMETRY].concat());
    packages.write("app/cheby.toml", APP);
    packages.write("app/src/main.cheby", &app(extra));
    packages
}

/// Every command reads a path dependency and its own dependency, under the
/// keys of each manifest (D-1856). Only the package's own tests run, and a
/// dependency's warnings are not shown (D-1868).
#[test]
fn every_command_reads_path_dependencies() {
    let packages = with_app("commands", "");
    packages.prints("app", &["check", "--deny-warnings"], "");
    for _ in 0..2 {
        packages.prints("app", &["run"], "12 3 7\n");
    }
    packages.prints("app", &["run", "--release"], "12 3 7\n");
    packages.prints(
        "app",
        &["test"],
        "test app \"app's test\" ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 filtered out\n",
    );
    for args in [["build"].as_slice(), &["build", "--release"]] {
        packages.prints("app", args, "");
        let out = process::run(
            Command::new(packages.dir.join("app/build/native/app")),
            None,
            TIMEOUT,
        )
        .unwrap();
        assert_eq!((out.stdout.as_str(), out.status), ("12 3 7\n", Some(0)));
    }
    let shape = GEOMETRY[2].1.replace("  1\n}", "  let spare = 2\n  1\n}");
    packages.write("geometry/src/shape.cheby", &shape);
    packages.prints("app", &["check", "--deny-warnings"], "");
    let out = packages.cheby_with("app", &["repl"], Some(b"import geo\ngeo::unit()\n"));
    assert!(out.stdout.contains('3'), "{}", out.stdout);
    assert!(
        !format!("{}{}", out.stdout, out.stderr).contains("W0002"),
        "{}{}",
        out.stdout,
        out.stderr
    );
}

/// A warm run sees an edit of a dependency's dependency, in a body and in a
/// signature, and the edit undone.
#[test]
fn an_edited_dependency_is_compiled_again() {
    let packages = with_app("edited", "");
    packages.prints("app", &["run"], "12 3 7\n");
    let units = UNITS[1].1;
    packages.write("units/src/main.cheby", &units.replace("x * 3", "x * 5"));
    packages.prints("app", &["run"], "20 5 7\n");
    packages.write(
        "units/src/main.cheby",
        &units.replace("x: Int", "x: String"),
    );
    let out = packages.cheby("app", &["run"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains(" --> geometry@0.1.0/src/shape.cheby:"),
        "{}",
        out.stderr
    );
    packages.write("units/src/main.cheby", units);
    packages.prints("app", &["run"], "12 3 7\n");
    let shape = GEOMETRY[2].1;
    packages.write("geometry/src/shape.cheby", &shape.replace("y: 7", "y: 8"));
    packages.prints("app", &["run"], "12 3 8\n");
    packages.prints("app", &["run", "--release"], "12 3 8\n");
}

/// A panic in a dependency names the package and its version (D-1858).
#[test]
fn a_panic_names_the_dependency() {
    let packages = with_app("panic", "  let n = shape::boom()\n  io::println(\"{n}\")\n");
    let out = packages.cheby("app", &["run"]);
    assert_eq!(out.status, Some(101));
    assert!(
        out.stderr
            .contains("panicked at geometry@0.1.0/src/shape.cheby:32:3:\nboom\n"),
        "{}",
        out.stderr
    );
}

/// The package is the boundary of constructors and of package-visible
/// items, and only a direct dependency can be imported (§7.1, §7.3, §7.5).
#[test]
fn a_dependency_is_another_package() {
    let packages = with_app("boundary", "  shape::area(shape::Square(1))\n  Nil\n");
    packages.check_fails(
        "app",
        "error[E0043]: the constructor `Square` is not visible outside its package\n",
    );
    packages.write("app/src/main.cheby", &app("  shape::unused()\n  Nil\n"));
    packages.check_fails(
        "app",
        "error[E0043]: `unused` is visible only inside its package\n",
    );
    packages.write(
        "app/src/main.cheby",
        "//! App.\n\nimport units\n\nfn main() {\n  Nil\n}\n",
    );
    packages.check_fails("app", "error[E0039]: there is no package named `units`\n");
    packages.write(
        "app/src/main.cheby",
        "//! App.\n\nimport geo::shap\n\nfn main() {\n  Nil\n}\n",
    );
    let out = packages.cheby("app", &["check"]);
    assert!(
        out.stderr
            .contains("a module with a similar name exists: `geo::shape`"),
        "{}",
        out.stderr
    );
}

/// The shapes module of a package for [`packages_of_one_name_stay_apart`].
const POINTS: &str = "//! Points.\n\n/// A point.\npub exposed type Point {\n  Point { x: Int, y: Int },\n}\n\n/// No points.\npub fn none() -> List<Point> {\n  []\n}\n";

/// The root package and two majors of one name, all with a module and a type
/// of one name: their `dyn` values are never equal, and each is equal to
/// itself (D-1313, D-1857, D-1863).
#[test]
fn packages_of_one_name_stay_apart() {
    let manifest =
        |version: &str, rest: &str| format!("name = \"points\"\nversion = \"{version}\"\n{rest}");
    let packages = Packages::new(
        "one_name",
        &[
            ("one/src/point.cheby", POINTS),
            ("two/src/point.cheby", POINTS),
            ("root/src/point.cheby", POINTS),
        ],
    );
    packages.write("one/cheby.toml", &manifest("0.1.0", "kind = \"library\"\n"));
    packages.write("two/cheby.toml", &manifest("0.2.0", "kind = \"library\"\n"));
    packages.write(
        "root/cheby.toml",
        &manifest(
            "0.1.0",
            "\n[dependencies]\none = { path = \"../one\" }\ntwo = { path = \"../two\" }\n",
        ),
    );
    packages.write(
        "root/src/main.cheby",
        "//! One name.\n\nimport one::point as one_point\nimport points::point\nimport std::io\nimport two::point as two_point\n\n/// Any value.\ninterface Any\n\nfn main() {\n  let a: dyn Any = point::Point { x: 1, y: 2 }\n  let b: dyn Any = one_point::Point { x: 1, y: 2 }\n  let c: dyn Any = two_point::Point { x: 1, y: 2 }\n  let d: dyn Any = two_point::Point { x: 1, y: 2 }\n  let ab = a == b\n  let bc = b == c\n  let cd = c == d\n  let e: dyn Any = point::none()\n  let f: dyn Any = one_point::none()\n  let g: dyn Any = two_point::none()\n  let h: dyn Any = two_point::none()\n  let ef = e == f\n  let fg = f == g\n  let gh = g == h\n  io::println(\"{ab} {bc} {cd} {ef} {fg} {gh}\")\n}\n",
    );
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        packages.prints("root", args, "False False True False False True\n");
    }
    packages.prints("root", &["build"], "");
    let out = process::run(
        Command::new(packages.dir.join("root/build/native/points")),
        None,
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(out.stdout, "False False True False False True\n");
}

/// A dependency's constants have its own `[const-eval]` limits, and the
/// root package's do not change them (D-1869).
#[test]
fn a_dependency_has_its_own_constant_limits() {
    let packages = with_app("limits", "");
    let limit = |text: &str| format!("\n[const-eval]\nmemory-limit = \"{text}\"\n");
    let shape = GEOMETRY[2]
        .1
        .replace("y: 7 }", "y: list::length(list::repeat(1, 100_000)) }")
        .replace("import u\n", "import std::list\nimport u\n");
    packages.write("geometry/src/shape.cheby", &shape);
    packages.write("app/cheby.toml", &format!("{APP}{}", limit("1KiB")));
    packages.prints("app", &["run"], "12 3 100000\n");
    packages.write("app/cheby.toml", APP);
    packages.write(
        "geometry/cheby.toml",
        &format!("{}{}", GEOMETRY[0].1, limit("1KiB")),
    );
    let out = packages.cheby("app", &["run"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains(
            "used more than the memory limit of 1KiB\n  --> geometry@0.1.0/src/shape.cheby:"
        ),
        "{}",
        out.stderr
    );
}

/// A package with native code, whose C function `symbol` returns `n`.
fn native_library(packages: &Packages, name: &str, symbol: &str, n: u32) {
    packages.write(
        &format!("{name}/cheby.toml"),
        &format!("name = \"{name}\"\nversion = \"0.1.0\"\nkind = \"library\"\ntargets = [\"native\"]\n\n[native]\nsources = [\"src/{name}.c\"]\n"),
    );
    packages.write(
        &format!("{name}/src/{name}.c"),
        &format!("#include <cheby.h>\nstatic int64_t helper(void) {{ return {n}; }}\nint64_t {symbol}(void) {{ return helper(); }}\ncheby_value *{symbol}_text(void) {{ return cheby_ffi_string_new(\"{name}\", {}); }}\n", name.len()),
    );
    packages.write(
        &format!("{name}/src/main.cheby"),
        &format!("//! {name}.\n\n/// A number.\n@external(native, \"{symbol}\")\npub fn number() -> Int\n\n/// A text.\n@external(native, \"{symbol}_text\")\npub fn text() -> String\n"),
    );
}

/// The C of each package is compiled in that package, and the program's has
/// all of it, in the JIT and in executables (D-940, D-941, D-1888).
#[test]
fn native_code_of_several_packages() {
    let packages = Packages::new("native", &[]);
    native_library(&packages, "one", "one_number", 1);
    native_library(&packages, "two", "two_number", 2);
    native_library(&packages, "app", "app_number", 3);
    packages.write(
        "app/cheby.toml",
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n[native]\nsources = [\"src/app.c\"]\n\n[dependencies]\none = { path = \"../one\" }\ntwo = { path = \"../two\" }\n",
    );
    packages.write(
        "app/src/main.cheby",
        "//! App.\n\nimport one\nimport std::io\nimport two\n\n@external(native, \"app_number\")\nfn number() -> Int\n\nfn main() {\n  let sum = one::number() + two::number() + number()\n  io::println(\"{sum}\")\n  io::println(one::text())\n  io::println(two::text())\n}\n",
    );
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        packages.prints("app", args, "6\none\ntwo\n");
    }
    packages.prints("app", &["build"], "");
    let exe = Command::new(packages.dir.join("app/build/native/app"));
    assert_eq!(
        process::run(exe, None, TIMEOUT).unwrap().stdout,
        "6\none\ntwo\n"
    );
    // An edit of a dependency's C is seen.
    let c = std::fs::read_to_string(packages.dir.join("two/src/two.c")).unwrap();
    packages.write("two/src/two.c", &c.replace("return 2", "return 20"));
    packages.prints("app", &["run"], "24\none\ntwo\n");
    // Two packages may not define one C name (D-1890).
    native_library(&packages, "two", "one_number", 2);
    for command in ["run", "build"] {
        let out = packages.cheby("app", &[command]);
        assert_eq!(out.status, Some(1), "{}", out.stderr);
        assert!(
            out.stderr.starts_with(
                "error: the native code of packages `one@0.1.0` and `two@0.1.0` both defines `one_number`; make the \
                 C function `static` in one, or give it another name\n"
            ),
            "{}",
            out.stderr
        );
    }
    packages.prints("app", &["check"], "");
}

/// Two majors of one name are two packages, so they may not define one C name
/// either (D-1857, D-1890).
#[test]
fn native_code_of_two_majors() {
    let packages = Packages::new("native_majors", &[]);
    for (dir, version, n) in [("one", "1.0.0", 1), ("two", "2.0.0", 2)] {
        native_library(&packages, dir, "geo_number", n);
        packages.write(
            &format!("{dir}/cheby.toml"),
            &format!("name = \"geo\"\nversion = \"{version}\"\nkind = \"library\"\ntargets = [\"native\"]\n\n[native]\nsources = [\"src/{dir}.c\"]\n"),
        );
    }
    packages.write(
        "app/cheby.toml",
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n[dependencies]\none = { path = \"../one\" }\ntwo = { path = \"../two\" }\n",
    );
    packages.write(
        "app/src/main.cheby",
        "//! App.\n\nimport one\nimport std::io\nimport two\n\nfn main() {\n  let sum = one::number() + two::number()\n  io::println(\"{sum}\")\n}\n",
    );
    for command in ["run", "build"] {
        let out = packages.cheby("app", &[command]);
        assert_eq!((out.stdout.as_str(), out.status), ("", Some(1)));
        assert!(
            out.stderr.starts_with(
                "error: the native code of packages `geo@1.0.0` and `geo@2.0.0` both defines `geo_number"
            ),
            "{}",
            out.stderr
        );
    }
}

/// A dependency's `test` blocks are no part of its user's build: one that does
/// not resolve or check fails only the dependency's own (D-1894).
#[test]
fn the_tests_of_a_dependency_are_its_own() {
    let packages = with_app("broken_tests", "");
    let shape = format!(
        "{}\ntest \"does not check\" {{\n  let n: Int = \"text\"\n  assert n == 1\n}}\n\ntest \"does not resolve\" {{\n  assert nowhere::found == missing\n}}\n",
        GEOMETRY[2].1
    );
    packages.write("geometry/src/shape.cheby", &shape);
    packages.prints("app", &["check", "--deny-warnings"], "");
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        packages.prints("app", args, "12 3 7\n");
    }
    packages.prints("app", &["build"], "");
    assert_eq!(packages.cheby("app", &["test"]).status, Some(0));
    for args in [["check"].as_slice(), &["build"], &["test"]] {
        let out = packages.cheby("geometry", args);
        assert_eq!(out.status, Some(1), "{args:?}");
        assert!(
            out.stderr.contains("error[E0047]") && out.stderr.contains("error[E0042]"),
            "{args:?}: {}",
            out.stderr
        );
    }
    // Any other error of the dependency is still its user's.
    packages.write(
        "geometry/src/shape.cheby",
        &GEOMETRY[2].1.replace("Square(side)\n}", "side\n}"),
    );
    packages.check_fails("app", "error[E0047]");
}

/// What a dependency must be, each an error in a manifest (D-1854, D-1857,
/// D-1864, D-1867, D-1873).
#[test]
fn manifest_errors_of_dependencies() {
    let library = |name: &str, rest: &str| {
        format!("name = \"{name}\"\nversion = \"0.1.0\"\nkind = \"library\"\n{rest}")
    };
    let packages = Packages::new("manifests", &[("program/src/main.cheby", "//! P.\n")]);
    for name in ["library", "native", "a", "b", "other"] {
        packages.write(&format!("{name}/src/main.cheby"), "//! L.\n");
    }
    packages.write(
        "program/cheby.toml",
        "name = \"program\"\nversion = \"0.1.0\"\n",
    );
    packages.write("library/cheby.toml", &library("library", ""));
    packages.write("other/cheby.toml", &library("library", ""));
    packages.write(
        "native/cheby.toml",
        &library("native", "targets = [\"native\"]\n"),
    );
    packages.write(
        "a/cheby.toml",
        &library("a", "\n[dependencies]\nb = { path = \"../b\" }\n"),
    );
    packages.write(
        "b/cheby.toml",
        &library("b", "\n[dependencies]\na = { path = \"../a\" }\n"),
    );
    let root = |dependencies: &str| {
        packages.write("root/src/main.cheby", "//! R.\n\nfn main() {\n  Nil\n}\n");
        packages.write(
            "root/cheby.toml",
            &format!("name = \"root\"\nversion = \"0.1.0\"\n\n[dependencies]\n{dependencies}"),
        );
    };
    root("p = { path = \"../program\" }\n");
    packages.check_fails(
        "root",
        "error[E0036]: dependency `p` is a program, not a library\n --> cheby.toml:5:1\n",
    );
    root("n = { path = \"../native\" }\n");
    packages.check_fails(
        "root",
        "error[E0036]: dependency `n` does not target `js`\n --> cheby.toml:5:1\n",
    );
    packages.prints("root", &["check", "--target", "native"], "");
    root("l = { path = \"../library\" }\nm = { path = \"../other\" }\n");
    packages.check_fails(
        "root",
        "error[E0036]: two directories hold major 0.1 of package `library`\n --> cheby.toml:6:1\n",
    );
    // Two keys may name one directory.
    root("l = { path = \"../library\" }\nm = { path = \"../library\" }\n");
    packages.prints("root", &["check"], "");
    root("a = { path = \"../a\" }\n");
    packages.check_fails(
        "root",
        "error[E0036]: packages depend on each other: a -> b -> a\n --> b@0.1.0/cheby.toml:6:1\n",
    );
    packages.check_fails(
        "a",
        "error[E0036]: packages depend on each other: a -> b -> a\n --> b@0.1.0/cheby.toml:6:1\n",
    );
}
