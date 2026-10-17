//! The deadlock detector's tests (D-724, D-856 to D-860, plan §3 DLr): the
//! `deadlock_*` tests, and `wait_site_cleared_and_read_concurrently`
//! (critique RISK-1).
//!
//! Rust-level fibers on runtimes with detection on
//! (`testing::tuned_detecting`), through the harness of
//! `fiber_ops::detached_tests`; real waits go through the kernels, which
//! record their wait sites. No JIT code runs here, so every published wait
//! frame is 0 and every line says `<unknown location>`; the locations
//! themselves are tested by `locate_*` and `cheby_jit`'s `code_map_*`. Waits are
//! bounded with `testing::hang` and `testing::scaled`; a test about time
//! asserts lower bounds, except the two latency bounds plan §3 names, which
//! are scaled.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::deadlock::{DomainCounts, HEADER, tests_support::render_lines};
use super::fiber::{FiberSched, WaitOp};
use super::testing::{self, Mailbox, PassHook};
use super::timer::{TimerTarget, Timers};
use super::{AfterSwitch, DetachedGroup, Runtime, RuntimeConfig, Tuning, park};
use crate::abi::{PanicKind, Word};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, MIN_STACK_SIZE, RunConfig};
use crate::fiber_ops::detached_tests::{
    self, close_scope, detach, group_of, hold, new_channel, open_scope, receive_on, release,
    send_on, spawn_into, unwinding, yield_until,
};
use crate::fiber_ops::join::cheby_fiber_sys_join;
use crate::fiber_ops::scope::ScopeState;
use crate::fiber_ops::select::{
    cheby_selector_sys_add_after, cheby_selector_sys_new, cheby_selector_sys_wait,
};
use crate::generic::tests::closure;
use crate::pool::Pool;
use crate::sched::blocking::{Hooks, run_blocking_in};
use crate::values;

/// The thread counts of the tests that run at several.
const THREADS: [usize; 3] = [1, 2, 4];

/// A report line of a fiber blocked in `op`, with no location.
fn line(op: &str) -> String {
    format!("  blocked in {op} at <unknown location>\n")
}

/// D-858's report with `lines`, in the order given.
fn report(lines: &[String]) -> String {
    let mut text = HEADER.to_owned();
    for line in lines {
        text.push_str(line);
    }
    text
}

/// The report of one fiber blocked in `channel::receive`.
fn one_receive() -> String {
    report(&[line("channel::receive")])
}

/// A runtime of `threads` workers with detection on, as every host has.
fn detecting(threads: usize) -> Runtime {
    testing::tuned_detecting(threads, false, true)
}

/// [`detecting`] whose spawned fibers have small stacks, for tests with
/// thousands of fibers.
fn detecting_small(threads: usize) -> Runtime {
    Runtime::tuned(
        &RuntimeConfig {
            threads: Some(threads),
            fiber_stack: 256 << 10,
            ..RuntimeConfig::default()
        },
        Tuning {
            stress_migrate: Some(false),
            rescue: true,
            detect_deadlocks: true,
        },
    )
    .unwrap()
}

/// An entry of `mode` with the harness's trampoline and `domain`.
fn config(domain: &Arc<Domain>, mode: EntryMode) -> EntryConfig {
    EntryConfig {
        mode,
        ..detached_tests::config(domain)
    }
}

/// The probe's key of `domain`.
fn key(domain: &Arc<Domain>) -> usize {
    Arc::as_ptr(domain).addr()
}

/// Waits on the host until `ready` holds, or panics after
/// [`testing::hang`].
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "the host waited too long"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// The root's outcome, or a panic after [`testing::hang`].
fn outcome_of(root: &mpsc::Receiver<EntryOutcome>) -> EntryOutcome {
    root.recv_timeout(testing::hang())
        .expect("the root did not end")
}

fn ctx_ptr(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// Receives on a new channel whose sender this fiber holds: blocks for good
/// unless the fiber is cancelled. Releases both ends and returns `Nil`, or
/// 0 with the flag set when cancelled.
fn receive_forever(me: &ExecCtx) -> Word {
    let (tx, rx) = new_channel(me, 0);
    let got = receive_on(me, rx);
    release(tx);
    release(rx);
    match got {
        Some(Ok(word) | Err(word)) => {
            release(word);
            values::nil()
        }
        None => 0,
    }
}

/// The two ends of a new channel of capacity 0, owned and marked shared
/// (D-705): their counts change on several fibers' threads at once, as a
/// spawn's captures do in generated code; the harness's Rust closures do
/// not mark them.
fn shared_channel(me: &ExecCtx) -> (Word, Word) {
    let (tx, rx) = new_channel(me, 0);
    // SAFETY: both ends are new, live, and only this fiber holds them.
    unsafe {
        crate::rc::mark_shared(tx);
        crate::rc::mark_shared(rx);
    }
    (tx, rx)
}

/// A `select` whose only arm is a timer of `nanoseconds`: whether it fired
/// (false when the fiber was cancelled meanwhile).
fn sleep_select(me: &ExecCtx, nanoseconds: i64) -> bool {
    let ctx = ctx_ptr(me);
    let apply = closure(&[values::int_word(0)]);
    // SAFETY: the running fiber's context, live words borrowed.
    let selector = unsafe {
        let empty = cheby_selector_sys_new(ctx);
        let timed = cheby_selector_sys_add_after(ctx, empty, 0, nanoseconds, apply);
        release(empty);
        timed
    };
    release(apply);
    // SAFETY: as above, with a live selector borrowed.
    let chosen = unsafe { cheby_selector_sys_wait(ctx, selector) };
    release(selector);
    if unwinding(me) {
        return false;
    }
    release(chosen);
    true
}

/// A gate a blocking-pool job waits at until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    entered: AtomicBool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Opens the gate.
    fn open(&self) {
        *self.open.lock() = true;
        self.opened.notify_all();
    }

    /// Whether a job waits at the gate, or passed it.
    fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Waits until the gate is open, at most [`testing::hang`].
    fn pass(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let mut open = self.open.lock();
        while !*open && started.elapsed() < testing::hang() {
            self.opened.wait_for(&mut open, Duration::from_millis(50));
        }
    }
}

