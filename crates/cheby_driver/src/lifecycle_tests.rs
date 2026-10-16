//! Dropping a `Jit` while a detached fiber still runs its code is safe
//! (L1's done-when, D-708, I-6): the runtime that ran `main` holds the
//! JIT's code token, so the code stays mapped until the runtime has torn
//! its fibers down and stopped (`CodeKeepAlive`, `Entry::run_root`).
//!
//! Each test compiles a package whose `main` spawns a detached fiber, runs
//! `main` as a root of a runtime the test owns, waits until the fiber
//! demonstrably runs JIT code (or waits in `channel::receive`), drops the
//! `Jit`, checks that the fiber goes on, and only then shuts the runtime
//! down, which cancels and unwinds the fiber through the dropped JIT's
//! code. The `run_main` tests go through `cheby run`'s path instead
//! (`run`, `Program::run`): a detached fiber waiting on the blocking pool
//! for input or a file is skipped (D-786, D-683), the stop gives up on the
//! pool, and the read returns on a pool thread after the `Jit` is dropped,
//! whose completion wakes nothing, so no thread returns into the dropped
//! JIT's code (D-815).

use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_runtime::alloc::Domain;
use cheby_runtime::entry::{EntryConfig, EntryMode, EntryOutcome};
use cheby_runtime::stats::{self, Event};
use cheby_runtime::{DetachedGroup, Runtime, RuntimeConfig};

use super::*;

/// How long a test waits for something it expects before it fails.
const PATIENCE: Duration = Duration::from_secs(10);

/// A package in a fresh temporary directory with `main.cheby` as its root
/// module.
fn package(name: &str, main: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cheby_driver_lifecycle_{}_{name}",
        std::process::id()
    ));
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

/// Compiles the package at `dir` and runs its `main` as a root fiber of a
/// new runtime with two workers, in `group` if given (`run_root_in`) and
/// otherwise in the runtime's default group (`run_root`). Returns the
/// runtime, the `Jit` (still alive) and the JIT's accounting domain.
fn run_main_as_root(
    dir: &std::path::Path,
    group: Option<&Arc<DetachedGroup>>,
) -> (Runtime, Jit, Arc<Domain>) {
    let Prepared::Ready(program) = prepare_run(dir, RunOptions::default()).unwrap() else {
        panic!("the package did not compile");
    };
    let Program { jit, entry, .. } = *program;
    let domain = jit.domain();
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(2),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let entry = jit.entry(entry).unwrap();
    let config = EntryConfig::new(EntryMode::Run);
    let outcome = match group {
        Some(group) => entry.run_root_in(&runtime, &config, group),
        None => entry.run_root(&runtime, &config),
    }
    .unwrap();
    assert!(
        matches!(outcome, EntryOutcome::Returned(_)),
        "main did not return: {outcome:?}"
    );
    (runtime, jit, domain)
}

/// Waits until `domain` has counted more allocations than `than`, and
/// returns the count then.
fn allocations_past(domain: &Domain, than: u64) -> u64 {
    let start = Instant::now();
    loop {
        let now = domain.stats().count(Event::Alloc);
        if now > than {
            return now;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "no allocation after {than} within {PATIENCE:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// (b) A detached fiber spinning in tail calls of JIT code, allocating as
/// it goes, keeps running after the `Jit` is dropped: the domain's count
/// of allocations, which only that fiber still makes, goes on rising for
/// 50 ms twice. `shutdown` then tears it down, and its unwinding through
/// the dropped JIT's code frees everything it held.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lifecycle_a_detached_spinner_outlives_its_jit() {
    stats::count_events();
    let dir = package(
        "spinner",
        "//! Spins in a detached fiber, allocating as it goes.\n\n\
         import std::fiber\nimport std::list\n\n\
         fn spin(n: Int) -> Int {\n  let made = list::range(0, n % 8)\n  spin((list::length(made) + n + 1) % 1000)\n}\n\n\
         fn main() {\n  fiber::spawn_detached(fn() { spin(0) })\n}\n",
    );
    let (runtime, jit, domain) = run_main_as_root(&dir, None);
    // `main` has returned: from now on only the spinner allocates in the
    // domain. Two counts that differ show it runs JIT code.
    let first = allocations_past(&domain, domain.stats().count(Event::Alloc));
    let running = allocations_past(&domain, first);
    drop(jit);
    let mut last = running;
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(50));
        let now = domain.stats().count(Event::Alloc);
        assert!(
            now > last,
            "the spinner made no allocation in 50 ms after the drop ({last})"
        );
        last = now;
    }
    runtime.shutdown();
    if cfg!(debug_assertions) {
        assert_eq!(domain.live_objects(), 0, "the torn-down spinner leaked");
    }
}

