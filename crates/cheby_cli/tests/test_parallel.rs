//! `cheby test` runs its tests at once (D-723, ADR-0062, §13.6): each
//! test's output is captured and replayed in source order with the bytes
//! and the order of a run one test after another; a failed write of that
//! replay ends the run with 1 and prints nothing more
//! (D-834); tests that run at once share standard
//! input (D-835); and tests that wait run
//! beside each other, so the run phase shrinks with the thread count.
//!
//! The run phase is the runner's own measure (`TestReport::run_time`,
//! compile excluded), so that test drives `cheby_driver` in-process with
//! an explicit thread count; the others run `cheby` as a child. Time is
//! asserted as a lower bound and as a ratio of two run phases measured in
//! the same test, never as an absolute upper bound (plan §0
//! "Determinism").
//!
//! Unix only: the tests use pipes and a shell, and tests run as fibers
//! only where the runtime runs (Windows is M6, D-545, D-714).

#![cfg(unix)]

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cheby_driver::{PreparedTests, TestOptions, TestOutcome, TestProgram, prepare_test};
use cheby_test_support::cache::{CACHE_ENV, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long a child may run before it is killed: a guard against a hang,
/// never a measure of speed.
const PATIENCE: Duration = Duration::from_secs(60);

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";

/// `pause(ms)`: waits on a timer.
const PAUSE: &str = "fn pause(ms: Int) -> Nil {\n  selector::new()\n  \
     |> selector::after(_, duration::milliseconds(ms), fn() { Nil })\n  \
     |> selector::select\n}\n\n";

/// A package `app` in a fresh directory under the test target's temporary
/// directory, whose root module is `source`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(name: &str, source: &str) -> PathBuf {
    let dir = Path::new(TMP).join("test_parallel").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("cheby.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("src/main.cheby"), source).unwrap();
    dir
}

/// Runs `cheby test` in `dir` through `/bin/sh` with `redirects` applied
/// and `CHEBY_THREADS=threads`, feeding `stdin` when given.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to start the shell fails the test"
)]
fn cheby_test(dir: &Path, redirects: &str, threads: &str, stdin: Option<&[u8]>) -> RunOutput {
    // A fresh build cache of this command (GAP(cache-tests-isolated)).
    let cache = HarnessCache::create("cli-test-parallel").unwrap();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("exec \"$0\" --color=never test {redirects}"))
        .arg(CHEBY)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_THREADS", threads);
    cache.apply(&mut cmd);
    process::run(cmd, stdin, PATIENCE).unwrap()
}

/// D-723, ADR-0062: with both streams on one pipe, each test's writes come
/// out in the order the test made them, standard output's and standard
/// error's interleaved as in a run without the capture (a line of
/// standard output reaches the pipe at its newline, and
/// `io::write_stdout` flushes, D-365), before the test's line, while the
/// tests run at once and the second ends first.
#[test]
fn test_parallel_one_pipe_keeps_each_tests_chunk_order() {
    let source = format!(
        "//! Two tests write to both streams.\n\n\
         import std::duration\nimport std::io\nimport std::selector\n\n{PAUSE}\
         fn main() {{\n  Nil\n}}\n\n\
         test \"first\" {{\n  io::println(\"out 1\")\n  io::eprintln(\"err 1\")\n  pause(200)\n  \
         io::println(\"out 2\")\n  let assert Ok(Nil) = io::write_stdout(\"prompt \")\n  \
         io::eprintln(\"err 2\")\n}}\n\n\
         test \"second\" {{\n  io::eprintln(\"err 3\")\n  io::println(\"out 3\")\n  \
         io::eprint(\"err 4 \")\n  io::eprintln(\"done\")\n}}\n"
    );
    let dir = package("one_pipe", &source);
    for threads in ["1", "2", "4"] {
        let out = cheby_test(&dir, "2>&1", threads, None);
        assert_eq!(
            out.stdout,
            "out 1\nerr 1\nout 2\nprompt err 2\ntest app \"first\" ... ok\n\
             err 3\nout 3\nerr 4 done\ntest app \"second\" ... ok\n\
             \ntest result: ok. 2 passed; 0 failed; 0 filtered out\n",
            "CHEBY_THREADS={threads}"
        );
        assert_eq!(out.stderr, "", "CHEBY_THREADS={threads}");
        assert_eq!(out.status, Some(0), "CHEBY_THREADS={threads}");
    }
}

