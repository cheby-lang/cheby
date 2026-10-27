//! `cargo xtask sanitize <address|thread|miri>`: the sanitizer and Miri
//! jobs of the nightly workflow (D-721), one driver for the workflow and for
//! the local runs that stand as its evidence before the push (D-862;
//! D-882).
//!
//! Every step is a `cargo` command on the pinned nightly [`NIGHTLY`] (D-781).
//! The driver edits no file, and it builds into its own target directory:
//! `--target-dir`, by default `target/sanitize-<s>` (the workflow's); local
//! runs pass a scratch directory.
//!
//! - **address** and **thread**: `cargo +<nightly> test --locked
//!   -Zbuild-std --target <host>` with `RUSTFLAGS` and `RUSTDOCFLAGS`
//!   `-Zsanitizer=<s> --cfg cheby_sanitize="<s>"`. `-Zbuild-std` is
//!   required by the thread sanitizer and keeps std instrumented like the rest;
//!   `--target` keeps the flags away from build scripts and proc macros.
//!   Every step sets `CHEBY_RT_TEST_EMULATED=1` (the runtime's tests keep
//!   their checks but assert no upper time bound) and `CHEBY_HARNESS_SCALE=10`
//!   (the harnesses' timeouts, D-748), and runs its tests one at a time
//!   (D-725). The `loom` models are skipped, since loom switches stacks on
//!   its own (D-863). The steps are [`STEPS`].
//! - **miri**: `cargo +<nightly> miri test --locked -p cheby_runtime --lib`
//!   over the subset of [`MIRI_GROUPS`], one step per group
//!   (D-884), with [`MIRIFLAGS`] and `PROPTEST_CASES=4`.
//!   `--jobs <n>` runs n groups at once; Miri interprets each on one core.
//!
//! The harness steps (`ui-deadlock`, `conformance`, `repl`) and the
//! poison-off pass leave `CHEBY_MILESTONE`, `CHEBY_PENDING` and
//! `CHEBY_PENDING_ALLOW` as the caller set them: the local evidence runs
//! with `CHEBY_MILESTONE=M3 CHEBY_PENDING=require`, so a failing M2 trial
//! fails the step, and the workflow needs neither once `tests/milestone`
//! names M3. Every other variable that selects a mode of `cheby` or of a
//! test (`CHEBY_THREADS`, `CHEBY_RT_STRESS_MIGRATE`, `CHEBY_RC`,
//! `CHEBY_RT_POISON`, `CHEBY_UNWIND_EDGES`, `CHEBY_BLESS`) is removed, and
//! a step sets only what it names, so a step always runs what `--list`
//! says.
//!
//! The sanitizers' options: `ASAN_OPTIONS` has `abort_on_error=1`,
//! `detect_stack_use_after_return=1` and `allocator_may_return_null=1`
//! (the out-of-memory tests expect a null, D-642); `TSAN_OPTIONS` has
//! `halt_on_error=1`, `second_deadlock_stack=1`,
//! `allocator_may_return_null=1` and
//! `suppressions=.github/tsan-suppressions.txt`, which lists reports from
//! dependencies only, each with its reason (D-863). On Linux, the leak
//! checker is on for the `jit` and `ui-deadlock` steps and off for
//! `runtime` and `runtime-stress`, whose unit tests leak on purpose (frozen
//! constants are immortal, leaked fixtures, child processes that end at a
//! halt or a refusal without cleanup), and for `conformance`,
//! `examples-poison-off` and `repl`, whose programs keep objects for good
//! on purpose: D-539 makes a constant's objects static, outside the
//! runtime's leak check, and only the constant's slot `cb$const$…` in the
//! JIT's data memory holds them, which the leak checker does not scan; and
//! the REPL never frees what its entries return, its session values
//! (D-547, no leak check in the REPL: D-458); the runtime's own leak check
//! (status 102, D-458) stays on everywhere (D-881). Fiber stack
//! slots are registered as root regions under the address cfg, so objects
//! held by fibers a halt freezes are not reported (D-708, D-859). The leak
//! checker does not run on macOS, so
//! `detect_leaks` is not passed there. A caller's `ASAN_OPTIONS`, `TSAN_OPTIONS`
//! and `MIRIFLAGS` come after the driver's and win.
//!
//! Each step prints one summary line, `sanitize <s> <step>: ...`, with its
//! status, its build and test times, the sanitizer reports found in its
//! output, and the summary lines of the test runs (libtest's `test result:`
//! and a harness's own `... harness:` lines). A report fails a step even if
//! the command exited 0. The last line sums the steps up. `--list` prints
//! the steps without running anything; `--no-run` only builds what they
//! need (a local `-Zbuild-std` build takes a lock its test run does not);
//! `--log-dir` keeps each step's output in a file as it comes.

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cheby_test_support::milestone::{Expectation, Setting, Verdicts};
use cheby_test_support::{example, golden, process, threads};

/// The dated nightly of the workflow (D-781, D-721).
pub const NIGHTLY: &str = "nightly-2026-10-01";

/// `CHEBY_HARNESS_SCALE` of every sanitizer step (D-748, D-725).
const HARNESS_SCALE: u32 = 10;

/// Miri's flags: permissive provenance, since the runtime turns words into
/// pointers with `with_exposed_provenance` throughout (`object.rs`);
/// isolation off, since proptest looks up its regression files with
/// `getcwd`; and Miri's leak check off, since Cheby objects hold each
/// other as integer words, which Miri cannot follow, so every object a
/// test makes immortal on purpose (a forgotten word, a count that
/// overflowed to static, D-454) would be reported. The runtime's own
/// accounting checks leaks instead: each test's domain returns to where it
/// started (D-884).
pub const MIRIFLAGS: &str =
    "-Zmiri-permissive-provenance -Zmiri-disable-isolation -Zmiri-ignore-leaks";

/// Proptest's cases per property under Miri, which interprets every case.
const PROPTEST_CASES: &str = "4";

/// Variables that select a mode of `cheby` or of a test. The driver
/// removes them, so a step runs exactly what it names.
const MODE_VARIABLES: [&str; 6] = [
    "CHEBY_THREADS",
    "CHEBY_RT_STRESS_MIGRATE",
    "CHEBY_RC",
    "CHEBY_RT_POISON",
    "CHEBY_UNWIND_EDGES",
    "CHEBY_BLESS",
];

