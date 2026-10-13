//! `loom` models of cancellation's races (D-730; the protocol is D-666,
//! D-667, D-668, D-670 and D-706, architecture §3.4, §7.2, §7.5 and §7.6).
//!
//! Each model is the protocol, not the runtime's code: a few
//! `loom::sync::Mutex`es and atomics stand for the fiber lock, a scope's
//! lock, a channel's or join cell's lock, a wait node's state and a fiber's
//! park state, and each step names the function it mirrors. loom 0.7 needs
//! no `--cfg loom` (its source has no `cfg(loom)` gating), so the models
//! build in the shared `target/` with plain `cargo test`:
//!
//! ```text
//! cargo test -p cheby_runtime fiber_ops::models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! Models (4) and (5) run with a preemption bound of three by default
//! ([`Search::Bounded`]). Two `#[ignore]`d variants search further and take
//! about a minute each optimized (minutes unoptimized): `model_4_exhaustive`
//! explores every interleaving, `model_5_deep` those with up to six
//! preemptions:
//!
//! ```text
//! cargo test -p cheby_runtime --release fiber_ops::models -- --include-ignored
//! ```
//!
//! | Model step                                          | Mirrors (path under `crates/cheby_runtime/src/`, function)                                        |
//! | --------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
//! | `Fiber::lock`, `target`, `wait`                     | `fiber_ops/cancel.rs` `FiberLock` (the fiber lock, D-706)                                         |
//! | `Fiber::set_target` (the outermost wins)            | `fiber_ops/cancel.rs` `retarget`, called by `reach` and `set_target`                              |
//! | `Fiber::request_cancel`                             | `ctx.rs` `request(ctx, reason::CANCEL)`                                                           |
//! | `Fiber::clear_cancel`                               | `ctx.rs` `clear_reason(ctx, reason::CANCEL)`                                                      |
//! | `Fiber::entry_check`                                | `panic.rs` `stack_slow` steps 1 and 3: `ctx.rs` `take`, then `fiber_ops/cancel.rs` `check`        |
//! | `Fiber::begin_park`, `Fiber::cancel_park`           | `sched/fiber.rs` `begin_park`, `cancel_park`                                                      |
//! | the CAS `RUNNING -> PARKING` before `after_switch`  | `sched/fiber.rs` `park`, for `AfterSwitch::Park`                                                  |
//! | `Fiber::after_switch`                               | `sched/worker.rs` `Worker::run_fiber`, the `AfterSwitch::Park` arm                                |
//! | `Fiber::wake`                                       | `sched/mod.rs` `wake`                                                                             |
//! | `Node::try_settle`, `Node::try_cancel`, `Node::get` | `sched/wait.rs` `WaitHeader::try_settle`, `try_cancel`, `state`                                   |
//! | unlink then settle; unlink if still linked          | `sched/wait.rs` `WaitQueue::pop_settled`; `WaitQueue::remove`                                     |
//! | model 1: `close_step_3`                             | `fiber_ops/cancel.rs` `at_close`, from `fiber_ops/scope.rs` `cheby_rt_scope_close` (step 3)       |
//! | model 1: the canceller                              | `fiber_ops/cancel.rs` `cancel` → `fiber_ops/scope.rs` `ScopeState::cancel_step` → `cancel::reach` |
//! | model 2: the waiter                                 | `fiber_ops/channel.rs` `send` (blocking path), with `cancel::publish` and `unpublish`             |
//! | model 2: the receiver                               | `fiber_ops/channel.rs` `receive` (`pop_settled`, then `wake_settled` after the unlock)            |
//! | model 2: the canceller                              | `fiber_ops/cancel.rs` `reach`, then `cancel`'s wake after the scope lock                          |
//! | model 3: reserve, fill                              | `fiber_ops/scope.rs` `ScopeState::join`, `ScopeState::fill`, from `fiber_ops/spawn.rs` `spawn`    |
//! | model 3: the spawner's target                       | `fiber_ops/cancel.rs` `set_target`, before `sched::schedule_new`                                  |
//! | model 3: the new fiber's start                      | `fiber_ops/base.rs` `fiber_main` (`cancel::check` before `f`)                                     |
//! | models 3 and 4: the walk                            | `fiber_ops/scope.rs` `ScopeState::cancel_step` (scope lock, then each fiber lock)                 |
//! | model 4: a panicker                                 | `unwind/record.rs` `start_panic` → `fiber_ops/scope.rs` `ScopeState::record_panic` → `cancel`     |
//! | model 4: the outer canceller                        | `fiber_ops/cancel.rs` `cancel` of the enclosing scope (its worklist reaches the nested one)       |
//! | model 5: the joiner                                 | `fiber_ops/base.rs` `JoinCell::join`, `fiber_ops/join.rs` `cheby_fiber_sys_join`                  |
//! | model 5: the finisher                               | `fiber_ops/base.rs` `JoinCell::finish(Outcome::Failed)` (wakes after the cell's lock)             |
//! | model 5: the canceller                              | `fiber_ops/cancel.rs` `reach`, then `cancel`'s wake                                               |
//!
//! Every model asserts the invariant survey-u5 §4.3 gives it. Models (1),
//! (2) and (3) have `#[should_panic]` twins with the ordering bugs they
//! guard against (clearing the target without the fiber lock; publishing
//! the node before `PARKING`, or `park` storing `PARKING` over a wake;
//! ignoring `fill`'s answer), which shows that each model can fail. The
//! models leave out what does not change their race: a scope's owner in
//! models (3) and (4), and the channel's buffer in model (2).

