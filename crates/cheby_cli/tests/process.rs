//! Children of `std::process` at the program's end and on cancellation, with real
//! processes (D-1912, D-1917, D-1911, D-1921, D-1927): `process::exit`, a panic that ends `main` and
//! `main` returning each kill a child that a detached fiber holds; a cancelled `output` kills
//! and reaps its child while the program runs on; SIGINT through `until_interrupted` unwinds
//! `with_child`, which kills the child; a child that inherits a standard input read from a
//! file gets the bytes the program read ahead and did not take; `process::exit` right after many
//! starts leaves no child (D-1917); and a program that inherits SIGCHLD as ignored still gets its
//! children's statuses (D-1927).
//!
//! Each child is a shell that writes its process id to `child.pid` in the package's directory
//! and then becomes `sleep 600`, so the id stays the child's. A test reads the id and waits,
//! within a bound, until no process has it: a killed child that nobody reaped would still have it
//! as a zombie. The children never hold this process's pipes, so a child left running fails the
//! check instead of hanging the test.
//!
//! Every `cheby` gets a fresh `CHEBY_CACHE` of its own (D-1128).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "test helpers: a failed setup step or a step past its bound fails the test; \
              a deadline is now plus a bound"
)]

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput, scaled_timeout};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");

/// A detached fiber starts a child and holds it, and `main` ends with `END` once the
/// child has written its id.
const DETACHED: &str = r#"//! Holds a child in a detached fiber, then ends main while the child runs.

import std::channel
import std::duration
import std::fiber
import std::io
import std::process
import std::result
import std::time

fn main() {
  let (ready_tx, ready) = channel::new::<Nil>(1)
  fiber::spawn_detached(fn() {
    let script = "printf '%s\n' $$ > child.pid; printf 'ready\n'; exec sleep 600"
    let spec = process::command("sh", ["-c", script])
      |> process::with_stderr(_, process::discarded())
    process::with_child(spec, fn(child) {
      use _ <- result::try(process::read_line(process::stdout(child)))
      let _ = channel::send(ready_tx, Nil)
      time::sleep(duration::hours(1))
      Ok(Nil)
    })
  })
  let _ = channel::receive(ready)
  io::println("the child runs")
  END
}
"#;

/// `output` inside `fiber::timeout` of a child that never ends; the program then says so
/// and sleeps, so the child must be reaped while it runs.
const CANCELLED_OUTPUT: &str = r#"//! Cancels an output whose child never ends, then sleeps.

import std::duration
import std::fiber
import std::io
import std::process
import std::time

fn main() {
  let script = "printf '%s\n' $$ > child.pid; exec sleep 600"
  let ran = fiber::timeout(duration::seconds(1), fn() {
    process::output(process::command("sh", ["-c", script]))
  })
  case ran {
    Err(fiber::TimedOut) => io::println("timed out")
    Ok(_) | Err(fiber::Panicked(_)) => io::println("not timed out")
  }
  time::sleep(duration::hours(1))
}
"#;

/// A child inside `with_child` inside `until_interrupted`; the program prints `waiting` once
/// the child has written its id, `interrupted` once SIGINT unwound the body, and sleeps.
const INTERRUPTED: &str = r#"//! Unwinds a with_child on SIGINT, then sleeps.

import std::duration
import std::io
import std::process
import std::result
import std::time

fn main() {
  let script = "printf '%s\n' $$ > child.pid; printf 'ready\n'; exec sleep 600"
  let spec = process::command("sh", ["-c", script])
    |> process::with_stderr(_, process::discarded())
  let outcome = process::until_interrupted(fn() {
    process::with_child(spec, fn(child) {
      use _ <- result::try(process::read_line(process::stdout(child)))
      io::println("waiting")
      time::sleep(duration::hours(1))
      Ok(Nil)
    })
  })
  case outcome {
    Err(process::Interrupted) => io::println("interrupted")
    Ok(_) | Err(process::Panicked(_)) => io::println("not interrupted")
  }
  time::sleep(duration::hours(1))
}
"#;

/// Reads a line, then runs `cat` on the inherited standard input.
const READ_AHEAD: &str = r#"//! Reads a line, then lets cat read the rest of standard input.

import std::io
import std::process
import std::result

fn main() -> Result<Nil, io::Error> {
  use first <- result::try(io::read_line())
  io::println("the program read {first:?}")
  use status <- result::try(process::run(process::command("cat", [])))
  io::println("{status:?}")
  Ok(Nil)
}
"#;

