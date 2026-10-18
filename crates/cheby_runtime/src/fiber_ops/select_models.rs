//! A `loom` model of a `select`'s shared done header (D-206, D-670, D-673,
//! D-704, D-706; architecture §7.2 and §7.3; `fiber_ops/select.rs`): the
//! three-way race between a sender's hand-off (`settle`), the cancellation
//! walk (`cancel`) and the select's timer (`fire`), each a compare-and-swap
//! on the one header the select's nodes share.
//!
//! As in `fiber_ops::models`, the model is the protocol, not the runtime's
//! code: loom mutexes and atomics stand for the channel lock, the fiber
//! lock, the shared header's word (state and arm), the timer entry's state
//! and the fiber's park state, and each step names what it mirrors. loom
//! 0.7 needs no `--cfg loom`, so the model builds in the shared `target/`:
//!
//! ```text
//! cargo test -p cheby_runtime fiber_ops::select_models
//! ```
//!
//! loom runs the model on generator coroutines that switch stacks, so it is
//! ignored on Windows until M6, like the fiber tests.
//!
//! | Model step                                   | Mirrors (under `crates/cheby_runtime/src/`)                                                  |
//! | -------------------------------------------- | -------------------------------------------------------------------------------------------- |
//! | `Header::try_settle`, `try_cancel`, `get`    | `sched/wait.rs` `WaitHeader::try_settle_arm`, `try_cancel`, `state` and `arm`                |
//! | the select: lock, link, publish, arm, park   | `fiber_ops/select.rs` `wait`, steps 3 and 4 (`ChanInner::link_receiver`, `cancel::publish`)  |
//! | the select: unpublish, unlink, cancel, read  | `fiber_ops/select.rs` `wait`, step 5                                                         |
//! | the sender                                   | `fiber_ops/channel.rs` `send`: `WaitQueue::pop_settled`, the value, `wake_settled`           |
//! | the walk                                     | `fiber_ops/cancel.rs` `reach`, then `cancel`'s wake after the scope lock                     |
//! | the timer                                    | `sched/timer.rs` firing (`Armed -> Fired`), then `select.rs` `SelectTimer::fire`             |
//! | `Timer::cancel`                              | `sched/timer.rs` `TimerHandle::cancel` (`Armed -> Cancelled`)                                |
//! | `Fiber::after_switch`, `Fiber::wake`         | `sched/worker.rs` after `AfterSwitch::ParkAll`; `sched/mod.rs` `wake`                        |
//!
//! Invariants: exactly one of the three wins a published header and wakes
//! the select once, never lost; the sender's value is owned by exactly one
//! side (the select took it, or the sender still holds it for its channel,
//! D-206); a walk that wins leaves the select with nothing (D-670); a
//! select refused at publication arms no timer and is never woken. The
//! `#[should_panic]` twin settles with a load and a store instead of a
//! compare-and-swap, and loom finds the interleaving in which two of them
//! win, so that the value is lost.

// Probes: `std` atomics that only count what happened in an execution.
use std::sync::atomic as probe;

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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

/// The bits of the header's word below the arm (`sched/wait.rs`).
const ARM_SHIFT: u32 = 8;

/// The arm of the select's receive node.
const CHANNEL_ARM: u64 = 0;

/// The arm of the select's timer.
const TIMER_ARM: u64 = 1;

/// The preemption bound of the default run, unless `LOOM_MAX_PREEMPTIONS`
/// says otherwise: two keeps `cargo test --workspace` quick (four threads
/// race here), and the twin fails within it. A deeper run:
/// `LOOM_MAX_PREEMPTIONS=3 cargo test -p cheby_runtime --release fiber_ops::select_models`.
const PREEMPTIONS: usize = 2;

/// How a waker settles the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    /// One compare-and-swap `Waiting -> state | arm` (`try_settle_arm`).
    Swap,
    /// The bug: read `Waiting`, then store.
    LoadThenStore,
}

/// The shared header: the state in the low byte and the winning arm above.
#[derive(Debug)]
struct Header {
    word: AtomicU64,
    settle: Settle,
}