// The probes: `std` atomics for what a model only counts or asserts on,
// never for its protocol. loom runs one model thread at a time, so a probe
// reads what happened so far in the execution, and costs no branch.
use std::sync::atomic as probe;

use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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

/// A wait node's states, as `sched/wait.rs`'s `WaitState` numbers them.
mod node {
    pub(super) const WAITING: u8 = 0;
    pub(super) const CLAIMED: u8 = 1;
    pub(super) const CANCELLED: u8 = 3;
}

/// Scope identities for a cancellation target. `OUTER` encloses `INNER`.
const NONE: u8 = 0;
const OUTER: u8 = 1;
const INNER: u8 = 2;

/// Whether scope `a` is a proper descendant of scope `b`.
fn is_proper_descendant(a: u8, b: u8) -> bool {
    a == INNER && b == OUTER
}

/// The parts of a fiber control block that cancellation touches.
#[derive(Debug)]
struct Fiber {
    /// The fiber lock (D-706, architecture §3.1): a leaf lock over the
    /// target and the wait-node pointer.
    lock: Mutex<()>,
    /// The cancellation target, under `lock`.
    target: AtomicU8,
    /// Whether a wait node is published, under `lock`.
    wait: AtomicBool,
    /// The CANCEL reason bit.
    cancel_reason: AtomicBool,
    /// The forced stack limit that sends the next entry into the slow path.
    forced: AtomicBool,
    /// The park state.
    state: AtomicU8,
    /// Wakes that found the fiber parked or parking (a probe).
    wakes: probe::AtomicUsize,
    /// Times the fiber was put on a run queue after a park (a probe).
    enqueued: probe::AtomicUsize,
    /// The fiber has retired: its control block and stack are gone (a
    /// probe).
    retired: probe::AtomicBool,
    /// Resumes a fiber that `after_switch` left parked.
    queue: Notify,
}

impl Fiber {
    fn new(target: u8) -> Self {
        Self {
            lock: Mutex::new(()),
            target: AtomicU8::new(target),
            wait: AtomicBool::new(false),
            cancel_reason: AtomicBool::new(false),
            forced: AtomicBool::new(false),
            state: AtomicU8::new(park::RUNNING),
            wakes: probe::AtomicUsize::new(0),
            enqueued: probe::AtomicUsize::new(0),
            retired: probe::AtomicBool::new(false),
            queue: Notify::new(),
        }
    }

    /// `cancel`'s per-fiber step, under the fiber lock (the caller holds
    /// it): the target becomes `to` if it is unset or a descendant of `to`
    /// (D-667, the outermost wins).
    fn set_target(&self, to: u8) {
        let now = self.target.load(Ordering::Relaxed);
        if now == NONE || is_proper_descendant(now, to) {
            self.target.store(to, Ordering::Relaxed);
        }
    }

    /// `ctx::request(ctx, reason::CANCEL)`: the reason, then the forced
    /// limit.
    fn request_cancel(&self) {
        self.cancel_reason.store(true, Ordering::SeqCst);
        self.forced.store(true, Ordering::SeqCst);
    }

    /// `ctx::clear_reason(ctx, reason::CANCEL)`: the forced limit stays.
    fn clear_cancel(&self) {
        self.cancel_reason.store(false, Ordering::SeqCst);
    }

    /// An entry check on the fiber itself: the fast path compares the
    /// forced limit; the slow path takes the reasons (`ctx::take`) and its
    /// step 3 decides from the target, read under the fiber lock
    /// (`cancel::check`, level-triggered, D-688). Returns the target it
    /// delivers. In these models every CANCEL request comes with a target
    /// and every clear takes both, so a taken CANCEL without a target is a
    /// model bug.
    fn entry_check(&self) -> Option<u8> {
        if !self.forced.load(Ordering::SeqCst) {
            return None;
        }
        self.forced.store(false, Ordering::SeqCst);
        let cancel = self.cancel_reason.swap(false, Ordering::SeqCst);
        let _fiber = self.lock.lock().unwrap();
        let target = self.target.load(Ordering::Relaxed);
        assert!(
            !cancel || target != NONE,
            "CANCEL was requested without a target to unwind to"
        );
        (target != NONE).then_some(target)
    }