/// A blocking-pool job on `pool` that waits at `gate`, then returns `Nil`
/// (also when the fiber was cancelled meanwhile: its result is dropped).
fn gated_job(me: &ExecCtx, pool: &'static Pool, gate: Arc<Gate>) -> Word {
    let _ = run_blocking_in(pool, me, move || gate.pass(), &Hooks::default());
    values::nil()
}

// ---------------------------------------------------------------------
// The actions (D-859)
// ---------------------------------------------------------------------

/// D-724, D-859: a `cheby run` root parked on a channel nobody else holds is
/// reported to its host as `Deadlocked`, nothing unwinds, and the host
/// completes the halt.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_parked_on_a_channel_gets_the_run_action() {
    for threads in THREADS {
        let runtime = detecting(threads);
        let domain = Domain::new();
        let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), |me| {
            receive_forever(me)
        });
        assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
        runtime.halt();
    }
}

/// D-724, D-859: a test's root parked on a channel is cancelled through its
/// base scope, unwinds, releasing what it held, and its catch point reports
/// `Deadlocked` with the report.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_parked_on_a_channel_gets_the_test_action() {
    for threads in THREADS {
        let runtime = detecting(threads);
        let domain = Domain::new();
        let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Test), |me| {
            receive_forever(me)
        });
        assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// D-859: a root is reported once, however many ticks follow.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_root_is_reported_once() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), |me| {
        receive_forever(me)
    });
    assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
    thread::sleep(testing::scaled(Duration::from_millis(100)));
    assert!(root.try_recv().is_err(), "a second outcome");
    assert_eq!(runtime.shared.probe.reports_of(key(&domain)), 1);
    runtime.halt();
}

/// `main`'s thunk for the `run_main` test: prints without a newline, then
/// blocks for good.
unsafe extern "C" fn main_prints_then_deadlocks(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the root's context.
    let me = unsafe { &*ctx };
    let text = values::string("partial output");
    // SAFETY: a live `String`, borrowed.
    unsafe { crate::io::cheby_io_print(text) };
    release(text);
    receive_forever(me)
}

/// Ends this child process with `status`, as `cheby run` ends with
/// `run_main`'s status, without the harness writing anything more.
fn exit_child(status: i32) -> ! {
    // SAFETY: `_exit` has no preconditions; the child's work is done.
    unsafe { libc::_exit(status) }
}

/// D-724, D-859, D-785, D-346: `run_main` returns 103 after a deadlock, and
/// writes the report to standard error after it flushed standard output;
/// no stats and no leak report follow. Both streams go to one pipe, so the
/// order shows.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_run_main_returns_103_and_writes_the_report_after_flushing() {
    const CHILD: &str = "CHEBY_RT_TEST_DEADLOCK_RUN_MAIN";
    const NAME: &str =
        "sched::deadlock_tests::deadlock_run_main_returns_103_and_writes_the_report_after_flushing";
    if std::env::var_os(CHILD).is_some() {
        let config = RunConfig {
            callbacks: detached_tests::callbacks(),
            domain: Domain::new(),
            threads: Some(2),
            ..RunConfig::default()
        };
        // SAFETY: a test thunk with the entry ABI.
        let status = unsafe { crate::entry::run_main(main_prints_then_deadlocks, config) };
        exit_child(status.unwrap_or(99));
    }
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut child = {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env_remove("CHEBY_RT_STATS")
            .stdin(Stdio::null())
            .stdout(writer.try_clone().unwrap())
            .stderr(writer);
        command.spawn().unwrap()
    };
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let mut output = String::new();
        reader.read_to_string(&mut output).ok();
        sender.send(output).ok();
    });
    let output = received
        .recv_timeout(testing::scaled(Duration::from_secs(20)))
        .unwrap_or_else(|_| {
            child.kill().ok();
            panic!("the child process did not end")
        });
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(103), "{output}");
    let expected = format!("partial output{}", one_receive());
    assert!(output.contains(&expected), "{output:?}");
    assert!(!output.contains("leak"), "{output:?}");
}

// ---------------------------------------------------------------------
// Pending timers and pool jobs (D-857)
// ---------------------------------------------------------------------

/// D-857: a pending timer holds detection off until it has fired; the
/// deadlock after it is then found.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_pending_timer_holds_detection_off_until_it_fires() {
    const DELAY: Duration = Duration::from_millis(100);
    let runtime = detecting(2);
    let domain = Domain::new();
    let started = Instant::now();
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), |me| {
        let nanoseconds = i64::try_from(DELAY.as_nanos()).unwrap();
        sleep_select(me, nanoseconds);
        receive_forever(me)
    });
    assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
    assert!(
        started.elapsed() >= DELAY,
        "reported before the timer fired"
    );
    runtime.halt();
}