impl Header {
    fn new(settle: Settle) -> Self {
        Self {
            word: AtomicU64::new(u64::from(state::WAITING)),
            settle,
        }
    }

    /// A waker's `try_settle_arm(Claimed, arm)`.
    fn try_settle(&self, arm: u64) -> bool {
        let settled = (arm << ARM_SHIFT) | u64::from(state::CLAIMED);
        match self.settle {
            Settle::Swap => self
                .word
                .compare_exchange(
                    u64::from(state::WAITING),
                    settled,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok(),
            Settle::LoadThenStore => {
                if self.word.load(Ordering::Acquire) == u64::from(state::WAITING) {
                    self.word.store(settled, Ordering::Release);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// The walk's `try_cancel`.
    fn try_cancel(&self) -> bool {
        self.word
            .compare_exchange(
                u64::from(state::WAITING),
                u64::from(state::CANCELLED),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The state and the arm.
    fn get(&self) -> (u8, u64) {
        let word = self.word.load(Ordering::Acquire);
        (u8::try_from(word & 0xFF).unwrap(), word >> ARM_SHIFT)
    }
}

/// The parts of the select's fiber that the race touches.
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
            wakes: probe::AtomicUsize::new(0),
            queue: Notify::new(),
        }
    }

    /// The worker after the switch, once it released every channel lock:
    /// `PARKING -> PARKED`, or runnable at once after a wake while parking.
    /// Returns whether the fiber is parked.
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

    /// `sched::wake`, by the side that won the header, with no lock held.
    fn wake(&self) {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                park::PARKED => park::RUNNABLE,
                park::PARKING => park::PARKING_WOKEN,
                other => panic!("a select was woken in the park state {other}: woken twice"),
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

/// How the select ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selected {
    /// `cancel::publish` refused: it unwound before waiting.
    Refused,
    /// It took the sender's value.
    Value,
    /// Its timer arm was chosen.
    Timer,
    /// The walk won: it unwound, taking nothing.
    Cancelled,
}

/// The model: a select waits on one capacity-0 channel and a timer; a
/// sender, the walk and the timer race for its shared header.
#[expect(
    clippy::too_many_lines,
    reason = "a model reads top to bottom as its threads and its checks"
)]
fn three_way_race(settle: Settle) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(PREEMPTIONS);
    }
    builder.check(move || {
        let chan = Arc::new(Mutex::new(()));
        let linked = Arc::new(AtomicBool::new(false));
        let header = Arc::new(Header::new(settle));
        let fiber = Arc::new(Fiber::new());
        let timer_state = Arc::new(AtomicU8::new(timer::ARMED));
        // Wins of the header, and owners of the sender's value.
        let winners = Arc::new(probe::AtomicUsize::new(0));
        let sender_kept = Arc::new(probe::AtomicBool::new(false));

        // The sender: `send` on the capacity-0 channel. Under the channel
        // lock it pops the select's node if it is linked and settles the
        // shared header for the channel's arm; the winner hands its value
        // over and wakes the select after unlocking. A loser keeps its
        // value (it would wait in `sendq` with it, D-206).
        let sender = {
            let (chan, linked, header, fiber, winners, kept) = (
                Arc::clone(&chan),
                Arc::clone(&linked),
                Arc::clone(&header),
                Arc::clone(&fiber),
                Arc::clone(&winners),
                Arc::clone(&sender_kept),
            );
            thread::spawn(move || {
                let guard = chan.lock().unwrap();
                let won = linked.load(Ordering::Relaxed) && {
                    linked.store(false, Ordering::Relaxed);
                    header.try_settle(CHANNEL_ARM)
                };
                drop(guard);
                if won {
                    winners.fetch_add(1, probe::Ordering::SeqCst);
                    fiber.wake();
                } else {
                    kept.store(true, probe::Ordering::SeqCst);
                }
            })
        };

        // The walk: under the fiber lock, set the target and cancel a
        // published header; wake after unlocking.
        let walk = {
            let (header, fiber, winners) = (
                Arc::clone(&header),
                Arc::clone(&fiber),
                Arc::clone(&winners),
            );
            thread::spawn(move || {
                let lock = fiber.lock.lock().unwrap();
                fiber.targeted.store(true, Ordering::Relaxed);
                let won = fiber.published.load(Ordering::Relaxed) && header.try_cancel();
                drop(lock);
                if won {
                    winners.fetch_add(1, probe::Ordering::SeqCst);
                    fiber.wake();
                }
            })
        };

        // The select, on this thread.
        let guard = chan.lock().unwrap();
        // `begin_park`, then link the node under the channel lock.
        fiber.state.store(park::PARKING, Ordering::SeqCst);
        linked.store(true, Ordering::Relaxed);
        // `cancel::publish`.
        let lock = fiber.lock.lock().unwrap();
        let refused = fiber.targeted.load(Ordering::Relaxed);
        let (selected, timer_thread) = if refused {
            drop(lock);
            linked.store(false, Ordering::Relaxed);
            drop(guard);
            fiber.state.store(park::RUNNING, Ordering::SeqCst);
            (Selected::Refused, None)
        } else {
            fiber.published.store(true, Ordering::Relaxed);
            drop(lock);
            // Arm the timer under the channel lock: from now on it may
            // fire, with no lock held.
            let timer_thread = {
                let (header, fiber, winners, timer_state) = (
                    Arc::clone(&header),
                    Arc::clone(&fiber),
                    Arc::clone(&winners),
                    Arc::clone(&timer_state),
                );
                thread::spawn(move || {
                    let fired = timer_state
                        .compare_exchange(
                            timer::ARMED,
                            timer::FIRED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok();
                    if fired && header.try_settle(TIMER_ARM) {
                        winners.fetch_add(1, probe::Ordering::SeqCst);
                        fiber.wake();
                    }
                })
            };
            // `park(ParkAll(_))`: the worker releases the channel lock
            // after the switch, then settles the park state.
            drop(guard);
            if fiber.after_switch() {
                // loom's `Notify` may wake spuriously: the fiber runs again
                // only once a wake made it runnable.
                while fiber.state.load(Ordering::SeqCst) != park::RUNNABLE {
                    fiber.queue.wait();
                }
            }
            // Woken: unpublish, unlink, cancel the timer, then read.
            {
                let _lock = fiber.lock.lock().unwrap();
                fiber.published.store(false, Ordering::Relaxed);
            }
            {
                let _guard = chan.lock().unwrap();
                linked.store(false, Ordering::Relaxed);
            }
            let _ = timer_state.compare_exchange(
                timer::ARMED,
                timer::CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            let selected = match header.get() {
                (state::CLAIMED, CHANNEL_ARM) => Selected::Value,
                (state::CLAIMED, TIMER_ARM) => Selected::Timer,
                (state::CANCELLED, _) => Selected::Cancelled,
                other => panic!("the select woke with the header {other:?}"),
            };
            (selected, Some(timer_thread))
        };

        sender.join().unwrap();
        walk.join().unwrap();
        if let Some(timer_thread) = timer_thread {
            timer_thread.join().unwrap();
        }

        let wins = winners.load(probe::Ordering::SeqCst);
        let wakes = fiber.wakes.load(probe::Ordering::SeqCst);
        let kept = sender_kept.load(probe::Ordering::SeqCst);
        if selected == Selected::Refused {
            assert_eq!((wins, wakes), (0, 0), "a refused select was woken");
            assert_eq!(header.get().0, state::WAITING);
        } else {
            assert_eq!(
                wins, 1,
                "the header was won twice: {wins} wakers ({selected:?})"
            );
            assert_eq!(wakes, 1, "the select was woken {wakes} times");
        }
        // The value has exactly one owner: the select took it, or the
        // sender kept it for its channel.
        assert!(
            (selected == Selected::Value) != kept,
            "the sender's value was lost or taken twice ({selected:?}, kept {kept})"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn select_model_shared_header_has_one_winner() {
    three_way_race(Settle::Swap);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "twice")]
fn select_model_settling_without_a_swap_loses_the_value() {
    three_way_race(Settle::LoadThenStore);
}
