//! Standard input, standard output and file IO under fibers, end to end
//! through `cheby run` (D-683, D-684, D-692, D-670): a fiber blocked in
//! `io::read_line` or in a file call waits on the blocking pool, so the
//! other fibers keep running, also at `CHEBY_THREADS=1`; a timeout around
//! a read waits for the read, whose line is released; one `println` is one
//! write.
//!
//! Each test runs its program as a child at `CHEBY_THREADS` 1, 2 and 4. A
//! test that waits ends through the child's output or through closing its
//! standard input, never through a timeout: [`PATIENCE`] only stops a hung
//! child. Time is asserted only as a lower bound.
//!
//! `BLOCKING_IO_CHEBY`, when set, names the `cheby` the tests run instead
//! of the one cargo built, so that a loop runs a copy that no build
//! replaces meanwhile.
//!
//! Unix only: the tests hold the child's standard input open as a pipe
//! (programs do not run on Windows until M6, D-545).

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cheby_test_support::cache::{CACHE_ENV, HarnessCache};
use cheby_test_support::process;

const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long a test waits for output it expects, or for the child to
/// exit, before it kills the child and fails: a guard against a hang,
/// never a measure of speed.
const PATIENCE: Duration = Duration::from_secs(60);

/// The thread counts every test runs its program at.
const THREADS: [&str; 3] = ["1", "2", "4"];

/// The `cheby` the tests run: `BLOCKING_IO_CHEBY`, or the one cargo built.
fn cheby() -> PathBuf {
    std::env::var_os("BLOCKING_IO_CHEBY")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_cheby")), PathBuf::from)
}

/// A package `name` in this test binary's scratch directory, with `main`
/// as its root module. The process id keeps two runs of the binary at once
/// apart.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(name: &str, main: &str) -> PathBuf {
    let dir = Path::new(TMP)
        .join("blocking_io")
        .join(format!("{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.cheby"), main).unwrap();
    dir
}

/// `cheby run` in `dir` at `CHEBY_THREADS=threads`, with standard input
/// null, killed after [`PATIENCE`].
fn run(dir: &Path, threads: &str) -> process::RunOutput {
    run_with_input(dir, threads, None)
}

/// `cheby run` in `dir` at `CHEBY_THREADS=threads`, with `input` piped to
/// standard input (null for `None`), killed after [`PATIENCE`].
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn run_with_input(dir: &Path, threads: &str, input: Option<&[u8]>) -> process::RunOutput {
    // A fresh build cache of this command (GAP(cache-tests-isolated)).
    let cache = HarnessCache::create("cli-blocking-io").unwrap();
    let mut cmd = Command::new(cheby());
    cmd.args(["--color=never", "run"])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_THREADS", threads);
    cache.apply(&mut cmd);
    process::run(cmd, input, PATIENCE).unwrap()
}

/// A running `cheby run` whose standard input the test holds open, and
/// whose standard output it reads line by line. Dropping it kills the
/// child, so a failed assertion leaves no process behind.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Option<JoinHandle<String>>,
    threads: &'static str,
    /// The child's build cache, deleted with this value.
    _cache: HarnessCache,
}