/// D-857: a pending blocking-pool job holds detection off; once it
/// completes and its fiber ends, the root's deadlock is found.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_pending_pool_job_holds_detection_off() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let gate = Gate::new();
    let job = Arc::clone(&gate);
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), move |me| {
        detach(me, move |me| gated_job(me, pool, job));
        receive_forever(me)
    });
    wait_until(|| gate.entered());
    assert!(
        root.recv_timeout(testing::scaled(Duration::from_millis(300)))
            .is_err(),
        "reported while a pool job was pending"
    );
    gate.open();
    assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
    runtime.halt();
}

/// A timer target that records its domain's pending count while it fires.
struct Watch {
    counts: Arc<DomainCounts>,
    during: AtomicUsize,
}

impl TimerTarget for Watch {
    fn fire(&self) {
        self.during.store(self.counts.timers(), Ordering::SeqCst);
    }
}

/// D-857, survey-deadlock §2.4 (the order fix): a domain's timer count
/// falls only after the target's `fire` has returned, so never between the
/// settle and the wake; a winning cancel and a clear lower it once; a
/// stopping runtime arms nothing and counts nothing.
#[test]
fn deadlock_a_timers_count_falls_after_its_firing_returns() {
    let now = Instant::now();
    let timers = Timers::new(now);
    let counts = Arc::new(DomainCounts::default());
    let watch = Arc::new(Watch {
        counts: Arc::clone(&counts),
        during: AtomicUsize::new(usize::MAX),
    });
    let (fired, _) = timers.arm(now, watch.clone(), Some(Arc::clone(&counts)), || false);
    assert_eq!(counts.timers(), 1);
    assert_eq!(timers.fire_due(now), 1);
    assert_eq!(
        watch.during.load(Ordering::SeqCst),
        1,
        "the count fell before the firing"
    );
    assert_eq!(counts.timers(), 0);
    assert!(!fired.cancel());
    assert_eq!(counts.timers(), 0);
    let later = now + Duration::from_secs(3600);
    let (cancelled, _) = timers.arm(later, watch.clone(), Some(Arc::clone(&counts)), || false);
    assert_eq!(counts.timers(), 1);
    assert!(cancelled.cancel());
    assert!(!cancelled.cancel());
    assert_eq!(counts.timers(), 0);
    timers.arm(later, watch.clone(), Some(Arc::clone(&counts)), || false);
    timers.arm(later, watch.clone(), Some(Arc::clone(&counts)), || false);
    assert_eq!(counts.timers(), 2);
    timers.clear();
    assert_eq!(counts.timers(), 0);
    let (never, armed) = timers.arm(now, watch, Some(Arc::clone(&counts)), || true);
    assert_eq!(armed, None);
    assert_eq!(counts.timers(), 0);
    assert!(!never.cancel());
    assert_eq!(counts.timers(), 0);
}

/// D-860 (the order fix, end to end): a `select` with a short timer, 1000
/// times, beside a fiber parked for good, never yields a report, with the
/// detector surveying on every pass.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_timers_firing_during_surveys_never_yield_a_report() {
    const ROUNDS: usize = 1000;
    let runtime = detecting(2);
    runtime
        .shared
        .probe
        .eager_detection
        .store(true, Ordering::SeqCst);
    let domain = Domain::new();
    let rounds = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&rounds);
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Test), move |me| {
        let (tx, rx) = shared_channel(me);
        let scope = open_scope(me);
        let held = hold(rx);
        release(spawn_into(me, scope, move |me| {
            let got = receive_on(me, held.word());
            drop(held);
            if let Some(Ok(word) | Err(word)) = got {
                release(word);
            }
            values::nil()
        }));
        release(rx);
        for _ in 0..ROUNDS {
            if !sleep_select(me, 100_000) {
                break;
            }
            counted.fetch_add(1, Ordering::SeqCst);
        }
        release(tx);
        close_scope(me, scope);
        values::nil()
    });
    assert!(matches!(outcome_of(&root), EntryOutcome::Returned(_)));
    assert_eq!(rounds.load(Ordering::SeqCst), ROUNDS);
    let probe = &runtime.shared.probe;
    assert_eq!(probe.reports_of(key(&domain)), 0);
    assert_eq!(probe.walked_live.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// Critique RISK-2: a blocking-pool completion races the survey 1000 times
/// beside a fiber parked for good; no confirmation comes before the real
/// deadlock at the end, and the collect step never walks the stack of a
/// fiber whose resume count changed since the passes.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_near_miss_walks_no_live_stack() {
    const ROUNDS: usize = 1000;
    let runtime = detecting(2);
    runtime
        .shared
        .probe
        .eager_detection
        .store(true, Ordering::SeqCst);
    let domain = Domain::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let rounds = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&rounds);
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Test), move |me| {
        let (tx, rx) = shared_channel(me);
        let scope = open_scope(me);
        let held = hold(rx);
        release(spawn_into(me, scope, move |me| {
            let got = receive_on(me, held.word());
            drop(held);
            match got {
                Some(Ok(word) | Err(word)) => {
                    release(word);
                    values::nil()
                }
                None => 0,
            }
        }));
        release(rx);
        for _ in 0..ROUNDS {
            if run_blocking_in(pool, me, || (), &Hooks::default()).is_err() {
                break;
            }
            counted.fetch_add(1, Ordering::SeqCst);
        }
        let word = receive_forever(me);
        close_scope(me, scope);
        release(tx);
        word
    });
    assert_eq!(
        outcome_of(&root),
        EntryOutcome::Deadlocked(report(&[
            line("channel::receive"),
            line("channel::receive")
        ]))
    );
    assert_eq!(rounds.load(Ordering::SeqCst), ROUNDS);
    let probe = &runtime.shared.probe;
    assert_eq!(probe.walked_live.load(Ordering::SeqCst), 0);
    assert_eq!(probe.confirmations.load(Ordering::SeqCst), 1);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

