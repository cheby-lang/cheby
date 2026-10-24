//! `loom` models of a sleep (`time.rs`'s `sys_sleep`, D-983, D-1058,
//! D-857): the race between the cancellation walk and the sleep's timer
//! for the sleep's wait header, and the race between a timed sleep's
//! firing and the deadlock detector's two passes.
//!
//! As in `fiber_ops::select_models`, the model is the protocol, not the
//! runtime's code: loom atomics and mutexes stand for the fiber lock, the
//! header's word, the timer entry's state, the domain's timer count, the
//! fiber's park state and its resume count, and each step names what it
//! mirrors. loom 0.7 needs no `--cfg loom`, so the models build in the
//! shared `target/`:
//!
//! ```text
//! cargo test -p cheby_runtime fiber_ops::sleep_models
//! ```
//!
//! | Model step                               | Mirrors (under `crates/cheby_runtime/src/`)                              |
//! | ---------------------------------------- | ------------------------------------------------------------------------ |
//! | the sleep: publish, arm, park            | `time.rs` `sleep`: `cancel::publish`, `Shared::arm_timer`, `park`        |
//! | the sleep: unpublish, cancel, read       | `time.rs` `sleep` after `park`: `unpublish`, `TimerHandle::cancel`       |
//! | the walk                                 | `fiber_ops/cancel.rs` `reach`, then `cancel`'s wake after the scope lock |
//! | the timer                                | `sched/timer.rs` `Timers::fire_due`, then `time.rs` `SleepTimer::fire`   |
//! | `Fiber::after_switch`, `wake`, `resume`  | `sched/worker.rs` after `AfterSwitch::Park`; `sched::wake`; `run_fiber`  |
//! | the detector's passes                    | `sched/deadlock.rs` steps 3 and 4: state, resumes, then the counts       |
//!
//! Invariants of the first model: exactly one of the walk and the timer
//! wins a published header and wakes the sleep once; the sleep unwinds
//! exactly when the walk won; a sleep refused at publication arms no timer
//! and is never woken; the domain's timer count is back to 0 once every
//! side is done. Of the second: a timed sleep is never confirmed as a
//! deadlock, because its timer's count falls only after the wake (D-857).
//! The `#[should_panic]` twin lowers the count before the wake, as the
//! runtime did before the H1 fix, and loom finds the confirmation.

// Probes: `std` atomics that only count what happened in an execution.
use std::sync::atomic as probe;

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex, Notify};
use loom::thread;

/// A fiber's park states, as `sched/fiber.rs`'s `state` module numbers
/// them.
mod park {
    pub(super) const RUNNABLE: u8 = 0;
    pub(super) const RUNNING: u8 = 1;
    pub(super) const PARKING: u8 = 2;
    pub(super) const PARKING_WOKEN: u8 = 3;
    pub(super) const PARKED: u8 = 4;
}

/// The header's states, as `sched/wait.rs`'s `WaitState` numbers them.
mod state {
    pub(super) const WAITING: u8 = 0;
    pub(super) const CLAIMED: u8 = 1;
    pub(super) const CANCELLED: u8 = 3;
}

/// A timer entry's states (`sched/timer.rs`).
mod timer {
    pub(super) const ARMED: u8 = 0;
    pub(super) const CANCELLED: u8 = 1;
    pub(super) const FIRED: u8 = 2;
}

/// The preemption bound of the default run, unless `LOOM_MAX_PREEMPTIONS`
/// says otherwise. A deeper run:
/// `LOOM_MAX_PREEMPTIONS=3 cargo test -p cheby_runtime --release fiber_ops::sleep_models`.
const PREEMPTIONS: usize = 2;

/// A model checker with the default preemption bound.
fn builder() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(PREEMPTIONS);
    }
    builder
}

/// The sleep's header (`WaitHeader`, one arm).
#[derive(Debug)]
struct Header(AtomicU64);

impl Header {
    fn new() -> Self {
        Self(AtomicU64::new(u64::from(state::WAITING)))
    }

