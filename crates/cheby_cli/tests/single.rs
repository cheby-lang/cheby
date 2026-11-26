//! Single-file mode end to end (D-1949): `cheby check`, `cheby run` and
//! `cheby test` take a `.cheby` file that is in no package, as the one
//! module of a package with a synthetic manifest, and `check` and `run`
//! take `-` for the source on standard input.
//!
//! Every test gets a fresh `CHEBY_CACHE` of its own (D-1128).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

const PROBE: &str = "//! A lone file.

import std::io
import std::list

fn double(n: Int) -> Int {
  n * 2
}

fn main() {
  let xs = list::map([1, 2, 3], double)
  io::println(\"{xs:?}\")
}

test \"doubles\" {
  assert double(2) == 4
}

test \"fails\" {
  assert double(2) == 5
}
";

/// A directory that is no package, with files, and a build cache.
struct Dir {
    dir: FreshDir,
    cache: FreshDir,
}

impl Dir {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = Self {
            dir: FreshDir::create("cli-single").unwrap(),
            cache: FreshDir::create("cli-single-cache").unwrap(),
        };
        for (file, text) in files {
            let path = dir.dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `cheby` with `args` in the directory, or in `within` below it.
    fn cheby_in(&self, within: &str, args: &[&str], stdin: Option<&str>) -> RunOutput {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(self.path().join(within))
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, self.cache.path())
            .env_remove("CHEBY_DUMP_DIR")
            .env_remove("CHEBY_PHASES");
        process::run(cmd, stdin.map(str::as_bytes), TIMEOUT).unwrap()
    }

    fn cheby(&self, args: &[&str]) -> RunOutput {
        self.cheby_in("", args, None)
    }
}

/// A lone file checks, runs and tests without a `cheby.toml`. Its module
/// and its program are named `script`, and a diagnostic names the file as
/// the command line did.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT code, which Windows cannot until M6 (D-545)"
)]
fn a_lone_file_checks_runs_and_tests() {
    let dir = Dir::new(&[("probe.cheby", PROBE)]);
    let check = dir.cheby(&["check", "probe.cheby"]);
    assert_eq!((check.status, check.stderr.as_str()), (Some(0), ""));
    let run = dir.cheby(&["run", "probe.cheby"]);
    assert_eq!((run.status, run.stdout.as_str()), (Some(0), "[2, 4, 6]\n"));
    let test = dir.cheby(&["test", "probe.cheby"]);
    assert_eq!(test.status, Some(1));
    assert_eq!(
        test.stdout,
        "test script \"doubles\" ... ok\ntest script \"fails\" ... FAILED\n\n---- script \"fails\" \
         ----\npanicked at probe.cheby:20:10:\nassertion failed: double(2) == 5\n  left: 4\n right: \
         5\n\ntest result: FAILED. 1 passed; 1 failed; 0 filtered out\n"
    );
    // The filter follows the file.
    let filtered = dir.cheby(&["test", "probe.cheby", "doubles"]);
    assert_eq!(filtered.status, Some(0));
    assert_eq!(
        filtered.stdout,
        "test script \"doubles\" ... ok\n\ntest result: ok. 1 passed; 0 failed; 1 filtered out\n"
    );
    // An edit is seen: the build cache keys the file by its text.
    std::fs::write(
        dir.path().join("probe.cheby"),
        PROBE.replace("n * 2", "n * 3"),
    )
    .unwrap();
    assert_eq!(dir.cheby(&["run", "probe.cheby"]).stdout, "[3, 6, 9]\n");
    // Arguments reach the program, and a file below the directory is named
    // by its path.
    let args = dir.cheby(&["run", "./probe.cheby", "--", "a"]);
    assert_eq!(args.status, Some(0), "{}", args.stderr);
}

