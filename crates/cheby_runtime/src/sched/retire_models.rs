//! `loom` models of the retire-and-resume handshake (D-795, D-657, D-737;
//! review finding CONC3-1 of checkpoint 3's gate 3): a retirement that
//! `set_thread_count` asks for while its worker resumes a fiber is never
//! lost. The worker looks at its slot's life word at the top of its loop,
//! with a `Relaxed` load (a hint, S7's K1); then it stores the fiber it
//! resumes in `running` and reads the life word again, exactly.
//! `Shared::retire_worker` stores `RETIRING` and then reads `running`. So
//! either the worker sees `RETIRING` (at the top, and leaves, or after
//! publishing the fiber, and asks it to yield), or `retire_worker` sees the
//! fiber and asks it to yield (`YIELD`, never `SAFEPOINT`, C-5); a fiber
//! asked to yield gives the worker back at its next entry check even when
//! it runs alone (D-737), and the top of the loop then sees `RETIRING`,
//! even with its `Relaxed` load: the request it acted on was ordered after
//! the `RETIRING` store, or its own exact load had seen it.
//!
//! Each model is the protocol, not the runtime's code: `loom` atomics
//! stand for the slot's life word, the worker's `running` pointer and the
//! fiber's yield request. The runtime's two sides use sequentially
//! consistent accesses (a store, then a load, each `SeqCst`), which the
//! single total order of `SeqCst` operations makes a Dekker pair; loom
//! checks `SeqCst` fences, not `SeqCst` accesses (it treats a `SeqCst`
//! store as a release store), so each side here has a `SeqCst` fence
//! between its store and its load, as `sched/park_models.rs`' model (4)
//! has. loom 0.7 needs no `--cfg loom`, so the models build with plain
//! `cargo test`:
//!
//! ```text
//! cargo test -p cheby_runtime sched::retire_models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step              | Mirrors (path under `crates/cheby_runtime/src/`, function)                         |
//! | ----------------------- | ---------------------------------------------------------------------------------- |
//! | `Slot::resume`          | `sched/worker.rs` `Worker::run` (the look at the loop's top), `Worker::run_fiber`  |
//! | `Slot::begin_leaving`   | `sched/worker.rs` `WorkerShared::begin_leaving`, called by `Worker::leave`         |
//! | `Slot::retire`          | `sched/mod.rs` `Shared::retire_worker` (`request_retire`, then `look_at_running`)  |
//! | `Slot::cancel`          | `sched/mod.rs` `Shared::set_thread_count` (`cancel_retire`)                        |
//!
//! Model (1) is the pair alone; it has a `#[should_panic]` twin whose
//! worker looks at its life word only at the top of its loop, before it
//! publishes the fiber (checkpoint 3's code before CONC3-1's fix): the
//! retirement is lost. Model (2) adds a third thread that takes the
//! retirement back: a retirement that still stands at the end was seen by
//! the worker or reached the running fiber, and the worker leaves only for
//! a retirement it took over (`RETIRING -> LEAVING`). Model (3) follows
//! the fiber asked to yield back to the top of the loop: the `Relaxed`
//! look there sees `RETIRING`, so the retirement costs at most that one
//! slice.

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering, fence};
use loom::thread;

/// A slot's life word, as `sched/worker.rs` numbers it.
mod life {
    pub(super) const LIVE: u8 = 0;
    pub(super) const RETIRING: u8 = 1;
    pub(super) const LEAVING: u8 = 2;
}

/// No fiber runs.
const NONE: usize = 0;
/// The fiber the worker resumes.
const FIBER: usize = 1;

/// Where the worker looks at its life word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// At the top of its loop and again after it stores `running` (the
    /// runtime's).
    TopAndAfterPublish,
    /// Only at the top of its loop: the twin.
    TopOnly,
}

/// How the worker's turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// It saw `RETIRING` at the top of its loop and began to leave.
    Left,
    /// It resumed the fiber.
    Resumed,
}

/// One worker slot and the fiber it resumes.
struct Slot {
    life: AtomicU8,
    running: AtomicUsize,
    /// The fiber's yield request (`ctx::request(.., YIELD)`), from either
    /// side.
    yield_requested: AtomicBool,
}

impl Slot {
    fn new() -> Self {
        Self {
            life: AtomicU8::new(life::LIVE),
            running: AtomicUsize::new(NONE),
            yield_requested: AtomicBool::new(false),
        }
    }