    /// `sched::fiber::begin_park`.
    fn begin_park(&self) {
        self.state.store(park::PARKING, Ordering::SeqCst);
    }

    /// `sched::fiber::cancel_park`.
    fn cancel_park(&self) {
        self.state.store(park::RUNNING, Ordering::SeqCst);
    }

    /// The worker's step after the fiber switched away to park, once it
    /// has released the handed-over lock: `PARKING -> PARKED`, or a wake
    /// that came while parking makes the fiber runnable at once. Returns
    /// whether the fiber is parked.
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
                self.enqueued.fetch_add(1, Ordering::SeqCst);
                false
            }
            Err(other) => panic!("a parking fiber was in the state {other}"),
        }
    }

    /// `sched::wake`. Called only by the side that won the fiber's node,
    /// after it released every lock it held.
    fn wake(&self) -> bool {
        assert!(
            !self.retired.load(Ordering::SeqCst),
            "a retired fiber was woken"
        );
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                park::PARKED => park::RUNNABLE,
                park::PARKING => park::PARKING_WOKEN,
                _ => return false,
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
            // The fiber is not in a queue yet, so it cannot have run and
            // retired since the swap.
            assert!(!self.retired.load(Ordering::SeqCst));
            self.enqueued.fetch_add(1, Ordering::SeqCst);
            self.queue.notify();
        }
        true
    }
}

/// A wait node's state (`sched/wait.rs` `WaitHeader`).
#[derive(Debug)]
struct Node {
    state: AtomicU8,
}

impl Node {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(node::WAITING),
        }
    }

    /// `WaitHeader::try_settle(WaitState::Claimed)`.
    fn try_settle(&self) -> bool {
        self.state
            .compare_exchange(
                node::WAITING,
                node::CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// `WaitHeader::try_cancel`.
    fn try_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                node::WAITING,
                node::CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// `WaitHeader::state`.
    fn get(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

/// How much of a model's interleavings loom explores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Search {
    /// At most [`PREEMPTIONS`] preemptions per execution (unless
    /// `LOOM_MAX_PREEMPTIONS` says otherwise): the default run, which
    /// keeps `cargo test --workspace` fast.
    Bounded,
    /// At most [`DEEP_PREEMPTIONS`] preemptions: model (5)'s `#[ignore]`d
    /// `model_5_deep`, for `--release`. Every interleaving of model (5)
    /// is out of reach (each extra preemption costs about four times the
    /// time).
    Deep,
    /// Every interleaving: model (4)'s `#[ignore]`d `model_4_exhaustive`,
    /// for `--release`.
    Exhaustive,
}

/// The preemption bound of [`Search::Bounded`]. Most concurrency bugs
/// need two or three preemptions to show (the loom documentation's advice).
const PREEMPTIONS: usize = 3;

/// The preemption bound of [`Search::Deep`].
const DEEP_PREEMPTIONS: usize = 6;

/// Runs the model `f` under `search`. `LOOM_MAX_PREEMPTIONS` overrides the
/// bound.
fn explore(search: Search, f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = match search {
            Search::Bounded => Some(PREEMPTIONS),
            Search::Deep => Some(DEEP_PREEMPTIONS),
            Search::Exhaustive => None,
        };
    }
    builder.check(f);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn smoke_two_threads_one_lock() {
    loom::model(|| {
        let lock = Arc::new(Mutex::new(0_u8));
        let flag = Arc::new(AtomicU8::new(0));
        let other = {
            let lock = Arc::clone(&lock);
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                *lock.lock().unwrap() += 1;
                flag.store(1, Ordering::Release);
            })
        };
        *lock.lock().unwrap() += 1;
        other.join().unwrap();
        assert_eq!(flag.load(Ordering::Acquire), 1);
        assert_eq!(*lock.lock().unwrap(), 2);
    });
}

// ---------------------------------------------------------------------------
// Model 1: close's step 3 against an outer cancel (architecture §7.5's
// named model).
// ---------------------------------------------------------------------------

/// How the owner's step 3 clears its own target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Close {
    /// Read, decide and clear under one hold of the fiber lock (D-706).
    UnderTheFiberLock,
    /// The bug: read under the lock, clear after releasing it.
    ClearWithoutTheFiberLock,
}

/// The owner's decision in step 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step3 {
    /// The target is a proper ancestor: raise `Cancel` again.
    PassThrough,
    /// The target was this scope (cleared) or unset: return.
    Return,
}