/// Variables of the caller's cargo setup that would change the build: the
/// driver passes its flags, target directory and toolchain explicitly.
const BUILD_VARIABLES: [&str; 7] = [
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_TARGET_DIR",
];

/// Which job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Sanitizer {
    /// The address sanitizer, with the leak checker on Linux.
    Address,
    /// The thread sanitizer.
    Thread,
    /// The Miri subset of the runtime.
    Miri,
}

impl Sanitizer {
    /// The name `-Zsanitizer` and `cheby_sanitize` take.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Thread => "thread",
            Self::Miri => "miri",
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// address, thread or miri
    #[arg(value_enum)]
    sanitizer: Sanitizer,
    /// Run only this step; repeat it for several (see `--list`)
    #[arg(long = "step", value_name = "NAME")]
    steps: Vec<String>,
    /// Print the steps and their commands without running them
    #[arg(long)]
    list: bool,
    /// The target directory (default: `target/sanitize-<sanitizer>`); local
    /// runs pass a scratch directory, never the main `target/`
    #[arg(long, value_name = "DIR")]
    target_dir: Option<PathBuf>,
    /// Miri only: how many groups run at once
    #[arg(long, default_value_t = NonZeroUsize::MIN)]
    jobs: NonZeroUsize,
    /// Also write each step's output, as it comes, to
    /// `<DIR>/<sanitizer>-<step>.log` (created if missing)
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
    /// Only build what the steps need, and run no test (so a build can
    /// take a lock that the test run does not need)
    #[arg(long)]
    no_run: bool,
}

/// One test group of the Miri subset: a libtest filter over the runtime's
/// unit tests and the tests it skips, each with its reason.
#[derive(Debug, Clone, Copy)]
pub struct MiriGroup {
    /// The step is `miri-<name>`.
    pub name: &'static str,
    /// The libtest filter (a substring of the test path).
    pub filter: &'static str,
    /// `--skip` filters with their reasons.
    pub skips: &'static [(&'static str, &'static str)],
}

/// The Miri subset (D-884): the runtime's pure modules, the
/// frame-walk tests (`locate_tests`) and the timer tests written for Miri. The scheduler,
/// the pool, io, entries and the `loom` models stay out: they switch
/// stacks, start threads that park or call the poller, which Miri cannot
/// run. A test is skipped only for its size; a test too slow under Miri at
/// its normal size gets a `cfg!(miri)` size in its own file where it can.
/// Ordered longest first, so `--jobs` finishes early.
pub const MIRI_GROUPS: &[MiriGroup] = &[
    MiriGroup {
        name: "hamt",
        filter: "hamt::",
        skips: &[],
    },
    MiriGroup {
        name: "num",
        filter: "num::",
        skips: &[
            (
                "num::math::reference::tests::exact_midpoints_of_pow_round_to_even",
                "size: one 256-bit pow by series, about 30 minutes under Miri; \
                 the stable and sanitizer builds run it",
            ),
            (
                "num::math::accurate::tests::constants_have_their_known_bits",
                "size: pi and ln 2 by series to 256 bits, about 6 minutes under Miri; \
                 the stable and sanitizer builds run it",
            ),
        ],
    },
    MiriGroup {
        name: "alloc",
        filter: "alloc::",
        skips: &[
            (
                "alloc::domain_tests::four_threads_count_exactly_in_one_domain",
                "size: 40,000 allocations on four threads, over 30 minutes under Miri; \
                 the stable and sanitizer builds run it",
            ),
            (
                "alloc::domain_tests::more_threads_than_shards_count_exactly",
                "size: 3,000 allocations on each of more threads than shards; \
                 the stable and sanitizer builds run it",
            ),
            (
                "alloc::domain_tests::a_producer_and_a_consumer_rarely_sum_the_shards",
                "size: 100,000 allocations handed between two threads, about 10 minutes \
                 under Miri; the stable and sanitizer builds run it",
            ),
            (
                "alloc::tests::custom_kinds_free_through_the_worklist",
                "size: a million-deep chain; the stable and sanitizer builds run it",
            ),
            (
                "alloc::tests::freezing_makes_deep_and_shared_values_static",
                "size: a million-deep value; the stable and sanitizer builds run it",
            ),
            (
                "alloc::out_of_memory_tests::",
                "size: they ask the allocator for impossible sizes and expect its null \
                 (D-642), which Miri reports as resource exhaustion instead; the stable \
                 and sanitizer builds run them (allocator_may_return_null=1)",
            ),
        ],
    },
    MiriGroup {
        name: "list",
        filter: "list::",
        skips: &[],
    },
    MiriGroup {
        name: "generic",
        filter: "generic::",
        skips: &[],
    },
    MiriGroup {
        name: "text",
        filter: "text::",
        skips: &[("num::text::", "in the `num` group")],
    },
    MiriGroup {
        name: "bytes",
        filter: "bytes::",
        skips: &[],
    },
    MiriGroup {
        name: "locate",
        filter: "locate_tests::",
        skips: &[],
    },
    // D-1039, D-1042: the backtrace's walks on synthetic stacks (N7b).
    MiriGroup {
        name: "backtrace",
        filter: "backtrace_tests::",
        skips: &[],
    },
    MiriGroup {
        name: "rc",
        filter: "rc::",
        skips: &[],
    },
    MiriGroup {
        name: "layout",
        filter: "layout::",
        skips: &[],
    },
    MiriGroup {
        name: "string",
        filter: "string::",
        skips: &[],
    },
    MiriGroup {
        name: "abi",
        filter: "abi::",
        skips: &[],
    },
    MiriGroup {
        name: "values",
        filter: "values::",
        skips: &[],
    },
    MiriGroup {
        name: "timer",
        filter: "sched::timer::tests::miri_timer_",
        skips: &[],
    },
];

/// Whether the leak checker runs in a step (D-881).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leaks {
    /// `detect_leaks=1` on Linux.
    On,
    /// `detect_leaks=0`: the runtime's unit tests leak on purpose, programs
    /// that read a constant hold its static objects (D-539) only in the
    /// JIT's data memory, which the leak checker does not scan, and the
    /// REPL never frees its session values (D-547, D-458).
    Off,
}