// ---------------------------------------------------------------------
// The survey (D-860)
// ---------------------------------------------------------------------

/// D-857, D-860: of two domains on one runtime, the deadlocked one is
/// reported within `scaled(1 s)` while the other keeps running; the busy
/// one is never reported.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_of_two_domains_only_the_deadlocked_one_is_reported() {
    let runtime = detecting(2);
    let (stuck, busy) = (Domain::new(), Domain::new());
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let busy_root =
        testing::start_root_with(&runtime, &config(&busy, EntryMode::Test), move |me| {
            let started = Instant::now();
            while !flag.load(Ordering::SeqCst) && started.elapsed() < testing::hang() {
                park(me, AfterSwitch::Requeue);
            }
            values::nil()
        });
    let started = Instant::now();
    let stuck_root = testing::start_root_with(&runtime, &config(&stuck, EntryMode::Test), |me| {
        receive_forever(me)
    });
    assert_eq!(
        outcome_of(&stuck_root),
        EntryOutcome::Deadlocked(one_receive())
    );
    let took = started.elapsed();
    assert!(
        took < testing::scaled(Duration::from_secs(1)),
        "found after {took:?}"
    );
    stop.store(true, Ordering::SeqCst);
    assert!(matches!(outcome_of(&busy_root), EntryOutcome::Returned(_)));
    let probe = &runtime.shared.probe;
    assert_eq!(probe.reports_of(key(&busy)), 0);
    assert_eq!(probe.reports_of(key(&stuck)), 1);
    runtime.shutdown();
    assert_eq!(stuck.live_objects(), 0, "objects leaked");
    assert_eq!(busy.live_objects(), 0, "objects leaked");
}

/// D-860: a fiber resumed between the two passes (its resume count
/// changed, whatever its state) cancels the confirmation; the deadlock is
/// confirmed by a later survey.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_resume_between_the_passes_cancels_the_confirmation() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let mailbox = Arc::new(Mailbox::default());
    let hooked = Arc::new(AtomicBool::new(false));
    {
        let (mailbox, hooked) = (Arc::clone(&mailbox), Arc::clone(&hooked));
        runtime
            .shared
            .probe
            .set_between_passes(Some(PassHook(Arc::new(move || {
                if !hooked.swap(true, Ordering::SeqCst) {
                    mailbox.send(1, false);
                }
            }))));
    }
    let waiter = Arc::clone(&mailbox);
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), move |me| {
        let scope = open_scope(me);
        release(spawn_into(me, scope, move |me| {
            // A wait the detector names, on the test mailbox.
            loop {
                me.sched.note_wait(WaitOp::Receive, 0);
                waiter.receive(me);
                me.sched.clear_wait();
            }
        }));
        close_scope(me, scope);
        0
    });
    assert_eq!(
        outcome_of(&root),
        EntryOutcome::Deadlocked(report(&[line("channel::receive"), line("fiber::scope")]))
    );
    assert!(hooked.load(Ordering::SeqCst));
    assert_eq!(runtime.shared.probe.confirmations.load(Ordering::SeqCst), 1);
    runtime.shared.probe.set_between_passes(None);
    runtime.halt();
}

/// D-860: once a halt has started nothing is reported, not even a
/// deadlock the passes confirmed.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_halting_turns_detection_off() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let shared = Arc::clone(&runtime.shared);
    runtime
        .shared
        .probe
        .set_between_passes(Some(PassHook(Arc::new(move || shared.begin_halt()))));
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), |me| {
        receive_forever(me)
    });
    let probe = &runtime.shared.probe;
    wait_until(|| probe.confirmations.load(Ordering::SeqCst) > 0);
    assert!(
        root.recv_timeout(testing::scaled(Duration::from_millis(300)))
            .is_err(),
        "reported during a halt"
    );
    assert_eq!(probe.reports_of(key(&domain)), 0);
    assert_eq!(probe.confirmations.load(Ordering::SeqCst), 1);
    probe.set_between_passes(None);
    runtime.halt();
}

/// §10.4, D-857: a root that finished while its detached fibers are
/// blocked is not deadlocked.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_finished_root_with_blocked_detached_fibers_is_not_reported() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let root = testing::start_root_in(&runtime, &config(&domain, EntryMode::Test), &group, |me| {
        detach(me, receive_forever);
        let group = group_of(me);
        yield_until(me, || group.parked() == 1);
        values::nil()
    });
    assert!(matches!(outcome_of(&root), EntryOutcome::Returned(_)));
    thread::sleep(testing::scaled(Duration::from_millis(200)));
    assert_eq!(runtime.shared.probe.reports_of(key(&domain)), 0);
    assert_eq!(group.parked(), 1);
    assert_eq!(runtime.teardown(&group).waited, 1);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// One fiber blocked in each of `kind`'s waits, spawned in the order of
