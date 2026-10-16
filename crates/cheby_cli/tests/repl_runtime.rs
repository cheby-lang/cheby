//! `cheby repl` on the runtime (D-712, D-194, plan L2): a session has one
//! runtime, its detached fibers run across entries, and the session's end
//! and a termination between entries end the process as §13.5 and the
//! answers to UQ-10 and UQ-11 say. Each test drives a real `cheby repl`
//! through pipes, so it can wait for a prompt before it writes the next
//! line, keep standard input open, or break standard error.
//!
//! The time limits are hang detectors, scaled by `CHEBY_HARNESS_SCALE`
//! (D-722): each measures from the moment the REPL has read everything
//! that comes before the step it times, never the session's start-up.
//!
//! Unix only: the tests need pipes whose other end the test closes
//! (D-545: no programs run on Windows until M6).

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cheby_test_support::process;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long a session may take to start and to answer an entry, before
/// `CHEBY_HARNESS_SCALE`: a hang detector only.
const ANSWER: Duration = Duration::from_secs(60);

/// How long the session's end may take once the REPL has read its last
/// line (plan L2's "within 5 s"), before `CHEBY_HARNESS_SCALE`.
const END: Duration = Duration::from_secs(5);

/// `base` scaled by `CHEBY_HARNESS_SCALE` (D-722).
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an invalid scale fails the test"
)]
fn scaled(base: Duration) -> Duration {
    process::scaled_timeout(base).unwrap()
}

/// An empty directory of its own for one test's session: no package.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to make the directory fails the test"
)]
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(TMP)
        .join("repl_runtime")
        .join(format!("{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A running `cheby repl` with standard input and output piped: standard
/// output is collected as it arrives, so a test can wait for a prompt.
struct Repl {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Arc<Mutex<Vec<u8>>>,
}

impl Repl {
    /// Starts `cheby repl` in a scratch directory named after `name`, with
    /// `env` added; standard error goes to a pipe that the test closes at
    /// once when `break_stderr`, so the session's first write to it fails
    /// (D-306), and is discarded otherwise.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: failing to start `cheby` fails the test"
    )]
    fn start(name: &str, env: &[(&str, &str)], break_stderr: bool) -> Self {
        let mut child = Command::new(CHEBY)
            .args(["--color=never", "repl"])
            .current_dir(scratch(name))
            .env("NO_COLOR", "1")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if break_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .spawn()
            .unwrap();
        // Closing the pipe's only read end makes every write to it fail
        // with a broken pipe: a stream open at start that fails (D-306).
        drop(child.stderr.take());
        let stdin = child.stdin.take();
        let mut pipe = child.stdout.take().unwrap();
        let stdout: Arc<Mutex<Vec<u8>>> = Arc::default();
        let sink = Arc::clone(&stdout);
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap()
                        .extend_from_slice(chunk.get(..n).unwrap_or_default()),
                }
            }
        });
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Writes `text` to the session's standard input.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: a closed input fails the test"
    )]
    fn send(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Closes the session's standard input: the end of input.
    fn close_input(&mut self) {
        drop(self.stdin.take());
    }

    /// What the session wrote to standard output so far.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: a poisoned lock fails the test"
    )]
    fn output(&self) -> String {
        process::normalize(&self.stdout.lock().unwrap())
    }

    /// Waits until standard output ends with `tail`, at most `limit`;
    /// whether it did. The REPL flushes a prompt only right before it
    /// reads (D-548), so a tail that ends with the prompt `> ` means the
    /// session has read and run everything before it.
    fn wait_for(&self, tail: &str, limit: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.output().ends_with(tail) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Waits at most `limit` for the session to exit: its status, or
    /// `None` when it was still running and was killed.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: failing to wait for `cheby` fails the test"
    )]
    fn wait_exit(&mut self, limit: Duration) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            if start.elapsed() > limit {
                self.child.kill().ok();
                self.child.wait().ok();
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        self.close_input();
        if matches!(self.child.try_wait(), Ok(None)) {
            self.child.kill().ok();
            self.child.wait().ok();
        }
    }
}

/// Lines that define `nap(ms)`, which waits on a timer for `ms`
/// milliseconds (as 019's `clock::sleep`), with the imports the tests use.
const PRELUDE: &str = concat!(
    "import std::duration\n",
    "import std::fiber\n",
    "import std::io\n",
    "import std::selector\n",
    "fn nap(ms: Int) {\n",
    "  selector::new()\n",
    "  |> selector::after(_, duration::milliseconds(ms), fn() { Nil })\n",
    "  |> selector::select\n",
    "}\n",
);

