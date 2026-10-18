//! Test helpers that run Rust closures as fibers of a [`Runtime`], for the
//! scheduler's tests and those of the fiber and channel runtime: runtimes
//! with a fixed thread count, root fibers, spawned fibers, a one-word
//! mailbox built on [`park`] and [`wake`], and the function-entry check
//! generated code performs.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use super::{
    AfterSwitch, FiberRef, Runtime, RuntimeConfig, Tuning, Unlock, park, spawn_fiber, wake,
};
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, MIN_STACK_SIZE};
use crate::panic::{cheby_rt_stack_slow, stack_position};

/// The factor `CHEBY_HARNESS_SCALE` sets for the runtime's own tests
/// (D-722; D-872): a whole number from 1 to 100,
/// read as `tests/support/src/process.rs` reads it; unset, empty or
/// invalid is 1 (the harnesses reject an invalid value themselves).
fn harness_scale() -> u32 {
    std::env::var("CHEBY_HARNESS_SCALE")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|scale| (1..=100).contains(scale))
        .unwrap_or(1)
}

/// `d` times `CHEBY_HARNESS_SCALE` (1 to 100, default 1;
/// D-872): every wait of a runtime test that bounds
/// how long something may take goes through it, so slow machines,
/// emulators and sanitizer builds scale it.
pub(crate) fn scaled(d: Duration) -> Duration {
    d.saturating_mul(harness_scale())
}

/// `scaled(60 s)`: how long a test waits for an outcome before it calls it
/// a hang. A function, not a constant, so that it scales (critique C4).
pub(crate) fn hang() -> Duration {
    scaled(Duration::from_secs(60))
}

/// Whether the tests run under an emulator, where only correctness is
/// asserted (`CHEBY_RT_TEST_EMULATED=1`, set by the container and Rosetta
/// runs).
pub(crate) fn emulated() -> bool {
    std::env::var_os("CHEBY_RT_TEST_EMULATED").is_some_and(|value| value == "1")
}

/// A runtime of `threads` workers with the migration stress mode off.
pub(crate) fn runtime(threads: usize) -> Runtime {
    tuned(threads, false, true)
}

/// A runtime of `threads` workers with the migration stress mode `stress`;
/// without `rescue`, the monitor never wakes a worker for a queued fiber,
/// so a lost wakeup hangs. Deadlock detection is off (D-860): the tests
/// that use it wake fibers from the host (the [`Mailbox`], waits with a
/// timeout), which the detector cannot see.
pub(crate) fn tuned(threads: usize, stress: bool, rescue: bool) -> Runtime {
    tuned_with(threads, stress, rescue, false)
}

/// As [`tuned`], with deadlock detection on (D-724, D-860), as in every
/// shipped host.
pub(crate) fn tuned_detecting(threads: usize, stress: bool, rescue: bool) -> Runtime {
    tuned_with(threads, stress, rescue, true)
}

/// A runtime of `threads` workers with every [`Tuning`] knob given.
fn tuned_with(threads: usize, stress: bool, rescue: bool, detect_deadlocks: bool) -> Runtime {
    Runtime::tuned(
        &RuntimeConfig {
            threads: Some(threads),
            ..RuntimeConfig::default()
        },
        Tuning {
            stress_migrate: Some(stress),
            rescue,
            detect_deadlocks,
        },
    )
    .unwrap()
}

