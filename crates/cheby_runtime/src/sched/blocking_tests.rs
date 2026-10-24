//! Tests of the detached completion (D-1065, `sched/blocking.rs`,
//! `run_blocking_detached`, `run_blocking_claiming`): a cancellation is
//! delivered at once while the job runs on, its result is dropped on its
//! pool thread, its pool token and the runtime's pool count are released
//! when the fiber abandons it (D-1165), so it holds
//! no stop and no deadlock report back, and the paths `run_blocking`
//! shares (the entry check, a refused thread, no runtime). Every test is
//! named `pool_detached_…`; `run_blocking`'s own tests, whose wait D-1065
//! keeps, are `pool::tests`' (`pool_a_cancellation_during_a_job_waits_for_it_and_drops_the_result`
//! among them, unchanged).
//!
//! Rust actions run as fibers through the harness of
//! `fiber_ops::detached_tests`, on private pools. A test that waits ends
//! through a gate, never by hanging; time is asserted only as a lower
//! bound, or as "before the gate opened".

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::blocking::{
    Hooks, Probe, holds_stop, run_blocking_claiming_in, run_blocking_detached_in,
};
use super::deadlock::HEADER;
use super::testing;
use super::{AfterSwitch, DetachedGroup, FiberRef, Runtime, begin_park, park, wake};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome};
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::fiber_ops::detached_tests::{
    self, config, detach, new_channel, receive_on, release, runtime, unwinding,
};
use crate::pool::{Pool, THREAD_NAME};
use crate::values;

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

/// A gate a job waits at until the test opens it.
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

    fn open(&self) {
        *self.open.lock() = true;
        self.opened.notify_all();
    }

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

/// A job's result that records its drop and the thread that dropped it.
struct Recorded {
    drops: Arc<AtomicUsize>,
    on_pool: Arc<AtomicBool>,
}