/// What one command of a step does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `cargo test` with these arguments (package and test selection, then
    /// `--` and libtest's arguments) and environment.
    Test {
        env: Vec<(&'static str, &'static str)>,
        args: Vec<&'static str>,
    },
    /// Builds the sanitized `cheby` and runs every example's run trial with
    /// `CHEBY_RT_POISON=0`, comparing with the goldens (D-844): the
    /// harnesses poison freed objects in every trial, so only this pass
    /// executes the block cache and the inline reuse paths.
    ExamplesPoisonOff,
}

/// One step of an address or thread job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    /// Whether the step runs under each sanitizer.
    pub address: bool,
    pub thread: bool,
    pub leaks: Leaks,
    pub actions: Vec<Action>,
}

/// Examples that run under the address sanitizer only, because they fail
/// under the thread sanitizer only when their own timeouts expire under its
/// slowdown
/// (D-863, D-725). Empty until a run shows one; the workflow's header names
/// each.
pub const THREAD_TIMING_SKIPS: &[&str] = &[];

/// The steps of the address and thread jobs (plan §2.3), in order.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "the table of steps reads best as one list"
)]
pub fn steps() -> Vec<Step> {
    let runtime_args = vec![
        "-p",
        "cheby_runtime",
        "--lib",
        "--",
        "--skip",
        "models::",
        "--test-threads",
        "1",
    ];
    let ui_deadlock_args = vec![
        "-p",
        "cheby_cli",
        "--test",
        "ui",
        "--",
        "--test-threads",
        "1",
        "10-concurrency/deadlock/",
    ];
    vec![
        Step {
            name: "runtime",
            address: true,
            thread: true,
            leaks: Leaks::Off,
            actions: vec![Action::Test {
                env: vec![],
                args: runtime_args.clone(),
            }],
        },
        Step {
            name: "runtime-stress",
            address: true,
            thread: true,
            leaks: Leaks::Off,
            actions: vec![Action::Test {
                env: vec![("CHEBY_RT_STRESS_MIGRATE", "1")],
                args: runtime_args,
            }],
        },
        Step {
            name: "jit",
            address: true,
            thread: true,
            leaks: Leaks::On,
            actions: vec![Action::Test {
                env: vec![],
                args: vec!["-p", "cheby_jit", "--", "--test-threads", "1"],
            }],
        },
        Step {
            name: "ui-deadlock",
            address: true,
            thread: true,
            leaks: Leaks::On,
            actions: vec![
                Action::Test {
                    env: vec![("CHEBY_THREADS", "4"), ("CHEBY_RT_STRESS_MIGRATE", "1")],
                    args: ui_deadlock_args.clone(),
                },
                Action::Test {
                    env: vec![("CHEBY_THREADS", "2"), ("CHEBY_RT_STRESS_MIGRATE", "1")],
                    args: ui_deadlock_args,
                },
            ],
        },
        Step {
            name: "conformance",
            address: true,
            thread: true,
            leaks: Leaks::Off,
            actions: vec![Action::Test {
                env: vec![("CHEBY_THREADS", "4"), ("CHEBY_RT_STRESS_MIGRATE", "1")],
                // The 2- and 4-thread variants of every run and `::test`
                // trial (testing.md).
                args: vec![
                    "-p",
                    "cheby_cli",
                    "--test",
                    "conformance",
                    "--",
                    "--test-threads",
                    "1",
                    "@threads=",
                    "--skip",
                    "@threads=1",
                ],
            }],
        },
        Step {
            name: "examples-poison-off",
            address: true,
            thread: false,
            leaks: Leaks::Off,
            actions: vec![Action::ExamplesPoisonOff],
        },
        Step {
            name: "repl",
            address: true,
            thread: true,
            leaks: Leaks::Off,
            actions: vec![Action::Test {
                env: vec![],
                args: vec![
                    "-p",
                    "cheby_cli",
                    "--test",
                    "repl",
                    "--",
                    "--test-threads",
                    "1",
                ],
            }],
        },
    ]
}

/// What `--list` and a run show for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// `runtime`, `miri-hamt`, ...
    pub name: String,
    /// One line per command.
    pub commands: Vec<String>,
}

/// The steps of `sanitizer`, in order.
#[must_use]
pub fn plan(sanitizer: Sanitizer) -> Vec<Planned> {
    match sanitizer {
        Sanitizer::Miri => MIRI_GROUPS
            .iter()
            .map(|group| Planned {
                name: format!("miri-{}", group.name),
                commands: vec![describe_miri(group)],
            })
            .collect(),
        Sanitizer::Address | Sanitizer::Thread => steps()
            .into_iter()
            .filter(|step| runs_under(step, sanitizer))
            .map(|step| Planned {
                name: step.name.to_owned(),
                commands: step
                    .actions
                    .iter()
                    .map(|action| describe_action(action, sanitizer))
                    .collect(),
            })
            .collect(),
    }
}

fn runs_under(step: &Step, sanitizer: Sanitizer) -> bool {
    match sanitizer {
        Sanitizer::Address => step.address,
        Sanitizer::Thread => step.thread,
        Sanitizer::Miri => false,
    }
}

fn describe_miri(group: &MiriGroup) -> String {
    let mut line = format!("cargo miri test -p cheby_runtime --lib -- {}", group.filter);
    for (skip, reason) in group.skips {
        let _ = write!(line, " --skip {skip} ({reason})");
    }
    line
}

fn describe_action(action: &Action, sanitizer: Sanitizer) -> String {
    match action {
        Action::Test { env, args } => {
            let mut words: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            words.push("cargo test".to_owned());
            words.extend(test_args(args, sanitizer).iter().map(|a| (*a).to_owned()));
            words.join(" ")
        }
        Action::ExamplesPoisonOff => {
            "cargo build -p cheby_cli --bin cheby, then `cheby run` of every example with \
             CHEBY_RT_POISON=0 against its goldens"
                .to_owned()
        }
    }
}

