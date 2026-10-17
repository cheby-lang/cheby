//! Tests of `set_thread_count`'s mechanics (D-657, D-689,
//! D-795): workers started at once and retired at
//! their next scheduling point, under load, with a pinned fiber (D-687),
//! with the poll holder (D-680), with a lone spinning fiber (D-737), and
//! joined by the runtime's stop and halt (C-7).
//!
//! The counts go through the kernels a Cheby program calls
//! (`runtime_info`), from a fiber. Where a test must retire one given
//! worker (the poll holder, the worker a lone fiber runs on), it lowers the
//! count by one and retires that worker with the scheduler's own step
//! ([`Shared::retire_worker`]), since `set_thread_count` retires the
//! highest indexes. Time is asserted only as a cap (plan §0
//! 'Determinism').

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::park::state as parker;
use super::testing::{Mailbox, entry_check, hang, root, runtime, spawn, start_root, tuned};
use super::worker::life;
use super::{AfterSwitch, Shared, TimerTarget, leave_callback, park};
use crate::ctx::ExecCtx;
use crate::entry::{EntryOutcome, MIN_STACK_SIZE};
use crate::runtime_info::{cheby_runtime_set_thread_count, cheby_runtime_thread_count};

/// How long a test waits for something that should happen soon.
const CAP: Duration = Duration::from_secs(10);

/// The context argument a kernel gets from the running fiber.
fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// `runtime::thread_count()` on the running fiber.
fn count(me: &ExecCtx) -> i64 {
    // SAFETY: the running fiber's context.
    unsafe { cheby_runtime_thread_count(ctx_of(me)) }
}

/// `runtime::set_thread_count(n)` on the running fiber.
fn set(me: &ExecCtx, n: i64) {
    // SAFETY: the running fiber's context.
    unsafe { cheby_runtime_set_thread_count(ctx_of(me), n) };
}

/// The worker threads of the runtime that have not ended yet.
fn os_threads(shared: &Shared) -> usize {
    shared
        .threads
        .lock()
        .iter()
        .filter(|(_, handle)| !handle.is_finished())
        .count()
}

/// The life of worker slot `index`.
fn life_of(shared: &Shared, index: usize) -> Option<u8> {
    shared.worker(index).map(super::worker::WorkerShared::life)
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

/// Lowers the count by one and retires worker `index`, as
/// `set_thread_count` retires its highest live worker.
fn retire_one(shared: &Arc<Shared>, index: usize) {
    let threads = shared.threads.lock();
    let target = shared.thread_count().saturating_sub(1).max(1);
    shared.thread_target.store(target, Ordering::SeqCst);
    drop(threads);
    shared.retire_worker(index);
}

/// The failures a fiber reports as bits of its word, since a Rust panic
/// cannot unwind out of it.
mod failed {
    pub(super) const COUNT: u64 = 1;
    pub(super) const GREW: u64 = 1 << 1;
    pub(super) const SPREAD: u64 = 1 << 2;
    pub(super) const SHRANK: u64 = 1 << 3;
    pub(super) const PROGRESS: u64 = 1 << 4;
    pub(super) const PINNED: u64 = 1 << 5;
    pub(super) const STAYED: u64 = 1 << 6;
    pub(super) const MOVED: u64 = 1 << 7;
    pub(super) const REACHED: u64 = 1 << 8;
    pub(super) const HELD: u64 = 1 << 9;
    pub(super) const ENDED: u64 = 1 << 10;
}

/// What the ping-pong pairs of the load test share.
struct Load {
    /// Rounds completed by every pair.
    rounds: AtomicU64,
    /// A bit per worker index a fiber of the pairs ran on.
    seen: AtomicU64,
    /// Set when the pairs end.
    stop: AtomicBool,
    /// Gets a word when the last fiber of the pairs ends.
    done: Mailbox,
    /// Fibers of the pairs not ended yet.
    left: AtomicUsize,
}

impl Load {
    /// Records the worker `fiber` runs on.
    fn saw(&self, fiber: &ExecCtx) {
        self.seen
            .fetch_or(1 << (fiber.sched.worker() % 64), Ordering::SeqCst);
    }

    /// One fiber of the pairs ended.
    fn ended(&self) {
        if self.left.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.done.send(0, false);
        }
    }
}