impl Running {
    /// Starts `cheby run` in `dir` at `CHEBY_THREADS=threads`.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: failing to spawn `cheby` fails the test"
    )]
    fn start(dir: &Path, threads: &'static str) -> Self {
        let cache = HarnessCache::create("cli-blocking-io").unwrap();
        let mut child = Command::new(cheby())
            .args(["--color=never", "run"])
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, cache.path())
            .env("CHEBY_THREADS", threads)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Self {
            child,
            stdin,
            lines,
            stderr: Some(stderr),
            threads,
            _cache: cache,
        }
    }

    /// The next line of standard output; fails if none comes within
    /// [`PATIENCE`] or the output ends first.
    #[expect(clippy::panic, reason = "a test helper: a missing line fails the test")]
    fn next_line(&mut self, what: &str) -> String {
        match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                self.kill();
                panic!(
                    "CHEBY_THREADS={}: no {what} within {PATIENCE:?}",
                    self.threads
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.kill();
                let stderr = self.stderr();
                panic!(
                    "CHEBY_THREADS={}: the output ended before the {what}: {stderr}",
                    self.threads
                );
            }
        }
    }

    /// A line of standard output that has already arrived, if any.
    fn arrived(&self) -> Option<String> {
        self.lines.try_recv().ok()
    }

    /// Writes `text` to the child's standard input and flushes it.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: a pipe that cannot be written fails the test"
    )]
    fn write(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Closes the child's standard input: its next read sees the end of
    /// input.
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Closes the child's standard input, waits for it to exit within
    /// [`PATIENCE`], checks that it wrote nothing to standard error, and
    /// returns its status and the lines of standard output not read yet.
    #[expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a test helper: a child that cannot be waited for fails the test"
    )]
    fn finish(mut self) -> (Option<i32>, Vec<String>) {
        self.close_stdin();
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code();
            }
            if start.elapsed() > PATIENCE {
                self.kill();
                panic!(
                    "CHEBY_THREADS={}: the child did not exit within {PATIENCE:?}",
                    self.threads
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut rest = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(PATIENCE) {
            rest.push(line);
        }
        let stderr = self.stderr();
        assert_eq!(stderr, "", "CHEBY_THREADS={}: standard error", self.threads);
        (status, rest)
    }

    /// The child's standard error, once it has ended.
    fn stderr(&mut self) -> String {
        self.stderr
            .take()
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default()
    }

    /// Kills the child, if it still runs.
    fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.kill();
        }
    }
}

/// A scoped reader that blocks in `io::read_line`, and a channel
/// ping-pong of 1000 rounds between two other fibers of the scope, whose
/// result `main` prints before it joins the reader.
const READER_AND_PING_PONG: &str = r#"//! A scoped reader blocks in `io::read_line` while two other fibers play a
//! channel ping-pong, whose result `main` prints before it joins the reader.

import std::channel
import std::fiber
import std::io

fn pong(requests: channel::Receiver<Int>, replies: channel::Sender<Int>) -> Nil {
  case channel::receive(requests) {
    Ok(-1) => Nil
    Ok(n) => {
      let assert Ok(Nil) = channel::send(replies, n + 1)
      pong(requests, replies)
    }
    Err(_) => Nil
  }
}

fn ping(n: Int, out: channel::Sender<Int>, back: channel::Receiver<Int>) -> Int {
  case n >= 1000 {
    True => {
      let assert Ok(Nil) = channel::send(out, -1)
      n
    }
    False => {
      let assert Ok(Nil) = channel::send(out, n)
      let assert Ok(m) = channel::receive(back)
      ping(m, out, back)
    }
  }
}

fn main() {
  let (started_tx, started_rx) = channel::new::<Nil>(1)
  let (requests_tx, requests_rx) = channel::new::<Int>(0)
  let (replies_tx, replies_rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    let reader = fiber::spawn(owner, fn() {
      let assert Ok(Nil) = channel::send(started_tx, Nil)
      io::read_line()
    })
    let assert Ok(Nil) = channel::receive(started_rx)
    let _ = fiber::spawn(owner, fn() { pong(requests_rx, replies_tx) })
    let pinger = fiber::spawn(owner, fn() { ping(0, requests_tx, replies_rx) })
    let rounds = fiber::join(pinger)
    io::println("ping-pong: {rounds} rounds")
    case fiber::join(reader) {
      Ok(Some(line)) => io::println("read: {line}")
      Ok(None) => io::println("read: end of input")
      Err(_) => io::println("read: failed")
    }
  }
  case outcome {
    Ok(_) => Nil
    Err(_) => io::println("panicked")
  }
}
"#;

/// I1's done-when (D-683, D-692): a fiber blocked in `io::read_line` on a
/// standard input that stays open leaves the other fibers running, also
/// at one thread, where before the pool the read held the only worker. The
/// ping-pong's line arrives while standard input is still open (the test
/// reads it before it closes the pipe); then the end of input ends the
/// read, and the program exits with 0.
#[test]
fn blocking_io_a_reader_on_open_stdin_leaves_the_other_fibers_running() {
    let dir = package("reader_and_ping_pong", READER_AND_PING_PONG);
    for threads in THREADS {
        let mut child = Running::start(&dir, threads);
        assert_eq!(
            child.next_line("ping-pong line"),
            "ping-pong: 1000 rounds",
            "CHEBY_THREADS={threads}"
        );
        child.close_stdin();
        let (status, rest) = child.finish();
        assert_eq!(rest, ["read: end of input"], "CHEBY_THREADS={threads}");
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}");
    }
}