/// An entry configuration for Rust-level roots in test mode.
pub(crate) fn config(stack_size: usize) -> EntryConfig {
    EntryConfig {
        stack_size,
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// Starts `body` as a root fiber with a stack of `stack_size` bytes; the
/// receiver gets its word, or its panic.
pub(crate) fn start_root(
    runtime: &Runtime,
    stack_size: usize,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> mpsc::Receiver<EntryOutcome> {
    runtime
        .spawn_root(
            &config(stack_size),
            Box::new(move |ctx| {
                // SAFETY: `ctx` is the root fiber's live control block.
                body(unsafe { &*ctx })
            }),
        )
        .unwrap()
}

/// Starts `body` as a root fiber with the entry configuration `config`
/// (its stack, trampolines, domain and backend); the receiver gets its
/// word, or its panic.
pub(crate) fn start_root_with(
    runtime: &Runtime,
    config: &EntryConfig,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> mpsc::Receiver<EntryOutcome> {
    runtime
        .spawn_root(
            config,
            Box::new(move |ctx| {
                // SAFETY: `ctx` is the root fiber's live control block.
                body(unsafe { &*ctx })
            }),
        )
        .unwrap()
}

/// [`start_root_with`] for a root in `group` (D-799).
pub(crate) fn start_root_in(
    runtime: &Runtime,
    config: &EntryConfig,
    group: &std::sync::Arc<super::DetachedGroup>,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> mpsc::Receiver<EntryOutcome> {
    runtime.shared.register_group(group);
    runtime
        .spawn_root_in(
            config,
            std::sync::Arc::clone(group),
            Box::new(move |ctx| {
                // SAFETY: `ctx` is the root fiber's live control block.
                body(unsafe { &*ctx })
            }),
            None,
        )
        .unwrap()
}

/// Runs `body` as a root fiber on the smallest stack and returns its word;
/// panics if it panics or hangs.
pub(crate) fn root(runtime: &Runtime, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) -> u64 {
    match start_root(runtime, MIN_STACK_SIZE, body).recv_timeout(hang()) {
        Ok(EntryOutcome::Returned(word)) => word,
        other => panic!("the root fiber did not return: {other:?}"),
    }
}

/// Spawns `body` as a fiber of `spawner`'s runtime on the smallest stack,
/// in the runnext slot with `next`.
pub(crate) fn spawn(spawner: &ExecCtx, next: bool, body: impl FnOnce(&ExecCtx) + Send + 'static) {
    spawn_fiber(
        spawner,
        MIN_STACK_SIZE,
        Box::new(move |ctx| {
            // SAFETY: `ctx` is the fiber's live control block.
            body(unsafe { &*ctx });
        }),
        next,
    )
    .unwrap();
}

/// A one-word mailbox: what a channel of capacity 1 does with a single
/// receiver, built on `park` and `wake` as the channel kernels will be.
#[derive(Default)]
pub(crate) struct Mailbox {
    state: parking_lot::Mutex<(Option<u64>, Option<FiberRef>)>,
}

impl Mailbox {
    /// Waits for a word.
    pub(crate) fn receive(&self, me: &ExecCtx) -> u64 {
        loop {
            let mut state = self.state.lock();
            if let Some(word) = state.0.take() {
                return word;
            }
            state.1 = Some(FiberRef::from_ctx(me));
            park(me, AfterSwitch::Park(Some(Unlock::new(state))));
        }
    }

    /// Puts a word in the empty mailbox, waking its receiver; with
    /// `from_running`, the receiver takes the waker's runnext slot.
    pub(crate) fn send(&self, word: u64, from_running: bool) {
        let mut state = self.state.lock();
        assert!(state.0.is_none(), "the mailbox is full");
        state.0 = Some(word);
        if let Some(waiter) = state.1.take() {
            // SAFETY: the waiter registered under this lock and parks with
            // it, so it is parking or parked, not retired.
            assert!(unsafe { wake(waiter, from_running) });
        }
    }

    /// The word in the mailbox, if any, without waiting.
    pub(crate) fn peek(&self) -> Option<u64> {
        self.state.lock().0
    }
}

/// Test hooks of one runtime's monitor: a delay between reading a worker's
/// running fiber and requesting a yield from it, which widens the window
/// in which that fiber could be retired, a count of requests aimed at a
/// retired control block (which must stay 0, I-8), a switch that keeps
/// it from firing timers, a hold on one worker right after it ended at a
/// stop (D-821), and a hold on the stop's give-up on
/// the blocking pool (D-815).
#[derive(Debug, Default)]
pub(crate) struct MonitorProbe {
    /// Milliseconds the monitor sleeps before each request.
    pub(crate) delay_ms: std::sync::atomic::AtomicU64,
    /// The control blocks this runtime retired and has not reused.
    retired: parking_lot::Mutex<std::collections::HashSet<usize>>,
    /// Requests the monitor was about to make on a retired control block.
    pub(crate) stale: std::sync::atomic::AtomicUsize,
    /// Whether the monitor leaves due timers to the workers, so that a
    /// test sees the poller fire them.
    pub(crate) skip_timers: std::sync::atomic::AtomicBool,
    /// Fibers a halt froze (D-708).
    pub(crate) frozen: std::sync::atomic::AtomicUsize,
    /// One more than the index of the worker that [`MonitorProbe::stop_exit`]
    /// holds right after it ended at a stop; 0 holds none.
    pub(crate) hold_stop_exit: std::sync::atomic::AtomicUsize,
    /// Set by the held worker once it ended at the stop and waits.
    pub(crate) stop_exit_held: std::sync::atomic::AtomicBool,
    /// Lets the held worker go on (its thread then ends).
    pub(crate) stop_exit_release: std::sync::atomic::AtomicBool,
    /// Keeps the stop from giving up on the fibers waiting on the blocking
    /// pool (D-815): its workers wait until none
    /// waits, at most [`hang`].
    pub(crate) hold_pool: std::sync::atomic::AtomicBool,
    /// The deadlock detector surveys on every monitor pass, with no sleep
    /// and no backoff (D-860).
    pub(crate) eager_detection: std::sync::atomic::AtomicBool,
    /// Surveys (pass 1 started) by domain address.
    surveys: parking_lot::Mutex<std::collections::HashMap<usize, usize>>,
    /// Surveys whose two passes agreed.
    pub(crate) confirmations: std::sync::atomic::AtomicUsize,
    /// Roots the detector reported, by domain address.
    reports: parking_lot::Mutex<std::collections::HashMap<usize, usize>>,
    /// Collect steps that walked the stack of a fiber that ran meanwhile
    /// (critique RISK-2) and gave the report up: 0 unless a test makes
    /// D-857's stuck-group wake land in the collect step.
    pub(crate) walked_live: std::sync::atomic::AtomicUsize,
    /// Run by the detector between its two passes.
    between_passes: parking_lot::Mutex<Option<PassHook>>,
    /// Run by the collect step before it locates a fiber's wait.
    before_locate: parking_lot::Mutex<Option<PassHook>>,
}

/// A hook the detector runs between its two passes (a test's).
#[derive(Clone)]
pub(crate) struct PassHook(pub(crate) std::sync::Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for PassHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PassHook").finish_non_exhaustive()
    }
}

impl MonitorProbe {
    /// The detector started a survey of the domain at `domain`.
    pub(crate) fn surveyed(&self, domain: usize) {
        let mut surveys = self.surveys.lock();
        let count = surveys.entry(domain).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// The surveys of the domain at `domain` so far.
    pub(crate) fn surveys_of(&self, domain: usize) -> usize {
        self.surveys.lock().get(&domain).copied().unwrap_or(0)
    }

    /// The detector's two passes agreed.
    pub(crate) fn confirmed(&self) {
        self.confirmations.fetch_add(1, Ordering::SeqCst);
    }

    /// The detector reported a root of the domain at `domain`.
    pub(crate) fn reported(&self, domain: usize) {
        let mut reports = self.reports.lock();
        let count = reports.entry(domain).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// The roots of the domain at `domain` the detector reported so far.
    pub(crate) fn reports_of(&self, domain: usize) -> usize {
        self.reports.lock().get(&domain).copied().unwrap_or(0)
    }

    /// Runs the hook between the detector's passes, if any, with the
    /// probe's lock released.
    pub(crate) fn between_passes(&self) {
        let hook = self.between_passes.lock().clone();
        if let Some(hook) = hook {
            (hook.0)();
        }
    }

    /// Sets (or clears) the hook run between the detector's passes.
    pub(crate) fn set_between_passes(&self, hook: Option<PassHook>) {
        *self.between_passes.lock() = hook;
    }

    /// Runs the hook of the collect step, if any, with the probe's lock
    /// released: under the scope lock of the fiber about to be located.
    pub(crate) fn before_locate(&self) {
        let hook = self.before_locate.lock().clone();
        if let Some(hook) = hook {
            (hook.0)();
        }
    }

    /// Sets (or clears) the hook run by the collect step before it locates
    /// a fiber's wait.
    pub(crate) fn set_before_locate(&self, hook: Option<PassHook>) {
        *self.before_locate.lock() = hook;
    }

    /// A worker left a fiber frozen.
    pub(crate) fn froze(&self) {
        self.frozen.fetch_add(1, Ordering::SeqCst);
    }

    /// Worker `index` ended at a stop, its slot marked `DEAD`, and its
    /// thread is about to end: if it is the one `hold_stop_exit` names, it
    /// says so and waits for `stop_exit_release`, at most [`hang`], so that
    /// a test can make another worker push to it meanwhile.
    pub(crate) fn stop_exit(&self, index: usize) {
        if self.hold_stop_exit.load(Ordering::SeqCst) != index.saturating_add(1) {
            return;
        }
        self.stop_exit_held.store(true, Ordering::SeqCst);
        let begun = std::time::Instant::now();
        while !self.stop_exit_release.load(Ordering::SeqCst) && begun.elapsed() < hang() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A control block was written at `fcb`.
    pub(crate) fn created(&self, fcb: *mut ExecCtx) {
        self.retired.lock().remove(&fcb.addr());
    }

    /// The control block at `fcb` was dropped.
    pub(crate) fn retired(&self, fcb: *mut ExecCtx) {
        self.retired.lock().insert(fcb.addr());
    }

    /// The monitor is about to request a yield from `fcb`.
    pub(crate) fn before_request(&self, fcb: *mut ExecCtx) {
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        if self.retired.lock().contains(&fcb.addr()) {
            self.stale.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Whether `me`'s runtime has set its stop flag or halts.
pub(crate) fn stopping(me: &ExecCtx) -> bool {
    me.sched.runtime().is_some_and(|shared| shared.stopping())
}

/// Whether `shared` has set its stop flag or halts.
pub(crate) fn runtime_stopping(shared: &super::Shared) -> bool {
    shared.stopping()
}

/// Keeps `shared`'s stop from giving up on the fibers waiting on the
/// blocking pool ([`MonitorProbe::hold_pool`]).
pub(crate) fn hold_pool(shared: &super::Shared) {
    shared.probe.hold_pool.store(true, Ordering::SeqCst);
}

/// The function-entry check generated code performs; returns whether the
/// slow path raised (a stack overflow, a deadline), after which generated
/// code would run its cleanup pad and return (D-733).
pub(crate) fn entry_check(ctx: &ExecCtx) -> bool {
    if stack_position() < ctx.stack_limit.load(Ordering::Relaxed) {
        // SAFETY: `ctx` is the running fiber's.
        unsafe { cheby_rt_stack_slow(std::ptr::from_ref(ctx).cast_mut(), 0) };
    }
    crate::unwind::flag::get(ctx) != crate::abi::unwind_flag::NONE
}

/// D-872, review finding CONC-W2-1 of gate 2: no
/// runtime test bounds how long something may take with a literal number
/// of seconds that ignores `CHEBY_HARNESS_SCALE`. The scan matches the
/// three forms such a bound took: a `recv_timeout` that calls a missing
/// outcome a hang, an asserted upper bound on an elapsed time, and a
/// deadline after which a test gives up. Far deadlines that a test arms
/// and never waits for (`Instant::now() + Duration::from_secs(3600)` in
/// the timer tests) are not bounds and do not match.
#[test]
fn hang_bounds_go_through_scaled() {
    use std::path::{Path, PathBuf};

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 50, "{} files", files.len());
    let unscaled = |line: &str| {
        line.contains("recv_timeout(Duration::from_secs(")
            || (line.contains("assert!(") && line.contains("< Duration::from_secs("))
            || (line.contains("assert!(") && line.contains("< Duration::from_millis("))
            || line.contains("let deadline = Instant::now() + Duration::from_secs(")
    };
    let mut found = Vec::new();
    for file in &files {
        let relative = file.strip_prefix(&root).unwrap();
        // This test names the forms it looks for.
        if relative == Path::new("sched/testing.rs") {
            continue;
        }
        let source = std::fs::read_to_string(file).unwrap();
        for (number, line) in source.lines().enumerate() {
            if unscaled(line) {
                found.push(format!(
                    "{}:{}: {}",
                    relative.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "time bounds that do not go through `scaled`:\n{}",
        found.join("\n")
    );
}