/// `args` with the skips of [`THREAD_TIMING_SKIPS`] appended to the
/// conformance step's libtest arguments under the thread sanitizer.
fn test_args(args: &[&'static str], sanitizer: Sanitizer) -> Vec<&'static str> {
    let mut out = args.to_vec();
    if sanitizer == Sanitizer::Thread && args.contains(&"conformance") {
        for example in THREAD_TIMING_SKIPS {
            out.push("--skip");
            out.push(example);
        }
    }
    out
}

/// The steps `names` selects from `planned`, in the plan's order; all of
/// them when `names` is empty.
///
/// # Errors
///
/// Returns a message naming an unknown step.
pub fn select(planned: Vec<Planned>, names: &[String]) -> Result<Vec<Planned>, String> {
    if let Some(unknown) = names
        .iter()
        .find(|name| !planned.iter().any(|step| step.name == **name))
    {
        let known: Vec<_> = planned.iter().map(|step| step.name.as_str()).collect();
        return Err(format!(
            "no step `{unknown}`; the steps are: {}",
            known.join(", ")
        ));
    }
    Ok(planned
        .into_iter()
        .filter(|step| names.is_empty() || names.contains(&step.name))
        .collect())
}

/// `ASAN_OPTIONS` or `TSAN_OPTIONS` of a step; `None` for Miri.
#[must_use]
pub fn sanitizer_options(
    sanitizer: Sanitizer,
    leaks: Leaks,
    linux: bool,
    suppressions: &Path,
) -> Option<(&'static str, String)> {
    match sanitizer {
        Sanitizer::Address => {
            let mut options = "abort_on_error=1:detect_stack_use_after_return=1:\
                               allocator_may_return_null=1"
                .to_owned();
            if linux {
                options.push_str(match leaks {
                    Leaks::On => ":detect_leaks=1",
                    Leaks::Off => ":detect_leaks=0",
                });
            }
            Some(("ASAN_OPTIONS", options))
        }
        Sanitizer::Thread => Some((
            "TSAN_OPTIONS",
            format!(
                "halt_on_error=1:second_deadlock_stack=1:allocator_may_return_null=1:\
                 suppressions={}",
                suppressions.display()
            ),
        )),
        Sanitizer::Miri => None,
    }
}

/// `ours`, then the caller's `value` of the same variable, which wins
/// where they overlap.
fn with_caller(ours: &str, caller: Option<String>, separator: char) -> String {
    match caller {
        Some(theirs) if !theirs.trim().is_empty() => format!("{ours}{separator}{theirs}"),
        _ => ours.to_owned(),
    }
}

/// Lines of a step's output the driver repeats in the step's summary.
#[must_use]
pub fn is_summary_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("test result:")
        || [
            "conformance harness:",
            "UI harness:",
            "REPL harness:",
            "ui harness:",
            "repl harness:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

/// Whether a line starts a sanitizer report or a Miri error.
#[must_use]
pub fn is_report_line(line: &str) -> bool {
    [
        "ERROR: AddressSanitizer",
        "WARNING: ThreadSanitizer",
        "ERROR: LeakSanitizer",
        "WARNING: ASan ",
        "error: Undefined Behavior",
    ]
    .iter()
    .any(|marker| line.contains(marker))
}

/// The output of one command, as the driver saw it.
#[derive(Debug, Default)]
struct Seen {
    success: bool,
    reports: usize,
    summary: Vec<String>,
    /// Everything the command printed, when the step's output is held back
    /// until it ends (`--jobs` above 1).
    held: Vec<u8>,
}

/// Where a step's output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// Printed as it comes.
    Live,
    /// Kept and printed when the step ends, so steps that run at once do
    /// not interleave.
    Held,
}

/// Runs `cmd`, forwarding or holding its output and appending it to `log`
/// as it comes, and scans its lines.
fn run_scanned(mut cmd: Command, output: Output, log: Option<&Path>) -> Result<Seen, String> {
    let log = match log {
        Some(path) => Some(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("{}: {e}", path.display()))?,
        )),
        None => None,
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {cmd:?}: {e}"))?;
    let stdout = child.stdout.take().ok_or("stdout is not piped")?;
    let stderr = child.stderr.take().ok_or("stderr is not piped")?;
    let seen = Mutex::new(Seen::default());
    std::thread::scope(|scope| {
        scope.spawn(|| scan(stdout, false, output, &seen, log.as_ref()));
        scope.spawn(|| scan(stderr, true, output, &seen, log.as_ref()));
    });
    let status = child.wait().map_err(|e| e.to_string())?;
    let mut seen = seen
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.success = status.success();
    Ok(seen)
}

fn scan(
    pipe: impl Read,
    is_stderr: bool,
    output: Output,
    seen: &Mutex<Seen>,
    log: Option<&Mutex<std::fs::File>>,
) {
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A command killed mid-line (`test x ... ` of a test that never
        // ended) still leaves the driver's own lines at a line's start.
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        if let Some(log) = log {
            // A full disk loses log lines, not the step.
            let _ = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_all(&line);
        }
        let text = String::from_utf8_lossy(&line);
        let mut seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if is_report_line(&text) {
            seen.reports = seen.reports.saturating_add(1);
        }
        if is_summary_line(&text) {
            seen.summary.push(text.trim().to_owned());
        }
        match output {
            Output::Held => seen.held.extend_from_slice(&line),
            Output::Live => {
                drop(seen);
                // A closed terminal is not the step's failure.
                let _ = if is_stderr {
                    std::io::stderr().write_all(&line)
                } else {
                    std::io::stdout().write_all(&line)
                };
            }
        }
    }
}

/// The result of one step.
#[derive(Debug)]
struct Outcome {
    name: String,
    success: bool,
    line: String,
}

/// The fixed context of a run.
#[derive(Debug)]
struct Context {
    root: PathBuf,
    sanitizer: Sanitizer,
    target_dir: PathBuf,
    host: String,
    linux: bool,
    log_dir: Option<PathBuf>,
    no_run: bool,
}

impl Context {
    /// The log file of `step` (`<log dir>/<sanitizer>-<step>.log`), if the
    /// run keeps logs.
    fn log(&self, step: &str) -> Option<PathBuf> {
        self.log_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}-{step}.log", self.sanitizer.name())))
    }

    /// `cargo +<nightly> <args>` with the build variables removed.
    fn cargo(&self) -> Command {
        let mut cmd = Command::new("cargo");
        cmd.current_dir(&self.root).arg(format!("+{NIGHTLY}"));
        for var in BUILD_VARIABLES.into_iter().chain(MODE_VARIABLES) {
            cmd.env_remove(var);
        }
        cmd
    }

    /// `cargo +<nightly> <subcommand> --locked -Zbuild-std --target <host>
    /// --target-dir <dir>` with the sanitizer's flags and the step's
    /// environment.
    fn sanitized(&self, subcommand: &str, leaks: Leaks) -> Command {
        let mut cmd = self.cargo();
        let name = self.sanitizer.name();
        let flags = format!("-Zsanitizer={name} --cfg cheby_sanitize=\"{name}\"");
        cmd.args([subcommand, "--locked", "-Zbuild-std", "--target"])
            .arg(&self.host)
            .arg("--target-dir")
            .arg(&self.target_dir)
            .env("RUSTFLAGS", &flags)
            .env("RUSTDOCFLAGS", &flags)
            .env("CHEBY_RT_TEST_EMULATED", "1")
            .env("CHEBY_HARNESS_SCALE", HARNESS_SCALE.to_string());
        let suppressions = self.root.join(".github/tsan-suppressions.txt");
        if let Some((var, options)) =
            sanitizer_options(self.sanitizer, leaks, self.linux, &suppressions)
        {
            cmd.env(var, with_caller(&options, std::env::var(var).ok(), ':'));
        }
        cmd
    }

    fn miri(&self) -> Command {
        let mut cmd = self.cargo();
        cmd.args(["miri", "test", "--locked", "-p", "cheby_runtime", "--lib"])
            .arg("--target-dir")
            .arg(&self.target_dir)
            .env(
                "MIRIFLAGS",
                with_caller(MIRIFLAGS, std::env::var("MIRIFLAGS").ok(), ' '),
            )
            .env("PROPTEST_CASES", PROPTEST_CASES);
        cmd
    }
}