/// A scoped fiber reads standard input to its end and checks every line.
const READ_IN_ORDER: &str = r#"//! A scoped fiber reads standard input to its end and checks that line n
//! is `line n`.

import std::fiber
import std::io

fn read_all(n: Int) -> String {
  case io::read_line() {
    Ok(Some(line)) => case line == "line {n}" {
      True => read_all(n + 1)
      False => "line {n} is {line}"
    }
    Ok(None) => {
      let count = n - 1
      "{count} lines in order"
    }
    Err(_) => "the read failed"
  }
}

fn main() {
  let outcome = {
    use owner <- fiber::scope()
    let reader = fiber::spawn(owner, fn() { read_all(1) })
    io::println(fiber::join(reader))
  }
  case outcome {
    Ok(_) => Nil
    Err(_) => io::println("panicked")
  }
}
"#;

/// D-823, D-474, D-305: 20001 piped lines, more than one
/// read of the reader's buffer, the last one with no ending, reach the
/// program whole and in order, whether a line was already buffered (taken
/// on the fiber) or had to be waited for (on the blocking pool).
#[test]
fn blocking_io_a_reader_gets_every_piped_line_in_order() {
    let dir = package("read_in_order", READ_IN_ORDER);
    let mut input = (1..=20000).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "line {n}");
        text
    });
    input.push_str("line 20001");
    for threads in THREADS {
        let out = run_with_input(&dir, threads, Some(input.as_bytes()));
        assert_eq!(out.stderr, "", "CHEBY_THREADS={threads}");
        assert_eq!(
            out.stdout, "20001 lines in order\n",
            "CHEBY_THREADS={threads}"
        );
        assert_eq!(
            out.status,
            Some(0),
            "CHEBY_THREADS={threads}: {}",
            out.describe_status()
        );
    }
}

/// A 100 ms timeout around `io::read_line`, then a second read.
const TIMED_READ: &str = r#"//! A 100 ms timeout around `io::read_line`, then a second read.

import std::duration
import std::fiber
import std::io

fn main() {
  io::println("waiting")
  case fiber::timeout(duration::milliseconds(100), fn() { io::read_line() }) {
    Err(fiber::TimedOut) => io::println("TimedOut")
    Err(fiber::Panicked(_)) => io::println("panicked")
    Ok(_) => io::println("returned in time")
  }
  case io::read_line() {
    Ok(Some(line)) => io::println("next: {line}")
    Ok(None) => io::println("next: end of input")
    Err(_) => io::println("next: failed")
  }
}
"#;

/// D-670, D-683: a read on the blocking pool cannot be interrupted. The
/// timeout's limit passes 100 ms into the read, but `TimedOut` comes only
/// once the read has returned, after the test writes the first line about
/// 500 ms after `waiting`; that line was consumed and released, and the
/// next `io::read_line` returns the second line.
#[test]
fn blocking_io_a_timeout_around_read_line_waits_for_the_line() {
    let dir = package("timed_read", TIMED_READ);
    for threads in THREADS {
        let mut child = Running::start(&dir, threads);
        assert_eq!(child.next_line("first line"), "waiting");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            child.arrived(),
            None,
            "CHEBY_THREADS={threads}: printed before the line was written"
        );
        child.write("first\n");
        assert_eq!(
            child.next_line("timeout's result"),
            "TimedOut",
            "CHEBY_THREADS={threads}"
        );
        child.write("second\n");
        assert_eq!(
            child.next_line("second read's line"),
            "next: second",
            "CHEBY_THREADS={threads}"
        );
        let (status, rest) = child.finish();
        assert_eq!(rest, Vec::<String>::new(), "CHEBY_THREADS={threads}");
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}");
    }
}