/// Spawns a pair that passes a word back and forth through the runnext
/// slot until `load.stop`, then ends; a word lost or doubled stops the
/// pair short, which hangs the test.
fn spawn_pair(me: &ExecCtx, load: &Arc<Load>) {
    const STOP: u64 = u64::MAX;
    let request = Arc::new(Mailbox::default());
    let reply = Arc::new(Mailbox::default());
    {
        let (request, reply, load) = (Arc::clone(&request), Arc::clone(&reply), Arc::clone(load));
        spawn(me, false, move |fiber| {
            loop {
                entry_check(fiber);
                let word = request.receive(fiber);
                load.saw(fiber);
                reply.send(word, true);
                if word == STOP {
                    break;
                }
            }
            load.ended();
        });
    }
    let load = Arc::clone(load);
    spawn(me, false, move |fiber| {
        let mut round = 0;
        loop {
            entry_check(fiber);
            let word = if load.stop.load(Ordering::SeqCst) {
                STOP
            } else {
                round
            };
            request.send(word, true);
            let back = reply.receive(fiber);
            load.saw(fiber);
            if back == STOP {
                break;
            }
            if back != round {
                return;
            }
            round = round.wrapping_add(1);
            load.rounds.fetch_add(1, Ordering::SeqCst);
        }
        load.ended();
    });
}

/// D-657, D-689: 100 fibers ping-pong in 50 pairs while one fiber raises
/// the count from 1 to 8 and lowers it back to 1. The new workers start at
/// once and take some of the pairs; the removed ones hand their fibers to
/// the global queue and end; no fiber is lost, and the pairs go on at one
/// thread.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_grows_and_shrinks_under_load() {
    const PAIRS: usize = 50;
    let runtime = runtime(1);
    let shared = Arc::clone(&runtime.shared);
    let load = Arc::new(Load {
        rounds: AtomicU64::new(0),
        seen: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        done: Mailbox::default(),
        left: AtomicUsize::new(PAIRS * 2),
    });
    let probe = Arc::clone(&shared);
    let word = {
        let load = Arc::clone(&load);
        root(&runtime, move |me| {
            for _ in 0..PAIRS {
                spawn_pair(me, &load);
            }
            let (rounds, seen) = (&load.rounds, &load.seen);
            let mut failures = 0;
            // Up: eight workers at once, and the pairs spread over them.
            yield_until(me, || rounds.load(Ordering::SeqCst) > 1_000);
            set(me, 8);
            if count(me) != 8 {
                failures |= failed::COUNT;
            }
            if probe.workers.live() != 8 || os_threads(&probe) != 8 {
                failures |= failed::GREW;
            }
            if !yield_until(me, || seen.load(Ordering::SeqCst).count_ones() >= 4) {
                failures |= failed::SPREAD;
            }
            // Down: back to one, and every removed worker ends.
            set(me, 1);
            if count(me) != 1 {
                failures |= failed::COUNT;
            }
            if !yield_until(me, || {
                os_threads(&probe) == 1 && probe.workers.live() == 1 && probe.idle.workers() == 1
            }) {
                failures |= failed::SHRANK;
            }
            // The pairs go on at one thread.
            let before = rounds.load(Ordering::SeqCst);
            if !yield_until(me, || rounds.load(Ordering::SeqCst) > before + 1_000) {
                failures |= failed::PROGRESS;
            }
            load.stop.store(true, Ordering::SeqCst);
            load.done.receive(me);
            failures
        })
    };
    assert_eq!(word, 0, "failures {word:#b}");
    println!(
        "thread_count: {} rounds, workers seen {:#b}",
        load.rounds.load(Ordering::SeqCst),
        load.seen.load(Ordering::SeqCst)
    );
    runtime.shutdown();
    assert_eq!(shared.live.load(Ordering::SeqCst), 0);
    assert!(shared.threads.lock().is_empty());
}

