//! `loom` models of the blocking pool's hand-off (D-683, survey-io §3.2,
//! `sched/blocking.rs`): a fiber stores `PARKING`, marks itself (D-786's
//! mark), submits its job and switches away, and its worker ends the
//! park; a pool thread takes the job, stores the result, clears the mark
//! and wakes the fiber. Whatever the interleaving: the wake is never lost
//! (the fiber is scheduled once the job returned), it happens exactly once,
//! the mark is clear and the result stored when the fiber is scheduled,
//! and a completion that finds the runtime stopping wakes nothing.
//!
//! Each model is the protocol, not the runtime's code: `loom` atomics for
//! the fiber's state word and its mark, a `loom` mutex and condition
//! variable for the pool's queue (one job slot), and a counter for the
//! fiber's entries into a run queue (`Shared::schedule`). loom 0.7 needs no
//! `--cfg loom`, so the models build with plain `cargo test`:
//!
//! ```text
//! cargo test -p cheby_runtime pool::models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step                    | Mirrors (path under `crates/cheby_runtime/src/`, function)          |
//! | ----------------------------- | ------------------------------------------------------------------- |
//! | `Fiber::submit_and_park`      | `sched/blocking.rs` `on_pool` (`begin_park`, mark, submit, `park`)  |
//! | `Fiber::end_park`             | `sched/worker.rs` `Worker::parked`                                  |
//! | `Pool::serve`                 | `pool/mod.rs` `Pool::serve`, then `sched/blocking.rs` `complete`    |
//! | `Fiber::wake`                 | `sched/mod.rs` `wake` (`PARKED -> RUNNABLE`, `PARKING -> PARKING_WOKEN`) |
//! | `Fiber::schedule`             | `sched/mod.rs` `Shared::schedule`                                   |
//!
//! Two `#[should_panic]` twins show the models can fail: one whose fiber
//! stores `PARKING` only after it submitted (the job may complete while the
//! fiber still runs, and the wake is lost), and one whose pool thread clears
//! the mark after the wake (a scheduled fiber is still marked, so a
//! teardown would skip it).
//!
//! A third model checks a worker's exit at a stop against a completion
//! (`sched/worker.rs` `Worker::run`, `sched/blocking.rs` `holds_stop` and
//! `leave`, GAP(pool-wakes-only-running)): the worker runs the fiber the
//! completion queues before it ends. Its two twins (a completion that
//! lowers the count before it queues, a worker that ends without looking
//! again) strand the fiber.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

/// The fiber states the hand-off uses, as `sched/fiber.rs` numbers them.
mod state {
    pub(super) const RUNNABLE: u8 = 0;
    pub(super) const RUNNING: u8 = 1;
    pub(super) const PARKING: u8 = 2;
    pub(super) const PARKING_WOKEN: u8 = 3;
    pub(super) const PARKED: u8 = 4;
}

/// Which variant of the protocol a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    /// The runtime's protocol.
    Runtime,
    /// The twin that stores `PARKING` after submitting.
    ParkAfterSubmit,
    /// The twin that clears the mark after the wake.
    ClearAfterWake,
}

/// One fiber waiting on one job.
struct Fiber {
    state: AtomicU8,
    /// D-786's mark.
    blocking: AtomicBool,
    /// The job's result.
    result: Mutex<Option<u32>>,
    /// Entries into a run queue.
    scheduled: AtomicUsize,
    /// The runtime stops or halts (`Shared::stopping`).
    stopping: AtomicBool,
}