    /// `try_settle_arm(Claimed, 0)` or `try_cancel`.
    fn try_settle(&self, to: u8) -> bool {
        self.0
            .compare_exchange(
                u64::from(state::WAITING),
                u64::from(to),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn get(&self) -> u8 {
        u8::try_from(self.0.load(Ordering::Acquire) & 0xFF).unwrap()
    }
}

/// The parts of the sleeping fiber that the races touch.
#[derive(Debug)]
struct Fiber {
    /// The fiber lock over `targeted` and `published`.
    lock: Mutex<()>,
    /// Whether the walk set a target, under `lock`.
    targeted: AtomicBool,
    /// Whether the header is published, under `lock`.
    published: AtomicBool,
    /// The park state.
    state: AtomicU8,
    /// The resume count (D-860), stored by the worker before the switch.
    resumes: AtomicU64,
    /// Wakes that found the fiber parked or parking.
    wakes: probe::AtomicUsize,
    /// Resumes a fiber that `after_switch` left parked.
    queue: Notify,
}

impl Fiber {
    fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            targeted: AtomicBool::new(false),
            published: AtomicBool::new(false),
            state: AtomicU8::new(park::RUNNING),
            resumes: AtomicU64::new(0),
            wakes: probe::AtomicUsize::new(0),
            queue: Notify::new(),
        }
    }

    /// The worker after the switch: `PARKING -> PARKED`, or runnable at
    /// once after a wake while parking. Returns whether the fiber is
    /// parked.
    fn after_switch(&self) -> bool {
        match self.state.compare_exchange(
            park::PARKING,
            park::PARKED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(park::PARKING_WOKEN) => {
                self.state.store(park::RUNNABLE, Ordering::SeqCst);
                false
            }
            Err(other) => panic!("a parking fiber was in the state {other}"),
        }
    }

    /// Parks until a wake made the fiber runnable, then resumes it as a
    /// worker does: the resume count first, then `RUNNING`.
    fn park_and_resume(&self) {
        if self.after_switch() {
            // loom's `Notify` may wake spuriously: the fiber runs again
            // only once a wake made it runnable.
            while self.state.load(Ordering::SeqCst) != park::RUNNABLE {
                self.queue.wait();
            }
        }
        let resumes = self.resumes.load(Ordering::Relaxed);
        self.resumes
            .store(resumes.wrapping_add(1), Ordering::Relaxed);
        self.state.store(park::RUNNING, Ordering::SeqCst);
    }

    /// `sched::wake`, by the side that won the header, with no lock held.
    fn wake(&self) {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                park::PARKED => park::RUNNABLE,
                park::PARKING => park::PARKING_WOKEN,
                other => panic!("a sleep was woken in the park state {other}: woken twice"),
            };
            match self
                .state
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        self.wakes.fetch_add(1, Ordering::SeqCst);
        if current == park::PARKED {
            self.queue.notify();
        }
    }
}

/// The armed timer: its entry's state and the domain's timer count.
#[derive(Debug)]
struct Timer {
    state: AtomicU8,
    count: Arc<AtomicUsize>,
}

