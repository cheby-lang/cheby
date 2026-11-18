//! Std in the build cache from the command line (K6: D-980, D-968, D-971,
//! D-978, D-1135): the second command on a fresh cache parses no std
//! module and compiles no std unit, whatever the first one was; `cheby
//! check`, `cheby test` and `cheby run` of other packages read the records
//! std's first use wrote; and an error found against std's records is
//! reported as a check of std from source reports it.
//!
//! Every test shares one fresh `CHEBY_CACHE` between its commands (D-1128),
//! set on each child's `Command`; nothing here sets a variable of the test
//! process.

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
            .join("std_cache")
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

    /// The package `app` with the root module `main` under `name`.
    fn package(&self, name: &str, main: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src").join("main.cheby"), main).unwrap();
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Prints a line through `std::io`.
const HELLO: &str = "//! Prints a greeting.

import std::io

fn main() {
  io::println(\"hello\")
}
";

/// Prints a JSON document, with a constant and a test.
const JSON: &str = "//! Prints a JSON document.

import std::io
import std::json
import std::list

const size: Int = list::length([1, 2, 3])

fn main() {
  io::println(json::to_string(json::Int(size)))
}

test \"size\" {
  assert size == 3
}
";

/// Names a `priv` function of `std::list`: an error found against std.
const PRIVATE: &str = "//! Names a private std function.

import std::list

fn main() {
  let n = list::prefix_length([1], fn(x) { x > 0 })
  assert n == 1
}
";

/// Logs a line with scoped fields under a fixed clock: `std::log` reads its
/// `priv` key of scoped fields (D-1619), and the line names the place of
/// the call (D-1631). Its test reads the scoped fields through `capture`.
const LOGS: &str = "//! Logs a line with scoped fields.

import std::context
import std::duration
import std::log
import std::time

fn main() {
  let at = time::from_unix(duration::milliseconds(1_794_909_600_500))
  use <- context::provide(time::clock, time::fixed_clock(at))
  use <- log::with_fields([log::field(\"request\", \"a17\")])
  log::info_with(\"listening\", [log::int(\"port\", 8080)])
}

test \"the scoped fields come first\" {
  let (_, records) = log::capture(fn() {
    use <- log::with_fields([log::field(\"request\", \"a17\")])
    log::info_with(\"in\", [log::int(\"n\", 1)])
  })
  let assert [record] = records
  assert record.fields == [log::field(\"request\", \"a17\"), log::int(\"n\", 1)]
  assert record.location.line == 18
}
";

/// The line `LOGS` writes to standard error.
const LOGGED: &str =
    "2026-11-17T10:00:00.5Z INFO src/main.cheby:12:3: listening request=a17 port=8080\n";

/// One `cheby` command's output and the lines of its `CHEBY_CACHE_LOG`.
struct Ran {
    out: RunOutput,
    log: Vec<String>,
}

impl Ran {
    /// The log lines that start with `prefix` and a space.
    fn lines(&self, prefix: &str) -> Vec<&str> {
        let prefix = format!("{prefix} ");
        self.log
            .iter()
            .filter(|l| l.starts_with(&prefix))
            .map(String::as_str)
            .collect()
    }

    /// Those of [`Ran::lines`] that end with ` miss`.
    fn misses(&self, prefix: &str) -> Vec<&str> {
        self.lines(prefix)
            .into_iter()
            .filter(|l| l.ends_with(" miss"))
            .collect()
    }
}

/// `cheby <args>` in `dir` with `CHEBY_CACHE` set to `cache` and its
/// `CHEBY_CACHE_LOG` in `log`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str], cache: &Path, log: &Path) -> Ran {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env("CHEBY_CACHE_LOG", log)
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_RC");
    let out = process::run(cmd, None, TIMEOUT).unwrap();
    let log = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Ran { out, log }
}

/// D-980 (CP2-5's std half): each command twice on a fresh cache; the
/// first is std's first use, which parses every std module once; the
/// second parses no std module, misses no std record and prints what the
/// first printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_second_command_parses_no_std() {
    let s = Scratch::new("second");
    let app = s.package("app", JSON);
    for command in ["run", "check", "test"] {
        let cache = s.path().join(format!("cache-{command}"));
        let first = cheby(&app, &[command], &cache, &s.path().join("1.log"));
        let second = cheby(&app, &[command], &cache, &s.path().join("2.log"));
        assert_eq!(first.out.status, Some(0), "{command}: {}", first.out.stderr);
        assert!(
            !first.lines("parse.std").is_empty(),
            "{command}: {:?}",
            first.log
        );
        assert!(
            second.lines("parse.std").is_empty(),
            "{command}: {:?}",
            second.log
        );
        assert!(
            second.misses("front.std").is_empty(),
            "{command}: {:?}",
            second.log
        );
        assert!(
            second.misses("code.std").is_empty(),
            "{command}: {:?}",
            second.log
        );
        assert!(
            !second.lines("code.std").is_empty(),
            "{command}: {:?}",
            second.log
        );
        assert_eq!(first.out.stdout, second.out.stdout, "{command}");
        assert_eq!(first.out.stderr, second.out.stderr, "{command}");
        // D-1135: `parse` and `front` name package modules only.
        for line in second.lines("parse").iter().chain(&second.lines("front")) {
            assert!(line.contains(" app"), "{command}: {line}");
        }
    }
}

/// D-980: after std's first use by one package, other commands of other
/// packages that import other std modules parse no std module and miss
/// no std front record, a `cheby check` first included
/// (D-1266: a check that reads no std record is std's
/// first use too).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_other_commands_after_the_first_use() {
    let s = Scratch::new("mixed");
    let hello = s.package("hello", HELLO);
    let json = s.package("json", JSON);
    for (first, then) in [
        ("run", ["check", "test", "run"]),
        ("check", ["run", "test", "check"]),
    ] {
        let cache = s.path().join(format!("cache-{first}"));
        let used = cheby(&hello, &[first], &cache, &s.path().join("first.log"));
        assert_eq!(used.out.status, Some(0), "{}", used.out.stderr);
        assert!(!used.lines("parse.std").is_empty(), "{:?}", used.log);
        for (n, command) in then.iter().enumerate() {
            let log = s.path().join(format!("{first}-{n}.log"));
            let ran = cheby(&json, &[command], &cache, &log);
            assert_eq!(
                ran.out.status,
                Some(0),
                "{first} then {command}: {}",
                ran.out.stderr
            );
            assert!(
                ran.lines("parse.std").is_empty(),
                "{first} then {command}: {:?}",
                ran.log
            );
            assert!(
                ran.misses("front.std").is_empty(),
                "{first} then {command}: {:?}",
                ran.log
            );
            assert!(
                ran.misses("code.std").is_empty(),
                "{first} then {command}: {:?}",
                ran.log
            );
        }
    }
}

/// D-953, D-1136's pattern: a decoded std interface holds no `priv` item,
/// so an error against std is found again with std checked from source; a
/// cold and a warm command print the error a check without a cache prints.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_error_against_std_is_reported_from_source() {
    let s = Scratch::new("private");
    let hello = s.package("hello", HELLO);
    let private = s.package("private", PRIVATE);
    let cache = s.path().join("cache");
    let _ = cheby(&hello, &["run"], &cache, &s.path().join("first.log"));
    let warm = cheby(&private, &["check"], &cache, &s.path().join("warm.log"));
    let bypass = s.path().join("bypass");
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "check"])
        .current_dir(&private)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, &bypass)
        .env("CHEBY_DUMP_DIR", s.path().join("dump"))
        .env_remove("CHEBY_RC");
    let Ok(cold) = process::run(cmd, None, TIMEOUT) else {
        panic!("`cheby check` did not run");
    };
    assert_ne!(warm.out.status, Some(0));
    assert!(
        warm.out.stderr.contains("prefix_length"),
        "{}",
        warm.out.stderr
    );
    assert_eq!(warm.out.stderr, cold.stderr);
    assert_eq!(warm.out.status, cold.status);
    let again = cheby(&private, &["check"], &cache, &s.path().join("again.log"));
    assert_eq!(again.out.stderr, warm.out.stderr);
}

/// D-1619, D-1631, D-1262: a program that logs with scoped fields reads
/// `std::log`'s `priv` key, and logs the same line, with the same place,
/// on std's first use, on a warm run, from an executable that `cheby
/// build` made from std's records, and with std checked from source
/// (`CHEBY_DUMP_DIR`, D-965); the first run evaluates the key and the
/// warm one takes it from its entry; and `cheby test` passes.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code and executables; Windows support is M6 (D-545, D-935)"
)]
fn std_cache_priv_constant_logs_on_every_path() {
    let s = Scratch::new("logs");
    let app = s.package("app", LOGS);
    let cache = s.path().join("cache");
    let cold = cheby(&app, &["run"], &cache, &s.path().join("cold.log"));
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    assert_eq!(cold.out.stderr, LOGGED);
    assert!(!cold.lines("parse.std").is_empty(), "{:?}", cold.log);
    assert!(
        cold.log.contains(&"const std::log::scoped miss".to_owned()),
        "{:?}",
        cold.log
    );
    let warm = cheby(&app, &["run"], &cache, &s.path().join("warm.log"));
    assert_eq!(warm.out.status, Some(0), "{}", warm.out.stderr);
    assert_eq!(warm.out.stderr, LOGGED);
    assert!(warm.lines("parse.std").is_empty(), "{:?}", warm.log);
    assert!(
        warm.log.contains(&"const std::log::scoped hit".to_owned()),
        "{:?}",
        warm.log
    );
    let exe = s.path().join("exe").join("app");
    let built = cheby(
        &app,
        &["build", "-o", exe.to_str().unwrap_or_default()],
        &cache,
        &s.path().join("build.log"),
    );
    assert_eq!(built.out.status, Some(0), "{}", built.out.stderr);
    assert!(built.lines("parse.std").is_empty(), "{:?}", built.log);
    let Ok(ran) = process::run(Command::new(&exe), None, TIMEOUT) else {
        panic!("the executable did not run");
    };
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    assert_eq!(ran.stderr, LOGGED);
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "run"])
        .current_dir(&app)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, s.path().join("bypass"))
        .env("CHEBY_DUMP_DIR", s.path().join("dump"))
        .env_remove("CHEBY_RC");
    let Ok(source) = process::run(cmd, None, TIMEOUT) else {
        panic!("`cheby run` did not run");
    };
    assert_eq!(source.status, Some(0), "{}", source.stderr);
    assert_eq!(source.stderr, LOGGED);
    let tested = cheby(&app, &["test"], &cache, &s.path().join("test.log"));
    assert_eq!(tested.out.status, Some(0), "{}", tested.out.stdout);
}