/// How long a step may take before the test fails: a bound, not a wait.
/// 64 detached fibers each start a child, which appends its process id to `pids` and becomes a
/// long `sleep`, and `main` calls `process::exit` at once, while many of those starts are still
/// queued or running on the blocking pool (D-1917).
const MANY_STARTS: &str = r#"//! Starts many children from detached fibers, then exits at once.

import std::fiber
import std::process

fn start_all(n: Int) -> Nil {
  case n {
    0 => Nil
    _ => {
      fiber::spawn_detached(fn() {
        let script = "printf '%s\n' $ >> pids; exec sleep 600"
        let spec = process::command("sh", ["-c", script])
          |> process::with_stdout(_, process::discarded())
          |> process::with_stderr(_, process::discarded())
        process::with_child(spec, fn(child) { process::wait(child) })
      })
      start_all(n - 1)
    }
  }
}

fn main() {
  start_all(64)
  process::exit(0)
}
"#;

/// Runs a child and prints its status (D-1927).
const STATUS: &str = r#"//! Runs a child and prints its status.

import std::io
import std::process
import std::result

fn main() -> Result<Nil, io::Error> {
  use status <- result::try(process::run(process::command("sh", ["-c", "exit 3"])))
  io::println("{status:?}")
  Ok(Nil)
}
"#;

fn limit() -> Duration {
    scaled_timeout(Duration::from_secs(120)).unwrap()
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

/// `cheby run` in `root` with `cache`.
fn cheby_run(root: &Path, cache: &HarnessCache) -> Command {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "run"])
        .current_dir(root)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::clear_inherited(&mut cmd);
    cmd
}

/// The process id that the child wrote to `child.pid` in `root`.
fn child_pid(root: &Path) -> String {
    let path = root.join("child.pid");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let pid = text.trim().to_owned();
    assert!(
        !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()),
        "child.pid: {text:?}"
    );
    pid
}

/// Whether a process, a zombie included, has the id `pid`.
fn exists(pid: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .unwrap()
        .success()
}