/// D-687, D-689: a fiber pinned to a worker (a kernel callback on its
/// stack) keeps that worker after the count drops: the worker leaves
/// `Idle` but does not end while the fiber waits; the fiber resumes on it
/// once woken, and once unpinned it moves to a live worker, and the
/// retired worker ends. In the migration stress mode, so that the fiber
/// first reaches worker 1, the one a lower count retires.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_pinned_fiber_keeps_its_retiring_worker() {
    let runtime = tuned(2, true, true);
    let shared = Arc::clone(&runtime.shared);
    let word = root(&runtime, move |me| {
        let ready = Arc::new(Mailbox::default());
        let gate = Arc::new(Mailbox::default());
        let result = Arc::new(Mailbox::default());
        {
            let (ready, gate, result) =
                (Arc::clone(&ready), Arc::clone(&gate), Arc::clone(&result));
            spawn(me, false, move |fiber| {
                for _ in 0..100 {
                    if fiber.sched.worker() == 1 {
                        break;
                    }
                    park(fiber, AfterSwitch::Requeue);
                }
                let pinned_on = fiber.sched.worker();
                fiber.sched.enter_callback();
                ready.send(u64::try_from(pinned_on).unwrap(), false);
                gate.receive(fiber);
                let resumed_on = fiber.sched.worker();
                leave_callback(fiber);
                park(fiber, AfterSwitch::Requeue);
                let moved_to = fiber.sched.worker();
                let mut failures = 0;
                if resumed_on != pinned_on {
                    failures |= failed::PINNED;
                }
                if moved_to == pinned_on {
                    failures |= failed::MOVED;
                }
                result.send(failures, false);
            });
        }
        let mut failures = 0;
        if ready.receive(me) != 1 {
            return failed::PINNED;
        }
        set(me, 1);
        // Worker 1 has left `Idle` and waits for its pinned fiber.
        if !yield_until(me, || life_of(&shared, 1) == Some(life::LEAVING)) {
            failures |= failed::SHRANK;
        }
        let held = Instant::now();
        while held.elapsed() < Duration::from_millis(30) {
            park(me, AfterSwitch::Requeue);
        }
        if life_of(&shared, 1) != Some(life::LEAVING) || os_threads(&shared) != 2 {
            failures |= failed::STAYED;
        }
        if shared.idle.workers() != 1 || shared.workers.live() != 1 {
            failures |= failed::SHRANK;
        }
        gate.send(0, false);
        failures |= result.receive(me);
        if !yield_until(me, || {
            os_threads(&shared) == 1 && life_of(&shared, 1) == Some(life::DEAD)
        }) {
            failures |= failed::SHRANK;
        }
        if shared.worker(1).is_none_or(|worker| worker.pinned() != 0) {
            failures |= failed::PINNED;
        }
        failures
    });
    assert_eq!(word, 0, "failures {word:#b}");
}

/// Sends the instant it fired at and its thread's name.
struct Report(parking_lot::Mutex<mpsc::Sender<(Instant, String)>>);

impl TimerTarget for Report {
    fn fire(&self) {
        let name = std::thread::current().name().unwrap_or("").to_owned();
        self.0.lock().send((Instant::now(), name)).ok();
    }
}

/// The worker blocked in the poller, if any.
fn poll_holder(shared: &Shared) -> Option<usize> {
    shared
        .workers
        .indexed()
        .find(|(_, worker)| worker.parker().state() == parker::PARKED_DRIVER)
        .map(|(index, _)| index)
}

/// D-680, D-689: the worker that holds the poller retires: it releases
/// the poller and ends, another idle worker takes the poller over, and a
/// timer armed afterwards is fired on time by a worker. The monitor fires
/// no timer here, so only a poll holder can.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_the_poll_holder_retires_and_another_worker_polls() {
    let runtime = tuned(3, false, true);
    let shared = Arc::clone(&runtime.shared);
    shared.probe.skip_timers.store(true, Ordering::SeqCst);
    let mut holder = None;
    assert!(
        wait_until(|| {
            holder = poll_holder(&shared);
            holder.is_some()
        }),
        "no worker polled"
    );
    let holder = holder.unwrap();
    retire_one(&shared, holder);
    assert!(
        wait_until(|| life_of(&shared, holder) == Some(life::DEAD) && os_threads(&shared) == 2),
        "the poll holder did not end"
    );
    let mut next = None;
    assert!(
        wait_until(|| {
            next = poll_holder(&shared);
            next.is_some()
        }),
        "no other worker took the poller over"
    );
    let next = next.unwrap();
    assert_ne!(next, holder);
    assert_eq!(life_of(&shared, next), Some(life::LIVE));
    let (sender, fired) = mpsc::channel();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(20))
        .unwrap();
    let _timer = shared.arm_timer(deadline, Arc::new(Report(parking_lot::Mutex::new(sender))));
    let Ok((at, on)) = fired.recv_timeout(CAP) else {
        panic!("no worker fired the timer after the poll holder retired");
    };
    assert!(at >= deadline, "the timer fired before its deadline");
    assert!(on.starts_with("cheby-worker-"), "fired on {on:?}");
    // The runtime still runs fibers, and the count is the one set.
    assert_eq!(root(&runtime, |me| u64::try_from(count(me)).unwrap()), 2);
    runtime.shutdown();
}

