//! Tests of the timers in a running runtime (D-682, D-652): a fiber woken
//! by its timer, firing while every worker spins (D-789),
//! and the timers at stop (C7).
//!
//! Time is asserted only as a lower bound ("never earlier than the
//! deadline", D-671) and a generous cap of 5 s times `CHEBY_HARNESS_SCALE`
//! (D-872); latencies are printed, as
//! informative readings (plan §0 'Determinism').

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::testing::{Mailbox, entry_check, root, runtime, scaled, spawn, start_root};
use super::{Runtime, TimerTarget};
use crate::entry::{EntryOutcome, MIN_STACK_SIZE};

/// A target that posts the instant it fired to a mailbox, waking the fiber
/// that waits there.
pub(super) struct PostOnFire {
    pub(super) mailbox: Arc<Mailbox>,
    pub(super) epoch: Instant,
}

impl TimerTarget for PostOnFire {
    fn fire(&self) {
        let nanos = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.mailbox.send(nanos, false);
    }
}

/// A target that only counts its firings.
#[derive(Default)]
struct Count(AtomicUsize);

impl TimerTarget for Count {
    fn fire(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Parks the running fiber on a timer `after` from now and returns how
/// late it fired and how late the fiber ran, from the deadline.
pub(super) fn sleep(me: &crate::ctx::ExecCtx, after: Duration) -> (Duration, Duration) {
    let shared = Arc::clone(me.sched.runtime().unwrap());
    let mailbox = Arc::new(Mailbox::default());
    let epoch = Instant::now();
    let deadline = epoch.checked_add(after).unwrap();
    let _handle = shared.arm_timer(
        deadline,
        Arc::new(PostOnFire {
            mailbox: Arc::clone(&mailbox),
            epoch,
        }),
    );
    let fired_at = epoch
        .checked_add(Duration::from_nanos(mailbox.receive(me)))
        .unwrap();
    let ran_at = Instant::now();
    assert!(fired_at >= deadline, "a timer fired before its deadline");
    (
        fired_at.saturating_duration_since(deadline),
        ran_at.saturating_duration_since(deadline),
    )
}

/// D-682: a fiber parked on a timer is woken once its deadline has passed,
/// never before (D-671), at one and at four workers.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timer_wakes_a_parked_fiber() {
    for threads in [1, 4] {
        let runtime = runtime(threads);
        let late = root(&runtime, |me| {
            let (fired, ran) = sleep(me, Duration::from_millis(20));
            assert!(ran >= fired);
            u64::try_from(ran.as_micros()).unwrap()
        });
        println!("timer_wakes_a_parked_fiber: {threads} workers, ran {late} us after the deadline");
        assert_eq!(runtime.shared.timers_pending(), 0);
    }
}

/// D-789: every worker runs a fiber that spins through
/// the function-entry check and never parks, so no worker polls; the
/// monitor fires the timer, the fiber it wakes goes to the global queue, and
/// the next tick preempts a spinner so that it runs, at one and at two
/// workers, within a cap of 5 s (scaled).
fn fires_while_every_worker_spins(threads: usize) -> Duration {
    let runtime = runtime(threads);
    let stop = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicUsize::new(0));
    let outcome = {
        let (stop, started) = (Arc::clone(&stop), Arc::clone(&started));
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            for _ in 0..threads {
                let (stop, started) = (Arc::clone(&stop), Arc::clone(&started));
                spawn(me, false, move |fiber| {
                    started.fetch_add(1, Ordering::SeqCst);
                    while !stop.load(Ordering::SeqCst) {
                        entry_check(fiber);
                    }
                });
            }
            let (_, ran) = sleep(me, Duration::from_millis(50));
            let all_spun = started.load(Ordering::SeqCst) == threads;
            stop.store(true, Ordering::SeqCst);
            assert!(all_spun, "not every worker ran a spinner");
            u64::try_from(ran.as_nanos()).unwrap()
        })
    };
    let cap = scaled(Duration::from_secs(5));
    let result = outcome.recv_timeout(cap);
    // Let the spinners end whatever happened, so the runtime can stop.
    stop.store(true, Ordering::SeqCst);
    let late = match result {
        Ok(EntryOutcome::Returned(nanos)) => Duration::from_nanos(nanos),
        other => panic!("the sleeping fiber did not run within {cap:?}: {other:?}"),
    };
    drop(runtime);
    late
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timer_fires_while_every_worker_spins() {
    for threads in [1, 2] {
        let late = fires_while_every_worker_spins(threads);
        println!(
            "timer_fires_while_every_worker_spins: {threads} workers, the woken fiber ran {:.3} ms after the deadline",
            late.as_secs_f64() * 1000.0
        );
    }
}

/// C7: stop joins every thread, then drops every timer entry unfired, so
/// each target's reference count is back to 1 even while handles live.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timer_entries_are_dropped_at_shutdown() {
    let runtime = runtime(2);
    let shared = Arc::clone(&runtime.shared);
    let targets: Vec<Arc<Count>> = (0..3).map(|_| Arc::new(Count::default())).collect();
    let far = Instant::now()
        .checked_add(Duration::from_secs(3600))
        .unwrap();
    let handles: Vec<_> = targets
        .iter()
        .map(|target| {
            let target: Arc<dyn TimerTarget> = Arc::<Count>::clone(target);
            shared.arm_timer(far, target)
        })
        .collect();
    // One armed from a fiber too.
    let from_fiber = Arc::new(Count::default());
    {
        let target = Arc::clone(&from_fiber);
        root(&runtime, move |me| {
            let shared = me.sched.runtime().unwrap();
            drop(shared.arm_timer(far, target));
            0
        });
    }
    assert_eq!(shared.timers_pending(), 4);
    runtime.shutdown();
    assert_eq!(shared.timers_pending(), 0);
    for target in targets.iter().chain([&from_fiber]) {
        assert_eq!(Arc::strong_count(target), 1);
        assert_eq!(target.0.load(Ordering::SeqCst), 0);
    }
    assert!(
        handles
            .iter()
            .all(|handle| !handle.fired() && !handle.cancel())
    );
}

/// C7: a runtime that has stopped arms nothing: the handle never fires and
/// the target is released at once.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timer_armed_after_stop_never_fires() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    runtime.shutdown();
    let target = Arc::new(Count::default());
    let now = Instant::now();
    let erased: Arc<dyn TimerTarget> = Arc::<Count>::clone(&target);
    let handle = shared.arm_timer(now, erased);
    assert_eq!(Arc::strong_count(&target), 1);
    assert_eq!(shared.timers_pending(), 0);
    assert_eq!(shared.fire_due(Instant::now()), 0);
    assert_eq!(target.0.load(Ordering::SeqCst), 0);
    assert!(!handle.fired());
    assert!(!handle.cancel());
}

/// C4: the thread count is the worker count until set, then the count set
/// (`thread_count_tests` checks the workers that start and retire).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timer_seam_reports_the_thread_target_and_fiber_stack() {
    let runtime = Runtime::new(&super::RuntimeConfig {
        threads: Some(3),
        fiber_stack: 1 << 20,
        ..super::RuntimeConfig::default()
    })
    .unwrap();
    assert_eq!(runtime.shared.thread_count(), 3);
    runtime.shared.set_thread_count(5);
    assert_eq!(runtime.shared.thread_count(), 5);
    assert_eq!(runtime.shared.fiber_stack(), 1 << 20);
    assert_eq!(
        super::RuntimeConfig::default().fiber_stack,
        crate::entry::DEFAULT_STACK_SIZE
    );
}
