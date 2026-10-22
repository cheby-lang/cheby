//! Standard streams closed when the program starts behave as a null
//! device: writes succeed and are discarded, reads see end of input, and
//! the program exits normally. Only a failure of a stream that was open
//! at start ends the program with status 1 (D-306).
//!
//! Unix only: the shell closes the descriptors before `exec`ing `cheby`.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `cheby run` on the `lets` package, which prints `hi\ngreet:
/// hi\nhi!\n` to standard output and `hi\n` to standard error, after the
/// shell applies `redirects`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn the shell fails the test"
)]
fn run_with(redirects: &str) -> RunOutput {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cheby_driver/tests/packages/lets");
    // A fresh build cache of this test (GAP(cache-tests-isolated)).
    let cache = HarnessCache::create("cli-closed-streams").unwrap();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("exec \"$0\" run {redirects}"))
        .arg(CHEBY)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

#[test]
fn closed_stdout_discards_output() {
    let out = run_with(">&-");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "hi\n");
    assert_eq!(out.status, Some(0));
}

#[test]
fn closed_stderr_discards_output() {
    let out = run_with("2>&-");
    assert_eq!(out.stdout, "hi\ngreet: hi\nhi!\n");
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(0));
}

#[test]
fn all_streams_closed() {
    let out = run_with("<&- >&- 2>&-");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(0));
}