/// C-7: workers `set_thread_count` started, and those it retired, are
/// joined by the runtime's stop: afterwards no worker thread holds the
/// runtime. A halt (D-708) joins them too, freezing the fibers that spin
/// on the added workers.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_shutdown_and_halt_join_workers_added_at_run_time() {
    const SPINNERS: usize = 4;
    // Stop: up to six, down to three, then shut down at once.
    let stopped = runtime(1);
    let shared = Arc::clone(&stopped.shared);
    let seen = root(&stopped, |me| {
        set(me, 6);
        set(me, 3);
        u64::try_from(count(me)).unwrap()
    });
    assert_eq!(seen, 3);
    stopped.shutdown();
    assert!(shared.threads.lock().is_empty());
    assert_eq!(
        Arc::strong_count(&shared),
        1,
        "a worker thread was not joined"
    );
    // Halt: four spinners on four workers, three of them added by the
    // root, all frozen and every thread joined.
    let halted = runtime(1);
    let shared = Arc::clone(&halted.shared);
    let started = Arc::new(AtomicUsize::new(0));
    {
        let started = Arc::clone(&started);
        root(&halted, move |me| {
            set(me, i64::try_from(SPINNERS).unwrap());
            for _ in 0..SPINNERS {
                let started = Arc::clone(&started);
                spawn(me, false, move |fiber| {
                    started.fetch_add(1, Ordering::SeqCst);
                    loop {
                        if entry_check(fiber) {
                            return;
                        }
                        std::hint::spin_loop();
                    }
                });
            }
            0
        });
    }
    assert!(wait_until(|| started.load(Ordering::SeqCst) == SPINNERS));
    assert_eq!(os_threads(&shared), SPINNERS);
    halted.halt();
    assert!(shared.threads.lock().is_empty());
    assert_eq!(shared.probe.frozen.load(Ordering::SeqCst), SPINNERS);
    assert!(
        shared
            .workers
            .iter()
            .all(|worker| worker.running().is_null())
    );
}

/// runtime.md's example: the count read right after `set_thread_count` is
/// the count just set, clamped to 1 to 1024 (D-298, D-657), and the live
/// workers already match it: added ones started, removed ones flagged.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_right_after_set_thread_count() {
    let runtime = runtime(2);
    let shared = Arc::clone(&runtime.shared);
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    {
        let seen = Arc::clone(&seen);
        root(&runtime, move |me| {
            let mut seen = seen.lock();
            seen.push((count(me), shared.workers.live()));
            for n in [5, 1, 0, -7, 3, 3, 2] {
                set(me, n);
                seen.push((count(me), shared.workers.live()));
            }
            0
        });
    }
    assert_eq!(
        *seen.lock(),
        [
            (2, 2),
            (5, 5),
            (1, 1),
            (1, 1),
            (1, 1),
            (3, 3),
            (3, 3),
            (2, 2)
        ]
    );
    assert!(wait_until(|| os_threads(&runtime.shared) == 2));
    assert_eq!(runtime.threads(), 2);
    runtime.shutdown();
}