/// `order`: 0 a send, 1 a receive, 2 a join of a fiber blocked in a
/// receive, 3 a `select` with no arm; the root waits at its scope's end.
/// Returns the report.
fn report_of_spawn_order(order: &'static [usize]) -> String {
    let runtime = detecting(2);
    let domain = Domain::new();
    let root = testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), move |me| {
        let (tx, rx) = shared_channel(me);
        let scope = open_scope(me);
        for &kind in order {
            match kind {
                0 => {
                    let held = hold(tx);
                    release(spawn_into(me, scope, move |me| {
                        let _ = send_on(me, held.word(), values::int_word(1));
                        drop(held);
                        0
                    }));
                }
                1 => release(spawn_into(me, scope, receive_forever)),
                2 => {
                    let joined = spawn_into(me, scope, receive_forever);
                    release(spawn_into(me, scope, move |me| {
                        // SAFETY: the running fiber's context and a live
                        // `Fiber` handle.
                        unsafe { cheby_fiber_sys_join(ctx_ptr(me), joined) };
                        0
                    }));
                }
                _ => release(spawn_into(me, scope, |me| {
                    let ctx = ctx_ptr(me);
                    // SAFETY: the running fiber's context; the selector is
                    // new and owned.
                    unsafe {
                        let selector = cheby_selector_sys_new(ctx);
                        cheby_selector_sys_wait(ctx, selector);
                    }
                    0
                })),
            }
        }
        close_scope(me, scope);
        release(rx);
        0
    });
    let outcome = outcome_of(&root);
    runtime.halt();
    let EntryOutcome::Deadlocked(report) = outcome else {
        panic!("not a deadlock: {outcome:?}");
    };
    report
}

/// D-858: the report's lines are sorted by location, then operation, so
/// their order does not depend on the order the fibers were spawned in.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_the_reports_order_does_not_depend_on_spawn_order() {
    let expected = report(&[
        line("channel::receive"),
        line("channel::receive"),
        line("channel::send"),
        line("fiber::join"),
        line("fiber::scope"),
        line("selector::select"),
    ]);
    assert_eq!(report_of_spawn_order(&[0, 1, 2, 3]), expected);
    assert_eq!(report_of_spawn_order(&[3, 2, 1, 0]), expected);
    assert_eq!(report_of_spawn_order(&[2, 0, 3, 1]), expected);
}

/// D-858: path bytes, then line and column as numbers, then operation;
/// `<unknown location>` lines last; equal lines kept.
#[test]
fn deadlock_report_lines_are_sorted_as_d858_says() {
    let lines = [
        (None, WaitOp::Select),
        (Some(("src/main.cheby", 10, 3)), WaitOp::Receive),
        (Some(("src/b.cheby", 2, 1)), WaitOp::Send),
        (Some(("src/main.cheby", 9, 30)), WaitOp::Join),
        (Some(("src/main.cheby", 10, 3)), WaitOp::Receive),
        (None, WaitOp::Receive),
        (Some(("src/main.cheby", 10, 12)), WaitOp::Scope),
        (Some(("<repl:3>", 1, 1)), WaitOp::Timeout),
        (Some(("src/main.cheby", 10, 3)), WaitOp::Join),
    ];
    let expected = concat!(
        "fatal error: all fibers are blocked\n",
        "  blocked in fiber::timeout at <repl:3>:1:1\n",
        "  blocked in channel::send at src/b.cheby:2:1\n",
        "  blocked in fiber::join at src/main.cheby:9:30\n",
        "  blocked in channel::receive at src/main.cheby:10:3\n",
        "  blocked in channel::receive at src/main.cheby:10:3\n",
        "  blocked in fiber::join at src/main.cheby:10:3\n",
        "  blocked in fiber::scope at src/main.cheby:10:12\n",
        "  blocked in channel::receive at <unknown location>\n",
        "  blocked in selector::select at <unknown location>\n",
    );
    assert_eq!(render_lines(&lines), expected);
    let mut reversed = lines;
    reversed.reverse();
    assert_eq!(render_lines(&reversed), expected);
}

// ---------------------------------------------------------------------
// The REPL (D-857, D-859)
// ---------------------------------------------------------------------

/// D-859: in the REPL only the entry unwinds; the session's detached
/// fibers of the current group are listed in the report and keep waiting.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_repl_report_lists_the_sessions_detached_fibers() {
    let runtime = detecting(2);
    let domain = Domain::new();
    let repl = config(&domain, EntryMode::Repl);
    let group = DetachedGroup::new();
    let first = testing::start_root_in(&runtime, &repl, &group, |me| {
        detach(me, receive_forever);
        let group = group_of(me);
        yield_until(me, || group.parked() == 1);
        values::nil()
    });
    assert!(matches!(outcome_of(&first), EntryOutcome::Returned(_)));
    let second = testing::start_root_in(&runtime, &repl, &group, receive_forever);
    assert_eq!(
        outcome_of(&second),
        EntryOutcome::Deadlocked(report(&[
            line("channel::receive"),
            line("channel::receive")
        ]))
    );
    assert_eq!(group.parked(), 1, "the session's fiber stopped waiting");
    assert_eq!(runtime.teardown(&group).waited, 1);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-857 under plan §7.1's answer to UQ-12 (b), critique RISK-12: a
