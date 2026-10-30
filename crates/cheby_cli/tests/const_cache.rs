//! Constant results in the store from the command line (K5b: D-977,
//! D-964, D-965, D-973, D-979, D-1187, D-1250): a warm `cheby run`,
//! `cheby test` and `cheby build` install cached images and evaluate
//! nothing; a warm `cheby check` installs nothing and prints the cached
//! diagnostics; `CHEBY_DUMP_DIR` bypasses reads.
//!
//! Every pair of commands shares one fresh `CHEBY_CACHE` (D-1128), set on
//! each child's `Command`; nothing here sets a variable of the test process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cheby_test_support::cache::CACHE_ENV;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(300);

/// A fresh directory of this test binary, removed on drop.
struct Scratch(PathBuf);

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a failing file operation fails the test"
)]
impl Scratch {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(TMP)
            .join("const_cache")
            .join(format!("{name}-{}-{n}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The root: prints the constants of `table`, which holds them all.
const MAIN: &str = "//! Root: prints the constants of `table`.

import app::table
import std::io
import std::list
import std::map

fn main() {
  let greeting = table::greeting
  let total = list::length(table::numbers)
  let squares = map::size(table::squares)
  io::println(\"{greeting} {total} {squares}\")
}

test \"reads the table\" {
  assert list::length(table::numbers) == 1000
}
";

/// Constants in a module that no edit touches.
const TABLE: &str = "//! Constants, in a module of their own.

import std::list
import std::map

/// A greeting.
pub const greeting: String = \"warm\"

/// A thousand numbers.
pub const numbers: List<Int> = list::range(0, 1000)

/// The squares of the first hundred numbers.
pub const squares: map::Map<Int, Int> = square(100, map::new())

fn square(n: Int, acc: map::Map<Int, Int>) -> map::Map<Int, Int> {
  case n {
    0 => acc
    _ => square(n - 1, map::insert(acc, n, n * n))
  }
}
";

const LINES: [&str; 3] = [
    "const app::table::greeting",
    "const app::table::numbers",
    "const app::table::squares",
];

/// Writes the package `app` into `dir` with `main` as its root.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(dir: &Path, main: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.cheby"), main).unwrap();
    std::fs::write(dir.join("src/table.cheby"), TABLE).unwrap();
    dir.to_path_buf()
}

/// One `cheby` command's output and the lines of its `CHEBY_CACHE_LOG`.
struct Ran {
    out: RunOutput,
    log: Vec<String>,
}

impl Ran {
    /// The log's lines that start with `prefix`.
    fn lines(&self, prefix: &str) -> Vec<String> {
        self.log
            .iter()
            .filter(|l| l.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// `cheby <args>` in `dir` with `CHEBY_CACHE` set to `cache`, its
/// `CHEBY_CACHE_LOG` in `log`, and `env` added.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` or read its log fails the test"
)]
fn cheby(dir: &Path, args: &[&str], cache: &Path, log: &Path, env: &[(&str, &str)]) -> Ran {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env("CHEBY_CACHE_LOG", log)
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_RC");
    for (name, value) in env {
        cmd.env(name, value);
    }
    let out = process::run(cmd, None, TIMEOUT).unwrap();
    let log = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Ran { out, log }
}

/// `<line> <verdict>` for each of `lines`.
fn every(lines: &[&str], verdict: &str) -> Vec<String> {
    lines.iter().map(|l| format!("{l} {verdict}")).collect()
}

/// Asserts that `second` printed and exited as `first` did.
fn same_output(first: &Ran, second: &Ran) {
    assert_eq!(second.out.status, first.out.status, "{}", second.out.stderr);
    assert_eq!(second.out.stdout, first.out.stdout);
    assert_eq!(second.out.stderr, first.out.stderr);
}

/// A warm `cheby run` installs the cached images and evaluates nothing:
/// every `const` line hits, and no evaluation module looks up `plain`
/// code (D-1187's second compile gone); the output is the cold one's.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_warm_run_evaluates_nothing() {
    let scratch = Scratch::new("run");
    let dir = package(&scratch.path().join("app"), MAIN);
    let cache = scratch.path().join("cache");
    let cold = cheby(&dir, &["run"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    assert_eq!(cold.out.stdout, "warm 1000 100\n");
    assert_eq!(cold.lines("const "), every(&LINES, "miss"));
    let warm = cheby(&dir, &["run"], &cache, &scratch.path().join("log-2"), &[]);
    same_output(&cold, &warm);
    assert_eq!(warm.lines("const "), every(&LINES, "hit"));
    assert_eq!(warm.lines("code.plain "), Vec::<String>::new());
}

/// D-1250 with D-1134: a warm `cheby run` of a package whose constants sit
/// in a module no edit touched parses nothing of that module when every
/// constant hits (K4d checks a module for constant evaluation only when
/// one of its constants misses, from `consts::misses_before_check`).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_warm_run_parses_no_constant_module() {
    let scratch = Scratch::new("parse");
    let dir = package(&scratch.path().join("app"), MAIN);
    let cache = scratch.path().join("cache");
    let cold = cheby(&dir, &["run"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    let warm = cheby(&dir, &["run"], &cache, &scratch.path().join("log-2"), &[]);
    same_output(&cold, &warm);
    assert_eq!(warm.lines("const "), every(&LINES, "hit"));
    assert_eq!(warm.lines("parse "), Vec::<String>::new());
}

/// A warm `cheby test` installs the cached images as `cheby run` does.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_warm_test_installs_cached_images() {
    let scratch = Scratch::new("test");
    let dir = package(&scratch.path().join("app"), MAIN);
    let cache = scratch.path().join("cache");
    let cold = cheby(&dir, &["test"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    // The test reaches only `numbers` (D-510's roots: the selected tests).
    assert_eq!(cold.lines("const "), ["const app::table::numbers miss"]);
    let warm = cheby(&dir, &["test"], &cache, &scratch.path().join("log-2"), &[]);
    same_output(&cold, &warm);
    assert_eq!(warm.lines("const "), ["const app::table::numbers hit"]);
    assert_eq!(warm.lines("code.plain "), Vec::<String>::new());
}

/// `cheby build` after a `cheby run` takes the cached images by value
/// (`constant_images`), and the executable prints what the run printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds and runs an executable; Windows support is M6 (D-545)"
)]
fn const_cache_build_takes_cached_images() {
    let scratch = Scratch::new("build");
    let dir = package(&scratch.path().join("app"), MAIN);
    let cache = scratch.path().join("cache");
    let run = cheby(&dir, &["run"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(run.out.status, Some(0), "{}", run.out.stderr);
    let exe = scratch.path().join("app-exe");
    let built = cheby(
        &dir,
        &["build", "-o", exe.to_str().unwrap()],
        &cache,
        &scratch.path().join("log-2"),
        &[],
    );
    assert_eq!(built.out.status, Some(0), "{}", built.out.stderr);
    assert_eq!(built.lines("const "), every(&LINES, "hit"));
    assert_eq!(built.lines("code.plain "), Vec::<String>::new());
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, run.out.stdout);
}

/// A constant that panics: a warm `cheby check` prints the cached
/// diagnostics, byte for byte the cold ones (D-965, D-977), and so does a
/// warm `cheby run`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_warm_check_prints_cached_diagnostics() {
    const BROKEN: &str = "//! A constant that panics.

import app::table

const broken: Int = 1 / zero()

fn zero() -> Int {
  0
}

fn main() {
  assert broken == 0
  assert table::greeting == \"warm\"
}
";
    let scratch = Scratch::new("check");
    let dir = package(&scratch.path().join("app"), BROKEN);
    let cache = scratch.path().join("cache");
    let cold = cheby(&dir, &["check"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(cold.out.status, Some(1), "{}", cold.out.stderr);
    assert!(cold.out.stderr.contains("E0075"), "{}", cold.out.stderr);
    assert!(
        cold.lines("const ")
            .contains(&"const app::broken miss".to_owned())
    );
    let warm = cheby(&dir, &["check"], &cache, &scratch.path().join("log-2"), &[]);
    same_output(&cold, &warm);
    assert!(
        warm.lines("const ")
            .contains(&"const app::broken hit".to_owned())
    );
    let run_cold = cheby(
        &dir,
        &["run"],
        &scratch.path().join("cache-run"),
        &scratch.path().join("log-3"),
        &[],
    );
    let run_warm = cheby(&dir, &["run"], &cache, &scratch.path().join("log-4"), &[]);
    same_output(&run_cold, &run_warm);
    assert!(
        run_warm
            .lines("const ")
            .contains(&"const app::broken hit".to_owned())
    );
}

/// `CHEBY_DUMP_DIR` bypasses reads (D-965): a warm run with it evaluates
/// every constant again and writes `consts.txt` whole.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_dump_dir_bypasses_reads() {
    let scratch = Scratch::new("dump");
    let dir = package(&scratch.path().join("app"), MAIN);
    let cache = scratch.path().join("cache");
    let cold = cheby(&dir, &["run"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    let dump = scratch.path().join("dump");
    let dumped = cheby(
        &dir,
        &["run"],
        &cache,
        &scratch.path().join("log-2"),
        &[("CHEBY_DUMP_DIR", dump.to_str().unwrap())],
    );
    same_output(&cold, &dumped);
    assert_eq!(dumped.lines("const "), every(&LINES, "miss"));
    let consts = std::fs::read_to_string(dump.join("consts.txt")).unwrap();
    assert_eq!(consts.lines().count(), 3, "{consts}");
}