/// D-737, C-5: a lone fiber is never preempted, but the worker it runs on
/// still retires: the retirement asks it to yield (never to freeze), and
/// it goes on, on the other worker, at its next entry check.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_lone_spinning_fiber_gives_its_retiring_worker_back() {
    let runtime = runtime(2);
    let shared = Arc::clone(&runtime.shared);
    // The worker the spinner started on, plus one; 0 until it runs.
    let first = Arc::new(AtomicUsize::new(0));
    let outcome = {
        let first = Arc::clone(&first);
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let start = me.sched.worker();
            first.store(start + 1, Ordering::SeqCst);
            let begun = Instant::now();
            while me.sched.worker() == start && begun.elapsed() < hang() {
                if entry_check(me) {
                    return u64::MAX;
                }
                std::hint::spin_loop();
            }
            u64::try_from(me.sched.worker()).unwrap()
        })
    };
    assert!(wait_until(|| first.load(Ordering::SeqCst) != 0));
    let start = first.load(Ordering::SeqCst) - 1;
    retire_one(&shared, start);
    match outcome.recv_timeout(CAP) {
        Ok(EntryOutcome::Returned(word)) => {
            assert_ne!(word, u64::MAX, "the spinner was stopped");
            assert_ne!(usize::try_from(word).unwrap(), start);
        }
        other => panic!("the spinner kept its retiring worker: {other:?}"),
    }
    assert!(wait_until(|| life_of(&shared, start) == Some(life::DEAD)));
    assert_eq!(shared.probe.frozen.load(Ordering::SeqCst), 0);
    runtime.shutdown();
}

/// D-657, D-737 (review finding CONC3-1 of gate 3): a retirement that
/// lands after the worker read its life word at the top of its loop and
/// before it published the fiber it resumes is not lost. The worker reads
/// the word again after it stores `running`, and `retire_worker` reads
/// `running` after it stores the word, so one of them asks that fiber to
/// yield. Here fiber S yields over and over, mostly on worker 1, while H
/// spins pinned to worker 0; the host lowers the count to 1 at varying
/// moments and raises it back. When S finds itself on worker 1 while it is
/// retiring, it spins alone with entry checks, which a lone fiber's monitor
/// never interrupts (D-737): only the retirement's `YIELD` moves it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_retirement_racing_the_next_resume_is_not_lost() {
    const TRIALS: usize = 300;
    let runtime = tuned(2, false, true);
    let shared = Arc::clone(&runtime.shared);
    let stop_all = Arc::new(AtomicBool::new(false));
    // Lets S out of its spin after a lost retirement, to end the test.
    let escape = Arc::new(AtomicBool::new(false));
    let cycles = Arc::new(AtomicU64::new(0));
    let pinned = {
        let stop_all = Arc::clone(&stop_all);
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            while me.sched.worker() != 0 {
                park(me, AfterSwitch::Requeue);
            }
            me.sched.enter_callback();
            while !stop_all.load(Ordering::SeqCst) {
                entry_check(me);
                std::hint::spin_loop();
            }
            leave_callback(me);
            0
        })
    };
    let cycling = {
        let (stop_all, escape, cycles, probe) = (
            Arc::clone(&stop_all),
            Arc::clone(&escape),
            Arc::clone(&cycles),
            Arc::clone(&shared),
        );
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            while !stop_all.load(Ordering::SeqCst) {
                if me.sched.worker() == 1 && life_of(&probe, 1) == Some(life::RETIRING) {
                    while me.sched.worker() == 1
                        && life_of(&probe, 1) == Some(life::RETIRING)
                        && !escape.load(Ordering::SeqCst)
                    {
                        entry_check(me);
                        std::hint::spin_loop();
                    }
                } else {
                    cycles.fetch_add(1, Ordering::SeqCst);
                }
                park(me, AfterSwitch::Requeue);
            }
            0
        })
    };
    let mut lost = None;
    for trial in 0..TRIALS {
        assert!(wait_until(|| life_of(&shared, 1) == Some(life::LIVE)));
        let before = cycles.load(Ordering::SeqCst);
        assert!(
            wait_until(|| cycles.load(Ordering::SeqCst) > before + 50),
            "trial {trial}: S stopped cycling"
        );
        for _ in 0..(trial % 97) {
            std::hint::spin_loop();
        }
        shared.set_thread_count(1);
        let begun = Instant::now();
        while life_of(&shared, 1) != Some(life::DEAD) && begun.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_micros(200));
        }
        if life_of(&shared, 1) != Some(life::DEAD) {
            lost = Some(trial);
            escape.store(true, Ordering::SeqCst);
            assert!(wait_until(|| life_of(&shared, 1) == Some(life::DEAD)));
            break;
        }
        shared.set_thread_count(2);
    }
    stop_all.store(true, Ordering::SeqCst);
    escape.store(true, Ordering::SeqCst);
    assert!(matches!(
        pinned.recv_timeout(hang()),
        Ok(EntryOutcome::Returned(0))
    ));
    assert!(matches!(
        cycling.recv_timeout(hang()),
        Ok(EntryOutcome::Returned(0))
    ));
    runtime.shutdown();
    assert_eq!(
        lost, None,
        "worker 1 kept running a lone fiber for 1 s after its retirement"
    );
}