/// `cargo xtask sanitize`.
pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    let planned = select(plan(args.sanitizer), &args.steps)?;
    let name = args.sanitizer.name();
    if args.list {
        for step in &planned {
            println!("{}", step.name);
            for command in &step.commands {
                println!("    {command}");
            }
        }
        return Ok(());
    }
    if args.sanitizer != Sanitizer::Miri && args.jobs.get() > 1 {
        return Err(format!(
            "--jobs applies to miri only: the {name} steps run their tests one at a time (D-725)"
        ));
    }
    let target_dir = match &args.target_dir {
        Some(dir) => crate::absolute(dir)?,
        None => root.join(format!("target/sanitize-{name}")),
    };
    let log_dir = match &args.log_dir {
        Some(dir) => {
            let dir = crate::absolute(dir)?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Some(dir)
        }
        None => None,
    };
    let cx = Context {
        root: root.to_owned(),
        sanitizer: args.sanitizer,
        host: host_triple()?,
        linux: cfg!(target_os = "linux"),
        target_dir,
        log_dir,
        no_run: args.no_run,
    };
    println!(
        "sanitize {name}: {} step(s) on {NIGHTLY} for {}, target directory {}",
        planned.len(),
        cx.host,
        cx.target_dir.display()
    );
    let start = Instant::now();
    let outcomes = match args.sanitizer {
        Sanitizer::Miri => run_miri(&cx, &planned, args.jobs)?,
        Sanitizer::Address | Sanitizer::Thread => {
            let all = steps();
            let mut outcomes = Vec::new();
            for step in all
                .iter()
                .filter(|step| planned.iter().any(|p| p.name == step.name))
            {
                println!("sanitize {name} {}: starting", step.name);
                let outcome = run_step(&cx, step);
                println!("{}", outcome.line);
                outcomes.push(outcome);
            }
            outcomes
        }
    };
    let failed: Vec<_> = outcomes
        .iter()
        .filter(|o| !o.success)
        .map(|o| o.name.as_str())
        .collect();
    println!("sanitize {name}: summary");
    for outcome in &outcomes {
        println!("  {}", outcome.line);
    }
    println!(
        "sanitize {name}: {} step(s), {} passed, {} failed, in {}",
        outcomes.len(),
        outcomes.len().saturating_sub(failed.len()),
        failed.len(),
        seconds(start.elapsed())
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "sanitize {name}: failed steps: {}",
            failed.join(", ")
        ))
    }
}