/// Panics through `option::expect`, whose call the compiler replaces by a
/// call of `option::expect_at` with the place of the call (D-1647).
const EXPECTS: &str = "//! Panics through `option::expect`.

import std::list
import std::option

fn main() {
  let words = [\"alpha\"]
  let second = option::expect(list::get(words, 1), \"a second word\")
  assert second == \"beta\"
}
";

/// The report `EXPECTS` writes to standard error.
const EXPECT_PANIC: &str = "panicked at src/main.cheby:8:16:\na second word\n";

/// D-1647, D-1631: the panic of `option::expect` names the place of the
/// call on std's first use and on a warm run, which reads the attribute
/// and the constructor from std's records, so both runs replace the call
/// the same way.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545, D-935)"
)]
fn std_cache_expect_panics_at_its_call_on_every_run() {
    let s = Scratch::new("expects");
    let app = s.package("app", EXPECTS);
    let cache = s.path().join("cache");
    let cold = cheby(&app, &["run"], &cache, &s.path().join("cold.log"));
    assert_eq!(cold.out.status, Some(101), "{}", cold.out.stderr);
    assert_eq!(cold.out.stderr, EXPECT_PANIC);
    assert!(!cold.lines("parse.std").is_empty(), "{:?}", cold.log);
    let warm = cheby(&app, &["run"], &cache, &s.path().join("warm.log"));
    assert_eq!(warm.out.status, Some(101), "{}", warm.out.stderr);
    assert_eq!(warm.out.stderr, EXPECT_PANIC);
    assert!(warm.lines("parse.std").is_empty(), "{:?}", warm.log);
}