/// D-657, D-687, D-737 (review finding CONC3-2 of gate 3): a fiber pinned
/// to a worker that leaves stays with it while pinned; once its outermost
/// callback returns, it is asked to yield, so it moves to a live worker at
/// its next entry check even when it computes alone (no monitor
/// preemption, no migration stress mode), and the removed worker ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_an_unpinned_fiber_leaves_its_leaving_worker() {
    let runtime = tuned(2, false, true);
    let shared = Arc::clone(&runtime.shared);
    let word = root(&runtime, move |me| {
        let ready = Arc::new(Mailbox::default());
        let gate = Arc::new(Mailbox::default());
        let result = Arc::new(Mailbox::default());
        {
            let (ready, gate, result) =
                (Arc::clone(&ready), Arc::clone(&gate), Arc::clone(&result));
            spawn(me, false, move |fiber| {
                let begun = Instant::now();
                while fiber.sched.worker() != 1 && begun.elapsed() < CAP {
                    park(fiber, AfterSwitch::Requeue);
                }
                let pinned_on = fiber.sched.worker();
                fiber.sched.enter_callback();
                ready.send(u64::try_from(pinned_on).unwrap(), false);
                gate.receive(fiber);
                leave_callback(fiber);
                // Computes alone, with entry checks, until it moves.
                let begun = Instant::now();
                while fiber.sched.worker() == pinned_on && begun.elapsed() < CAP {
                    if entry_check(fiber) {
                        break;
                    }
                    std::hint::spin_loop();
                }
                result.send(u64::from(fiber.sched.worker() == pinned_on), false);
            });
        }
        let mut failures = 0;
        if ready.receive(me) != 1 {
            return failed::PINNED;
        }
        set(me, 1);
        if !yield_until(me, || life_of(&shared, 1) == Some(life::LEAVING)) {
            failures |= failed::SHRANK;
        }
        gate.send(0, false);
        if result.receive(me) != 0 {
            failures |= failed::STAYED;
        }
        if !yield_until(me, || {
            os_threads(&shared) == 1 && life_of(&shared, 1) == Some(life::DEAD)
        }) {
            failures |= failed::SHRANK;
        }
        failures
    });
    assert_eq!(word, 0, "failures {word:#b}");
    runtime.shutdown();
}

/// D-657, C-7 (review finding CONC3-3 of gate 3): a stop that comes while
/// a worker retires still runs every runnable fiber to its end
/// (`Runtime::shutdown`'s contract): a worker that is retiring when the
/// stop begins stays, and one already leaving rejoins, so no fiber it
/// handed to the global queue is stranded after the live workers ended. A
/// halt joins every thread too. Three roots change the count at random
/// and yield until the runtime stops, which comes at varying moments.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_stop_while_workers_retire_ends_every_fiber() {
    const ROUNDS: usize = 200;
    for round in 0..ROUNDS {
        let runtime = tuned(2, false, true);
        let shared = Arc::clone(&runtime.shared);
        let started = Arc::new(AtomicUsize::new(0));
        for seed in 0..3_u64 {
            let (probe, started) = (Arc::clone(&shared), Arc::clone(&started));
            drop(start_root(&runtime, MIN_STACK_SIZE, move |me| {
                started.fetch_add(1, Ordering::SeqCst);
                let mut k = seed;
                while !probe.stopping() {
                    k = k.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    set(me, i64::try_from((k >> 33) % 7).unwrap());
                    if entry_check(me) {
                        return 1;
                    }
                    park(me, AfterSwitch::Requeue);
                }
                0
            }));
        }
        assert!(wait_until(|| started.load(Ordering::SeqCst) == 3));
        std::thread::sleep(Duration::from_micros(
            u64::try_from(round % 7).unwrap() * 300,
        ));
        let begun = Instant::now();
        let halted = round % 4 == 3;
        if halted {
            runtime.halt();
        } else {
            runtime.shutdown();
        }
        assert!(
            begun.elapsed() < CAP,
            "round {round}: the stop took {:?}",
            begun.elapsed()
        );
        assert!(shared.threads.lock().is_empty(), "round {round}");
        if !halted {
            assert_eq!(
                shared.live.load(Ordering::SeqCst),
                0,
                "round {round}: a runnable fiber was stranded"
            );
            assert_eq!(Arc::strong_count(&shared), 1, "round {round}");
        }
    }
}