/// `:reload` whose teardown skipped one member parked on a pending pool job
/// and one blocked inline marks its group stuck; a later entry that
/// deadlocks while that job is still pending is reported, with no line for
/// a stuck fiber; the stuck job's completion leaves the domain's pool count
/// as it was, so a pending job of a later entry still holds detection off;
/// the session's end changes no count.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_after_a_stuck_group_in_the_repl() {
    let runtime = detecting(3);
    let domain = Domain::new();
    let repl = config(&domain, EntryMode::Repl);
    let counts = runtime.shared.domains.of(&domain);
    let pool = Pool::private(2, Duration::from_secs(10));
    let refusing = Pool::private(1, Duration::from_secs(10));
    refusing.refuse_threads(true);
    let (parked, inline, later) = (Gate::new(), Gate::new(), Gate::new());
    let stuck = DetachedGroup::new();
    let (a, b) = (Arc::clone(&parked), Arc::clone(&inline));
    let first = testing::start_root_in(&runtime, &repl, &stuck, move |me| {
        detach(me, move |me| gated_job(me, pool, a));
        detach(me, move |me| gated_job(me, refusing, b));
        values::nil()
    });
    assert!(matches!(outcome_of(&first), EntryOutcome::Returned(_)));
    wait_until(|| parked.entered() && inline.entered());
    assert_eq!(counts.pool(), 2);
    // `:reload`: both members are skipped (D-786, D-804), and the group is
    // stuck, its jobs out of the domain's count.
    let skipped = runtime.teardown(&stuck);
    assert_eq!(skipped.skipped, 2);
    assert!(stuck.stuck());
    assert_eq!(counts.pool(), 0);
    // An entry in the fresh group deadlocks: reported, with no line for
    // either stuck fiber, while the inline one occupies a worker.
    let current = DetachedGroup::new();
    let second = testing::start_root_in(&runtime, &repl, &current, receive_forever);
    assert_eq!(outcome_of(&second), EntryOutcome::Deadlocked(one_receive()));
    // The stuck job completes: its count left at the mark, never again.
    parked.open();
    wait_until(|| stuck.live() == 1);
    assert_eq!(counts.pool(), 0);
    // A later entry's pending job still holds detection off.
    let job = Arc::clone(&later);
    let third = testing::start_root_in(&runtime, &repl, &current, move |me| {
        detach(me, move |me| gated_job(me, pool, job));
        receive_forever(me)
    });
    wait_until(|| later.entered());
    assert_eq!(counts.pool(), 1);
    assert!(
        third
            .recv_timeout(testing::scaled(Duration::from_millis(300)))
            .is_err(),
        "reported while a later entry's job was pending"
    );
    later.open();
    assert_eq!(outcome_of(&third), EntryOutcome::Deadlocked(one_receive()));
    assert_eq!(counts.pool(), 0);
    // The session's end: the stuck group's second teardown changes no
    // count.
    inline.open();
    wait_until(|| stuck.live() == 0);
    runtime.teardown(&stuck);
    assert_eq!(counts.pool(), 0);
    runtime.teardown(&current);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-857, D-860 (gate-1 review MEM-W1-1, CONC-1): a stuck fiber whose call
/// returns while the collect step builds a later entry's line wakes that
/// entry's root through the channel's last `Sender`. The step re-checks
/// after the walk, sees that the root ran and gives the report up; no
/// report reaches the root, which then receives `Closed` and returns.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_a_stuck_wake_during_the_collect_step_gives_the_report_up() {
    let runtime = detecting(3);
    let domain = Domain::new();
    let repl = config(&domain, EntryMode::Repl);
    let pool = Pool::private(2, Duration::from_secs(10));
    let gate = Gate::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let stuck = DetachedGroup::new();
    let rx_slot: Arc<Mutex<Option<crate::fiber_ops::Owned>>> = Arc::new(Mutex::new(None));
    let (g, slot, gone) = (
        Arc::clone(&gate),
        Arc::clone(&rx_slot),
        Arc::clone(&dropped),
    );
    let first = testing::start_root_in(&runtime, &repl, &stuck, move |me| {
        let (tx, rx) = shared_channel(me);
        *slot.lock() = Some(hold(rx));
        let sender = hold(tx);
        detach(me, move |me| {
            let word = gated_job(me, pool, g);
            // The stuck call returned: the channel's last `Sender` goes,
            // which wakes the receiving root at once.
            drop(sender);
            gone.store(true, Ordering::SeqCst);
            word
        });
        release(tx);
        release(rx);
        values::nil()
    });
    assert!(matches!(outcome_of(&first), EntryOutcome::Returned(_)));
    wait_until(|| gate.entered());
    // `:reload`: the member is skipped, and the group is stuck.
    let skipped = runtime.teardown(&stuck);
    assert_eq!(skipped.skipped, 1);
    assert!(stuck.stuck());
    let fired = Arc::new(AtomicBool::new(false));
    {
        let (gate, dropped, fired) = (Arc::clone(&gate), Arc::clone(&dropped), Arc::clone(&fired));
        runtime
            .shared
            .probe
            .set_before_locate(Some(PassHook(Arc::new(move || {
                if fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                // The stuck call returns while the line is being built.
                gate.open();
                wait_until(|| dropped.load(Ordering::SeqCst));
            }))));
    }
    let current = DetachedGroup::new();
    let slot = Arc::clone(&rx_slot);
    let second = testing::start_root_in(&runtime, &repl, &current, move |me| {
        let Some(rx) = slot.lock().take() else {
            return values::nil();
        };
        if let Some(Ok(word) | Err(word)) = receive_on(me, rx.word()) {
            release(word);
        }
        drop(rx);
        values::nil()
    });
    assert!(
        matches!(outcome_of(&second), EntryOutcome::Returned(_)),
        "a woken root was reported"
    );
    runtime.shared.probe.set_before_locate(None);
    assert!(fired.load(Ordering::SeqCst), "the collect step never ran");
    assert_eq!(
        runtime.shared.probe.walked_live.load(Ordering::SeqCst),
        1,
        "the re-check after the walk did not see the wake"
    );
    assert_eq!(runtime.shared.probe.reports_of(key(&domain)), 0);
    wait_until(|| stuck.live() == 0);
    runtime.teardown(&stuck);
    runtime.teardown(&current);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// The wave-2 model 8 on the real types: a stuck group's pending pool jobs leave