/// The host's target triple, from the nightly's `rustc -vV`.
fn host_triple() -> Result<String, String> {
    let mut cmd = Command::new("rustc");
    for var in BUILD_VARIABLES {
        cmd.env_remove(var);
    }
    let out = cmd
        .arg(format!("+{NIGHTLY}"))
        .arg("-vV")
        .output()
        .map_err(|e| format!("cannot run rustc +{NIGHTLY}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!(
            "`rustc +{NIGHTLY} -vV` failed; install the toolchain with \
             `rustup toolchain install {NIGHTLY} --component rust-src --component miri`:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_owned())
        .ok_or_else(|| format!("`rustc +{NIGHTLY} -vV` names no host:\n{text}"))
}

fn seconds(elapsed: Duration) -> String {
    format!("{:.0} s", elapsed.as_secs_f64())
}

/// Runs one address or thread step: each command's build (`--no-run`),
/// then its tests.
fn run_step(cx: &Context, step: &Step) -> Outcome {
    let name = cx.sanitizer.name();
    let mut build_time = Duration::ZERO;
    let mut test_time = Duration::ZERO;
    let mut success = true;
    let mut reports: usize = 0;
    let mut summary = Vec::new();
    let mut notes = Vec::new();
    let log = cx.log(step.name);
    for action in &step.actions {
        match action {
            Action::Test { env, args } => {
                let args = test_args(args, cx.sanitizer);
                let (selection, libtest) = split_args(&args);
                let started = Instant::now();
                let mut no_run = cx.sanitized("test", step.leaks);
                no_run.args(selection).arg("--no-run");
                let built = run_scanned(no_run, Output::Live, log.as_deref());
                build_time = build_time.saturating_add(started.elapsed());
                match built {
                    Ok(seen) if seen.success => {}
                    Ok(_) => {
                        success = false;
                        notes.push("build failed".to_owned());
                        continue;
                    }
                    Err(e) => {
                        success = false;
                        notes.push(e);
                        continue;
                    }
                }
                if cx.no_run {
                    continue;
                }
                let started = Instant::now();
                let mut test = cx.sanitized("test", step.leaks);
                test.args(selection).arg("--").args(libtest);
                for (key, value) in env {
                    test.env(key, value);
                }
                match run_scanned(test, Output::Live, log.as_deref()) {
                    Ok(seen) => {
                        success &= seen.success && seen.reports == 0;
                        reports = reports.saturating_add(seen.reports);
                        summary.extend(seen.summary);
                    }
                    Err(e) => {
                        success = false;
                        notes.push(e);
                    }
                }
                test_time = test_time.saturating_add(started.elapsed());
            }
            Action::ExamplesPoisonOff => {
                let started = Instant::now();
                let cheby = build_cheby(cx, step.leaks);
                build_time = build_time.saturating_add(started.elapsed());
                if cx.no_run {
                    if let Err(e) = cheby {
                        success = false;
                        notes.push(e);
                    }
                    continue;
                }
                let started = Instant::now();
                match cheby.and_then(|cheby| examples_poison_off(cx, &cheby, step.leaks)) {
                    Ok(result) => {
                        success &= result.success;
                        reports = reports.saturating_add(result.reports);
                        summary.push(result.summary);
                    }
                    Err(e) => {
                        success = false;
                        notes.push(e);
                    }
                }
                test_time = test_time.saturating_add(started.elapsed());
            }
        }
    }
    let mut line = format!(
        "sanitize {name} {}: {} (build {}, tests {}), reports {reports}",
        step.name,
        if success { "ok" } else { "FAILED" },
        seconds(build_time),
        seconds(test_time),
    );
    for part in notes.iter().chain(&summary) {
        line.push_str(" | ");
        line.push_str(part);
    }
    Outcome {
        name: step.name.to_owned(),
        success,
        line,
    }
}

/// `args` split at `--`: cargo's package and test selection, then
/// libtest's arguments.
fn split_args<'a>(args: &'a [&'static str]) -> (&'a [&'static str], &'a [&'static str]) {
    match args.iter().position(|a| *a == "--") {
        Some(at) => (
            args.get(..at).unwrap_or_default(),
            args.get(at.saturating_add(1)..).unwrap_or_default(),
        ),
        None => (args, &[]),
    }
}

/// Builds the sanitized `cheby` and returns its path, from cargo's JSON
/// messages (the layout of the target directory is cargo's business).
fn build_cheby(cx: &Context, leaks: Leaks) -> Result<PathBuf, String> {
    let mut cmd = cx.sanitized("build", leaks);
    cmd.args([
        "-p",
        "cheby_cli",
        "--bin",
        "cheby",
        "--message-format=json-render-diagnostics",
    ]);
    let seen = run_scanned(cmd, Output::Held, None)?;
    let text = String::from_utf8_lossy(&seen.held);
    let mut executable = None;
    for line in text.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            // Diagnostics and cargo's own lines.
            println!("{line}");
            continue;
        };
        if message.get("reason").and_then(|v| v.as_str()) == Some("compiler-artifact")
            && message.pointer("/target/name").and_then(|v| v.as_str()) == Some("cheby")
            && let Some(path) = message.get("executable").and_then(|v| v.as_str())
        {
            executable = Some(PathBuf::from(path));
        }
    }
    match (seen.success, executable) {
        (true, Some(path)) => Ok(path),
        (true, None) => Err("cargo built no `cheby` executable".to_owned()),
        (false, _) => Err("cannot build the sanitized `cheby`".to_owned()),
    }
}

/// The result of the poison-off pass.
#[derive(Debug)]
struct PassResult {
    success: bool,
    reports: usize,
    summary: String,
}

/// Runs every example's run trial with the sanitized `cheby` and
/// `CHEBY_RT_POISON=0`, as the conformance harness runs the base trial
/// (the example's `env` file, `NO_COLOR=1`, no `CHEBY_THREADS` or
/// `CHEBY_RC`), and compares the result with the goldens. Examples follow
/// `tests/milestone` as the harness does (D-288, D-544): an example of a
/// later milestone is not run, and one of the milestone in progress is
/// judged by `CHEBY_PENDING`.
fn examples_poison_off(cx: &Context, cheby: &Path, leaks: Leaks) -> Result<PassResult, String> {
    let setting = Setting::read(&cx.root)?;
    let verdicts = Verdicts::from_env()?;
    let timeout = process::scale_timeout(Duration::from_secs(60), HARNESS_SCALE);
    let suppressions = cx.root.join(".github/tsan-suppressions.txt");
    let options = sanitizer_options(cx.sanitizer, leaks, cx.linux, &suppressions);
    let (mut passed, mut failed, mut skipped, mut reports) = (0_usize, 0_usize, 0_usize, 0_usize);
    for ex in example::all(&cx.root)? {
        let expectation = setting.expectation(ex.config.milestone);
        if ex.config.skip_run || expectation == Expectation::Fail {
            skipped = skipped.saturating_add(1);
            continue;
        }
        let goldens = ex.goldens()?;
        let mut cmd = Command::new(cheby);
        cmd.arg("--color=never").arg("run").current_dir(&ex.dir);
        crate::cache::apply(&mut cmd)?;
        if !goldens.args.is_empty() {
            cmd.arg("--").args(&goldens.args);
        }
        for var in MODE_VARIABLES {
            cmd.env_remove(var);
        }
        threads::apply(
            &mut cmd,
            &threads::child_env(&goldens.env, threads::Variant::Unset),
        );
        cmd.env("CHEBY_RT_POISON", "0");
        if let Some((var, value)) = &options {
            cmd.env(var, with_caller(value, std::env::var(var).ok(), ':'));
        }
        let out = process::run(cmd, goldens.stdin.as_deref().map(str::as_bytes), timeout)?;
        let found = out.stderr.lines().filter(|l| is_report_line(l)).count();
        reports = reports.saturating_add(found);
        let result = judge_run(&goldens, &out, found);
        let name = format!("{}@poison=off", ex.name);
        match verdicts.judge(&name, ex.config.milestone, expectation, result) {
            Ok(()) => {
                passed = passed.saturating_add(1);
                println!("ok    {name}");
            }
            Err(why) => {
                failed = failed.saturating_add(1);
                println!("FAIL  {name}: {why}");
            }
        }
    }
    verdicts.report("examples with CHEBY_RT_POISON=0");
    Ok(PassResult {
        success: failed == 0,
        reports,
        summary: format!(
            "examples poison off: {passed} passed, {failed} failed, {skipped} not run \
             (skip-run or a later milestone)"
        ),
    })
}

fn judge_run(
    goldens: &example::Goldens,
    out: &process::RunOutput,
    reports: usize,
) -> Result<(), String> {
    if reports > 0 {
        return Err(format!("{reports} sanitizer report(s)\n{}", out.stderr));
    }
    let expected_exit = goldens.exit.unwrap_or(0);
    if out.timed_out || out.status != Some(expected_exit) {
        return Err(format!(
            "expected exit code {expected_exit}, got {}\nstderr:\n{}",
            out.describe_status(),
            out.stderr
        ));
    }
    let stdout = goldens.stdout.as_deref().ok_or("missing expected.stdout")?;
    golden::compare("stdout", stdout, &out.stdout)?;
    golden::compare(
        "stderr",
        goldens.stderr.as_deref().unwrap_or(""),
        &out.stderr,
    )
}

/// Builds the Miri test binary once, then runs the groups, `jobs` at a
/// time, each with its output held back until it ends when `jobs` is above
/// 1.
fn run_miri(cx: &Context, planned: &[Planned], jobs: NonZeroUsize) -> Result<Vec<Outcome>, String> {
    let groups: Vec<&MiriGroup> = MIRI_GROUPS
        .iter()
        .filter(|g| planned.iter().any(|p| p.name == format!("miri-{}", g.name)))
        .collect();
    let started = Instant::now();
    let mut no_run = cx.miri();
    no_run.arg("--no-run");
    let built = run_scanned(no_run, Output::Live, cx.log("build").as_deref())?;
    println!(
        "sanitize miri build: {} in {}",
        if built.success { "ok" } else { "FAILED" },
        seconds(started.elapsed())
    );
    if !built.success {
        return Err("cannot build the runtime's tests under Miri".to_owned());
    }
    if cx.no_run {
        return Ok(Vec::new());
    }
    let output = if jobs.get() > 1 {
        Output::Held
    } else {
        Output::Live
    };
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.get().min(groups.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(group) = groups.get(index) else {
                        break;
                    };
                    let outcome = run_miri_group(cx, group, output);
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((index, outcome));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, outcome)| outcome).collect())
}