/// D-687, D-795 (fixer-w3-1 of checkpoint 3, D-821):
/// in the migration stress mode a fiber that yields goes to the targeted
/// queue of another live worker, so a stop must not leave a fiber in the
/// queue of a worker that has already ended. A live worker ends at a stop
/// only under its targeted queue's lock with that queue empty, and marks
/// its slot `DEAD` there, so a later push goes to the global queue, which
/// the pushing worker looks at before it ends. Three roots yield until the
/// runtime stops, which comes at varying moments; every fiber ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_stop_in_the_migration_stress_mode_strands_no_fiber() {
    const ROUNDS: usize = 400;
    for round in 0..ROUNDS {
        let runtime = tuned(2, true, true);
        let shared = Arc::clone(&runtime.shared);
        let started = Arc::new(AtomicUsize::new(0));
        for _ in 0..3_u64 {
            let (probe, started) = (Arc::clone(&shared), Arc::clone(&started));
            drop(start_root(&runtime, MIN_STACK_SIZE, move |me| {
                started.fetch_add(1, Ordering::SeqCst);
                while !probe.stopping() {
                    if entry_check(me) {
                        return 1;
                    }
                    park(me, AfterSwitch::Requeue);
                }
                0
            }));
        }
        assert!(wait_until(|| started.load(Ordering::SeqCst) == 3));
        std::thread::sleep(Duration::from_micros(
            u64::try_from(round % 7).unwrap() * 300,
        ));
        let begun = Instant::now();
        runtime.shutdown();
        assert!(
            begun.elapsed() < CAP,
            "round {round}: the stop took {:?}",
            begun.elapsed()
        );
        assert!(shared.threads.lock().is_empty(), "round {round}");
        assert_eq!(
            shared.live.load(Ordering::SeqCst),
            0,
            "round {round}: a runnable fiber was stranded"
        );
    }
}

/// D-687, D-795 (fixer-w3-1 of checkpoint 3, D-821),
/// without a race: fiber F runs on worker 0 while the runtime stops, and
/// worker 1, idle, ends at the stop; the probe holds worker 1 right after
/// its end while F yields. In the migration stress mode F's yield goes to
/// another live worker, and worker 1's slot is `DEAD` now, so F goes to
/// the global queue instead, worker 0 runs it, and no fiber is left.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_yield_after_a_worker_ended_at_a_stop_runs_on_a_live_one() {
    let runtime = tuned(2, true, true);
    let shared = Arc::clone(&runtime.shared);
    // Worker 1's index plus one.
    shared.probe.hold_stop_exit.store(2, Ordering::SeqCst);
    let on_zero = Arc::new(AtomicBool::new(false));
    let outcome = {
        let (probe, on_zero) = (Arc::clone(&shared), Arc::clone(&on_zero));
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let begun = Instant::now();
            while me.sched.worker() != 0 {
                if begun.elapsed() > CAP {
                    return failed::REACHED;
                }
                park(me, AfterSwitch::Requeue);
            }
            on_zero.store(true, Ordering::SeqCst);
            // Stays on worker 0 with no entry check, so no preemption
            // moves it, until worker 1 has ended at the stop.
            let begun = Instant::now();
            while !probe.probe.stop_exit_held.load(Ordering::SeqCst) {
                if begun.elapsed() > CAP {
                    return failed::HELD;
                }
                std::hint::spin_loop();
            }
            let mut failures = 0;
            if life_of(&probe, 1) != Some(life::DEAD) {
                failures |= failed::ENDED;
            }
            park(me, AfterSwitch::Requeue);
            if me.sched.worker() != 0 {
                failures |= failed::MOVED;
            }
            probe.probe.stop_exit_release.store(true, Ordering::SeqCst);
            failures
        })
    };
    assert!(wait_until(|| on_zero.load(Ordering::SeqCst)));
    let begun = Instant::now();
    runtime.shutdown();
    assert!(begun.elapsed() < CAP, "the stop took {:?}", begun.elapsed());
    match outcome.try_recv() {
        Ok(EntryOutcome::Returned(word)) => assert_eq!(word, 0, "failures {word:#b}"),
        other => panic!("the yielding fiber did not end: {other:?}"),
    }
    assert!(shared.probe.stop_exit_held.load(Ordering::SeqCst));
    assert_eq!(
        shared.live.load(Ordering::SeqCst),
        0,
        "a fiber was stranded"
    );
    assert_eq!(shared.left_at_stop.load(Ordering::SeqCst), 0);
    assert!(shared.threads.lock().is_empty());
}