/// the domain's count once, whether the completion or the mark comes
/// first; a job registered after the mark is never counted; marking again
/// changes nothing.
#[test]
fn deadlock_a_stuck_groups_jobs_leave_the_count_once() {
    let counts = Arc::new(DomainCounts::default());
    let group = DetachedGroup::new();
    let done_first = counts.pool_submitted();
    group.track_job(&done_first);
    let pending = counts.pool_submitted();
    group.track_job(&pending);
    assert_eq!(counts.pool(), 2);
    assert!(done_first.release(), "the completion came first");
    assert_eq!(counts.pool(), 1);
    group.mark_stuck();
    assert_eq!(counts.pool(), 0);
    assert!(!pending.release(), "the mark released it already");
    assert!(!done_first.release());
    group.mark_stuck();
    assert_eq!(counts.pool(), 0);
    let late = counts.pool_submitted();
    group.track_job(&late);
    assert_eq!(counts.pool(), 0, "a stuck group's new job was counted");
    assert!(!late.release());
    // The race, from two threads: exactly one release lowers the count.
    for _ in 0..1000 {
        let counts = Arc::new(DomainCounts::default());
        let group = DetachedGroup::new();
        let job = counts.pool_submitted();
        group.track_job(&job);
        let completion = {
            let job = Arc::clone(&job);
            thread::spawn(move || job.release())
        };
        group.mark_stuck();
        completion.join().unwrap();
        assert_eq!(counts.pool(), 0);
        assert!(!job.release());
    }
}

/// Gate-1 review ROB1-1 (D-860: tracking a pool job is O(1) at its
/// submission): tracking N jobs that are all pending at once examines
/// O(N) tokens in all, not O(N^2), and finished jobs are forgotten, so
/// the list stays within twice the live ones plus a constant.
#[test]
fn deadlock_tracking_many_pending_pool_jobs_costs_linear_work() {
    const JOBS: usize = 20_000;
    let counts = Arc::new(DomainCounts::default());
    let group = DetachedGroup::new();
    let pending: Vec<_> = (0..JOBS)
        .map(|_| {
            let job = counts.pool_submitted();
            group.track_job(&job);
            job
        })
        .collect();
    let (tracked, examined) = group.tracked_jobs();
    assert_eq!(tracked, JOBS);
    assert!(
        examined <= JOBS.saturating_mul(4),
        "tracking {JOBS} pending jobs examined {examined} tokens"
    );
    // Every job completes; later jobs come and go one at a time.
    for job in &pending {
        assert!(job.release());
    }
    drop(pending);
    for _ in 0..JOBS {
        let job = counts.pool_submitted();
        group.track_job(&job);
        assert!(job.release());
    }
    let (tracked, examined) = group.tracked_jobs();
    assert!(tracked <= 32, "{tracked} finished jobs are still tracked");
    assert!(
        examined <= JOBS.saturating_mul(8),
        "tracking {} jobs examined {examined} tokens",
        JOBS.saturating_mul(2)
    );
    assert_eq!(counts.pool(), 0);
    group.mark_stuck();
    assert_eq!(counts.pool(), 0);
}

// ---------------------------------------------------------------------
// Load (critique RISK-10) and the root's base frame (critique C5, RISK-3)
// ---------------------------------------------------------------------

/// Critique RISK-10: 10 000 fibers parked on a channel beside a root that
/// loops on a 1 ms `select` timer cost a bounded number of surveys, and a
/// timer of a second domain fires within its scaled bound meanwhile.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_many_parked_fibers_beside_a_timer_loop_cost_bounded_surveys() {
    const FIBERS: usize = 10_000;
    const TICKS: usize = 300;
    const OTHER: Duration = Duration::from_millis(50);
    let runtime = detecting_small(4);
    let (busy, other) = (Domain::new(), Domain::new());
    let ticked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&ticked);
    let loops = testing::start_root_with(&runtime, &config(&busy, EntryMode::Test), move |me| {
        let (tx, rx) = shared_channel(me);
        let scope = open_scope(me);
        for _ in 0..FIBERS {
            let held = hold(rx);
            release(spawn_into(me, scope, move |me| {
                let got = receive_on(me, held.word());
                drop(held);
                if let Some(Ok(word) | Err(word)) = got {
                    release(word);
                }
                values::nil()
            }));
        }
        release(rx);
        for _ in 0..TICKS {
            if !sleep_select(me, 1_000_000) {
                break;
            }
            counted.fetch_add(1, Ordering::SeqCst);
        }
        // The receivers' channel closes: each returns.
        release(tx);
        close_scope(me, scope);
        values::nil()
    });
    wait_until(|| ticked.load(Ordering::SeqCst) > 0);
    let started = Instant::now();
    let timer = testing::start_root_with(&runtime, &config(&other, EntryMode::Test), |me| {
        let nanoseconds = i64::try_from(OTHER.as_nanos()).unwrap();
        sleep_select(me, nanoseconds);
        values::nil()
    });
    assert!(matches!(outcome_of(&timer), EntryOutcome::Returned(_)));
    let took = started.elapsed();
    assert!(took >= OTHER);
    assert!(
        took < testing::scaled(Duration::from_secs(1)),
        "the second domain's timer took {took:?}"
    );
    assert!(matches!(outcome_of(&loops), EntryOutcome::Returned(_)));
    assert_eq!(ticked.load(Ordering::SeqCst), TICKS);
    let probe = &runtime.shared.probe;
    let surveys = probe.surveys_of(key(&busy));
    assert!(
        surveys <= TICKS / 10,
        "{surveys} surveys of the busy domain"
    );
    assert_eq!(probe.reports_of(key(&busy)), 0);
    runtime.shutdown();
    assert_eq!(
        busy.live_objects(),
        0,
        "objects leaked: {:?}",
        crate::alloc::leaked_kinds_line(busy.live_kinds(), busy.live_objects()),
    );
    assert_eq!(other.live_objects(), 0, "objects leaked");
}