/// `cheby_rt_scope_close`'s step 3 on the owner of `INNER`.
fn close_step_3(owner: &Fiber, close: Close) -> Step3 {
    let fiber = owner.lock.lock().unwrap();
    let target = owner.target.load(Ordering::Relaxed);
    if target != NONE && is_proper_descendant(INNER, target) {
        return Step3::PassThrough;
    }
    if target == INNER {
        match close {
            Close::UnderTheFiberLock => {
                owner.target.store(NONE, Ordering::Relaxed);
                owner.clear_cancel();
                drop(fiber);
            }
            Close::ClearWithoutTheFiberLock => {
                drop(fiber);
                owner.target.store(NONE, Ordering::Relaxed);
                owner.clear_cancel();
            }
        }
    }
    Step3::Return
}

/// Model 1. The owner of `INNER`, a member of `OUTER`, runs step 3 of
/// `INNER`'s close (unwinding `INNER`'s own cancellation, or not unwinding
/// at all) while another fiber cancels `OUTER`. Invariant (survey-u5 §4.3):
/// either the owner passes through to `OUTER`, or `OUTER` is set after the
/// clear and the level-triggered entry check delivers it. No target is
/// lost.
fn close_against_outer_cancel(close: Close) {
    for start in [INNER, NONE] {
        loom::model(move || {
            let outer = Arc::new(Mutex::new(()));
            let owner = Arc::new(Fiber::new(start));
            let canceller = {
                let outer = Arc::clone(&outer);
                let owner = Arc::clone(&owner);
                // `cancel(OUTER)`: under OUTER's lock, then the member's
                // fiber lock.
                thread::spawn(move || {
                    let _scope = outer.lock().unwrap();
                    let _fiber = owner.lock.lock().unwrap();
                    owner.set_target(OUTER);
                    owner.request_cancel();
                })
            };
            let step3 = close_step_3(&owner, close);
            let early = match step3 {
                Step3::PassThrough => Some(OUTER),
                Step3::Return => owner.entry_check(),
            };
            canceller.join().unwrap();
            let delivered = early.or_else(|| owner.entry_check());
            assert_eq!(
                delivered,
                Some(OUTER),
                "the outer cancellation was lost ({step3:?}, start {start})"
            );
        });
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_1_close_step_3_against_an_outer_cancel() {
    close_against_outer_cancel(Close::UnderTheFiberLock);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the outer cancellation was lost")]
fn model_1_twin_clearing_the_target_without_the_fiber_lock_loses_it() {
    close_against_outer_cancel(Close::ClearWithoutTheFiberLock);
}

// ---------------------------------------------------------------------------
// Model 2: a waiter, a waker and a canceller on one wait node.
// ---------------------------------------------------------------------------

/// When the waiter makes `PARKING` visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parking {
    /// `begin_park` before the node is published (survey-u5 §2.3, hazard 1).
    BeforePublishing,
    /// The bug: publish the node, then store `PARKING` (the order before
    /// `begin_park`).
    AfterPublishing,
    /// The bug: `begin_park` before publishing, but `park(Park(_))` stores
    /// `PARKING` again after publishing, which overwrites a canceller's
    /// `PARKING_WOKEN`.
    BeforeAndAgainInPark,
}

/// How the waiter's send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// The check before parking found the cancellation.
    CancelledBeforeParking,
    /// The waiter parked with its node published.
    Parked,
}

/// Model 2. A sender parks on a full channel with its value in its node;
/// a receiver takes the value from the node (`settle`) and wakes it; the
/// walk cancels it. Invariants (survey-u5 §4.3): the waiter is woken
/// exactly once, never lost, and the value is owned by exactly one side.
#[expect(
    clippy::too_many_lines,
    reason = "a model reads top to bottom as its three threads and its checks"
)]
fn waiter_waker_canceller(parking: Parking) {
    loom::model(move || {
        let chan = Arc::new(Mutex::new(()));
        let linked = Arc::new(AtomicBool::new(false));
        let node = Arc::new(Node::new());
        let waiter = Arc::new(Fiber::new(NONE));
        // Times the value was taken: by the receiver, or released by the
        // sender's unwinding.
        let owners = Arc::new(probe::AtomicUsize::new(0));

        // The receiver: `receive` takes from a blocked sender under the
        // channel lock (`settle` CASes `Waiting -> Claimed` before it
        // touches the value), then wakes it after unlocking.
        let receiver = {
            let (chan, linked, node, waiter, owners) = (
                Arc::clone(&chan),
                Arc::clone(&linked),
                Arc::clone(&node),
                Arc::clone(&waiter),
                Arc::clone(&owners),
            );
            thread::spawn(move || {
                // `WaitQueue::pop_settled`: unlink the front node, then
                // settle it; a cancelled node stays unlinked and skipped.
                let guard = chan.lock().unwrap();
                let won = linked.load(Ordering::Relaxed) && {
                    linked.store(false, Ordering::Relaxed);
                    node.try_settle()
                };
                if won {
                    owners.fetch_add(1, Ordering::SeqCst);
                }
                drop(guard);
                if won {
                    assert!(
                        waiter.wake(),
                        "lost wakeup: the receiver found the waiter running"
                    );
                }
            })
        };

        // The walk: under the fiber lock, set the target, request CANCEL
        // and CAS a published node `Waiting -> Cancelled`; wake after
        // unlocking.
        let canceller = {
            let (node, waiter) = (Arc::clone(&node), Arc::clone(&waiter));
            thread::spawn(move || {
                let fiber = waiter.lock.lock().unwrap();
                waiter.set_target(INNER);
                waiter.request_cancel();
                let won = waiter.wait.load(Ordering::Relaxed) && node.try_cancel();
                drop(fiber);
                if won {
                    // A lost wake is caught below, by the parked state.
                    let _ = waiter.wake();
                }
            })
        };

        // The sender, on this thread: `send`'s blocking path.
        let sent = {
            let chan_guard = chan.lock().unwrap();
            if parking != Parking::AfterPublishing {
                waiter.begin_park();
            }
            // Link the node into `sendq` under the channel lock, then
            // `cancel::publish`: under the fiber lock, refuse if a target
            // is set, else publish the node.
            linked.store(true, Ordering::Relaxed);
            let fiber = waiter.lock.lock().unwrap();
            let refused = waiter.target.load(Ordering::Relaxed) != NONE;
            if refused {
                drop(fiber);
                // Refused: unlink (still under the channel lock, so no
                // waker saw the node), unlock, undo the park and raise.
                linked.store(false, Ordering::Relaxed);
                drop(chan_guard);
                waiter.cancel_park();
                // Unwinding releases the value that was never enqueued.
                owners.fetch_add(1, Ordering::SeqCst);
                Sent::CancelledBeforeParking
            } else {
                waiter.wait.store(true, Ordering::Relaxed);
                drop(fiber);
                if parking == Parking::BeforePublishing {
                    // `park(Park(_))`: CAS `RUNNING -> PARKING`, a no-op
                    // after `begin_park` or a canceller's wake.
                    let _ = waiter.state.compare_exchange(
                        park::RUNNING,
                        park::PARKING,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                } else {
                    // `park` storing `PARKING` itself, after the node is
                    // reachable by the walk.
                    waiter.state.store(park::PARKING, Ordering::SeqCst);
                }
                // `park(Park(lock))`: the worker releases the channel lock
                // after the switch, then settles the park state.
                drop(chan_guard);
                waiter.after_switch();
                Sent::Parked
            }
        };

        receiver.join().unwrap();
        canceller.join().unwrap();

        match sent {
            Sent::CancelledBeforeParking => {
                assert_eq!(waiter.wakes.load(Ordering::SeqCst), 0);
                assert_eq!(node.get(), node::WAITING);
            }
            Sent::Parked => {
                let state = waiter.state.load(Ordering::SeqCst);
                // Parked, or a woken-while-parking state no worker will
                // see again: either way the waiter never runs.
                assert_eq!(
                    state,
                    park::RUNNABLE,
                    "lost wakeup: the waiter was left in the park state {state} \
                     (wakes {}, node {})",
                    waiter.wakes.load(Ordering::SeqCst),
                    node.get()
                );
                assert_eq!(waiter.wakes.load(Ordering::SeqCst), 1);
                assert_eq!(waiter.enqueued.load(Ordering::SeqCst), 1);
                // The waiter resumes: `cancel::unpublish`, then it reads
                // its node.
                {
                    let _fiber = waiter.lock.lock().unwrap();
                    waiter.wait.store(false, Ordering::Relaxed);
                }
                match node.get() {
                    node::CLAIMED => {}
                    node::CANCELLED => {
                        // `WaitQueue::remove` under the channel lock (a
                        // receiver may have unlinked it already), drop the
                        // value (D-670) and raise `Cancel`.
                        let guard = chan.lock().unwrap();
                        linked.store(false, Ordering::Relaxed);
                        drop(guard);
                        owners.fetch_add(1, Ordering::SeqCst);
                    }
                    other => panic!("a woken waiter's node is in the state {other}"),
                }
            }
        }
        assert_eq!(
            owners.load(Ordering::SeqCst),
            1,
            "the value has more or fewer than one owner"
        );
        assert!(!linked.load(Ordering::Relaxed), "the node stayed linked");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_2_a_waiter_a_waker_and_a_canceller_on_one_node() {
    waiter_waker_canceller(Parking::BeforePublishing);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "lost wakeup")]
fn model_2_twin_publishing_the_node_before_parking_loses_a_wake() {
    waiter_waker_canceller(Parking::AfterPublishing);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "lost wakeup")]
fn model_2_twin_park_storing_parking_after_begin_park_loses_a_wake() {
    waiter_waker_canceller(Parking::BeforeAndAgainInPark);
}

// ---------------------------------------------------------------------------
// Model 3: a spawn against the walk.
// ---------------------------------------------------------------------------

/// The scope's slot for the new fiber.
mod slot {
    pub(super) const EMPTY: u8 = 0;
    pub(super) const RESERVED: u8 = 1;
    pub(super) const FILLED: u8 = 2;
}

/// Model 3. A fiber spawns into `INNER` (reserve under the scope lock,
/// create outside it, fill under it, schedule) while the walk cancels
/// `INNER` (mark, collect the filled members, set each target and request
/// CANCEL under the scope lock, then the fiber lock). Invariant
/// (survey-u5 §4.3): the new fiber ends cancelled: it never calls `f`, or
/// it carries the target and a CANCEL request its next entry check
/// delivers.
fn spawn_against_walk(spawn: Spawn) {
    loom::model(move || {
        let scope = Arc::new(Mutex::new(()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(AtomicU8::new(slot::EMPTY));
        let new = Arc::new(Fiber::new(NONE));

        let walk = {
            let (scope, cancelled, slot, new) = (
                Arc::clone(&scope),
                Arc::clone(&cancelled),
                Arc::clone(&slot),
                Arc::clone(&new),
            );
            thread::spawn(move || {
                let _scope = scope.lock().unwrap();
                cancelled.store(true, Ordering::Relaxed);
                // A reserved slot has no fiber yet: the spawner sees the
                // mark when it fills the slot.
                if slot.load(Ordering::Relaxed) == slot::FILLED {
                    let _fiber = new.lock.lock().unwrap();
                    new.set_target(INNER);
                    new.request_cancel();
                }
            })
        };

        // `ScopeState::join`: reserve a slot (the scope has not ended),
        // and read the cancellation state.
        let at_reserve = {
            let _scope = scope.lock().unwrap();
            slot.store(slot::RESERVED, Ordering::Relaxed);
            cancelled.load(Ordering::Relaxed)
        };
        // The fiber is created outside the lock, then `ScopeState::fill`
        // fills the slot and returns the scope's cancellation state.
        let at_fill = {
            let _scope = scope.lock().unwrap();
            slot.store(slot::FILLED, Ordering::Relaxed);
            cancelled.load(Ordering::Relaxed)
        };
        let was_cancelled = match spawn {
            Spawn::UsesFillsState => at_fill,
            Spawn::IgnoresFillsState => at_reserve,
        };
        if was_cancelled {
            // `cancel::set_target`: only the fiber lock.
            let _fiber = new.lock.lock().unwrap();
            new.set_target(INNER);
            new.request_cancel();
        }
        // `schedule_new`, then `fiber_main` on the new fiber: a target at
        // the start means `f` is never called.
        let calls_f = {
            let _fiber = new.lock.lock().unwrap();
            new.target.load(Ordering::Relaxed) == NONE
        };

        walk.join().unwrap();
        assert_eq!(
            new.target.load(Ordering::Relaxed),
            INNER,
            "the new fiber escaped the cancellation"
        );
        if calls_f {
            assert_eq!(
                new.entry_check(),
                Some(INNER),
                "the new fiber runs `f` uncancelled"
            );
        }
    });
}

/// What the spawner does with `ScopeState::fill`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spawn {
    /// Gives the new fiber its target if `fill` found the scope cancelled
    /// (survey-u5 §2.3, hazard 2; `cancelled` is sticky, so this covers a
    /// cancellation before the reservation too).
    UsesFillsState,
    /// The bug: only the state read at the reservation counts.
    IgnoresFillsState,
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_3_a_spawn_against_the_walk() {
    spawn_against_walk(Spawn::UsesFillsState);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the new fiber escaped the cancellation")]
fn model_3_twin_ignoring_fills_state_lets_the_fiber_escape() {
    spawn_against_walk(Spawn::IgnoresFillsState);
}

// ---------------------------------------------------------------------------
// Model 4: the first cause with two panickers and an outer canceller.
// ---------------------------------------------------------------------------

/// What the scope `INNER` holds under its lock.
#[derive(Debug, Default)]
struct ScopeInner {
    cancelled: bool,
    /// The panicker whose report is the first cause (1 or 2), or 0.
    first_cause: u8,
    /// Visits of the scope's lock, numbered.
    ticket: usize,
    /// The ticket of the outer walk's visit, once it ran.
    outer_visit: Option<usize>,
    /// The ticket at which the first cause was recorded.
    recorded_at: Option<usize>,
}

/// The walk over `INNER`'s members for a cancellation targeting `to`:
/// under `INNER`'s lock, each member's fiber lock in turn.
fn walk_inner(scope: &Mutex<ScopeInner>, members: &[&Fiber; 2], to: u8, outer: bool) {
    let mut inner = scope.lock().unwrap();
    inner.cancelled = true;
    inner.ticket = inner.ticket.wrapping_add(1);
    if outer {
        inner.outer_visit = Some(inner.ticket);
    }
    for member in members {
        let _fiber = member.lock.lock().unwrap();
        member.set_target(to);
        member.request_cancel();
    }
}

/// Model 4. Two members of `INNER` panic while another fiber cancels
/// `OUTER`, which encloses `INNER`. Each panicker records under `INNER`'s
/// lock only if it has no first cause and is not cancelled, then (after
/// releasing that lock) cancels `INNER`. Invariants (survey-u5 §4.3): at
/// most one cause is recorded, and none once the scope is cancelled from
/// outside; the outermost target wins.
fn first_cause(search: Search) {
    explore(search, || {
        let outer = Arc::new(Mutex::new(false));
        let scope = Arc::new(Mutex::new(ScopeInner::default()));
        let members = Arc::new([Fiber::new(NONE), Fiber::new(NONE)]);
        let recorded = Arc::new(probe::AtomicUsize::new(0));

        let panicker = |who: u8| {
            let (scope, members, recorded) = (
                Arc::clone(&scope),
                Arc::clone(&members),
                Arc::clone(&recorded),
            );
            move || {
                // `start_panic` → `ScopeState::record_panic`.
                let records = {
                    let mut inner = scope.lock().unwrap();
                    inner.ticket = inner.ticket.wrapping_add(1);
                    let records = inner.first_cause == 0 && !inner.cancelled;
                    if records {
                        inner.first_cause = who;
                        inner.recorded_at = Some(inner.ticket);
                    }
                    records
                };
                if records {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    // `cancel(INNER)`, entered with no lock held.
                    let [a, b] = &*members;
                    walk_inner(&scope, &[a, b], INNER, false);
                }
            }
        };

        let first = thread::spawn(panicker(1));
        let canceller = {
            let (outer, scope, members) =
                (Arc::clone(&outer), Arc::clone(&scope), Arc::clone(&members));
            thread::spawn(move || {
                // `cancel(OUTER)`: mark OUTER and collect INNER under
                // OUTER's lock, release it, then walk INNER.
                {
                    let mut cancelled = outer.lock().unwrap();
                    *cancelled = true;
                }
                let [a, b] = &*members;
                walk_inner(&scope, &[a, b], OUTER, true);
            })
        };
        panicker(2)();
        first.join().unwrap();
        canceller.join().unwrap();

        let inner = scope.lock().unwrap();
        assert!(
            recorded.load(Ordering::SeqCst) <= 1,
            "two first causes were recorded"
        );
        if let Some(at) = inner.recorded_at {
            let outer_visit = inner.outer_visit.unwrap();
            assert!(
                at < outer_visit,
                "a cause was recorded after the scope was cancelled from outside"
            );
        }
        for member in members.iter() {
            assert_eq!(
                member.target.load(Ordering::Relaxed),
                OUTER,
                "the outermost target did not win"
            );
        }
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_4_the_first_cause_with_two_panickers_and_an_outer_canceller() {
    first_cause(Search::Bounded);
}

#[test]
#[cfg_attr(not(windows), ignore = "loom: run with --release")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_4_exhaustive() {
    first_cause(Search::Exhaustive);
}

// ---------------------------------------------------------------------------
// Model 5: a joiner's park against `finish` against a cancel.
// ---------------------------------------------------------------------------

/// The join cell's states (`fiber_ops/base.rs` `Outcome`).
mod cell {
    pub(super) const RUNNING: u8 = 0;
    pub(super) const FAILED: u8 = 2;
}

/// How the joiner's `join` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Joined {
    /// It unwinds for its cancellation.
    Unwinds,
    /// It panics `joined fiber failed` (D-668).
    PanicsJoinedFailed,
}

/// Model 5. A fiber joins a sibling that fails (`finish(Failed)`) while
/// the walk cancels the joiner. Invariants (survey-u5 §4.3, D-668): no
/// wake reaches a retired joiner, nobody touches its node after it
/// retired, the joiner is woken at most once and never left in the cell,
/// and a joiner being cancelled unwinds.
#[expect(
    clippy::too_many_lines,
    reason = "a model reads top to bottom as its three threads and its checks"
)]
fn join_against_finish_and_cancel(search: Search) {
    explore(search, || {
        let cell = Arc::new(Mutex::new(cell::RUNNING));
        let linked = Arc::new(AtomicBool::new(false));
        let node = Arc::new(Node::new());
        let node_alive = Arc::new(probe::AtomicBool::new(true));
        let joiner = Arc::new(Fiber::new(NONE));

        // The failing fiber's `finish(Failed)`: under the cell's lock,
        // `pop_settled` unlinks each node and settles it `Waiting ->
        // Claimed`, skipping a cancelled one; `finish` wakes the won
        // joiners after it released the cell's lock.
        let finisher = {
            let (cell, linked, node, node_alive, joiner) = (
                Arc::clone(&cell),
                Arc::clone(&linked),
                Arc::clone(&node),
                Arc::clone(&node_alive),
                Arc::clone(&joiner),
            );
            thread::spawn(move || {
                let mut state = cell.lock().unwrap();
                *state = cell::FAILED;
                let won = linked.load(Ordering::Relaxed) && {
                    assert!(
                        node_alive.load(Ordering::SeqCst),
                        "a retired node was touched"
                    );
                    linked.store(false, Ordering::Relaxed);
                    node.try_settle()
                };
                drop(state);
                if won {
                    assert!(
                        joiner.wake(),
                        "lost wakeup: the finisher found the joiner running"
                    );
                }
            })
        };

        // The walk cancels the joiner.
        let canceller = {
            let (node, node_alive, joiner) = (
                Arc::clone(&node),
                Arc::clone(&node_alive),
                Arc::clone(&joiner),
            );
            thread::spawn(move || {
                let fiber = joiner.lock.lock().unwrap();
                joiner.set_target(INNER);
                joiner.request_cancel();
                let won = joiner.wait.load(Ordering::Relaxed) && {
                    assert!(
                        node_alive.load(Ordering::SeqCst),
                        "a retired node was touched"
                    );
                    node.try_cancel()
                };
                drop(fiber);
                if won {
                    assert!(
                        joiner.wake(),
                        "lost wakeup: the canceller found the joiner running"
                    );
                }
            })
        };

        // The joiner, on this thread.
        let being_cancelled = || {
            let _fiber = joiner.lock.lock().unwrap();
            joiner.target.load(Ordering::Relaxed) != NONE
        };
        // `cheby_fiber_sys_join` on `Joined::Failed`.
        let on_failed = || {
            if being_cancelled() {
                Joined::Unwinds
            } else {
                Joined::PanicsJoinedFailed
            }
        };
        let state = cell.lock().unwrap();
        let outcome = if *state == cell::FAILED {
            drop(state);
            on_failed()
        } else {
            // `begin_park`, link the node under the cell's lock, then
            // `cancel::publish` under the fiber lock.
            joiner.begin_park();
            linked.store(true, Ordering::Relaxed);
            let fiber = joiner.lock.lock().unwrap();
            let refused = joiner.target.load(Ordering::Relaxed) != NONE;
            if refused {
                drop(fiber);
                // Refused: unlink under the cell's lock, unlock, undo the
                // park and raise.
                linked.store(false, Ordering::Relaxed);
                drop(state);
                joiner.cancel_park();
                Joined::Unwinds
            } else {
                joiner.wait.store(true, Ordering::Relaxed);
                drop(fiber);
                // `park(Park(lock))`: its CAS `RUNNING -> PARKING` is a
                // no-op after `begin_park`; the worker releases the cell's
                // lock after the switch.
                drop(state);
                if joiner.after_switch() {
                    // A worker resumes the fiber once a wake has put it on
                    // a run queue (`Notify` may wake spuriously).
                    while joiner.enqueued.load(Ordering::SeqCst) == 0 {
                        joiner.queue.wait();
                    }
                }
                // Resumed: `cancel::unpublish`, then read the node. A
                // cancelled node may still be linked, and the joiner
                // unlinks it under the cell's lock; a claimed one means
                // `finish` ended the cell, which `join` reads again
                // (`Failed`).
                {
                    let _fiber = joiner.lock.lock().unwrap();
                    joiner.wait.store(false, Ordering::Relaxed);
                }
                let settled = node.get();
                if settled == node::CANCELLED {
                    // `WaitQueue::remove`: `finish` may have unlinked it.
                    let _cell = cell.lock().unwrap();
                    linked.store(false, Ordering::Relaxed);
                }
                match settled {
                    node::CANCELLED => Joined::Unwinds,
                    node::CLAIMED => on_failed(),
                    other => panic!("a woken joiner's node is in the state {other}"),
                }
            }
        };
        // The joiner's frame ends and the fiber retires.
        node_alive.store(false, Ordering::SeqCst);
        joiner.retired.store(true, Ordering::SeqCst);

        finisher.join().unwrap();
        canceller.join().unwrap();
        assert!(
            joiner.wakes.load(Ordering::SeqCst) <= 1,
            "the joiner was woken twice"
        );
        assert!(
            !linked.load(Ordering::Relaxed),
            "a joiner was left in the cell"
        );
        match outcome {
            // Only a cancellation unwinds a joiner.
            Joined::Unwinds => assert_ne!(joiner.target.load(Ordering::Relaxed), NONE),
            // It found the cell failed before the walk reached it.
            Joined::PanicsJoinedFailed => assert_eq!(*cell.lock().unwrap(), cell::FAILED),
        }
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_5_a_joiners_park_against_finish_against_a_cancel() {
    join_against_finish_and_cancel(Search::Bounded);
}

#[test]
#[cfg_attr(not(windows), ignore = "loom: run with --release")]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_5_deep() {
    join_against_finish_and_cancel(Search::Deep);
}