impl Fiber {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(state::RUNNING),
            blocking: AtomicBool::new(false),
            result: Mutex::new(None),
            scheduled: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
        }
    }

    /// `Shared::schedule`: the fiber enters a run queue. It must be
    /// unmarked and its result stored.
    fn schedule(&self) {
        assert!(
            !self.blocking.load(Ordering::SeqCst),
            "a scheduled fiber is still marked as blocked"
        );
        assert!(
            self.result.lock().unwrap().is_some(),
            "a fiber was scheduled before its result"
        );
        self.scheduled.fetch_add(1, Ordering::SeqCst);
    }

    /// `sched::wake`, from the pool thread (no worker: the global queue).
    fn wake(&self) {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                state::PARKED => state::RUNNABLE,
                state::PARKING => state::PARKING_WOKEN,
                // A fiber that still runs: the wake does nothing (the
                // twin's lost wake).
                _ => return,
            };
            match self
                .state
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        if current == state::PARKED {
            self.schedule();
        }
    }

    /// The fiber's side up to the switch: `PARKING`, the mark, the submit.
    fn submit_and_park(&self, pool: &Pool, variant: Variant) {
        if variant != Variant::ParkAfterSubmit {
            self.state.store(state::PARKING, Ordering::SeqCst);
        }
        self.blocking.store(true, Ordering::SeqCst);
        pool.submit();
        if variant == Variant::ParkAfterSubmit {
            // `park`'s own `RUNNING -> PARKING`.
            let _ = self.state.compare_exchange(
                state::RUNNING,
                state::PARKING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    /// `Worker::parked`, after the switch.
    fn end_park(&self) {
        match self.state.compare_exchange(
            state::PARKING,
            state::PARKED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(state::PARKING_WOKEN) => {
                self.state.store(state::RUNNABLE, Ordering::SeqCst);
                self.schedule();
            }
            Err(other) => panic!("a parking fiber was in the state {other}"),
        }
    }
}

/// The pool's queue with one job slot.
#[derive(Default)]
struct Pool {
    queued: Mutex<bool>,
    work: Condvar,
}

impl Pool {
    fn submit(&self) {
        *self.queued.lock().unwrap() = true;
        self.work.notify_one();
    }

    /// A pool thread: takes the job, runs it, completes it.
    fn serve(&self, fiber: &Fiber, variant: Variant) {
        let mut queued = self.queued.lock().unwrap();
        while !*queued {
            queued = self.work.wait(queued).unwrap();
        }
        *queued = false;
        drop(queued);
        // The job, then its completion.
        *fiber.result.lock().unwrap() = Some(42);
        if variant != Variant::ClearAfterWake {
            fiber.blocking.store(false, Ordering::SeqCst);
        }
        if !fiber.stopping.load(Ordering::SeqCst) {
            fiber.wake();
        }
        if variant == Variant::ClearAfterWake {
            fiber.blocking.store(false, Ordering::SeqCst);
        }
    }
}

/// At most three preemptions per execution, the loom documentation's
/// advice.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(f);
}

/// One fiber, one pool thread: the fiber is scheduled exactly once, after
/// its result and with its mark clear. With `stopping`, a host has begun
/// to stop the runtime first, and nothing is scheduled.
fn hand_off(variant: Variant, stopping: bool) {
    explore(move || {
        let fiber = Arc::new(Fiber::new());
        fiber.stopping.store(stopping, Ordering::SeqCst);
        let pool = Arc::new(Pool::default());
        let server = {
            let (fiber, pool) = (Arc::clone(&fiber), Arc::clone(&pool));
            thread::spawn(move || pool.serve(&fiber, variant))
        };
        fiber.submit_and_park(&pool, variant);
        fiber.end_park();
        server.join().unwrap();
        let expected = usize::from(!stopping);
        assert_eq!(
            fiber.scheduled.load(Ordering::SeqCst),
            expected,
            "the fiber was scheduled a wrong number of times (a lost or double wake)"
        );
        assert!(
            !fiber.blocking.load(Ordering::SeqCst),
            "the mark stayed set"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_the_completion_wakes_once_after_clearing_the_mark() {
    hand_off(Variant::Runtime, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_a_completion_after_the_stop_wakes_nothing() {
    hand_off(Variant::Runtime, true);
}

#[test]
#[should_panic(expected = "a lost or double wake")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_twin_parking_after_the_submit_loses_the_wake() {
    hand_off(Variant::ParkAfterSubmit, false);
}

#[test]
#[should_panic(expected = "still marked as blocked")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_twin_clearing_the_mark_after_the_wake_is_caught() {
    hand_off(Variant::ClearAfterWake, false);
}

/// Which variant of the stop's exit a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopVariant {
    /// The runtime's protocol.
    Runtime,
    /// The twin whose completion lowers the count before it queues.
    CountFirst,
    /// The twin whose worker ends without looking again.
    NoSecondLook,
}

/// A worker's exit at a stop against one pool completion
/// (`sched/worker.rs` `Worker::run`'s stop exit, `sched/blocking.rs`
/// `holds_stop` and `leave`, GAP(pool-wakes-only-running)): one fiber waits
/// on the pool when the stop flag is already set. The completion queues
/// the fiber and then lowers the count; the worker does not end while the
/// count is above 0, and after it reads 0 it looks once more. Whatever the
/// interleaving, the worker runs the fiber before it ends: no fiber is
/// stranded in a queue that no worker looks at. Each twin can strand it.
fn stop_exit(variant: StopVariant) {
    explore(move || {
        let waiting = Arc::new(AtomicUsize::new(1));
        let queued = Arc::new(AtomicBool::new(false));
        let completion = {
            let (waiting, queued) = (Arc::clone(&waiting), Arc::clone(&queued));
            thread::spawn(move || {
                if variant == StopVariant::CountFirst {
                    waiting.fetch_sub(1, Ordering::SeqCst);
                    queued.store(true, Ordering::SeqCst);
                } else {
                    queued.store(true, Ordering::SeqCst);
                    waiting.fetch_sub(1, Ordering::SeqCst);
                }
            })
        };
        // The worker's loop at the stop: look for a fiber, then the exit.
        // (The look is a load: loom 0.7's `swap` can miss a store that
        // happens before it, which C++'s coherence rules forbid.)
        let mut looked = variant == StopVariant::NoSecondLook;
        let ran = loop {
            if queued.load(Ordering::SeqCst) {
                break true;
            }
            if waiting.load(Ordering::SeqCst) > 0 {
                looked = variant == StopVariant::NoSecondLook;
                thread::yield_now();
                continue;
            }
            if !looked {
                looked = true;
                continue;
            }
            break false;
        };
        completion.join().unwrap();
        assert!(ran, "a fiber queued by a pool completion was stranded");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_a_stop_runs_the_fiber_its_completion_queues() {
    stop_exit(StopVariant::Runtime);
}

#[test]
#[should_panic(expected = "was stranded")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_twin_lowering_the_count_first_strands_the_fiber() {
    stop_exit(StopVariant::CountFirst);
}

#[test]
#[should_panic(expected = "was stranded")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn pool_model_twin_ending_without_a_second_look_strands_the_fiber() {
    stop_exit(StopVariant::NoSecondLook);
}