/// Diagnostics name the file as it was given, `--fix` writes to it, and the
/// warning about uses elsewhere in a package is left out (D-1371).
#[test]
#[cfg_attr(
    windows,
    ignore = "a check that passes evaluates constants with the JIT (D-545)"
)]
fn diagnostics_and_fixes_of_a_lone_file() {
    let habits = "//! Habits.

import std::io

fn unused() -> Int {
  1
}

fn main() {
  var flag = true;
  io::println(\"{flag:?}\")
}
";
    let dir = Dir::new(&[("sub/habits.cheby", habits)]);
    let check = dir.cheby(&["check", "sub/habits.cheby"]);
    assert_eq!(check.status, Some(1));
    assert!(
        check.stderr.starts_with(
            "error[E0013]: expected a line break or `}`, found identifier\n  --> \
             sub/habits.cheby:10:7\n"
        ),
        "{}",
        check.stderr
    );
    let fixed = dir.cheby(&["check", "--fix", "sub/habits.cheby"]);
    assert_eq!(fixed.status, Some(0), "{}", fixed.stderr);
    assert_eq!(
        fixed.stderr,
        "fixed: sub/habits.cheby:10:3: Cheby has no `var`; write `let`\n\
         fixed: sub/habits.cheby:10:18: statements are separated by line breaks, not `;`\n\
         fixed: sub/habits.cheby:10:14: `Bool`'s constructors are `True` and `False`; write \
         `True`\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/habits.cheby")).unwrap(),
        habits.replace("var flag = true;", "let flag = True")
    );
}

/// `-` reads the source from standard input, shown as `<stdin>`; `--fix`
/// has nothing to write to there, and `cheby test` does not take it.
#[test]
fn the_source_on_standard_input() {
    let dir = Dir::new(&[]);
    let source = "import std::io\n\nfn main() {\n  io::println(oops)\n}\n";
    let check = dir.cheby_in("", &["check", "-"], Some(source));
    assert_eq!(check.status, Some(1));
    assert!(
        check.stderr.starts_with(
            "error[E0042]: cannot find value `oops` in this scope\n --> <stdin>:4:15\n"
        ),
        "{}",
        check.stderr
    );
    let fix = dir.cheby_in("", &["check", "--fix", "-"], Some(source));
    assert_eq!(fix.status, Some(1));
    assert_eq!(
        fix.stderr,
        "error: `--fix` has no file to write to when the source is on standard input\n"
    );
    let test = dir.cheby_in("", &["test", "-"], Some(source));
    assert_eq!(
        test.stderr,
        "error: `cheby test` takes no source from standard input; give it a `.cheby` file\n"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT code, which Windows cannot until M6 (D-545)"
)]
fn standard_input_runs() {
    let dir = Dir::new(&[]);
    let source = "//! S.\n\nimport std::io\n\nfn main() {\n  io::println(\"from stdin\")\n}\n";
    let run = dir.cheby_in("", &["run", "-"], Some(source));
    assert_eq!((run.status, run.stdout.as_str()), (Some(0), "from stdin\n"));
}

/// A module of a package is not taken alone, from inside the package or
/// from above it; a file beside its `src/` is, and so is no argument that
/// is no `.cheby` file.
#[test]
fn a_module_of_a_package_is_refused() {
    let dir = Dir::new(&[
        ("pkg/cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n"),
        ("pkg/src/main.cheby", "//! Root.\n\nfn main() {}\n"),
        (
            "pkg/scratch.cheby",
            "//! Beside.\n\nfn main() {\n  let x: Int = oops\n}\n",
        ),
    ]);
    let inside = dir.cheby_in("pkg", &["check", "src/main.cheby"], None);
    assert_eq!(inside.status, Some(1));
    assert_eq!(
        inside.stderr,
        "error: `src/main.cheby` is a module of the package `app`, so it is not taken alone; \
         run `cheby check` in the package\n"
    );
    let above = dir.cheby(&["run", "pkg/src/main.cheby"]);
    assert!(
        above
            .stderr
            .starts_with("error: `pkg/src/main.cheby` is a module of the package `app`"),
        "{}",
        above.stderr
    );
    // A file beside `src/` is a lone file, checked without the package.
    let beside = dir.cheby_in("pkg", &["check", "scratch.cheby"], None);
    assert!(
        beside.stderr.contains("cannot find value `oops`")
            && beside.stderr.contains("--> scratch.cheby:4:"),
        "{}",
        beside.stderr
    );
    let missing = dir.cheby(&["check", "nope.cheby"]);
    assert_eq!(missing.stderr, "error: there is no file `nope.cheby`\n");
    let other = dir.cheby(&["check", "notes.txt"]);
    assert!(
        other
            .stderr
            .starts_with("error: `notes.txt` is not a `.cheby` file"),
        "{}",
        other.stderr
    );
    let two = dir.cheby_in("pkg", &["test", "one", "two"], None);
    assert!(
        two.stderr
            .starts_with("error: `cheby test` takes one filter"),
        "{}",
        two.stderr
    );
}