/// Waits until `group` has `count` members parked.
fn parked(group: &DetachedGroup, count: usize) {
    let start = Instant::now();
    while group.parked() != count {
        assert!(
            start.elapsed() < PATIENCE,
            "{} members parked, not {count}, after {PATIENCE:?}",
            group.parked()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// (c) A detached fiber parked in `channel::receive` stays parked after
/// the `Jit` is dropped, and `shutdown` then cancels it: its `receive`
/// unwinds through the dropped JIT's code and releases the channel, so
/// nothing is left in the domain.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lifecycle_a_detached_receiver_outlives_its_jit() {
    let dir = package(
        "receiver",
        "//! A detached fiber waits in `channel::receive` for good.\n\n\
         import std::channel\nimport std::fiber\n\n\
         fn wait_forever() -> Result<Int, Nil> {\n  let (keep, never) = channel::new::<Int>(1)\n  \
         let got = channel::receive(never)\n  let _ = channel::send(keep, 0)\n  got\n}\n\n\
         fn main() {\n  fiber::spawn_detached(fn() { wait_forever() })\n}\n",
    );
    let group = DetachedGroup::new();
    let (runtime, jit, domain) = run_main_as_root(&dir, Some(&group));
    parked(&group, 1);
    assert_eq!(group.live(), 1);
    drop(jit);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(group.parked(), 1, "the receiver left its wait");
    runtime.shutdown();
    assert_eq!(group.live(), 0, "shutdown left the receiver in its group");
    if cfg!(debug_assertions) {
        assert_eq!(domain.live_objects(), 0, "the torn-down receiver leaked");
    }
}

/// The environment variable that makes a test run its child's part.
const CHILD: &str = "CHEBY_DRIVER_LIFECYCLE_CHILD";

/// Runs the test `name` of this binary as a child process with `CHILD`
/// set, standard input a pipe the parent holds, two workers and standard
/// error read line by line. Returns the child, its standard input and a
/// receiver of its standard error's lines.
fn spawn_child(
    name: &str,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    std::sync::mpsc::Receiver<String>,
) {
    use std::io::BufRead as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("CHEBY_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let (lines, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    (child, stdin, receiver)
}

/// Waits up to `PATIENCE` for `child` to exit, killing it after that, and
/// returns its status (`None` if it had to be killed).
fn wait_child(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if start.elapsed() > PATIENCE {
            child.kill().ok();
            child.wait().ok();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// D-786, D-708, I-6 (review finding MEM-1 of gate 2): `cheby run`'s path
/// (`run`, so `Program::run` and `Entry::run_main`) returns while a
/// detached fiber of `main` waits in `io::read_line`: the teardown skips
/// it (D-786), since it waits on the blocking pool with D-786's mark set,
/// and the stop gives up on the pool after its two polls
/// (D-814, D-815). Then the
/// `Jit` is dropped with the `Program`. When a line arrives, the read
/// returns on a pool thread (D-683), whose completion wakes nothing, so
/// the fiber never resumes into the dropped JIT's code, and the process
/// lives on. Before gate 2's fix the read blocked a worker inline, whose
/// code token was never attached, and the child died of `SIGSEGV` when the
/// worker returned. Run as a child process, whose standard input the test
/// controls.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lifecycle_run_main_keeps_the_code_a_blocked_worker_returns_into() {
    use std::io::Write as _;
    const NAME: &str =
        "lifecycle_tests::lifecycle_run_main_keeps_the_code_a_blocked_worker_returns_into";
    if std::env::var_os(CHILD).is_some() {
        let dir = package(
            "blocked_reader",
            "//! A detached fiber blocks in `io::read_line` while `main` returns.\n\n\
             import std::duration\nimport std::fiber\nimport std::io\nimport std::selector\n\n\
             fn main() {\n  fiber::spawn_detached(fn() { io::read_line() })\n  \
             selector::new()\n  |> selector::after(_, duration::milliseconds(100), fn() { Nil })\n  \
             |> selector::select\n  io::println(\"main returns\")\n}\n",
        );
        let result = run(&dir, RunOptions::default()).unwrap();
        let RunResult::Ran { exit, .. } = result else {
            panic!("the package did not compile");
        };
        // `run` has returned, and the `Program` with its `Jit` is gone.
        eprintln!("RUN RETURNED {exit}");
        std::thread::sleep(Duration::from_millis(1500));
        eprintln!("SURVIVED");
        return;
    }
    let (mut child, mut stdin, lines) = spawn_child(NAME);
    let mut seen = Vec::new();
    loop {
        let Ok(line) = lines.recv_timeout(PATIENCE) else {
            child.kill().ok();
            panic!("the child never said that `run` returned: {seen:?}");
        };
        let returned = line.starts_with("RUN RETURNED");
        seen.push(line);
        if returned {
            break;
        }
    }
    assert_eq!(seen.last().map(String::as_str), Some("RUN RETURNED 0"));
    std::thread::sleep(Duration::from_millis(200));
    // The read returns now, on a pool thread, after the `Jit` was dropped.
    stdin.write_all(b"a line\n").unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let status = wait_child(&mut child);
    seen.extend(lines.try_iter());
    assert!(
        status.is_some_and(|status| status.success()) && seen.iter().any(|line| line == "SURVIVED"),
        "the child did not survive its blocked worker's return: {status:?}\n{seen:#?}"
    );
}

/// D-786, D-708, I-6 (review finding CONC-W2-2 of gate 2), in process: a
/// detached fiber of `main` waits while a pool thread opens a FIFO
/// (D-683), the teardown skips it, `Program::run` returns and drops the
/// `Jit`. Then the test opens the FIFO for writing: the read returns on
/// the pool thread, whose completion wakes nothing once the stop gave up
/// on the pool (D-815), so nothing returns into the
/// dropped JIT's code, and the test process lives on. Before gate 2's fix
/// a worker blocked inline returned into unmapped code and the process
/// died of `SIGSEGV` here.
#[test]
#[cfg(unix)]
fn lifecycle_program_run_keeps_the_code_of_a_worker_blocked_in_file_io() {
    use std::io::Write as _;
    let dir = package(
        "blocked_fifo",
        "//! Placeholder, replaced below once the FIFO's path is known.\n\nfn main() {\n  Nil\n}\n",
    );
    let fifo = dir.join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo failed");
    std::fs::write(
        dir.join("src/main.cheby"),
        format!(
            "//! A detached fiber blocks inline opening a FIFO while `main` returns.\n\n\
             import std::duration\nimport std::fiber\nimport std::io\nimport std::selector\n\n\
             fn main() {{\n  fiber::spawn_detached(fn() {{\n    \
             let _ = io::read_file(\"{}\")\n    Nil\n  }})\n  \
             selector::new()\n  |> selector::after(_, duration::milliseconds(100), fn() {{ Nil }})\n  \
             |> selector::select\n}}\n",
            fifo.display()
        ),
    )
    .unwrap();
    let Prepared::Ready(program) = prepare_run(&dir, RunOptions::default()).unwrap() else {
        panic!("the package did not compile");
    };
    assert_eq!(program.run().unwrap(), 0);
    // The `Jit` is gone. The open returns now, on the pool thread.
    let mut writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    writer.write_all(b"hello").unwrap();
    drop(writer);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_dir_all(&dir).unwrap();
}
