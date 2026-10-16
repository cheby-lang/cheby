//! Tests of the per-worker slot cache's scheduler side
//! (GAP(worker-slot-cache), refines D-677 and D-749): a worker keeps the
//! slots of the fibers it retires, up to four, for the fibers its running
//! fiber spawns; a root started by a host takes its slot from the global
//! pool; and the cache is empty once the worker's thread ends, at a stop,
//! a halt or a retirement (D-795), so no slot stays cached across them.
//!
//! The cache is read on its worker's own thread, by a fiber running there,
//! or once no thread runs in the slot (joined, or seen `DEAD`). Time is
//! asserted only as a cap (plan §0 'Determinism').

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::testing::{Mailbox, entry_check, root, runtime, spawn, start_root};
use super::worker::life;
use super::{AfterSwitch, Shared, leave_callback, park};
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;

/// How long a test waits for something that should happen soon.
const CAP: Duration = Duration::from_secs(10);

/// The slots a worker's cache holds at most.
const CACHE_SLOTS: usize = 4;

/// The failures a fiber reports as bits of its word, since a Rust panic
/// cannot unwind out of it.
mod failed {
    pub(super) const RETIRED: u64 = 1;
    pub(super) const KEPT: u64 = 1 << 1;
    pub(super) const TAKEN: u64 = 1 << 2;
    pub(super) const REFILLED: u64 = 1 << 3;
}

/// The slots in the cache of the worker `me` runs on, read on that
/// worker's thread; `usize::MAX` if `me` has no worker.
fn cached_here(shared: &Shared, me: &ExecCtx) -> usize {
    shared
        .worker(me.sched.worker())
        .map_or(usize::MAX, |worker| {
            // SAFETY: `me` runs on that worker's thread now, and nothing else
            // runs there while it reads.
            unsafe { worker.cached_slots() }
        })
}

/// The slots in every worker's cache, once every worker thread of the
/// runtime is joined.
fn cached_after_join(shared: &Shared) -> usize {
    assert!(shared.threads.lock().is_empty(), "a worker was not joined");
    shared
        .workers
        .iter()
        // SAFETY: every thread that ran in a slot was joined.
        .map(|worker| unsafe { worker.cached_slots() })
        .sum()
}

/// Waits in fiber `me`, yielding, up to [`CAP`] for `ready`.
fn yield_until(me: &ExecCtx, mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() > CAP {
            return false;
        }
        park(me, AfterSwitch::Requeue);
    }
    true
}

/// Waits on the host up to [`CAP`] for `ready`.
fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() > CAP {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// In the root `me` of a one-worker runtime: spawns six fibers that end at
/// once and waits until the worker retired them all; its cache then holds
/// four slots. Returns the failures.
fn fill_cache(shared: &Shared, me: &ExecCtx) -> u64 {
    let mut failures = 0;
    for _ in 0..6 {
        spawn(me, false, |_| {});
    }
    // One worker: it retires each fiber before it runs the root again.
    if !yield_until(me, || shared.live.load(Ordering::SeqCst) == 1) {
        failures |= failed::RETIRED;
    }
    if cached_here(shared, me) != CACHE_SLOTS {
        failures |= failed::KEPT;
    }
    failures
}

/// GAP(worker-slot-cache), D-677, D-749: a worker keeps the slots of the
/// fibers it retires, at most four (the rest go to the global pool), and a
/// fiber running on it spawns on one of them.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_keeps_retired_slots_for_the_worker_s_spawns() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    let word = root(&runtime, move |me| {
        let mut failures = fill_cache(&shared, me);
        let ran = Arc::new(Mailbox::default());
        {
            let ran = Arc::clone(&ran);
            spawn(me, false, move |_| ran.send(0, false));
        }
        // The spawn took one of the cached slots.
        if cached_here(&shared, me) != CACHE_SLOTS - 1 {
            failures |= failed::TAKEN;
        }
        ran.receive(me);
        if !yield_until(me, || shared.live.load(Ordering::SeqCst) == 1) {
            failures |= failed::RETIRED;
        }
        // Its retirement gave the slot back to the cache.
        if cached_here(&shared, me) != CACHE_SLOTS {
            failures |= failed::REFILLED;
        }
        failures
    });
    assert_eq!(word, 0, "failures {word:#b}");
    runtime.shutdown();
}

/// GAP(worker-slot-cache): a root started by a host takes its slot from the
/// global pool, never from a worker's cache, even when it runs on that
/// worker.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_a_root_started_by_the_host_uses_the_global_pool() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    let filled = {
        let shared = Arc::clone(&shared);
        root(&runtime, move |me| fill_cache(&shared, me))
    };
    assert_eq!(filled, 0, "failures {filled:#b}");
    // The first root's own slot went to the global pool, as every root's
    // does; the second root's came from it, so the cache still holds four.
    let cached = {
        let shared = Arc::clone(&shared);
        root(&runtime, move |me| {
            u64::try_from(cached_here(&shared, me)).unwrap_or(u64::MAX)
        })
    };
    assert_eq!(cached, 4);
    runtime.shutdown();
}