impl Drop for Recorded {
    fn drop(&mut self) {
        let pool = thread::current().name() == Some(THREAD_NAME);
        self.on_pool.store(pool, Ordering::SeqCst);
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// What a test reads of one detached wait.
#[derive(Default)]
struct Seen {
    /// 1: returned the result; 2: returned `Cancelled`.
    outcome: AtomicUsize,
    drops: Arc<AtomicUsize>,
    on_pool: Arc<AtomicBool>,
}

const RETURNED: usize = 1;
const CANCELLED: usize = 2;

impl Seen {
    fn outcome(&self) -> usize {
        self.outcome.load(Ordering::SeqCst)
    }

    /// A detached wait of `me` on `pool` for a job that passes `gate`,
    /// recorded here.
    fn wait(self: &Arc<Self>, me: &ExecCtx, pool: &'static Pool, gate: Arc<Gate>, hooks: &Hooks) {
        let (drops, on_pool) = (Arc::clone(&self.drops), Arc::clone(&self.on_pool));
        let result = run_blocking_detached_in(
            pool,
            me,
            move || {
                gate.pass();
                Recorded { drops, on_pool }
            },
            hooks,
        );
        let outcome = match result {
            Ok(_) => RETURNED,
            Err(Cancelled) => CANCELLED,
        };
        drop(result);
        self.outcome.store(outcome, Ordering::SeqCst);
    }
}

/// D-1065: with no cancellation, the completion claims the wait, clears
/// the mark, wakes the fiber, and the fiber takes the result, as with
/// `run_blocking`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_job_returns_its_result() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let probe = Arc::new(Probe::default());
    let seen = Arc::new(Seen::default());
    let gate = Gate::new();
    gate.open();
    let (s, g, p) = (Arc::clone(&seen), Arc::clone(&gate), Arc::clone(&probe));
    let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        s.wait(me, pool, g, &Hooks { probe: Some(p) });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    assert_eq!(seen.outcome(), RETURNED);
    assert_eq!(seen.drops.load(Ordering::SeqCst), 1);
    assert!(
        !seen.on_pool.load(Ordering::SeqCst),
        "the fiber's result was dropped on the pool"
    );
    assert_eq!(probe.woke.load(Ordering::SeqCst), 1);
    assert_eq!(probe.abandoned.load(Ordering::SeqCst), 0);
    assert!(
        !probe.marked_at_wake.load(Ordering::SeqCst),
        "woken with the mark set"
    );
    // The completion lowers the runtime's count only after its wake
    // (`blocking::leave`, D-815: the fiber is queued before the count
    // falls), so the root may report while the pool thread has still to
    // lower it: wait for it rather than read it once (wave 5's amd64 gate
    // read 1 under emulation).
    wait_until(|| runtime.shared.pool_waiting.load(Ordering::SeqCst) == 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-1065, D-1165: a teardown's cancellation reaches a
/// detached fiber waiting on a job that never returns on its own: the
/// fiber unwinds at once (the teardown waits for it and skips nothing),
/// while the job still runs; its pool token and the runtime's pool count
/// were released when the fiber abandoned it, so it holds no stop
/// (`holds_stop`); once the job returns, its completion wakes nothing and
/// drops the result on its pool thread.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_cancellation_is_delivered_at_once() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let counts = runtime.shared.domains.of(&domain);
    let group = DetachedGroup::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let probe = Arc::new(Probe::default());
    let seen = Arc::new(Seen::default());
    let gate = Gate::new();
    let (s, g, p) = (Arc::clone(&seen), Arc::clone(&gate), Arc::clone(&probe));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            s.wait(me, pool, g, &Hooks { probe: Some(p) });
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| gate.entered() && group.blocked() == 1);
    assert_eq!(counts.pool(), 1, "the job is pending in its domain");
    assert!(
        holds_stop(&runtime.shared),
        "a waiting fiber holds the stop"
    );
    let report = runtime.teardown(&group);
    assert_eq!(
        (report.waited, report.skipped),
        (1, 0),
        "the teardown waited for the job, or skipped the fiber"
    );
    assert_eq!(seen.outcome(), CANCELLED);
    assert_eq!(
        seen.drops.load(Ordering::SeqCst),
        0,
        "the result was dropped before the job ended"
    );
    assert_eq!(counts.pool(), 0, "the abandoned job's token was kept");
    assert!(
        !holds_stop(&runtime.shared),
        "an abandoned job holds the stop"
    );
    assert_eq!(probe.woke.load(Ordering::SeqCst), 0);
    gate.open();
    wait_until(|| probe.abandoned.load(Ordering::SeqCst) == 1);
    assert_eq!(seen.drops.load(Ordering::SeqCst), 1);
    assert!(
        seen.on_pool.load(Ordering::SeqCst),
        "the abandoned result was not dropped on the pool thread"
    );
    assert_eq!(probe.woke.load(Ordering::SeqCst), 0, "a gone fiber woken");
    assert_eq!(probe.skipped.load(Ordering::SeqCst), 0);
    assert_eq!(counts.pool(), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-816, kept by D-1065: a fiber already targeted by a cancellation
/// starts no detached job.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_cancellation_pending_before_starts_no_job() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let ready = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(AtomicUsize::new(0));
    let (r, j, o) = (Arc::clone(&ready), Arc::clone(&ran), Arc::clone(&outcome));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            r.store(true, Ordering::SeqCst);
            let started = Instant::now();
            while cancel::check(me).is_ok() {
                assert!(started.elapsed() < testing::hang(), "never cancelled");
                park(me, AfterSwitch::Requeue);
            }
            let result = run_blocking_detached_in(
                pool,
                me,
                move || j.store(true, Ordering::SeqCst),
                &Hooks::default(),
            );
            o.store(
                if result == Err(Cancelled) {
                    CANCELLED
                } else {
                    RETURNED
                },
                Ordering::SeqCst,
            );
            values::nil()
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| ready.load(Ordering::SeqCst));
    let report = runtime.teardown(&group);
    assert_eq!(report.skipped, 0);
    assert_eq!(outcome.load(Ordering::SeqCst), CANCELLED);
    assert!(!ran.load(Ordering::SeqCst), "a job ran");
    assert_eq!(runtime.shared.pool_waiting.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-1065: a detached job still queued when its fiber is cancelled (the
/// pool's one thread is busy) never runs: the fiber abandons it at once,
/// and the pool thread that later takes it skips it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_job_cancelled_before_it_starts_never_runs() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let pool = Pool::private(1, Duration::from_secs(10));
    // The pool's one thread waits at `busy`.
    let busy = Gate::new();
    let b = Arc::clone(&busy);
    let (finished, done) = (Arc::new(AtomicBool::new(false)), mpsc::channel::<()>());
    let f = Arc::clone(&finished);
    let (done_tx, done_rx) = done;
    assert!(matches!(
        pool.submit(Box::new(move || {
            b.pass();
            f.store(true, Ordering::SeqCst);
            done_tx.send(()).unwrap();
        })),
        crate::pool::Submitted::Queued
    ));
    wait_until(|| busy.entered());
    let ran = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(AtomicUsize::new(0));
    let (r, o) = (Arc::clone(&ran), Arc::clone(&outcome));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let result = run_blocking_detached_in(
                pool,
                me,
                move || r.store(true, Ordering::SeqCst),
                &Hooks::default(),
            );
            o.store(
                if result == Err(Cancelled) {
                    CANCELLED
                } else {
                    RETURNED
                },
                Ordering::SeqCst,
            );
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1 && pool.queued() == 1);
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (1, 0));
    assert_eq!(outcome.load(Ordering::SeqCst), CANCELLED);
    busy.open();
    done_rx.recv_timeout(testing::hang()).unwrap();
    wait_until(|| pool.queued() == 0);
    assert!(finished.load(Ordering::SeqCst));
    // The skipped job was taken off the queue; give its thread a moment to
    // have run it, had it run it.
    thread::sleep(Duration::from_millis(50));
    assert!(!ran.load(Ordering::SeqCst), "an abandoned job ran");
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-817: when the system refuses a pool thread and none runs, the
/// detached job runs inline on the fiber's worker, with the mark, and its
/// claim holds: the fiber returns its result.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_refused_thread_runs_the_job_inline() {
    let runtime = runtime(1);
    let domain = Domain::new();
    let pool = Pool::private(1, Duration::from_secs(10));
    pool.refuse_threads(true);
    let seen = Arc::new(Mutex::new(None));
    let s = Arc::clone(&seen);
    let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        let worker = thread::current().id();
        let result = run_blocking_claiming_in(
            pool,
            me,
            move |claim| {
                let claimed = claim.claim();
                (thread::current().id() == worker, claimed)
            },
            &Hooks::default(),
        );
        *s.lock() = Some((result, me.sched.blocking()));
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    assert_eq!(*seen.lock(), Some((Ok((true, true)), false)));
    assert_eq!(runtime.shared.pool_waiting.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-817 with D-1065 (review finding SOUND-W4B-1): after a refused thread
/// the fiber's header is already published, so a cancellation's walk can
/// win it before the fiber's own claim; the walk wakes the fiber only once
/// its locks are released. The fiber must wait for that wake before it
/// goes on, so that the wake never lands on a later park ("a fiber is
/// woken once per park", `sched::wake`). Here the fiber holds until the
/// walk won, the walk's wakes are delayed, and after `Err(Cancelled)` the
/// fiber parks in a wait that only the test wakes: it must stay parked.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_a_refused_thread_waits_for_the_walks_wake() {
    const DELAY: Duration = Duration::from_millis(300);
    let runtime = runtime(2);
    let domain = Domain::new();
    let counts = runtime.shared.domains.of(&domain);
    let group = DetachedGroup::new();
    let pool = Pool::private(1, Duration::from_secs(10));
    pool.refuse_threads(true);
    let probe = Arc::new(Probe::default());
    probe.hold_refused.store(true, Ordering::SeqCst);
    let ran = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(AtomicUsize::new(0));
    let parked: Arc<Mutex<Option<FiberRef>>> = Arc::new(Mutex::new(None));
    let resumed = Arc::new(AtomicBool::new(false));
    let (p, r, o, pk, rs) = (
        Arc::clone(&probe),
        Arc::clone(&ran),
        Arc::clone(&outcome),
        Arc::clone(&parked),
        Arc::clone(&resumed),
    );
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let result = run_blocking_claiming_in(
                pool,
                me,
                move |_| r.store(true, Ordering::SeqCst),
                &Hooks { probe: Some(p) },
            );
            o.store(
                if result == Err(Cancelled) {
                    CANCELLED
                } else {
                    RETURNED
                },
                Ordering::SeqCst,
            );
            // A wait that only the test wakes: `PARKING` before the test
            // learns of it, so its wake is never lost.
            begin_park(me);
            *pk.lock() = Some(FiberRef::from_ctx(me));
            park(me, AfterSwitch::Park(None));
            rs.store(true, Ordering::SeqCst);
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1);
    thread::scope(|s| {
        let teardown = s.spawn(|| {
            cancel::hooks::delay_wakes(DELAY);
            runtime.teardown(&group)
        });
        wait_until(|| parked.lock().is_some());
        assert_eq!(outcome.load(Ordering::SeqCst), CANCELLED);
        // The walk's delayed wake has come by now, wherever it landed.
        thread::sleep(DELAY + Duration::from_millis(200));
        assert!(
            !resumed.load(Ordering::SeqCst),
            "the walk's wake resumed the fiber in a later, unrelated wait"
        );
        let fiber = parked.lock().take().unwrap();
        // SAFETY: the fiber is parking or parked in the test's own wait,
        // which nothing else wakes, and cannot end before this wake.
        assert!(unsafe { wake(fiber, false) }, "the fiber was not waiting");
        let report = teardown.join().unwrap();
        assert_eq!((report.waited, report.skipped), (1, 0));
    });
    assert!(resumed.load(Ordering::SeqCst));
    assert!(!ran.load(Ordering::SeqCst), "the abandoned job ran");
    assert_eq!(counts.pool(), 0, "the abandoned job's token was kept");
    assert_eq!(runtime.shared.pool_waiting.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-692: without a runtime the detached job runs inline and cannot be
/// abandoned: its claim holds.
#[test]
fn pool_detached_without_a_runtime_the_job_runs_inline() {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let seen = Arc::new(Mutex::new(None));
    let s = Arc::clone(&seen);
    let task = move || -> Word {
        let me = crate::entry::current_fiber().unwrap();
        let worker = thread::current().id();
        let result = run_blocking_claiming_in(
            Pool::global(),
            me,
            move |claim| (thread::current().id() == worker, claim.claim()),
            &Hooks::default(),
        );
        *s.lock() = Some(result);
        0
    };
    // SAFETY: the task makes no Cheby value and uses no trampoline.
    let outcome = unsafe { crate::entry::run_task(&config, task) }.unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(*seen.lock(), Some(Ok((true, true))));
}

/// A root that waits on a channel nobody else holds: the deadlock report
/// of one fiber blocked in `channel::receive`.
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

/// D-857 under D-1165: a pending detached job holds
/// detection off while its fiber waits for it; once a cancellation made
/// the fiber abandon it, the job no longer counts, and the root, blocked
/// for good, is reported while the job still runs.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_detached_an_abandoned_job_does_not_hold_detection_off() {
    let runtime: Runtime = testing::tuned_detecting(2, false, true);
    let domain = Domain::new();
    let counts = runtime.shared.domains.of(&domain);
    let group = DetachedGroup::new();
    let pool = Pool::private(2, Duration::from_secs(10));
    let seen = Arc::new(Seen::default());
    let gate = Gate::new();
    let (s, g) = (Arc::clone(&seen), Arc::clone(&gate));
    let run = EntryConfig {
        mode: EntryMode::Run,
        ..detached_tests::config(&domain)
    };
    let root: mpsc::Receiver<EntryOutcome> =
        testing::start_root_in(&runtime, &run, &group, move |me| {
            detach(me, move |me| {
                s.wait(me, pool, g, &Hooks::default());
                if unwinding(me) { 0 } else { values::nil() }
            });
            receive_forever(me)
        });
    wait_until(|| gate.entered());
    assert!(
        root.recv_timeout(testing::scaled(Duration::from_millis(300)))
            .is_err(),
        "reported while a fiber waited on a pool job"
    );
    assert_eq!(counts.pool(), 1);
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (1, 0));
    assert_eq!(seen.outcome(), CANCELLED);
    let expected = format!("{HEADER}  blocked in channel::receive at <unknown location>\n");
    assert_eq!(
        root.recv_timeout(testing::hang()).unwrap(),
        EntryOutcome::Deadlocked(expected)
    );
    assert_eq!(
        seen.drops.load(Ordering::SeqCst),
        0,
        "the job ended before the report"
    );
    gate.open();
    wait_until(|| seen.drops.load(Ordering::SeqCst) == 1);
    runtime.halt();
}
