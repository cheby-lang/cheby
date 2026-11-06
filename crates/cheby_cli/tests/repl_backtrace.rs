//! The panic backtrace in the hosts that no UI test runs it in (D-1038 to
//! D-1043): a panicking REPL entry with `CHEBY_BACKTRACE=1` prints its
//! backtrace after the report, and the REPL and `cheby test` refuse a bad
//! value before anything runs (the UI test `backtrace/test_failure` pins a
//! failing test's backtrace in its block). Each test runs a real `cheby`
//! as a child with the variable on its command (no test sets the variable
//! in its own process).
//!
//! Unix only, as `repl_runtime.rs` is: no programs run on Windows until M6
//! (D-545).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long one child may take, before `CHEBY_HARNESS_SCALE`: a hang
/// detector only.
const LIMIT: Duration = Duration::from_secs(120);

/// An empty directory of its own for one test.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to make the directory fails the test"
)]
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(TMP)
        .join("repl_backtrace")
        .join(format!("{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `cheby <args>` in `dir` with `CHEBY_BACKTRACE=<backtrace>`, a fresh
/// cache (D-1128) and `input` on standard input.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to run `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str], backtrace: &str, input: Option<&str>) -> RunOutput {
    let cache = HarnessCache::create("repl_backtrace").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never"])
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_BACKTRACE", backtrace);
    cache.apply(&mut cmd);
    let timeout = process::scaled_timeout(LIMIT).unwrap();
    process::run(cmd, input.map(str::as_bytes), timeout).unwrap()
}

/// A REPL entry that panics in a function defined in an earlier entry:
/// the backtrace names the function by its REPL item path and the entry
/// as `<repl:N>`, each at its position, and ends at the entry thunk
/// (D-1039, D-1041).
const PANICKING_SESSION: &str = "fn check(n: Int) -> Int {
  case {
    n > 2 => panic as \"too big: {n}\"
    _ => n
  }
}
1 + check(3)
\"the session continues\"
";

/// D-1038, D-1039, D-1041, D-1042: a panicking REPL entry's report is
/// followed by its backtrace in the same write, on standard error, and the
/// session goes on.
#[test]
fn repl_backtrace_entry_prints_its_frames() {
    let dir = scratch("entry");
    let out = cheby(&dir, &["repl"], "1", Some(PANICKING_SESSION));
    assert_eq!(out.status, Some(0), "{out:?}");
    assert_eq!(
        out.stderr,
        "panicked at <repl:1>:3:14:\n\
         too big: 3\n\
         backtrace:\n\
         \x20 in <repl:1>::check at <repl:1>:3:14\n\
         \x20 in <repl:2> at <repl:2>:1:5\n",
        "{out:?}"
    );
    assert!(out.stdout.contains("\"the session continues\""), "{out:?}");
}

/// Without the variable the same session prints the report's two lines
/// only (D-1038).
#[test]
fn repl_backtrace_off_prints_the_report_only() {
    let dir = scratch("off");
    let out = cheby(&dir, &["repl"], "0", Some(PANICKING_SESSION));
    assert_eq!(out.status, Some(0), "{out:?}");
    assert_eq!(
        out.stderr, "panicked at <repl:1>:3:14:\ntoo big: 3\n",
        "{out:?}"
    );
}

/// D-1038: the REPL reads the variable once per session, before its first
/// entry runs, and a bad value stops it with the error and status 1.
#[test]
fn repl_backtrace_bad_value_stops_the_session() {
    let dir = scratch("bad");
    let out = cheby(&dir, &["repl"], "yes", Some("1 + 1\n"));
    assert_eq!(out.status, Some(1), "{out:?}");
    assert_eq!(
        out.stderr, "error: CHEBY_BACKTRACE must be 0 or 1, not \"yes\"\n",
        "{out:?}"
    );
}

/// The program of `cheby test`'s two checks: a test that passes and one
/// that fails in a function it calls.
const TESTS: &str = "//! Tests, one of which fails.

fn check(n: Int) -> Int {
  case {
    n > 2 => panic as \"too big: {n}\"
    _ => n
  }
}

test \"small numbers pass\" {
  assert check(1) == 1
}

test \"large numbers fail\" {
  assert check(3) == 3
}
";

/// D-1038: `cheby test` reads the variable before its first test runs,
/// and a bad value stops it with the error and status 1, no test run.
#[test]
fn repl_backtrace_cheby_test_refuses_a_bad_value() {
    let dir = scratch("test-bad");
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"demo\"\nversion = \"0.0.0\"\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    std::fs::create_dir_all(dir.join("src")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("src/main.cheby"), TESTS).unwrap_or_else(|e| panic!("{e}"));
    let out = cheby(&dir, &["test"], "2", None);
    assert_eq!(out.status, Some(1), "{out:?}");
    assert_eq!(
        out.stderr, "error: CHEBY_BACKTRACE must be 0 or 1, not \"2\"\n",
        "{out:?}"
    );
    assert!(!out.stdout.contains("test "), "{out:?}");
}