/// D-834: a test's own write to a broken standard
/// error goes to its capture and does not fail the test; the replay's
/// write of it fails, so the run stops there, prints nothing more (the
/// rest of that test's output, its line, the later tests and the summary),
/// and exits with 1, while a later test still runs. Standard error is a
/// pipe whose reading end is closed before the replay (a closed
/// descriptor is a null device instead, D-306).
#[test]
fn test_parallel_a_broken_stderr_during_replay_ends_the_run_with_one() {
    let source = format!(
        "//! The second test writes to a broken standard error.\n\n\
         import std::duration\nimport std::io\nimport std::selector\n\n{PAUSE}\
         fn main() {{\n  Nil\n}}\n\n\
         test \"first\" {{\n  pause(200)\n  io::println(\"first output\")\n}}\n\n\
         test \"writes\" {{\n  io::println(\"before\")\n  io::eprintln(\"lost\")\n  \
         io::println(\"never printed\")\n}}\n\n\
         test \"third\" {{\n  pause(1000)\n  io::println(\"never printed either\")\n}}\n"
    );
    let dir = package("broken_stderr", &source);
    let cache = HarnessCache::create("cli-test-parallel").unwrap();
    let mut child = Command::new(CHEBY)
        .args(["--color=never", "test"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache.path())
        .env("CHEBY_THREADS", "4")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if start.elapsed() > PATIENCE {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = reader.join().unwrap();
    assert_eq!(
        stdout, "first output\ntest app \"first\" ... ok\nbefore\n",
        "status {status:?}"
    );
    assert_eq!(status, Some(1));
}

/// D-835: tests that run at once share
/// standard input and take its lines in an unspecified order; both
/// finish, each with one of the lines.
#[test]
fn test_parallel_two_tests_that_read_stdin_both_finish() {
    let source = format!(
        "//! Two tests read a line each.\n\n\
         import std::duration\nimport std::io\nimport std::selector\n\n{PAUSE}\
         fn main() {{\n  Nil\n}}\n\n\
         fn read_one() -> Nil {{\n  let assert Ok(Some(line)) = io::read_line()\n  \
         assert line == \"one\" || line == \"two\"\n}}\n\n\
         test \"a reads\" {{\n  read_one()\n  pause(100)\n}}\n\n\
         test \"b reads\" {{\n  read_one()\n  pause(100)\n}}\n"
    );
    let dir = package("stdin", &source);
    for threads in ["1", "2", "4"] {
        let out = cheby_test(&dir, "", threads, Some(b"one\ntwo\n"));
        assert_eq!(
            out.stdout,
            "test app \"a reads\" ... ok\ntest app \"b reads\" ... ok\n\
             \ntest result: ok. 2 passed; 0 failed; 0 filtered out\n",
            "CHEBY_THREADS={threads}: {}",
            out.stderr
        );
        assert_eq!(out.status, Some(0), "CHEBY_THREADS={threads}");
    }
}

/// Compiles the package at `dir` with every test selected, to run on
/// `threads` workers.
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper: a package that does not build fails the test"
)]
fn compiled(dir: &Path, threads: usize) -> TestProgram {
    let PreparedTests::Ready(mut program) = prepare_test(dir, &TestOptions::default()).unwrap()
    else {
        panic!("the tests did not build");
    };
    program.set_threads(threads);
    *program
}

/// D-723, §13.6: eight tests that each wait 200 ms on a timer take at
/// least 1.6 s one at a time, and at four threads, run four at once, at
/// most 0.6 times that run phase (about 0.25 times with no overhead).
#[test]
fn test_parallel_eight_waiting_tests_run_at_once() {
    let mut tests = String::new();
    for n in 1..=8 {
        write!(tests, "\ntest \"waits {n}\" {{\n  pause(200)\n}}\n").unwrap();
    }
    let source = format!(
        "//! Eight tests wait on a timer.\n\n\
         import std::duration\nimport std::selector\n\n{PAUSE}fn main() {{\n  Nil\n}}\n{tests}"
    );
    let dir = package("eight_waits", &source);
    let phase = |threads: usize| {
        let mut out = Vec::new();
        let report = compiled(&dir, threads)
            .run(&mut out, &mut std::io::sink())
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(report.success(), "{text}");
        assert!(
            report
                .modules
                .iter()
                .flat_map(|module| &module.tests)
                .all(|test| test.outcome == TestOutcome::Passed),
            "{text}"
        );
        assert!(text.ends_with("test result: ok. 8 passed; 0 failed; 0 filtered out\n"));
        report.run_time
    };
    let one = phase(1);
    assert!(
        one >= Duration::from_millis(1600),
        "one at a time, the run phase took {one:?}"
    );
    let four = phase(4);
    assert!(
        four.as_secs_f64() <= 0.6 * one.as_secs_f64(),
        "four at once took {four:?}, one at a time {one:?}"
    );
}
