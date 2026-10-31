//! `process::until_interrupted` with real signals (D-1073, ADR-0071): an
//! executable and `cheby run` whose `main` waits inside the scope on a
//! channel nobody sends to end through `main`'s own error path when SIGINT
//! or SIGTERM arrives, with their output flushed and the status of `main`'s
//! result; and a second SIGINT ends a program whose body is stuck, by the
//! signal, because the first restored the default action.
//!
//! Each test waits for a marker line the program prints once it waits
//! (never a fixed delay, R26), then sends the signal with the shell's
//! `kill`. A signal that the test's own process ignores (a job started in
//! the background by a non-interactive shell ignores SIGINT) would stay
//! ignored in the program, which then rightly keeps it ignored (D-1073's
//! "as before"), so in that case the program is started through `perl`,
//! which puts the default action back first.
//!
//! Every `cheby` gets a fresh `CHEBY_CACHE` of its own (D-1128). The
//! executables link against the runtime archive next to `cheby`, so
//! `cargo build -p cheby_rt_exe` comes first (D-929).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "test helpers: a failed setup step or a step past its bound fails the test; \
              a deadline is now plus a bound"
)]

use std::io::{BufRead as _, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, scaled_timeout};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");

/// `main` waits inside `until_interrupted` on a channel nobody sends to,
/// after printing `waiting`; a detached fiber's sleep is pending work, so
/// the wait is no deadlock (D-984, D-1073). On `Err(Interrupted)` it
/// prints `interrupted` and returns `Ok(Nil)`: status 0.
const INTERRUPTED: &str = "//! Waits inside `until_interrupted` on a channel nobody sends to, and says so when a signal ends it.

import std::channel
import std::duration
import std::fiber
import std::io
import std::process
import std::time