fn run_miri_group(cx: &Context, group: &MiriGroup, output: Output) -> Outcome {
    let step = format!("miri-{}", group.name);
    if output == Output::Live {
        println!("sanitize miri {step}: starting");
    }
    let started = Instant::now();
    let mut cmd = cx.miri();
    // Each test's time (the nightly's libtest), so a test that outgrows
    // Miri shows in the log.
    cmd.args(["--", "-Zunstable-options", "--report-time", group.filter]);
    for (skip, _) in group.skips {
        cmd.args(["--skip", skip]);
    }
    let seen = run_scanned(cmd, output, cx.log(group.name).as_deref());
    let elapsed = started.elapsed();
    let (success, mut line) = match &seen {
        Ok(seen) => {
            let success = seen.success && seen.reports == 0;
            (
                success,
                format!(
                    "sanitize miri {step}: {} in {}, reports {}",
                    if success { "ok" } else { "FAILED" },
                    seconds(elapsed),
                    seen.reports
                ),
            )
        }
        Err(e) => (false, format!("sanitize miri {step}: FAILED: {e}")),
    };
    if let Ok(seen) = &seen {
        for part in &seen.summary {
            line.push_str(" | ");
            line.push_str(part);
        }
    }
    {
        // One group's output and its line stay together.
        let mut out = std::io::stdout().lock();
        if let Ok(seen) = &seen {
            let _ = out.write_all(&seen.held);
        }
        let _ = writeln!(out, "{line}");
    }
    Outcome {
        name: step,
        success,
        line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(words: &[&str]) -> Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("sanitize").chain(words.iter().copied()))
            .map(|cli| cli.args)
    }

    fn names(sanitizer: Sanitizer) -> Vec<String> {
        plan(sanitizer).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn arguments_parse() {
        let args = parse(&["thread"]).unwrap();
        assert_eq!(args.sanitizer, Sanitizer::Thread);
        assert!(args.steps.is_empty() && !args.list && args.target_dir.is_none());
        assert_eq!(args.jobs.get(), 1);

        let args = parse(&[
            "miri",
            "--step",
            "miri-hamt",
            "--step",
            "miri-rc",
            "--list",
            "--target-dir",
            "/tmp/x",
            "--jobs",
            "3",
            "--log-dir",
            "/tmp/logs",
        ])
        .unwrap();
        assert_eq!(args.sanitizer, Sanitizer::Miri);
        assert_eq!(args.steps, ["miri-hamt", "miri-rc"]);
        assert!(args.list);
        assert_eq!(args.target_dir.as_deref(), Some(Path::new("/tmp/x")));
        assert_eq!(args.jobs.get(), 3);
        assert_eq!(args.log_dir.as_deref(), Some(Path::new("/tmp/logs")));

        assert_eq!(parse(&["address"]).unwrap().sanitizer, Sanitizer::Address);
        assert!(!parse(&["address"]).unwrap().no_run);
        assert!(parse(&["address", "--no-run"]).unwrap().no_run);
        assert!(parse(&[]).is_err());
        assert!(parse(&["memory"]).is_err());
        assert!(parse(&["miri", "--jobs", "0"]).is_err());
    }

    #[test]
    fn the_steps_follow_the_plan() {
        assert_eq!(
            names(Sanitizer::Address),
            [
                "runtime",
                "runtime-stress",
                "jit",
                "ui-deadlock",
                "conformance",
                "examples-poison-off",
                "repl"
            ]
        );
        assert_eq!(
            names(Sanitizer::Thread),
            [
                "runtime",
                "runtime-stress",
                "jit",
                "ui-deadlock",
                "conformance",
                "repl"
            ]
        );
        let thread = plan(Sanitizer::Thread);
        let runtime = &thread[0].commands;
        assert_eq!(
            runtime,
            &["cargo test -p cheby_runtime --lib -- --skip models:: --test-threads 1"]
        );
        assert_eq!(
            thread[1].commands,
            [
                "CHEBY_RT_STRESS_MIGRATE=1 cargo test -p cheby_runtime --lib -- --skip models:: \
              --test-threads 1"
            ]
        );
        assert_eq!(
            thread[3].commands,
            [
                "CHEBY_THREADS=4 CHEBY_RT_STRESS_MIGRATE=1 cargo test -p cheby_cli --test ui -- \
                 --test-threads 1 10-concurrency/deadlock/",
                "CHEBY_THREADS=2 CHEBY_RT_STRESS_MIGRATE=1 cargo test -p cheby_cli --test ui -- \
                 --test-threads 1 10-concurrency/deadlock/"
            ]
        );
        assert!(
            thread[4].commands[0]
                .ends_with("--test conformance -- --test-threads 1 @threads= --skip @threads=1")
        );
        assert_eq!(
            thread[5].commands,
            ["cargo test -p cheby_cli --test repl -- --test-threads 1"]
        );
    }

    #[test]
    fn leak_checking_follows_the_step() {
        let on: Vec<_> = steps()
            .into_iter()
            .filter(|s| s.leaks == Leaks::On)
            .map(|s| s.name)
            .collect();
        assert_eq!(on, ["jit", "ui-deadlock"]);
        let supp = Path::new("/r/.github/tsan-suppressions.txt");
        let (var, linux_on) = sanitizer_options(Sanitizer::Address, Leaks::On, true, supp).unwrap();
        assert_eq!(var, "ASAN_OPTIONS");
        assert!(linux_on.ends_with(":detect_leaks=1"));
        let (_, linux_off) = sanitizer_options(Sanitizer::Address, Leaks::Off, true, supp).unwrap();
        assert!(linux_off.ends_with(":detect_leaks=0"));
        let (_, mac) = sanitizer_options(Sanitizer::Address, Leaks::On, false, supp).unwrap();
        assert!(!mac.contains("detect_leaks"));
        let (var, tsan) = sanitizer_options(Sanitizer::Thread, Leaks::On, true, supp).unwrap();
        assert_eq!(var, "TSAN_OPTIONS");
        assert!(tsan.ends_with(":suppressions=/r/.github/tsan-suppressions.txt"));
        assert!(sanitizer_options(Sanitizer::Miri, Leaks::On, true, supp).is_none());
        assert_eq!(with_caller("a=1", Some("b=2".to_owned()), ':'), "a=1:b=2");
        assert_eq!(with_caller("a=1", Some(" ".to_owned()), ':'), "a=1");
        assert_eq!(with_caller("-Za", None, ' '), "-Za");
    }

    /// D-863: a per-step option is named, with its cause, in the workflow's
    /// header. Review finding MEM2-1 of gate 2: the header gave unscanned
    /// fiber stacks as the cause for the program steps, which are root
    /// regions under the address cfg; the reports come from D-539's static
    /// constants, held only in the JIT's data memory, and from the REPL's
    /// session values, which it never frees (D-547, D-458).
    #[test]
    fn the_header_names_why_leak_checking_is_off() {
        let workflow =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/nightly.yml");
        let text = std::fs::read_to_string(&workflow).unwrap();
        let paragraph: String = text
            .lines()
            .skip_while(|line| !line.starts_with("#   LeakSanitizer is on"))
            .take_while(|line| !line.starts_with("#   A report whose"))
            .map(|line| line.trim_start_matches('#').trim())
            .collect::<Vec<_>>()
            .join(" ");
        for step in steps() {
            assert!(
                paragraph.contains(&format!("`{}`", step.name)),
                "the header's LeakSanitizer paragraph does not name `{}`: {paragraph}",
                step.name
            );
        }
        for cause in ["D-539", "D-547", "cb$const$", "D-708, D-859"] {
            assert!(paragraph.contains(cause), "no {cause}: {paragraph}");
        }
        assert!(
            !paragraph.contains("stacks it does not scan"),
            "{paragraph}"
        );
    }

    #[test]
    fn the_miri_subset_is_the_planned_one() {
        let filters: Vec<_> = MIRI_GROUPS.iter().map(|g| g.filter).collect();
        let mut expected = vec![
            "abi::",
            "values::",
            "string::",
            "rc::",
            "hamt::",
            "list::",
            "text::",
            "bytes::",
            "num::",
            "generic::",
            "layout::",
            "alloc::",
            "locate_tests::",
            "backtrace_tests::",
            "sched::timer::tests::miri_timer_",
        ];
        let mut sorted = filters.clone();
        sorted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
        let steps = names(Sanitizer::Miri);
        assert!(steps.iter().all(|s| s.starts_with("miri-")));
        let mut unique = steps.clone();
        unique.dedup();
        assert_eq!(unique.len(), steps.len());
        // Every skip has a reason, and no group reaches the scheduler, the
        // pool, io, entries or the models.
        for group in MIRI_GROUPS {
            assert!(group.skips.iter().all(|(_, why)| !why.trim().is_empty()));
            for out in ["fiber_ops::", "pool::", "io::", "entry::", "models::"] {
                assert!(!group.filter.contains(out), "{}", group.name);
            }
        }
        assert_eq!(
            MIRIFLAGS,
            "-Zmiri-permissive-provenance -Zmiri-disable-isolation -Zmiri-ignore-leaks"
        );
    }

    #[test]
    fn steps_are_selected_by_name() {
        let all = plan(Sanitizer::Address);
        let picked = select(all.clone(), &["repl".to_owned(), "runtime".to_owned()]).unwrap();
        // The plan's order, not the command line's.
        let picked: Vec<_> = picked.into_iter().map(|p| p.name).collect();
        assert_eq!(picked, ["runtime", "repl"]);
        assert_eq!(select(all.clone(), &[]).unwrap().len(), all.len());
        let err = select(plan(Sanitizer::Thread), &["examples-poison-off".to_owned()]).unwrap_err();
        assert!(err.contains("no step `examples-poison-off`"), "{err}");
    }

    #[test]
    fn output_lines_are_recognized() {
        assert!(is_summary_line("test result: ok. 3 passed; 0 failed"));
        assert!(is_summary_line("conformance harness: 120 passed, 0 failed"));
        assert!(!is_summary_line("test foo ... ok"));
        assert!(is_report_line(
            "==123==ERROR: AddressSanitizer: stack-buffer-overflow"
        ));
        assert!(is_report_line(
            "WARNING: ThreadSanitizer: data race (pid=1)"
        ));
        assert!(is_report_line(
            "==9==ERROR: LeakSanitizer: detected memory leaks"
        ));
        assert!(is_report_line(
            "==1==WARNING: ASan is ignoring requested __asan_handle_no_return"
        ));
        assert!(is_report_line(
            "error: Undefined Behavior: constructing invalid value"
        ));
        assert!(!is_report_line("test sched::deadlock_tests::x ... ok"));
    }

    #[test]
    fn arguments_split_at_the_separator() {
        let args = ["-p", "x", "--", "--test-threads", "1"];
        assert_eq!(
            split_args(&args),
            (&["-p", "x"][..], &["--test-threads", "1"][..])
        );
        assert_eq!(split_args(&["-p", "x"]), (&["-p", "x"][..], &[][..]));
    }
}