/// Eight fibers, each printing 200 lines of 1000 copies of its own
/// letter.
const EIGHT_PRINTERS: &str = r#"//! Eight fibers, each printing 200 lines of 1000 copies of its own letter.

import std::fiber
import std::io
import std::list
import std::string

fn print_lines(line: String, left: Int) -> Nil {
  case left > 0 {
    True => {
      io::println(line)
      print_lines(line, left - 1)
    }
    False => Nil
  }
}

fn main() {
  let outcome = {
    use owner <- fiber::scope()
    list::each(["a", "b", "c", "d", "e", "f", "g", "h"], fn(letter) {
      let line = string::repeat(letter, 1000)
      let _ = fiber::spawn(owner, fn() { print_lines(line, 200) })
      Nil
    })
  }
  case outcome {
    Ok(_) => Nil
    Err(_) => io::println("panicked")
  }
}
"#;

/// D-684: one `println` is one write under standard output's lock, so the
/// lines of fibers that print at once never mix: every line is 1000
/// copies of one letter, and each letter has exactly 200 lines. The order
/// of the lines is not checked.
#[test]
fn blocking_io_printed_lines_stay_whole_under_concurrency() {
    let dir = package("eight_printers", EIGHT_PRINTERS);
    let expected: BTreeMap<char, usize> = ('a'..='h').map(|letter| (letter, 200)).collect();
    for threads in THREADS {
        let out = run(&dir, threads);
        assert_eq!(out.stderr, "", "CHEBY_THREADS={threads}");
        assert_eq!(
            out.status,
            Some(0),
            "CHEBY_THREADS={threads}: {}",
            out.describe_status()
        );
        let mut counts: BTreeMap<char, usize> = BTreeMap::new();
        for (index, line) in out.stdout.lines().enumerate() {
            let mut letters = line.chars();
            let first = letters.next();
            assert!(
                line.len() == 1000 && letters.all(|letter| Some(letter) == first),
                "CHEBY_THREADS={threads}: line {index} is not whole: {line:?}"
            );
            if let Some(letter) = first {
                let count = counts.entry(letter).or_default();
                *count = count.saturating_add(1);
            }
        }
        assert_eq!(counts, expected, "CHEBY_THREADS={threads}");
        assert!(
            out.stdout.ends_with('\n'),
            "CHEBY_THREADS={threads}: the last line has no ending"
        );
    }
}

/// Sixteen fibers, each writing its own file and reading it back.
const SIXTEEN_FILES: &str = r#"//! Sixteen fibers, each writing its own file and reading it back.

import std::fiber
import std::io
import std::list
import std::string

fn round_trip(n: Int) -> String {
  let path = "file_{n}.txt"
  let text = string::repeat("fiber {n}\n", 100)
  case io::write_file(path, text) {
    Ok(_) => case io::read_file(path) {
      Ok(back) => case back == text {
        True => "file {n}: ok"
        False => "file {n}: read back other text"
      }
      Err(_) => "file {n}: the read failed"
    }
    Err(_) => "file {n}: the write failed"
  }
}

fn main() {
  let outcome = {
    use owner <- fiber::scope()
    let fibers = list::map(list::range(1, 17), fn(n) {
      fiber::spawn(owner, fn() { round_trip(n) })
    })
    list::each(fibers, fn(child) { io::println(fiber::join(child)) })
  }
  case outcome {
    Ok(_) => Nil
    Err(_) => io::println("panicked")
  }
}
"#;

/// D-683, D-692: sixteen fibers of one scope write and read back their own
/// files at once, each parked while the blocking pool runs its call, and
/// each gets its own text back. Joined in a fixed order, the output is
/// fixed.
#[test]
fn blocking_io_fibers_write_and_read_back_their_own_files() {
    let dir = package("sixteen_files", SIXTEEN_FILES);
    let expected = (1..=16).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "file {n}: ok");
        text
    });
    for threads in THREADS {
        let out = run(&dir, threads);
        assert_eq!(out.stderr, "", "CHEBY_THREADS={threads}");
        assert_eq!(out.stdout, expected, "CHEBY_THREADS={threads}");
        assert_eq!(
            out.status,
            Some(0),
            "CHEBY_THREADS={threads}: {}",
            out.describe_status()
        );
    }
}