fn main() -> Result<Nil, String> {
  let (keep, never) = channel::new::<Int>(0)
  // Pending work, so that the wait below is not a deadlock (D-984, D-1073).
  fiber::spawn_detached(fn() {
    time::sleep(duration::hours(1))
    channel::send(keep, 0)
  })
  let waited = process::until_interrupted(fn() {
    io::println(\"waiting\")
    channel::receive(never)
  })
  case waited {
    Ok(_) => Err(\"the wait ended\")
    Err(process::Interrupted) => {
      io::println(\"interrupted\")
      Ok(Nil)
    }
    Err(process::Panicked(failure)) => Err(fiber::message(failure))
  }
}
";

/// Two scopes: a fiber's, armed before the body's starts, which prints
/// `first signal` to standard error once a signal ended it, and the
/// body's, which prints `waiting` and then a megabyte that nobody reads,
/// so it stays blocked in the write after a signal cancelled it.
const STUCK: &str = "//! Stays in a write that nobody reads after a signal cancels it, so that a second signal ends it.

import std::channel
import std::duration
import std::fiber
import std::io
import std::process
import std::string
import std::time

fn main() {
  let (keep, never) = channel::new::<Int>(0)
  // Pending work, so that the waits below are not a deadlock (D-984, D-1073).
  fiber::spawn_detached(fn() {
    time::sleep(duration::hours(1))
    channel::send(keep, 0)
  })
  let (ready, armed) = channel::new::<Nil>(1)
  let filler = string::repeat(\"x\", 1_000_000)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() {
      let waited = process::until_interrupted(fn() {
        let _ = channel::send(ready, Nil)
        channel::receive(never)
      })
      io::eprintln(\"first signal\")
      waited
    })
    // The fiber's scope is armed before the write blocks this worker.
    let _ = channel::receive(armed)
    process::until_interrupted(fn() { io::print(\"waiting\\n{filler}\") })
  }
  let _ = outcome
  io::println(\"not reached\")
}
";

/// Opens one short scope after another, each of whose bodies returns at
/// once, after printing `ready`; a scope that a signal interrupted prints
/// `interrupted` to standard error, and the loop goes on.
const SPIN: &str =
    "//! Opens short `until_interrupted` scopes in a loop and says when a signal interrupted one.

import std::io
import std::process

fn spin(n: Int) -> Nil {
  case n {
    0 => io::println(\"done\")
    _ => case process::until_interrupted(fn() { n }) {
      Ok(_) => spin(n - 1)
      Err(_) => {
        io::eprintln(\"interrupted\")
        spin(n - 1)
      }
    }
  }
}

fn main() {
  io::println(\"ready\")
  spin(1_000_000_000)
}
";

/// How long a step may take before the test fails: a bound, not a wait.
fn limit() -> Duration {
    scaled_timeout(Duration::from_secs(180)).unwrap()
}

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

/// `cheby build -o <exe>` of the package at `root`, with `cache`.
fn build(root: &Path, exe: &Path, cache: &HarnessCache) {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "build", "-o", exe.to_str().unwrap()])
        .current_dir(root)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    let out = process::run(cmd, None, limit()).unwrap();
    assert_eq!(out.status, Some(0), "cheby build: {}", out.stderr);
}

/// Whether this process ignores SIGINT: a shell it starts then survives
/// sending SIGINT to itself.
fn sigint_ignored() -> bool {
    let out = Command::new("sh")
        .args(["-c", "kill -INT $$; echo survived"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains("survived")
}

/// A command for `program` with `args`, started with SIGINT's and
/// SIGTERM's default actions even when this process ignores SIGINT
/// (module documentation).
fn command(program: &Path, args: &[&str]) -> Command {
    if sigint_ignored() {
        let mut cmd = Command::new("perl");
        cmd.args([
            "-e",
            "$SIG{INT} = 'DEFAULT'; $SIG{TERM} = 'DEFAULT'; exec { $ARGV[0] } @ARGV or die \"exec: $!\"",
        ])
        .arg(program)
        .args(args);
        cmd
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
}

/// A line of the program's output, by stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Out(String),
    Err(String),
}

/// What [`Running::end_or`] saw first.
#[derive(Debug)]
enum Ending {
    /// The program ended, with this status.
    Ended(ExitStatus),
    /// The line arrived while the program ran.
    Saw,
    /// Neither, within the bound.
    Neither,
}

/// A running program whose output lines arrive on `lines`.
struct Running {
    child: Child,
    lines: mpsc::Receiver<Line>,
    /// Every line seen so far, in arrival order.
    seen: Vec<Line>,
    /// Dropping it lets a reader that stopped reading release its pipe.
    release: mpsc::Sender<()>,
}

/// Forwards each line of `pipe` as `wrap(line)`; after a line equal to
/// `stop_after`, stops reading but keeps the pipe open until `release`
/// is dropped, so the writer blocks once the pipe is full.
fn forward(
    pipe: impl Read + Send + 'static,
    wrap: fn(String) -> Line,
    tx: mpsc::Sender<Line>,
    stop_after: Option<&'static str>,
    release: mpsc::Receiver<()>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let text = line.trim_end_matches('\n').to_owned();
            line.clear();
            let stop = stop_after == Some(text.as_str());
            if tx.send(wrap(text)).is_err() {
                break;
            }
            if stop {
                let _ = release.recv();
                break;
            }
        }
    });
}

impl Running {
    /// Starts `cmd` in `dir` with `cache` and the extra environment `env`;
    /// its standard output stops being read after `stop_after`.
    fn start(
        mut cmd: Command,
        dir: &Path,
        cache: &HarnessCache,
        env: &[(&str, &str)],
        stop_after: Option<&'static str>,
    ) -> Self {
        cmd.current_dir(dir)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cache.apply(&mut cmd);
        process::clear_inherited(&mut cmd);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        let (tx, lines) = mpsc::channel();
        let (release, out_release) = mpsc::channel();
        let (_unused, err_release) = mpsc::channel::<()>();
        forward(
            child.stdout.take().unwrap(),
            Line::Out,
            tx.clone(),
            stop_after,
            out_release,
        );
        forward(
            child.stderr.take().unwrap(),
            Line::Err,
            tx,
            None,
            err_release,
        );
        Self {
            child,
            lines,
            seen: Vec::new(),
            release,
        }
    }

    /// Waits until `line` has arrived, or fails after [`limit`].
    fn wait_for(&mut self, line: &Line) {
        let deadline = Instant::now() + limit();
        while !self.seen.contains(line) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(next) => self.seen.push(next),
                Err(error) => {
                    let _ = self.child.kill();
                    panic!("no {line:?} ({error}); seen {:?}", self.seen);
                }
            }
        }
    }

    /// Sends `signal` (`INT`, `TERM`) with the shell's `kill`.
    fn signal(&self, signal: &str) {
        let status = Command::new("sh")
            .args(["-c", &format!("kill -{signal} {}", self.child.id())])
            .status()
            .unwrap();
        assert!(status.success(), "kill -{signal} failed");
    }

    /// Waits until the program ends or `line` arrives, at most `bound`;
    /// when neither happens it kills the program.
    fn end_or(&mut self, line: &Line, bound: Duration) -> Ending {
        let deadline = Instant::now() + bound;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Ending::Ended(status);
            }
            if self.seen.contains(line) {
                return Ending::Saw;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Ending::Neither;
            }
            if let Ok(next) = self.lines.recv_timeout(Duration::from_millis(10)) {
                self.seen.push(next);
            }
        }
    }

    /// Waits for the program to end, or kills it and fails after
    /// [`limit`]; returns its status and every line it wrote.
    fn finish(mut self) -> (ExitStatus, Vec<Line>) {
        let deadline = Instant::now() + limit();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the program did not end; seen {:?}", self.seen);
            }
            thread::sleep(Duration::from_millis(10));
        };
        drop(self.release);
        // The readers end at the pipes' end, the process having ended.
        while let Ok(line) = self.lines.recv_timeout(limit()) {
            self.seen.push(line);
        }
        (status, self.seen)
    }
}