    /// One turn of the worker's loop with a fiber found: the look at the
    /// top (which leaves if it can take the retirement over, and otherwise
    /// goes on), then the resume.
    fn resume(&self, check: Check) -> Turn {
        if self.top_sees_retiring() && self.begin_leaving() {
            return Turn::Left;
        }
        self.running.store(FIBER, Ordering::Relaxed);
        if check == Check::TopAndAfterPublish {
            fence(Ordering::SeqCst);
            if self.life.load(Ordering::Relaxed) == life::RETIRING {
                self.yield_requested.store(true, Ordering::SeqCst);
            }
        }
        Turn::Resumed
    }

    /// The look at the top of the worker's loop: `Relaxed`, as
    /// `WorkerShared::life_hint`.
    fn top_sees_retiring(&self) -> bool {
        self.life.load(Ordering::Relaxed) == life::RETIRING
    }

    /// The fiber's entry check: whether it was asked to yield. The
    /// runtime's is `ctx::take`'s `SeqCst` swap of the reason word, which
    /// acquires the request.
    fn asked_to_yield(&self) -> bool {
        self.yield_requested.load(Ordering::SeqCst)
    }

    /// `RETIRING -> LEAVING`; false if the retirement was taken back.
    fn begin_leaving(&self) -> bool {
        self.life
            .compare_exchange(
                life::RETIRING,
                life::LEAVING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// `retire_worker`: marks the slot retiring, then asks the fiber it
    /// runs, if any, to yield.
    fn retire(&self) {
        if self
            .life
            .compare_exchange(
                life::LIVE,
                life::RETIRING,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        fence(Ordering::SeqCst);
        if self.running.load(Ordering::Relaxed) == FIBER {
            self.yield_requested.store(true, Ordering::SeqCst);
        }
    }

    /// `set_thread_count` raises the count again: takes back a retirement
    /// the worker has not taken over.
    fn cancel(&self) {
        let _ = self.life.compare_exchange(
            life::RETIRING,
            life::LIVE,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

/// At most three preemptions per execution, the loom documentation's
/// advice; these small models are exhaustive within it.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(f);
}

/// Model (1): the worker resumes a fiber while `retire_worker` runs.
/// Whatever the interleaving, the worker left, or the fiber it resumed was
/// asked to yield.
fn resume_against_retire(check: Check) {
    explore(move || {
        let slot = Arc::new(Slot::new());
        let retirer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.retire())
        };
        let turn = slot.resume(check);
        retirer.join().unwrap();
        assert!(
            turn == Turn::Left || slot.yield_requested.load(Ordering::SeqCst),
            "lost retirement"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn retire_model_1_a_retirement_racing_a_resume_is_seen() {
    resume_against_retire(Check::TopAndAfterPublish);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "lost retirement")]
fn retire_model_1_twin_looking_only_at_the_loop_top_loses_it() {
    resume_against_retire(Check::TopOnly);
}

/// Model (2): as (1), with a third thread that takes the retirement back.
/// A retirement that stands at the end reached the running fiber; a
/// worker that left took a retirement over; one taken back leaves the
/// worker live.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn retire_model_2_a_retirement_taken_back_meanwhile() {
    explore(|| {
        let slot = Arc::new(Slot::new());
        let retirer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.retire())
        };
        let canceller = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.cancel())
        };
        let turn = slot.resume(Check::TopAndAfterPublish);
        retirer.join().unwrap();
        canceller.join().unwrap();
        match slot.life.load(Ordering::SeqCst) {
            life::RETIRING => {
                assert_eq!(turn, Turn::Resumed);
                assert!(
                    slot.yield_requested.load(Ordering::SeqCst),
                    "lost retirement"
                );
            }
            life::LEAVING => assert_eq!(turn, Turn::Left),
            _ => assert_eq!(
                turn,
                Turn::Resumed,
                "left after the retirement was taken back"
            ),
        }
    });
}

/// Model (3): as (1), then the fiber's entry check and, when it was asked
/// to yield, the worker's next look at the top of its loop, `Relaxed`
/// (S7's K1): that look sees `RETIRING` and the worker leaves, whichever
/// side asked. A retirement nobody asked the fiber about was seen at the
/// first top.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn retire_model_3_the_yielded_fiber_s_worker_leaves_at_its_next_top() {
    explore(|| {
        let slot = Arc::new(Slot::new());
        let retirer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.retire())
        };
        let first = slot.resume(Check::TopAndAfterPublish);
        if first == Turn::Resumed && slot.asked_to_yield() {
            // The fiber yields; the worker clears `running` and is back at
            // the top of its loop.
            slot.running.store(NONE, Ordering::SeqCst);
            assert!(slot.top_sees_retiring(), "a stale look after the yield");
            assert!(slot.begin_leaving(), "a stale look after the yield");
        }
        retirer.join().unwrap();
        assert!(
            first == Turn::Left || slot.yield_requested.load(Ordering::SeqCst),
            "lost retirement"
        );
    });
}