/// GAP(worker-slot-cache), gate 3's MEM-W3-2: a root's slot never enters
/// the cache, whatever its class (here the REPL's and `cheby test`'s
/// 64 MiB main stack, which no spawn would take): after four roots, the
/// cache is empty and still serves this worker's spawns.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_keeps_no_root_slot() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    for _ in 0..CACHE_SLOTS {
        let outcome = start_root(&runtime, 64 << 20, |_| 0).recv_timeout(CAP);
        assert!(
            matches!(outcome, Ok(EntryOutcome::Returned(0))),
            "{outcome:?}"
        );
    }
    let word = root(&runtime, move |me| {
        let mut failures = 0;
        if cached_here(&shared, me) != 0 {
            failures |= failed::KEPT;
        }
        spawn(me, false, |_| {});
        if !yield_until(me, || shared.live.load(Ordering::SeqCst) == 1) {
            failures |= failed::RETIRED;
        }
        // The spawned fiber's slot is cached; no root's ever was.
        if cached_here(&shared, me) != 1 {
            failures |= failed::REFILLED;
        }
        failures
    });
    assert_eq!(word, 0, "failures {word:#b}");
    runtime.shutdown();
}

/// GAP(worker-slot-cache), D-708: a stop drains every worker's cache to the
/// global pool before the worker's thread ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_is_empty_after_a_stop() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    let filled = {
        let shared = Arc::clone(&shared);
        root(&runtime, move |me| fill_cache(&shared, me))
    };
    assert_eq!(filled, 0, "failures {filled:#b}");
    runtime.shutdown();
    assert_eq!(cached_after_join(&shared), 0);
    assert_eq!(shared.live.load(Ordering::SeqCst), 0);
}

/// GAP(worker-slot-cache), D-708, MEM-W1-1: a halt drains every worker's
/// cache to the global pool, and the slot of the fiber it froze is never
/// retired, so it enters no cache.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_is_empty_after_a_halt() {
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    let started = Arc::new(AtomicBool::new(false));
    let filled = {
        let (shared, started) = (Arc::clone(&shared), Arc::clone(&started));
        root(&runtime, move |me| {
            let failures = fill_cache(&shared, me);
            spawn(me, false, move |fiber| {
                started.store(true, Ordering::SeqCst);
                loop {
                    if entry_check(fiber) {
                        return;
                    }
                    std::hint::spin_loop();
                }
            });
            failures
        })
    };
    assert_eq!(filled, 0, "failures {filled:#b}");
    assert!(wait_until(|| started.load(Ordering::SeqCst)));
    runtime.halt();
    assert_eq!(shared.probe.frozen.load(Ordering::SeqCst), 1);
    assert_eq!(cached_after_join(&shared), 0);
    // The frozen fiber was never retired.
    assert_eq!(shared.live.load(Ordering::SeqCst), 1);
}

/// What the retirement test's fiber reports: the worker it ran on and the
/// slots that worker's cache held.
const NO_REPORT: u64 = u64::MAX;

/// GAP(worker-slot-cache), D-795, D-687: a worker that retires drains its
/// cache before its slot is marked `DEAD` (a thread started there later
/// would use the cache). A fiber pinned to the other worker spawns into
/// that worker's runnext slot, which no thief takes, so the spawned fiber
/// is retired there and its slot cached; then that worker is retired.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_is_drained_when_its_worker_retires() {
    let runtime = runtime(2);
    let shared = Arc::clone(&runtime.shared);
    let report = Arc::new(AtomicUsize::new(usize::MAX));
    let cached_at = Arc::new(AtomicUsize::new(usize::MAX));
    let word = {
        let (shared, report, cached_at) = (
            Arc::clone(&shared),
            Arc::clone(&report),
            Arc::clone(&cached_at),
        );
        root(&runtime, move |me| {
            let home = me.sched.worker();
            {
                let (shared, report, cached_at) = (
                    Arc::clone(&shared),
                    Arc::clone(&report),
                    Arc::clone(&cached_at),
                );
                spawn(me, false, move |fiber| {
                    // Pinned, so it stays on this worker while it waits.
                    fiber.sched.enter_callback();
                    let ended = Arc::new(Mailbox::default());
                    {
                        let ended = Arc::clone(&ended);
                        spawn(fiber, true, move |_| ended.send(0, false));
                    }
                    ended.receive(fiber);
                    cached_at.store(cached_here(&shared, fiber), Ordering::SeqCst);
                    leave_callback(fiber);
                    report.store(fiber.sched.worker(), Ordering::SeqCst);
                });
            }
            // Busy, with no entry check, so the other worker steals the
            // fiber and nothing preempts the root meanwhile.
            let begun = Instant::now();
            while report.load(Ordering::SeqCst) == usize::MAX && begun.elapsed() < CAP {
                std::hint::spin_loop();
            }
            let index = report.load(Ordering::SeqCst);
            if index == usize::MAX || index == home {
                return NO_REPORT;
            }
            u64::try_from(index).unwrap_or(NO_REPORT)
        })
    };
    assert_ne!(
        word, NO_REPORT,
        "the pinned fiber did not run on the other worker"
    );
    let index = usize::try_from(word).unwrap();
    assert!(
        cached_at.load(Ordering::SeqCst) >= 1,
        "the spawned fiber's slot was not cached on its worker"
    );
    // Retire that worker, as `set_thread_count` retires its highest live
    // one, and wait for its slot to be `DEAD`.
    {
        let threads = shared.threads.lock();
        shared.thread_target.store(1, Ordering::SeqCst);
        drop(threads);
        shared.retire_worker(index);
    }
    assert!(wait_until(|| {
        shared
            .worker(index)
            .is_some_and(|worker| worker.life() == life::DEAD)
    }));
    // SAFETY: the slot was seen `DEAD`: its thread drained the cache before
    // it marked it so, and no new thread starts there.
    let left = shared
        .worker(index)
        .map_or(usize::MAX, |worker| unsafe { worker.cached_slots() });
    assert_eq!(left, 0);
    runtime.shutdown();
    assert_eq!(cached_after_join(&shared), 0);
}