/// The signal that ended a process, if one did.
#[cfg(unix)]
fn killed_by(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// No signal ends a process outside Unix.
#[cfg(not(unix))]
fn killed_by(_status: ExitStatus) -> Option<i32> {
    None
}

/// The lines of one stream, in order.
fn stream(lines: &[Line], out: bool) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Out(text) if out => Some(text.as_str()),
            Line::Err(text) if !out => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Runs `cmd` in `dir`, sends `signal` once it waits, and checks that it
/// ended through `main`'s error path: `waiting` and `interrupted`
/// flushed, nothing on standard error, and status 0, `main`'s `Ok(Nil)`.
fn interrupt_once(cmd: Command, dir: &Path, cache: &HarnessCache, signal: &str, what: &str) {
    let mut running = Running::start(cmd, dir, cache, &[], None);
    running.wait_for(&Line::Out("waiting".to_owned()));
    running.signal(signal);
    let (status, lines) = running.finish();
    assert_eq!(
        stream(&lines, true),
        ["waiting", "interrupted"],
        "{what} {signal}: {lines:?}"
    );
    assert_eq!(stream(&lines, false), Vec::<&str>::new(), "{what} {signal}");
    assert_eq!(status.code(), Some(0), "{what} {signal}: {status}");
}

/// D-1073: SIGINT and SIGTERM each cancel the scope of an executable's
/// `main`, which prints `interrupted` and returns `Ok(Nil)`: its output is
/// flushed and its status is 0.
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn until_interrupted_sigint_and_sigterm_end_an_executable_through_main() {
    let tmp = FreshDir::create("interrupt-exe").unwrap();
    let cache = HarnessCache::create_with("cli-interrupt", None).unwrap();
    let root = package(tmp.path(), "interrupted", INTERRUPTED);
    let exe = tmp.path().join("exe").join("interrupted");
    build(&root, &exe, &cache);
    for signal in ["INT", "TERM"] {
        interrupt_once(command(&exe, &[]), &root, &cache, signal, "executable");
    }
}

/// D-1073: the same under `cheby run`, whose runtime runs in the `cheby`
/// process: SIGINT and SIGTERM reach `main`'s scope.
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn until_interrupted_sigint_and_sigterm_end_cheby_run_through_main() {
    let tmp = FreshDir::create("interrupt-run").unwrap();
    let cache = HarnessCache::create_with("cli-interrupt", None).unwrap();
    let root = package(tmp.path(), "interrupted", INTERRUPTED);
    for signal in ["INT", "TERM"] {
        let cmd = command(Path::new(CHEBY), &["--color=never", "run"]);
        interrupt_once(cmd, &root, &cache, signal, "cheby run");
    }
}

/// D-1073: after the first signal the default action is back, so a second
/// SIGINT ends a program whose body is stuck (in a write nobody reads) by
/// the signal, while the first one cancelled both scopes (the other
/// scope's fiber reports it).
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn until_interrupted_a_second_sigint_ends_a_stuck_executable() {
    let tmp = FreshDir::create("interrupt-stuck").unwrap();
    let cache = HarnessCache::create_with("cli-interrupt", None).unwrap();
    let root = package(tmp.path(), "stuck", STUCK);
    let exe = tmp.path().join("exe").join("stuck");
    build(&root, &exe, &cache);
    // Two workers at least: one stays blocked in the write.
    let env = [("CHEBY_THREADS", "4")];
    let mut running = Running::start(command(&exe, &[]), &root, &cache, &env, Some("waiting"));
    running.wait_for(&Line::Out("waiting".to_owned()));
    running.signal("INT");
    running.wait_for(&Line::Err("first signal".to_owned()));
    running.signal("INT");
    let (status, lines) = running.finish();
    assert_eq!(killed_by(status), Some(2), "{status}: {lines:?}");
    assert_eq!(stream(&lines, false), ["first signal"]);
    assert!(!stream(&lines, true).contains(&"not reached"));
}

/// D-1073 (the review's RB10-1): a program that opens one short scope after
/// another never loses a signal. One SIGINT or SIGTERM, sent at a varying
/// point of the loop, either interrupts the scope that was open when it
/// came, or, when that scope closed before the signal was acted on, takes
/// the default action; it is never charged to a later scope. After it the
/// default action stays, so a second SIGINT always ends the program.
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn until_interrupted_a_signal_between_short_scopes_is_never_lost() {
    let tmp = FreshDir::create("interrupt-spin").unwrap();
    let cache = HarnessCache::create_with("cli-interrupt", None).unwrap();
    let root = package(tmp.path(), "spin", SPIN);
    let exe = tmp.path().join("exe").join("spin");
    build(&root, &exe, &cache);
    let bound = scaled_timeout(Duration::from_secs(30)).unwrap();
    let mut endings = [0_usize; 2];
    for trial in 0..40_u64 {
        let (signal, number) = if trial % 2 == 0 {
            ("INT", 2)
        } else {
            ("TERM", 15)
        };
        let mut running = Running::start(command(&exe, &[]), &root, &cache, &[], None);
        running.wait_for(&Line::Out("ready".to_owned()));
        // Not a wait for anything: it moves the signal to another point of
        // the loop in each trial.
        thread::sleep(Duration::from_millis(trial * 7 % 50));
        running.signal(signal);
        match running.end_or(&Line::Err("interrupted".to_owned()), bound) {
            Ending::Ended(status) => {
                assert_eq!(killed_by(status), Some(number), "trial {trial}: {status}");
                endings[0] += 1;
            }
            Ending::Saw => {
                running.signal("INT");
                let (status, lines) = running.finish();
                assert_eq!(
                    killed_by(status),
                    Some(2),
                    "trial {trial}: a second SIGINT did not end it: {status}: {lines:?}"
                );
                endings[1] += 1;
            }
            Ending::Neither => panic!("trial {trial}: the {signal} was lost"),
        }
    }
    eprintln!(
        "ended by the signal: {}; a scope interrupted, then a second SIGINT: {}",
        endings[0], endings[1]
    );
}