/// What the REPL echoes for [`PRELUDE`], ending with the prompt of the
/// next entry: the REPL is then waiting for input.
const PRELUDE_ECHO: &str = concat!(
    "> import std::duration\n",
    "> import std::fiber\n",
    "> import std::io\n",
    "> import std::selector\n",
    "> fn nap(ms: Int) {\n",
    ".   selector::new()\n",
    ".   |> selector::after(_, duration::milliseconds(ms), fn() { Nil })\n",
    ".   |> selector::select\n",
    ". }\n",
    "> ",
);

/// D-712, D-194: at the end of input, the session tears down a detached
/// fiber that never waits (it spins through tail calls, each one an entry
/// check where its cancellation reaches it), shuts its runtime down and
/// exits with 0, within 5 s of the end of input.
#[test]
fn repl_runtime_end_of_input_with_a_detached_spinner_exits() {
    let mut repl = Repl::start("spinner", &[], false);
    repl.send(PRELUDE);
    repl.send("fn spin(n: Int) -> Int {\n  spin(n + 1)\n}\n");
    repl.send("fiber::spawn_detached(fn() { spin(0) })\n");
    let read = "> fiber::spawn_detached(fn() { spin(0) })\n> ";
    assert!(repl.wait_for(read, scaled(ANSWER)), "{}", repl.output());
    repl.close_input();
    let status = repl.wait_exit(scaled(END));
    assert_eq!(status, Some(0), "{}", repl.output());
}

/// D-712, D-786, D-804 and GAP(repl-detached-stdin): `:quit` while a
/// detached fiber is blocked in `io::read_line` on standard input that
/// stays open ends the session with 0 within 5 s. The fiber first naps,
/// so its read starts after the REPL is already reading the next line;
/// the REPL takes `:quit`, and the fiber's read, on the blocking pool
/// (D-683), then waits for a line that never comes: the teardown skips it
/// and counts it, and the runtime's stop gives up on it.
#[test]
fn repl_runtime_quit_with_a_detached_reader_blocked_on_an_open_pipe_exits() {
    let mut repl = Repl::start("blocked_reader", &[], false);
    repl.send(PRELUDE);
    assert!(
        repl.wait_for(PRELUDE_ECHO, scaled(ANSWER)),
        "{}",
        repl.output()
    );
    repl.send("fiber::spawn_detached(fn() {\n  nap(300)\n  io::read_line()\n})\n");
    assert!(
        repl.wait_for(". })\n> ", scaled(ANSWER)),
        "{}",
        repl.output()
    );
    // The REPL reads the next line now; the fiber's read starts after its
    // nap and waits behind it.
    std::thread::sleep(Duration::from_secs(1));
    repl.send(":quit\n");
    let status = repl.wait_exit(scaled(END));
    assert_eq!(status, Some(0), "{}", repl.output());
    assert!(repl.output().ends_with("> :quit\n"), "{}", repl.output());
}

/// D-306, D-709 and GAP(repl-termination-between-entries): a detached
/// fiber's write to a broken standard error, while no entry runs, ends the
/// session with status 1 before the next prompt, printing nothing more:
/// the next entry is never evaluated, so its value line never appears.
#[test]
fn repl_runtime_a_detached_write_failure_between_entries_ends_the_session_with_one() {
    let mut repl = Repl::start("write_failure", &[], true);
    repl.send(PRELUDE);
    assert!(
        repl.wait_for(PRELUDE_ECHO, scaled(ANSWER)),
        "{}",
        repl.output()
    );
    repl.send("fiber::spawn_detached(fn() {\n  nap(100)\n  io::eprintln(\"lost\")\n})\n");
    assert!(
        repl.wait_for(". })\n> ", scaled(ANSWER)),
        "{}",
        repl.output()
    );
    // The fiber's write fails 100 ms after its spawn, while the REPL waits
    // for this line; the line is read only to end the session, so it is
    // neither echoed nor run (a root started after the termination would
    // not run either, D-802, but the echo would show).
    std::thread::sleep(Duration::from_secs(2));
    repl.send("1 + 1\n");
    let status = repl.wait_exit(scaled(ANSWER));
    let output = repl.output();
    assert_eq!(status, Some(1), "{output}");
    assert!(!output.contains("1 + 1"), "{output}");
    assert!(!output.contains("2 : Int"), "{output}");
    assert!(output.starts_with(PRELUDE_ECHO), "{output}");
}

