//! `process::exit` under every host (D-1022, D-1023, D-709, D-828): under
//! `cheby run` and in an executable the process ends with the status,
//! after standard output is flushed, with a status outside 0 to 255 read
//! as 255; under `cheby test` only the test that called it fails, with
//! `process::exit called with status N`, and the run continues; in the
//! REPL the session ends with the status.
//!
//! Every `cheby` gets a fresh `CHEBY_CACHE` of its own (D-1128). The
//! executable test links against the runtime archive next to `cheby`, so
//! `cargo build -p cheby_rt_exe` comes first (D-929).

#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

/// Writes `main` with `io::print`, which standard output buffers, then
/// exits with `STATUS` while a detached fiber waits on a channel forever:
/// the flush shows the text, the waiting fiber stops without a deadlock
/// report, and no leak check runs (D-709, D-710, D-1022).
const EXITS: &str = "//! Exits with a status.\n\nimport std::channel\nimport std::fiber\nimport std::io\nimport std::process\n\nfn main() {\n  let (tx, rx) = channel::new::<Int>(0)\n  fiber::spawn_detached(fn() {\n    let _ = channel::receive(rx)\n    io::println(\"never\")\n  })\n  io::print(\"before exit\")\n  process::exit(STATUS)\n  let _ = channel::send(tx, 1)\n  io::println(\"after exit\")\n}\n";

/// A package `name` in `dir` whose root module is `main`.
fn package(dir: &Path, name: &str, main: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    std::fs::write(root.join("src/main.cheby"), main).unwrap();
    root
}

/// A fresh build cache (D-1128).
fn fresh_cache() -> HarnessCache {
    HarnessCache::create_with("cli-process-exit", None).unwrap()
}

/// `cheby <args>` in `dir` with a fresh cache, feeding `stdin` when given.
fn cheby(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> RunOutput {
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, stdin, TIMEOUT).unwrap()
}

/// `cheby run` ends with the status `process::exit` gives, after flushing
/// what `main` printed; nothing after the call runs, the waiting detached
/// fiber neither prints nor deadlocks, and a status outside 0 to 255 is
/// 255 (D-1022, D-709).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn process_exit_run_ends_with_its_status() {
    let tmp = FreshDir::create("process-exit-run").unwrap();
    for (status, expected) in [("0", 0), ("3", 3), ("255", 255), ("256", 255), ("-1", 255)] {
        let source = EXITS.replace("STATUS", status);
        let root = package(
            tmp.path(),
            &format!("exits_{}", status.replace('-', "m")),
            &source,
        );
        let out = cheby(&root, &["run"], None);
        assert_eq!(out.status, Some(expected), "{status}: {}", out.stderr);
        assert_eq!(out.stdout, "before exit", "{status}");
        assert_eq!(out.stderr, "", "{status}");
    }
}

/// An executable ends as `cheby run` does (D-1023): the same status,
/// output and standard error.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn process_exit_executable_matches_run() {
    let tmp = FreshDir::create("process-exit-exe").unwrap();
    for (status, expected) in [("7", 7), ("1000", 255)] {
        let source = EXITS.replace("STATUS", status);
        let root = package(tmp.path(), &format!("exe_exits_{status}"), &source);
        let exe = tmp.path().join("exe").join(format!("exits_{status}"));
        let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], None);
        assert_eq!(build.status, Some(0), "{}", build.stderr);
        let mut cmd = Command::new(&exe);
        cmd.current_dir(&root);
        let ran = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(ran.status, Some(expected), "{status}: {}", ran.stderr);
        assert_eq!(ran.stdout, "before exit", "{status}");
        assert_eq!(ran.stderr, "", "{status}");
        let run = cheby(&root, &["run"], None);
        assert_eq!(run.status, ran.status);
        assert_eq!(run.stdout, ran.stdout);
    }
}

/// Under `cheby test` the test that calls `process::exit` fails with
/// `process::exit called with status N`, and the other tests run on
/// (D-1023): the run reports one failure and one pass, and exits with 1.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn process_exit_fails_only_the_calling_test() {
    let tmp = FreshDir::create("process-exit-test").unwrap();
    let root = package(
        tmp.path(),
        "exits_in_a_test",
        "//! Two tests, one of which exits.\n\nimport std::io\nimport std::process\n\nfn main() {\n  Nil\n}\n\ntest \"exits\" {\n  io::println(\"exiting\")\n  process::exit(4)\n}\n\ntest \"passes\" {\n  assert 1 + 1 == 2\n}\n",
    );
    let out = cheby(&root, &["test"], None);
    assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .contains("test exits_in_a_test \"exits\" ... FAILED"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("test exits_in_a_test \"passes\" ... ok"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("---- exits_in_a_test \"exits\" ----\nprocess::exit called with status 4\n"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("1 passed; 1 failed"), "{}", out.stdout);
}

/// A fiber of a test that calls `process::exit`, in a scope or detached,
/// fails that test as the test's own call does, and the run continues
/// (D-1023, D-723).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn process_exit_in_a_fiber_fails_its_test() {
    let tmp = FreshDir::create("process-exit-fibers").unwrap();
    let root = package(
        tmp.path(),
        "exits_in_fibers",
        "//! Fibers that exit.\n\nimport std::channel\nimport std::fiber\nimport std::process\n\nfn main() {\n  Nil\n}\n\ntest \"a scoped fiber exits\" {\n  let outcome = {\n    use owner <- fiber::scope()\n    let child = fiber::spawn(owner, fn() { process::exit(5) })\n    fiber::join(child)\n  }\n  let _ = outcome\n  Nil\n}\n\ntest \"a detached fiber exits\" {\n  let (tx, rx) = channel::new::<Int>(0)\n  fiber::spawn_detached(fn() { process::exit(6) })\n  let _ = channel::receive(rx)\n  let _ = channel::send(tx, 1)\n  Nil\n}\n\ntest \"passes\" {\n  assert 2 * 3 == 6\n}\n",
    );
    let out = cheby(&root, &["test"], None);
    assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
    for expected in [
        "test exits_in_fibers \"a scoped fiber exits\" ... FAILED",
        "test exits_in_fibers \"a detached fiber exits\" ... FAILED",
        "test exits_in_fibers \"passes\" ... ok",
        "---- exits_in_fibers \"a scoped fiber exits\" ----\nprocess::exit called with status 5\n",
        "---- exits_in_fibers \"a detached fiber exits\" ----\nprocess::exit called with status 6\n",
        "1 passed; 2 failed",
    ] {
        assert!(out.stdout.contains(expected), "{expected}\n{}", out.stdout);
    }
}

/// In the REPL `process::exit` ends the session with its status (D-828,
/// D-1023): the entries after it are not read.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn process_exit_ends_the_repl_session() {
    let tmp = FreshDir::create("process-exit-repl").unwrap();
    let out = cheby(
        tmp.path(),
        &["repl"],
        Some(b"import std::io\nimport std::process\nio::print(\"by\" + \"e!\")\nprocess::exit(6)\n\"after\"\n"),
    );
    assert_eq!(out.status, Some(6), "{}\n{}", out.stdout, out.stderr);
    // The text the entry printed, not its echo, which splits it.
    assert!(out.stdout.contains("bye!"), "{}", out.stdout);
    assert!(!out.stdout.contains("after"), "{}", out.stdout);
}