impl Timer {
    /// `Timers::arm`: the domain's count rises under the heap's lock.
    fn arm(count: &Arc<AtomicUsize>) -> Arc<Self> {
        count.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            state: AtomicU8::new(timer::ARMED),
            count: Arc::clone(count),
        })
    }

    /// `fire_due` and `SleepTimer::fire`: `Armed -> Fired`, settle and
    /// wake, and only then the count falls, unless `early` lowers it
    /// before the wake (the bug of the twin).
    fn fire(&self, header: &Header, fiber: &Fiber, early: bool) -> bool {
        let fired = self
            .state
            .compare_exchange(
                timer::ARMED,
                timer::FIRED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if !fired {
            return false;
        }
        if early {
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
        let won = header.try_settle(state::CLAIMED);
        if won {
            fiber.wake();
        }
        if !early {
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
        won
    }

    /// `TimerHandle::cancel`: `Armed -> Cancelled`, and the count falls.
    fn cancel(&self) {
        if self
            .state
            .compare_exchange(
                timer::ARMED,
                timer::CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// How a sleep ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slept {
    /// `cancel::publish` refused: it unwound before waiting.
    Refused,
    /// Its timer woke it: it returns.
    Woke,
    /// The walk won: it unwinds.
    Cancelled,
}

/// The sleep's steps up to its park, as `time.rs`'s `sleep` takes them:
/// `begin_park`, `cancel::publish`, then the timer is armed. Returns the
/// timer, or `None` when the publication was refused (the fiber then
/// stored `RUNNING` back and armed nothing).
fn publish_and_arm(fiber: &Fiber, count: &Arc<AtomicUsize>) -> Option<Arc<Timer>> {
    fiber.state.store(park::PARKING, Ordering::SeqCst);
    let lock = fiber.lock.lock().unwrap();
    if fiber.targeted.load(Ordering::Relaxed) {
        drop(lock);
        fiber.state.store(park::RUNNING, Ordering::SeqCst);
        return None;
    }
    fiber.published.store(true, Ordering::Relaxed);
    drop(lock);
    Some(Timer::arm(count))
}

/// The sleep's steps once woken: unpublish, cancel the timer, read the
/// header.
fn finish(fiber: &Fiber, timer: &Timer, header: &Header) -> Slept {
    {
        let _lock = fiber.lock.lock().unwrap();
        fiber.published.store(false, Ordering::Relaxed);
    }
    timer.cancel();
    match header.get() {
        state::CLAIMED => Slept::Woke,
        state::CANCELLED => Slept::Cancelled,
        other => panic!("the sleep woke with the header {other}"),
    }
}

/// The first model: a sleep with a timer, the walk and the timer race.
fn cancel_and_timer_race() {
    builder().check(|| {
        let header = Arc::new(Header::new());
        let fiber = Arc::new(Fiber::new());
        let count = Arc::new(AtomicUsize::new(0));
        let walk_won = Arc::new(probe::AtomicBool::new(false));

        // The walk: under the fiber lock, set the target and cancel a
        // published header; wake after unlocking.
        let walk = {
            let (header, fiber, walk_won) = (
                Arc::clone(&header),
                Arc::clone(&fiber),
                Arc::clone(&walk_won),
            );
            thread::spawn(move || {
                let lock = fiber.lock.lock().unwrap();
                fiber.targeted.store(true, Ordering::Relaxed);
                let won =
                    fiber.published.load(Ordering::Relaxed) && header.try_settle(state::CANCELLED);
                drop(lock);
                if won {
                    walk_won.store(true, probe::Ordering::SeqCst);
                    fiber.wake();
                }
            })
        };

        // The sleep, on this thread.
        let (slept, timer_thread) = match publish_and_arm(&fiber, &count) {
            None => (Slept::Refused, None),
            Some(armed) => {
                let timer_thread = {
                    let (armed, header, fiber) =
                        (Arc::clone(&armed), Arc::clone(&header), Arc::clone(&fiber));
                    thread::spawn(move || armed.fire(&header, &fiber, false))
                };
                fiber.park_and_resume();
                (finish(&fiber, &armed, &header), Some(timer_thread))
            }
        };

        walk.join().unwrap();
        let timer_won = timer_thread.is_some_and(|thread| thread.join().unwrap());
        let walk_won = walk_won.load(probe::Ordering::SeqCst);
        let wakes = fiber.wakes.load(probe::Ordering::SeqCst);
        match slept {
            Slept::Refused => {
                assert_eq!(wakes, 0, "a refused sleep was woken");
                assert!(!walk_won && !timer_won);
            }
            Slept::Woke => assert!(timer_won && !walk_won && wakes == 1, "{wakes} wakes"),
            Slept::Cancelled => assert!(walk_won && !timer_won && wakes == 1, "{wakes} wakes"),
        }
        assert_eq!(count.load(Ordering::SeqCst), 0, "the timer count stayed");
    });
}

/// The second model: a timed sleep and the detector's two passes; with
/// `early`, the timer's count falls before its wake.
fn detector_never_confirms_a_timed_sleep(early: bool) {
    builder().check(move || {
        let header = Arc::new(Header::new());
        let fiber = Arc::new(Fiber::new());
        let count = Arc::new(AtomicUsize::new(0));
        let armed = publish_and_arm(&fiber, &count).expect("nothing cancels here");
        let timer_thread = {
            let (armed, header, fiber) =
                (Arc::clone(&armed), Arc::clone(&header), Arc::clone(&fiber));
            thread::spawn(move || armed.fire(&header, &fiber, early))
        };
        // The detector: a pass reads the state, stops unless `PARKED`,
        // then the resume count, then the domain's count after the walk.
        let detector = {
            let (fiber, count) = (Arc::clone(&fiber), Arc::clone(&count));
            thread::spawn(move || {
                let pass = || {
                    if fiber.state.load(Ordering::SeqCst) != park::PARKED {
                        return None;
                    }
                    let resumes = fiber.resumes.load(Ordering::Relaxed);
                    (count.load(Ordering::SeqCst) == 0).then_some(resumes)
                };
                let first = pass();
                first.is_some() && first == pass()
            })
        };
        fiber.park_and_resume();
        assert_eq!(finish(&fiber, &armed, &header), Slept::Woke);
        assert!(timer_thread.join().unwrap());
        let confirmed = detector.join().unwrap();
        assert!(!confirmed, "a timed sleep was confirmed as a deadlock");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn sleep_model_cancel_and_timer_have_one_winner() {
    cancel_and_timer_race();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn sleep_model_a_timed_sleep_is_never_a_deadlock() {
    detector_never_confirms_a_timed_sleep(false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "confirmed as a deadlock")]
fn sleep_model_a_count_that_falls_before_the_wake_is_confirmed() {
    detector_never_confirms_a_timed_sleep(true);
}