/// D-821, D-687, D-786, D-710: a
/// fiber pinned to its worker (a kernel callback on its stack) is parked
/// when the host stops the runtime, and the host wakes it after that
/// worker ended at the stop (a host-API case: hosts of Cheby code tear
/// their groups down first). Only that worker may run it, so it is left
/// alone and counted, never fatal; it never runs again, its control block
/// stays, and the stop returns.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn thread_count_a_pinned_fiber_woken_after_its_worker_ended_at_a_stop_is_left_and_counted() {
    let runtime = tuned(2, false, true);
    let shared = Arc::clone(&runtime.shared);
    let gate = Arc::new(Mailbox::default());
    // The worker the fiber is pinned to, plus one; 0 until then.
    let pinned_on = Arc::new(AtomicUsize::new(0));
    let resumed = Arc::new(AtomicBool::new(false));
    let outcome = {
        let (gate, pinned_on, resumed) = (
            Arc::clone(&gate),
            Arc::clone(&pinned_on),
            Arc::clone(&resumed),
        );
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            me.sched.enter_callback();
            pinned_on.store(me.sched.worker() + 1, Ordering::SeqCst);
            gate.receive(me);
            resumed.store(true, Ordering::SeqCst);
            leave_callback(me);
            0
        })
    };
    assert!(wait_until(|| pinned_on.load(Ordering::SeqCst) != 0));
    let index = pinned_on.load(Ordering::SeqCst) - 1;
    assert!(wait_until(|| {
        shared.worker(index).is_some_and(|worker| {
            worker.pinned() == 1 && worker.running().is_null() && !worker.has_waiting()
        })
    }));
    shared
        .probe
        .hold_stop_exit
        .store(index + 1, Ordering::SeqCst);
    // The host's wake, on another thread, once the fiber's worker has
    // ended at the stop and while the stop joins it.
    let waker = {
        let (shared, gate) = (Arc::clone(&shared), Arc::clone(&gate));
        std::thread::spawn(move || {
            let held = wait_until(|| shared.probe.stop_exit_held.load(Ordering::SeqCst));
            let dead = life_of(&shared, index) == Some(life::DEAD);
            if held {
                gate.send(1, false);
            }
            let counted = shared.left_at_stop.load(Ordering::SeqCst);
            shared.probe.stop_exit_release.store(true, Ordering::SeqCst);
            (held, dead, counted)
        })
    };
    let begun = Instant::now();
    runtime.shutdown();
    assert!(begun.elapsed() < CAP, "the stop took {:?}", begun.elapsed());
    let (held, dead, counted) = waker.join().unwrap();
    assert!(held, "the fiber's worker did not end at the stop");
    assert!(dead, "the worker that ended at the stop is not DEAD");
    assert_eq!(counted, 1, "the pinned fiber was not counted");
    assert_eq!(shared.left_at_stop.load(Ordering::SeqCst), 1);
    assert!(!resumed.load(Ordering::SeqCst), "the pinned fiber ran");
    assert!(matches!(outcome.try_recv(), Err(mpsc::TryRecvError::Empty)));
    // Left alone: never run, requeued or retired.
    assert_eq!(shared.live.load(Ordering::SeqCst), 1);
    assert!(shared.threads.lock().is_empty());
}