/// Runs `body` as a root of `mode` and checks the root's base frame: the
/// outcome `expect` accepts arrives after the root's base scope ended
/// under its lock, and the root unregisters after it (critique C5, RISK-3).
fn root_end_case(
    mode: EntryMode,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
    expect: impl Fn(&EntryOutcome) -> bool,
) {
    let runtime = detecting(2);
    let domain = Domain::new();
    let seen: Arc<Mutex<Option<Arc<ScopeState>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&seen);
    let root = testing::start_root_with(&runtime, &config(&domain, mode), move |me| {
        *slot.lock() = me.conc.spawn_scope();
        body(me)
    });
    let outcome = outcome_of(&root);
    assert!(expect(&outcome), "{outcome:?}");
    let base = seen.lock().take().expect("the root had no base scope");
    assert!(base.is_root_base());
    assert!(base.ended(), "the root reported before its base ended");
    wait_until(|| runtime.shared.roots.lock().is_empty());
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// Critique C5: a `cheby run` root that returns ends its base first.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_end_returned() {
    root_end_case(
        EntryMode::Run,
        |_| 0,
        |outcome| *outcome == EntryOutcome::Returned(0),
    );
}

/// Critique C5: a test's root that panics ends its base first.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_end_panicked() {
    root_end_case(
        EntryMode::Test,
        |me| {
            let (message, location) = (values::string("boom"), values::string("t.cheby:1:1"));
            // SAFETY: the running fiber's context and live `String`s.
            unsafe {
                crate::panic::cheby_rt_panic(
                    ctx_ptr(me),
                    PanicKind::Explicit.code(),
                    message,
                    location,
                );
            }
            release(message);
            release(location);
            0
        },
        |outcome| matches!(outcome, EntryOutcome::Panicked(report) if report.message == "boom"),
    );
}

/// Critique C5, RISK-3: a test's root that deadlocks unwinds through its
/// base scope's cancellation and ends its base before it reports.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_end_deadlocked() {
    root_end_case(EntryMode::Test, receive_forever, |outcome| {
        *outcome == EntryOutcome::Deadlocked(one_receive())
    });
}

/// Critique C5, RISK-3: the same for a REPL entry.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_root_end_deadlocked_in_the_repl() {
    root_end_case(EntryMode::Repl, receive_forever, |outcome| {
        *outcome == EntryOutcome::Deadlocked(one_receive())
    });
}

/// Critique RISK-3: 10 000 short roots while the monitor surveys on every
/// pass with no backoff: no survey reads a retired root's block (a stale
/// read would crash or report), and none is reported.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deadlock_short_roots_under_a_busy_monitor() {
    const ROOTS: usize = 10_000;
    const BATCH: usize = 100;
    let runtime = detecting(4);
    runtime
        .shared
        .probe
        .eager_detection
        .store(true, Ordering::SeqCst);
    let domain = Domain::new();
    let config = EntryConfig {
        stack_size: MIN_STACK_SIZE,
        ..config(&domain, EntryMode::Test)
    };
    for _ in 0..ROOTS / BATCH {
        let roots: Vec<_> = (0..BATCH)
            .map(|_| {
                testing::start_root_with(&runtime, &config, |me| {
                    park(me, AfterSwitch::Requeue);
                    park(me, AfterSwitch::Requeue);
                    values::nil()
                })
            })
            .collect();
        for root in roots {
            assert!(matches!(outcome_of(&root), EntryOutcome::Returned(_)));
        }
    }
    let probe = &runtime.shared.probe;
    assert_eq!(probe.reports_of(key(&domain)), 0);
    assert_eq!(probe.walked_live.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// Critique RISK-1: the wait site is two atomics, cleared by its fiber
/// while the detector reads it: a read sees no site, or the site's
/// operation with its frame or with 0, never anything else.
#[test]
fn wait_site_cleared_and_read_concurrently() {
    const ITERATIONS: usize = 100_000;
    const FRAME: usize = 0x7f00_1000;
    let sched = Arc::new(FiberSched::isolated());
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (sched, done) = (Arc::clone(&sched), Arc::clone(&done));
        thread::spawn(move || {
            for _ in 0..ITERATIONS {
                sched.note_wait(WaitOp::Receive, FRAME);
                sched.clear_wait();
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    while !done.load(Ordering::SeqCst) {
        match sched.wait_site() {
            None | Some((WaitOp::Receive, FRAME | 0)) => {}
            Some(other) => panic!("a torn wait site: {other:?}"),
        }
    }
    writer.join().unwrap();
    assert_eq!(sched.wait_site(), None);
}