/// The entries of the constants test: a detached fiber calls `f` through
/// its slot in a loop while entries redefine `f` with the same signature
/// to read constants no entry has evaluated yet: a standard-library
/// constant, then each entry's own, then one whose initializer calls
/// another redefinition of the same entry.
fn redefinitions_reading_new_constants() -> String {
    let mut text = String::from(
        "import std::duration\n\
         import std::fiber\n\
         fn f() -> Int {\n  1\n}\n\
         fn g() -> Int {\n  7\n}\n\
         fn spin(n: Int) -> Int {\n\
         \x20 case f() {\n\
         \x20   0 => n\n\
         \x20   _ => spin(n + 1)\n\
         \x20 }\n\
         }\n\
         fiber::spawn_detached(fn() { spin(0) })\n\
         fn f() -> Int {\n  duration::to_milliseconds(duration::zero) + 1\n}\n\
         f()\n",
    );
    text.extend((1..=20).map(|n| format!("const k{n}: Int = {n}\nfn f() -> Int {{\n  k{n}\n}}\n")));
    text.push_str("f()\n");
    text.push_str("fn g() -> Int {\n  8\n}\nconst m: Int = g()\nfn f() -> Int {\n  m\n}\n");
    text.push_str("f()\n1 + 1\n");
    text
}

/// D-712, D-547, D-194 and GAP(repl-publish-after-constants) (gate 3's
/// CONC-W3-1): a redefinition's new code reaches a detached caller only
/// once the constants it reads are filled, so the caller never reads an
/// empty constant slot (a fatal error outside constant evaluation); a
/// constant whose initializer calls a redefinition of its own entry sees
/// the new code. At 1, 2 and 4 threads.
#[test]
fn repl_runtime_a_redefinition_reads_its_new_constants_under_a_detached_caller() {
    for threads in ["1", "2", "4"] {
        let dir = scratch(&format!("new_constants_{threads}"));
        let mut cmd = Command::new(CHEBY);
        cmd.args(["--color=never", "repl"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env("CHEBY_THREADS", threads);
        let input = redefinitions_reading_new_constants();
        let out = process::run(cmd, Some(input.as_bytes()), scaled(ANSWER)).unwrap();
        assert_eq!(
            out.status,
            Some(0),
            "threads {threads}\n{}\n{}",
            out.stdout,
            out.stderr
        );
        assert_eq!(out.stderr, "", "threads {threads}");
        let values: Vec<&str> = out
            .stdout
            .lines()
            .filter(|line| !line.starts_with("> ") && !line.starts_with(". "))
            .filter(|line| line.ends_with(" : Int"))
            .collect();
        assert_eq!(
            values,
            ["1 : Int", "20 : Int", "8 : Int", "2 : Int"],
            "threads {threads}\n{}",
            out.stdout
        );
    }
}

/// The entries of the redefinition test, shared with the session test in
/// `cheby_driver::session` (plan L2): a detached fiber calls `f` through
/// its slot in a loop while 200 entries redefine `f` with the same
/// signature; it reports how many changes it saw once `f` returns 200, or
/// `-1` if it ever saw an older definition after a newer one.
fn redefinitions() -> String {
    let mut text = String::from(
        "import std::channel\n\
         import std::fiber\n\
         fn f() -> Int {\n  0\n}\n\
         fn watch(last: Int, changes: Int, done: channel::Sender<Int>) -> Int {\n\
         \x20 let now = f()\n\
         \x20 case {\n\
         \x20   now < last => {\n\
         \x20     let _ = channel::send(done, -1)\n\
         \x20     changes\n\
         \x20   }\n\
         \x20   now == 200 => {\n\
         \x20     let _ = channel::send(done, changes + 1)\n\
         \x20     changes\n\
         \x20   }\n\
         \x20   now > last => watch(now, changes + 1, done)\n\
         \x20   _ => watch(last, changes, done)\n\
         \x20 }\n\
         }\n\
         let (done_tx, done_rx) = channel::new::<Int>(1)\n\
         fiber::spawn_detached(fn() { watch(0, 0, done_tx) })\n",
    );
    text.extend((1..=200).map(|n| format!("fn f() -> Int {{\n  {n}\n}}\n")));
    text.push_str("channel::receive(done_rx)\n");
    text
}

/// D-712 and D-687: the 200 redefinitions under the migration stress mode,
/// which only the environment selects, so it goes on the child's own
/// command (never `std::env::set_var` here). The watcher never sees an
/// older `f` after a newer one, and sees the last.
#[test]
fn repl_runtime_redefinition_under_stress() {
    let dir = scratch("stress");
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "repl"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_RT_STRESS_MIGRATE", "1")
        .env("CHEBY_THREADS", "4");
    let input = redefinitions();
    let out = process::run(
        cmd,
        Some(input.as_bytes()),
        scaled(Duration::from_secs(300)),
    )
    .unwrap();
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    assert_eq!(out.stderr, "");
    let line = out
        .stdout
        .lines()
        .find_map(|line| line.strip_suffix(" : Result<Int, Nil>"))
        .unwrap_or_else(|| panic!("no value line: {}", out.stdout));
    let changes: i64 = line
        .strip_prefix("Ok(")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("not a count: {line}"));
    assert!((1..=200).contains(&changes), "{line}");
}