/// Waits until no process has the id `pid`, or fails after [`limit`]; `what` names the
/// case. A child still running after the bound is killed, so that no test leaves one behind.
fn assert_gone(pid: &str, what: &str) {
    let deadline = Instant::now() + limit();
    while exists(pid) {
        if Instant::now() > deadline {
            let _ = Command::new("sh")
                .args(["-c", &format!("kill -9 {pid} 2>/dev/null")])
                .status();
            panic!("{what}: the child {pid} still exists");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A running `cheby run` whose standard output lines arrive on `lines`.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Running {
    /// Starts `cmd` with standard input null, standard error discarded and standard output
    /// read line by line.
    fn start(mut cmd: Command) -> Self {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// Waits until the program prints `line`, or kills it and fails after [`limit`].
    fn wait_for(&mut self, line: &str) {
        let deadline = Instant::now() + limit();
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(next) if next == line => return,
                Ok(next) => seen.push(next),
                Err(error) => {
                    self.stop();
                    panic!("no {line:?} ({error}); seen {seen:?}");
                }
            }
        }
    }

    /// Kills the program and waits for it.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Sends SIGINT with the shell's `kill`.
    fn interrupt(&self) {
        let status = Command::new("sh")
            .args(["-c", &format!("kill -INT {}", self.child.id())])
            .status()
            .unwrap();
        assert!(status.success(), "kill -INT failed");
    }
}

/// Whether this process ignores SIGINT: a shell it starts then survives sending SIGINT to itself.
fn sigint_ignored() -> bool {
    let out = Command::new("sh")
        .args(["-c", "kill -INT $$; echo survived"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains("survived")
}

/// D-1917: `main` ending in each way the runtime controls, here `process::exit`, a panic and a
/// return, kills a child that a detached fiber holds, and the program ends at once.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
fn process_children_killed_when_main_ends() {
    let tmp = FreshDir::create("process-end").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    for (name, end, status) in [
        ("exits", "process::exit(0)", Some(0)),
        ("panics", "panic as \"main fails\"", Some(101)),
        ("returns", "Nil", Some(0)),
    ] {
        let root = package(tmp.path(), name, &DETACHED.replace("END", end));
        let out: RunOutput = process::run(cheby_run(&root, &cache), None, limit()).unwrap();
        assert_eq!(out.status, status, "{name}: {}", out.stderr);
        assert_eq!(out.stdout, "the child runs\n", "{name}");
        assert_gone(&child_pid(&root), name);
    }
}

/// D-1911: a cancelled `output` kills its child and the runtime reaps it, while the program
/// runs on, so not even a zombie is left.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
fn process_cancelled_output_kills_and_reaps_the_child() {
    let tmp = FreshDir::create("process-cancel").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    let root = package(tmp.path(), "cancelled", CANCELLED_OUTPUT);
    let mut running = Running::start(cheby_run(&root, &cache));
    running.wait_for("timed out");
    assert_gone(&child_pid(&root), "a cancelled output");
    running.stop();
}

/// D-1912, D-1073: SIGINT cancels the scope of `until_interrupted`, so the body of `with_child`
/// unwinds and the child is killed and reaped while the program runs on.
#[test]
#[cfg_attr(windows, ignore = "children and signals on Windows are M6 (D-1919)")]
fn process_sigint_unwinds_with_child_and_kills_the_child() {
    let tmp = FreshDir::create("process-interrupt").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    let root = package(tmp.path(), "interrupted", INTERRUPTED);
    let cmd = if sigint_ignored() {
        // A signal that this process ignores would stay ignored in `cheby` (D-1073), so
        // `perl` puts the default action back first, as in `interrupt.rs`.
        let mut cmd = Command::new("perl");
        cmd.args([
            "-e",
            "$SIG{INT} = 'DEFAULT'; exec { $ARGV[0] } @ARGV or die \"exec: $!\"",
            CHEBY,
            "--color=never",
            "run",
        ])
        .current_dir(&root)
        .env("NO_COLOR", "1");
        cache.apply(&mut cmd);
        process::clear_inherited(&mut cmd);
        cmd
    } else {
        cheby_run(&root, &cache)
    };
    let mut running = Running::start(cmd);
    running.wait_for("waiting");
    let pid = child_pid(&root);
    assert!(exists(&pid), "the child {pid} runs before the signal");
    running.interrupt();
    running.wait_for("interrupted");
    assert_gone(&pid, "SIGINT through until_interrupted");
    running.stop();
}

/// D-1921: a child that inherits a standard input that is a file gets the bytes that the program's
/// reader read ahead and did not take, since the runtime moves the descriptor back over them.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
fn process_inherited_stdin_file_gives_back_the_read_ahead() {
    let tmp = FreshDir::create("process-stdin").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    let root = package(tmp.path(), "read_ahead", READ_AHEAD);
    let input = tmp.path().join("input.txt");
    std::fs::write(&input, "first\nsecond\nthird\n").unwrap();
    let mut cmd = cheby_run(&root, &cache);
    cmd.stdin(std::fs::File::open(&input).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "the program read Some(\"first\")\nsecond\nthird\nExited(0)\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

/// D-1917: `process::exit` right after many starts leaves no child running, also of the starts
/// that were still queued or running on the blocking pool: the end refuses later starts, waits
/// for those in the call that creates a child, and kills each child.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
fn process_exit_during_many_starts_leaves_no_child() {
    let tmp = FreshDir::create("process-many-starts").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    let root = package(tmp.path(), "many_starts", MANY_STARTS);
    for round in 0..4 {
        let _ = std::fs::remove_file(root.join("pids"));
        let out: RunOutput = process::run(cheby_run(&root, &cache), None, limit()).unwrap();
        assert_eq!(out.status, Some(0), "round {round}: {}", out.stderr);
        // A child that escaped the kill writes its id within this time.
        thread::sleep(Duration::from_millis(500));
        let pids = std::fs::read_to_string(root.join("pids")).unwrap_or_default();
        for pid in pids.lines() {
            assert_gone(pid, "a child started while process::exit ran");
        }
    }
}

/// D-1927: under a shell that ignores SIGCHLD, the runtime puts the default action back before
/// its first child, so `run` reports the child's status instead of failing.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
fn process_run_reports_the_status_when_sigchld_was_ignored() {
    let tmp = FreshDir::create("process-sigchld").unwrap();
    let cache = HarnessCache::create_with("cli-process", None).unwrap();
    let root = package(tmp.path(), "sigchld", STATUS);
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "trap '' CHLD; exec \"$0\" --color=never run", CHEBY])
        .current_dir(&root)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    let out: RunOutput = process::run(cmd, None, limit()).unwrap();
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "Exited(3)\n");
}
