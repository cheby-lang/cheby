//! `loom` models of the deadlock detector's protocol (D-724, D-857, D-860;
//! UQ-1 (a) and UQ-12 (b) of checkpoint 5), as `sched/deadlock.rs`'s module
//! documentation states it: the two-pass survey, the pending counts that
//! fall only after the wake they stand for, a root's base frame, the
//! `reported` flag, the collect step's re-check, and the stuck groups.
//!
//! Each model is the protocol, not the runtime's code: `loom` atomics stand
//! for a fiber's state word, resume count, blocking mark and wait site, for
//! a domain's two pending counts and for a pool job's token; a `loom` mutex
//! stands for a base scope's lock (with its `ended` flag), the root
//! registry's lock and a group's job lock. A thread plays one fiber and the
//! worker that runs it: it parks as the runtime parks (the fiber stores
//! `PARKING`, the worker's compare-and-swap makes it `PARKED`, or queues it
//! when a waker found it `PARKING`), and it resumes as `Worker::run_fiber`
//! resumes it (`RUNNING`, then the resume count plus one, `Relaxed`). A
//! waker wakes as `sched::wake` does, through the fiber's wait node: the
//! fiber publishes the node under the node's lock before it parks, as a
//! waiter registers under a channel's or a join cell's lock, and the waker
//! takes it under that lock (a pool completion holds its fiber instead).
//! The run queue is a flag under a lock with a condition variable: loom
//! cannot explore a spin loop, so nothing in the models spins. The
//! detector's reads have the orderings of `deadlock.rs`: the state and the
//! mark `SeqCst`, the resume count and the wait site `Relaxed`, the counts
//! `SeqCst`. loom 0.7 needs no `--cfg loom`, so the models build with plain
//! `cargo test`:
//!
//! ```text
//! cargo test -p cheby_runtime sched::deadlock_models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step                  | Mirrors (path under `crates/cheby_runtime/src/`, function)                              |
//! | --------------------------- | --------------------------------------------------------------------------------------- |
//! | `Fiber::begin_park`         | `sched/fiber.rs` `begin_park`, after `FiberSched::note_wait`                            |
//! | `Fiber::end_park`           | `sched/worker.rs` `Worker::parked`                                                      |
//! | `Fiber::try_wake`           | `sched/mod.rs` `wake`                                                                   |
//! | `Fiber::resume`             | `sched/worker.rs` `Worker::run_fiber` (the state, then `FiberSched::count_resume`)      |
//! | `read`, `pass`, `survey`    | `sched/deadlock.rs` `pass`, `survey` (steps 3 and 4)                                    |
//! | `Base::step`                | `fiber_ops/scope.rs` `ScopeState::survey_step`                                          |
//! | `Base::end`                 | `fiber_ops/scope.rs` `close_root_base` (`ScopeState::end_base`)                         |
//! | `detect`                    | `sched/deadlock.rs` `detect`, `candidates`, `look`, `walk`, `act`                       |
//! | `collect`                   | `sched/deadlock.rs` `collect` (step 5), `locate.rs` `LiveStack::word`                   |
//! | `PoolPending::release`      | `sched/deadlock.rs` `PoolPending::release`                                              |
//! | `Group::mark_stuck`         | `sched/lifecycle.rs` `DetachedGroup::mark_stuck`                                        |
//! | model 2's pool thread       | `sched/blocking.rs` `Completion::complete`                                              |
//! | model 3's timer thread      | `sched/timer.rs` `Timers::fire_due` (`settle`, `fire`, `done`)                          |
//! | model 4's and 7's root      | `sched/mod.rs` `Runtime::spawn_root_in`'s start closure, `Worker::retire`               |
//! | model 5's termination, host | `sched/lifecycle.rs` `Shared::terminate`; `entry.rs` `run_main`, `deadlocked`           |
//!
//! The models, each with a `#[should_panic]` twin that breaks one rule:
//!
//! 1. The two-pass survey against fibers that wake each other and park, in
//!    turn, so that two are never parked at once: no deadlock is declared.
//!    Twins: one pass, and a pass that reads the resume count before the
//!    state (a fiber woken, resumed and parked again between the passes
//!    then looks unchanged).
//! 2. A pool completion (clear the mark, wake, release the token) against
//!    the survey. Twin: the token released before the wake. In the runtime
//!    the pool's park records no wait site, which stops a pass on its own
//!    (`blocking.rs` `on_pool`; D-857 and D-860 judge only fibers parked
//!    outside the blocking pool); the model gives the waiting fiber a site
//!    so that the mark and the count's order are checked alone, as step 4's
//!    soundness argument uses them.
//! 3. A timer's firing (settle, fire, lower the count) against the survey.
//!    Twin: checkpoint 4's order (`timer.rs:398-399`), the count lowered
//!    between the settle and the firing.
//! 4. A `cheby run` root that returns, ends its base, reports `Returned`,
//!    unregisters and retires, while the detached fiber that woke it parks:
//!    no deadlock is declared for it. Twin: an ended base read as a root
//!    that has left the domain, so the survey of a root that unregistered
//!    goes on with the group's fibers.
//! 5. The `reported` compare-and-swap against a termination, with the
//!    detector acting on two monitor ticks and `run_main` taking the first
//!    outcome: one `Deadlocked` at most, and the status the host returns is
//!    the first status fixed. Twin: no compare-and-swap.
//! 6. The collect step after the confirmation against a wake from outside
//!    the survey (D-857's stuck fiber), after which the fiber runs, writes
//!    its stack and parks again at the same site: every line of a report
//!    was read from a stack that did not run. Twin: the lines collected
//!    inside pass 2 with no re-check (survey-deadlock §4.2's first draft).
//!    In C11 the model holds only with a fence pair: a release fence after
//!    the resume count, before the fiber runs, and an acquire fence after
//!    the stack's reads, before the re-check (the seqlock pattern). Without
//!    it, a relaxed read of a word the resumed fiber wrote does not order
//!    the re-check after the resume, and the re-check's `SeqCst` load may
//!    still return `PARKED` (board item DLm-1). The runtime has the acquire
//!    half (`deadlock::collect`) and relies on the hardware for the other:
//!    on `AArch64` `run_fiber`'s `SeqCst` load of `life` orders the fiber's
//!    later stores after its count, and x86-64 is TSO; D-860's one relaxed
//!    store per resume keeps the release fence out. A third test keeps the
//!    acquire half alone and fails. loom finds it only through the initial
//!    value, which is not a `SeqCst` store: at a `SeqCst` load loom returns
//!    the newest `SeqCst` store, which is stronger than C11.
//! 7. The survey against a root's end: a root that runs and returns while
//!    its detached fiber is parked, the root's control block a `loom`
//!    `UnsafeCell` that the retirement writes (its slot reused): the block is
//!    read only under the base's lock while the base has not ended, and
//!    `close_root_base` comes before the unregistration and the retirement,
//!    so loom finds no concurrent access. Twin: the root's block read after
//!    `end_base`.
//! 8. A teardown marking a group stuck against the completion of one of its
//!    members' pool jobs and a survey of a later entry: the job leaves the
//!    domain's pool count exactly once, and the survey never lists the stuck
//!    fiber. Twin: the stuck job lowered both at the mark and at its
//!    completion.
//!
//! What the models leave out: the workers and their deques (a woken
//! fiber's own thread takes it from its queue), the channels and their
//! waiter lists (one wait node per fiber stands for them), the backoff and
//! the triggers (a model's detector surveys at once), the report's text and
//! the frame walk (a line is one stack word), the halt's freezing, and
//! `cheby test`'s and the REPL's cancellation (model 5 is `cheby run`'s
//! action; model 7 reads only).

use loom::cell::UnsafeCell;
use loom::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence,
};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

/// A fiber's states, as `sched/fiber.rs` numbers them.
mod state {
    pub(super) const RUNNABLE: u8 = 0;
    pub(super) const RUNNING: u8 = 1;
    pub(super) const PARKING: u8 = 2;
    pub(super) const PARKING_WOKEN: u8 = 3;
    pub(super) const PARKED: u8 = 4;
    pub(super) const DONE: u8 = 5;
}

/// No wait site (`FiberSched::wait_site` is `None`).
const NO_SITE: usize = 0;
/// The frame a wait site records (`note_wait`'s frame).
const SITE: usize = 0x40;
/// Model 6: the stack word at the wait site's frame while the fiber waits.
const FRAME: usize = 0x1000;
/// Model 6: what the fiber writes there while it runs.
const SCRATCH: usize = 0xdead;

/// One fiber's scheduling words (`FiberSched`) and its run queue.
#[derive(Debug)]
struct Fiber {
    /// `FiberSched::id`.
    id: u64,
    /// `FiberSched::state`.
    state: AtomicU8,
    /// `FiberSched::resumes`.
    resumes: AtomicU64,
    /// The blocking mark (`FiberSched::blocking`).
    blocking: AtomicBool,
    /// The wait site's frame (`FiberSched::wait_site`), `NO_SITE` for none.
    site: AtomicUsize,
    /// The fiber's wait node, published under its lock before the fiber
    /// parks (a channel's or a join cell's waiter, registered under that
    /// lock) and taken by the waker that wakes it.
    node: Mutex<bool>,
    published: Condvar,
    /// Whether the fiber is in a run queue: a waker pushes it, its worker
    /// takes it.
    queued: Mutex<bool>,
    pushed: Condvar,
}

impl Fiber {
    /// A fiber that runs, resumed once.
    fn running(id: u64) -> Self {
        Self::new(id, state::RUNNING, NO_SITE)
    }

    /// A fiber parked at `site`, resumed once before it parked, its wait
    /// node published if it waits in one (`site` is not `NO_SITE`).
    fn parked(id: u64, site: usize) -> Self {
        Self::new(id, state::PARKED, site)
    }

    fn new(id: u64, state: u8, site: usize) -> Self {
        Self {
            id,
            state: AtomicU8::new(state),
            resumes: AtomicU64::new(1),
            blocking: AtomicBool::new(false),
            site: AtomicUsize::new(site),
            node: Mutex::new(site != NO_SITE),
            published: Condvar::new(),
            queued: Mutex::new(false),
            pushed: Condvar::new(),
        }
    }

    /// `note_wait`: the fiber records its wait site before it parks.
    fn note_wait(&self, site: usize) {
        self.site.store(site, Ordering::Relaxed);
    }

    /// `clear_wait`: the fiber clears its wait site once resumed.
    fn clear_wait(&self) {
        self.site.store(NO_SITE, Ordering::Relaxed);
    }

    /// `begin_park`: the fiber, before it switches away.
    fn begin_park(&self) {
        self.state.store(state::PARKING, Ordering::SeqCst);
    }

    /// `Worker::parked`: the worker, after the switch.
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
                self.push();
            }
            Err(other) => panic!("a parking fiber was in the state {other}"),
        }
    }

    /// `sched::wake`: makes a parked fiber runnable and queues it, or tells
    /// a parking one; false for a fiber in any other state.
    fn try_wake(&self) -> bool {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                state::PARKED => state::RUNNABLE,
                state::PARKING => state::PARKING_WOKEN,
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
        if current == state::PARKED {
            self.push();
        }
        true
    }

    /// Wakes the fiber through its wait node: waits until the node is
    /// published, takes it under its lock, then wakes the fiber, which is
    /// `PARKING` or `PARKED` by then.
    fn wake(&self) {
        let mut node = self.node.lock().unwrap();
        while !*node {
            node = self.published.wait(node).unwrap();
        }
        *node = false;
        drop(node);
        assert!(
            self.try_wake(),
            "a waker took a node whose fiber was not parking"
        );
    }

    /// Pushes the fiber on its run queue.
    fn push(&self) {
        *self.queued.lock().unwrap() = true;
        self.pushed.notify_all();
    }

    /// `Worker::run_fiber`: takes the fiber from its queue, then stores
    /// `RUNNING` and counts the resume, as the runtime does.
    fn resume(&self) {
        let mut queued = self.queued.lock().unwrap();
        while !*queued {
            queued = self.pushed.wait(queued).unwrap();
        }
        *queued = false;
        drop(queued);
        self.state.store(state::RUNNING, Ordering::SeqCst);
        let resumes = self.resumes.load(Ordering::Relaxed);
        self.resumes
            .store(resumes.wrapping_add(1), Ordering::Relaxed);
    }

    /// Waits at `site` until woken, then runs on.
    fn park(&self, site: usize) {
        self.park_for_good(site);
        self.resume();
        self.clear_wait();
    }

    /// Parks at `site`, through a wait node a waker can take; the caller
    /// resumes it, or nothing in the model wakes it again.
    fn park_for_good(&self, site: usize) {
        let mut node = self.node.lock().unwrap();
        self.note_wait(site);
        self.begin_park();
        *node = true;
        self.published.notify_all();
        // The worker releases the waiter's lock after the switch, then ends
        // the park.
        drop(node);
        self.end_park();
    }

    /// The fiber returned (`Worker::retire` stores `DONE`).
    fn finish(&self) {
        self.state.store(state::DONE, Ordering::SeqCst);
    }
}

/// A domain's pending counts (`DomainCounts`).
#[derive(Debug, Default)]
struct Counts {
    timers: AtomicUsize,
    pool: AtomicUsize,
}

impl Counts {
    fn with(timers: usize, pool: usize) -> Self {
        Self {
            timers: AtomicUsize::new(timers),
            pool: AtomicUsize::new(pool),
        }
    }

    /// `DomainCounts::quiet`: timers first, then the pool.
    fn quiet(&self) -> bool {
        self.timers.load(Ordering::SeqCst) == 0 && self.pool.load(Ordering::SeqCst) == 0
    }

    /// `DomainCounts::timer_done`.
    fn timer_done(&self) {
        assert!(
            self.timers.fetch_sub(1, Ordering::SeqCst) != 0,
            "a domain's pending timer count fell below 0"
        );
    }

    /// The pool count's fall in `PoolPending::release`.
    fn pool_done(&self) {
        assert!(
            self.pool.fetch_sub(1, Ordering::SeqCst) != 0,
            "a domain's pending pool count fell below 0"
        );
    }
}

/// How a pool job's token leaves the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// Once: the first caller's swap of `counted` lowers the count (the
    /// runtime's).
    Once,
    /// Every caller lowers it: model 8's twin.
    EveryCaller,
}

/// One pool job counted in its domain (`PoolPending`).
#[derive(Debug)]
struct PoolPending {
    counts: Arc<Counts>,
    counted: AtomicBool,
    release: Release,
}

impl PoolPending {
    /// A job counted in `counts` (the count was raised for it).
    fn new(counts: &Arc<Counts>, release: Release) -> Self {
        Self {
            counts: Arc::clone(counts),
            counted: AtomicBool::new(true),
            release,
        }
    }

    /// `PoolPending::release`.
    fn release(&self) {
        match self.release {
            Release::Once => {
                if self.counted.swap(false, Ordering::SeqCst) {
                    self.counts.pool_done();
                }
            }
            Release::EveryCaller => self.counts.pool_done(),
        }
    }
}

/// In what order a pass reads a fiber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reads {
    /// The state, the mark, the resume count, then the wait site (step 3,
    /// the runtime's).
    StateFirst,
    /// The resume count before the state: model 1's second twin.
    ResumesFirst,
}

/// One fiber as a pass reads it (step 3): `(id, resumes)` if it is parked,
/// unmarked and in a wait, else `None` (the pass stops).
fn read(fiber: &Fiber, reads: Reads) -> Option<(u64, u64)> {
    let early = match reads {
        Reads::StateFirst => None,
        Reads::ResumesFirst => Some(fiber.resumes.load(Ordering::Relaxed)),
    };
    if fiber.state.load(Ordering::SeqCst) != state::PARKED || fiber.blocking.load(Ordering::SeqCst)
    {
        return None;
    }
    let resumes = early.unwrap_or_else(|| fiber.resumes.load(Ordering::Relaxed));
    (fiber.site.load(Ordering::Relaxed) != NO_SITE).then_some((fiber.id, resumes))
}

/// One pass over `fibers` (step 3): their sorted `(id, resumes)`, or `None`
/// when one is not blocked or a count is above 0 after the walk.
fn pass(fibers: &[&Fiber], counts: &Counts, reads: Reads) -> Option<Vec<(u64, u64)>> {
    let mut seen = Vec::with_capacity(fibers.len());
    for fiber in fibers {
        seen.push(read(fiber, reads)?);
    }
    if !counts.quiet() {
        return None;
    }
    seen.sort_unstable();
    Some(seen)
}

/// How many passes confirm a deadlock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Passes {
    /// Two, with equal lists (step 4, the runtime's).
    Two,
    /// One: model 1's first twin.
    One,
}

/// The survey of a domain whose fibers are `fibers` (steps 3 and 4):
/// whether it declares a deadlock.
fn survey(fibers: &[&Fiber], counts: &Counts, passes: Passes, reads: Reads) -> bool {
    let Some(first) = pass(fibers, counts, reads) else {
        return false;
    };
    match passes {
        Passes::One => true,
        Passes::Two => pass(fibers, counts, reads).is_some_and(|second| second == first),
    }
}

/// At most three preemptions per execution, the loom documentation's
/// advice, as `sched/park_models.rs` bounds them.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    explore_within(3, f);
}

fn explore_within(preemptions: usize, f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(preemptions);
    }
    builder.check(f);
}

/// The text every model's declaration check panics with.
const FALSE_DEADLOCK: &str = "a deadlock was declared, and a wake came after it";

/// Model (1): two fibers hand control to each other through four wakes,
/// each fiber waking the other right before it parks, then both return; so
/// two are never parked at once. X waits first; W wakes it and parks; X
/// wakes W and parks again at the same site; W wakes X and parks again; X
/// wakes W and returns; W returns. The survey, at any point, must not
/// declare a deadlock: a wake follows every moment it could have seen.
fn hand_offs(passes: Passes, reads: Reads) {
    explore(move || {
        let counts = Arc::new(Counts::default());
        let x = Arc::new(Fiber::parked(1, SITE));
        let w = Arc::new(Fiber::running(2));
        let waker = {
            let (x, w) = (Arc::clone(&x), Arc::clone(&w));
            thread::spawn(move || {
                x.wake();
                w.park(SITE);
                x.wake();
                w.park(SITE);
                w.finish();
            })
        };
        let woken = {
            let (x, w) = (Arc::clone(&x), Arc::clone(&w));
            thread::spawn(move || {
                x.resume();
                x.clear_wait();
                w.wake();
                x.park(SITE);
                w.wake();
                x.finish();
            })
        };
        let declared = survey(&[&x, &w], &counts, passes, reads);
        waker.join().unwrap();
        woken.join().unwrap();
        assert!(!declared, "{FALSE_DEADLOCK}");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_1_two_passes_declare_no_deadlock_a_wake_ends() {
    hand_offs(Passes::Two, Reads::StateFirst);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared, and a wake came after it")]
fn model_1_twin_with_one_pass_declares_a_false_deadlock() {
    hand_offs(Passes::One, Reads::StateFirst);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared, and a wake came after it")]
fn model_1_twin_reading_resumes_before_state_declares_a_false_deadlock() {
    hand_offs(Passes::Two, Reads::ResumesFirst);
}

/// The order of a pool completion after it clears the mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    /// Wake, then release the token (`Completion::complete`).
    WakeThenRelease,
    /// Release, then wake: model 2's twin.
    ReleaseThenWake,
}

/// Model (2): X waits on the blocking pool (marked, its job counted); the
/// pool thread completes the job; X then returns. The survey must not
/// declare a deadlock.
fn pool_completion(order: Completion) {
    explore(move || {
        let counts = Arc::new(Counts::with(0, 1));
        let x = Arc::new(Fiber::parked(1, SITE));
        x.blocking.store(true, Ordering::SeqCst);
        let token = Arc::new(PoolPending::new(&counts, Release::Once));
        let pool = {
            let x = Arc::clone(&x);
            thread::spawn(move || {
                // The completion holds its fiber, not a wait node.
                x.blocking.store(false, Ordering::SeqCst);
                match order {
                    Completion::WakeThenRelease => {
                        assert!(x.try_wake(), "the pool's fiber was not parked");
                        token.release();
                    }
                    Completion::ReleaseThenWake => {
                        token.release();
                        assert!(x.try_wake(), "the pool's fiber was not parked");
                    }
                }
            })
        };
        let woken = {
            let x = Arc::clone(&x);
            thread::spawn(move || {
                x.resume();
                x.clear_wait();
                x.finish();
            })
        };
        let declared = survey(&[&x], &counts, Passes::Two, Reads::StateFirst);
        pool.join().unwrap();
        woken.join().unwrap();
        assert!(!declared, "{FALSE_DEADLOCK}");
        assert_eq!(counts.pool.load(Ordering::SeqCst), 0);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_2_a_pool_completion_lowers_the_count_after_its_wake() {
    pool_completion(Completion::WakeThenRelease);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared, and a wake came after it")]
fn model_2_twin_lowering_the_count_before_the_wake_declares_a_false_deadlock() {
    pool_completion(Completion::ReleaseThenWake);
}

/// A timer entry's states (`sched/timer.rs`).
mod timer_state {
    pub(super) const ARMED: u8 = 0;
    pub(super) const FIRED: u8 = 1;
}

/// The order of a firing after its settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Firing {
    /// Fire (the wake), then lower the count (`Timers::fire_due`).
    FireThenLower,
    /// Lower the count, then fire: checkpoint 4's order, model 3's twin.
    LowerThenFire,
}

/// Model (3): X waits in a `select` whose timer is armed (counted); the
/// timer thread fires it; X then returns. The survey must not declare a
/// deadlock.
fn timer_firing(order: Firing) {
    explore(move || {
        let counts = Arc::new(Counts::with(1, 0));
        let x = Arc::new(Fiber::parked(1, SITE));
        let entry = Arc::new(AtomicU8::new(timer_state::ARMED));
        let timer = {
            let (x, counts) = (Arc::clone(&x), Arc::clone(&counts));
            thread::spawn(move || {
                // `settle`: the CAS out of `ARMED`.
                if entry
                    .compare_exchange(
                        timer_state::ARMED,
                        timer_state::FIRED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_err()
                {
                    return;
                }
                match order {
                    Firing::FireThenLower => {
                        // `SelectTimer::fire` wakes the selecting fiber.
                        x.wake();
                        counts.timer_done();
                    }
                    Firing::LowerThenFire => {
                        counts.timer_done();
                        x.wake();
                    }
                }
            })
        };
        let woken = {
            let x = Arc::clone(&x);
            thread::spawn(move || {
                x.resume();
                x.clear_wait();
                x.finish();
            })
        };
        let declared = survey(&[&x], &counts, Passes::Two, Reads::StateFirst);
        timer.join().unwrap();
        woken.join().unwrap();
        assert!(!declared, "{FALSE_DEADLOCK}");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_3_a_firing_lowers_the_count_after_its_wake() {
    timer_firing(Firing::FireThenLower);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared, and a wake came after it")]
fn model_3_twin_with_checkpoint_4s_timer_order_declares_a_false_deadlock() {
    timer_firing(Firing::LowerThenFire);
}

/// The memory of a fiber's control block (model 7): the survey's reads of
/// the root go through `with`, the retirement (the slot reused) through
/// `with_mut`, so loom reports any read the retirement can race with.
#[derive(Debug)]
struct Block(UnsafeCell<u64>);

impl Block {
    fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    /// A read of the control block.
    fn touch(&self) {
        self.0.with(|_| ());
    }

    /// The retirement: the slot is given back and reused.
    fn reuse(&self) {
        self.0.with_mut(|_| ());
    }
}

/// A base scope: its owner, its lock and its `ended` flag
/// (`ScopeState::new_root_base`, `new_base`).
#[derive(Debug)]
struct Base {
    owner: Arc<Fiber>,
    /// The owner's control block (model 7).
    block: Arc<Block>,
    ended: Mutex<bool>,
}

impl Base {
    fn new(owner: &Arc<Fiber>) -> Self {
        Self {
            owner: Arc::clone(owner),
            block: Arc::new(Block::new()),
            ended: Mutex::new(false),
        }
    }

    /// `ScopeState::survey_step`: `look` at the owner under the lock, unless
    /// the scope has ended (`None`).
    fn step<R>(&self, look: impl FnOnce(&Fiber) -> R) -> Option<R> {
        let ended = self.ended.lock().unwrap();
        if *ended {
            return None;
        }
        self.block.touch();
        Some(look(&self.owner))
    }

    /// Model 7's twin: the owner's block read under the lock even after the
    /// scope ended; `None` still for an ended scope.
    fn step_reading_after_end<R>(&self, look: impl FnOnce(&Fiber) -> R) -> Option<R> {
        let ended = self.ended.lock().unwrap();
        self.block.touch();
        let seen = look(&self.owner);
        (!*ended).then_some(seen)
    }

    /// `close_root_base` (`ScopeState::end_base`): under the lock.
    fn end(&self) {
        *self.ended.lock().unwrap() = true;
    }
}

/// A registered root (`RootEntry`).
#[derive(Debug)]
struct Root {
    base: Arc<Base>,
    reported: AtomicBool,
}

impl Root {
    fn new(base: &Arc<Base>) -> Self {
        Self {
            base: Arc::clone(base),
            reported: AtomicBool::new(false),
        }
    }
}

/// An outcome a host receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Returned,
    Deadlocked,
    Exited(i64),
}

/// How the survey reads a base scope that has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// As busy: the pass fails (step 3, the runtime's).
    Busy,
    /// As a root that left the domain: skipped, the survey goes on with the
    /// rest (model 4's twin).
    Left,
    /// As busy, but the owner's block read first (model 7's twin).
    ReadAfterEnd,
}

/// What one run of the detector saw.
#[derive(Debug, Default)]
struct Detected {
    /// The `(id, resumes)` list of a confirmed deadlock.
    list: Option<Vec<(u64, u64)>>,
}

/// The state the detector reads for one domain.
#[derive(Debug)]
struct Domain {
    /// The registered roots (`Shared.roots`).
    roots: Mutex<Vec<Arc<Root>>>,
    registered: Condvar,
    /// The domain's groups.
    groups: Vec<Arc<Group>>,
    counts: Arc<Counts>,
    /// What the hosts of the domain's roots were sent, in order (a `cheby
    /// run` root's channel).
    outcomes: Mutex<Vec<Outcome>>,
    sent: Condvar,
}

/// A detached group (`DetachedGroup`): its members' bases, its stuck mark
/// and its tracked pool jobs.
#[derive(Debug)]
struct Group {
    members: Vec<Arc<Base>>,
    stuck: AtomicBool,
    jobs: Mutex<Vec<Arc<PoolPending>>>,
}

impl Group {
    fn new(members: Vec<Arc<Base>>) -> Self {
        Self {
            members,
            stuck: AtomicBool::new(false),
            jobs: Mutex::new(Vec::new()),
        }
    }

    /// `DetachedGroup::mark_stuck`: under the job lock, set the mark and
    /// take the list; then release every token.
    fn mark_stuck(&self) {
        let jobs = {
            let mut jobs = self.jobs.lock().unwrap();
            self.stuck.store(true, Ordering::SeqCst);
            std::mem::take(&mut *jobs)
        };
        for job in jobs {
            job.release();
        }
    }
}

impl Domain {
    fn new(roots: Vec<Arc<Root>>, groups: Vec<Arc<Group>>, counts: &Arc<Counts>) -> Self {
        Self {
            roots: Mutex::new(roots),
            registered: Condvar::new(),
            groups,
            counts: Arc::clone(counts),
            outcomes: Mutex::new(Vec::new()),
            sent: Condvar::new(),
        }
    }

    /// Registers a root (`Shared::register_root`).
    fn register(&self, root: Arc<Root>) {
        self.roots.lock().unwrap().push(root);
        self.registered.notify_all();
    }

    /// Waits until a root is registered.
    fn wait_for_a_root(&self) {
        let mut roots = self.roots.lock().unwrap();
        while roots.is_empty() {
            roots = self.registered.wait(roots).unwrap();
        }
    }

    /// Sends a host `outcome`.
    fn send(&self, outcome: Outcome) {
        self.outcomes.lock().unwrap().push(outcome);
        self.sent.notify_all();
    }

    /// The first outcome sent, once there is one (`run_main`'s receive).
    fn first_outcome(&self) -> Outcome {
        let mut outcomes = self.outcomes.lock().unwrap();
        loop {
            if let Some(outcome) = outcomes.first() {
                return *outcome;
            }
            outcomes = self.sent.wait(outcomes).unwrap();
        }
    }

    /// A root reports its outcome and unregisters.
    fn unregister(&self, root: &Arc<Root>, outcome: Outcome) {
        self.send(outcome);
        self.roots
            .lock()
            .unwrap()
            .retain(|known| !Arc::ptr_eq(known, root));
    }

    /// One run of `detect` for the domain (steps 1 to 4 and 7, for
    /// `cheby run` roots): the candidates under the registry's lock, the
    /// O(1) look, the two passes over the roots' bases and the bases of the
    /// groups not marked stuck, and the action.
    fn detect(&self, ended: Ended) -> Detected {
        let roots: Vec<Arc<Root>> = self
            .roots
            .lock()
            .unwrap()
            .iter()
            .filter(|root| !root.reported.load(Ordering::SeqCst))
            .cloned()
            .collect();
        if roots.is_empty() {
            return Detected::default();
        }
        if !self.look(&roots, ended) {
            return Detected::default();
        }
        let groups: Vec<&Arc<Group>> = self
            .groups
            .iter()
            .filter(|group| !group.stuck.load(Ordering::SeqCst))
            .collect();
        let Some(first) = self.walk(&roots, &groups, ended) else {
            return Detected::default();
        };
        let Some(second) = self.walk(&roots, &groups, ended) else {
            return Detected::default();
        };
        if first != second {
            return Detected::default();
        }
        for root in &roots {
            if root
                .reported
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.send(Outcome::Deadlocked);
            }
        }
        Detected { list: Some(second) }
    }

    /// Reads the owner of `base` under its lock as `ended` says.
    fn step<R>(base: &Base, ended: Ended, look: impl FnOnce(&Fiber) -> R) -> Option<R> {
        match ended {
            Ended::Busy | Ended::Left => base.step(look),
            Ended::ReadAfterEnd => base.step_reading_after_end(look),
        }
    }

    /// Step 1, the look: both counts 0 and every root parked, read under
    /// its base's lock while the base has not ended.
    fn look(&self, roots: &[Arc<Root>], ended: Ended) -> bool {
        if !self.counts.quiet() {
            return false;
        }
        for root in roots {
            let parked = Self::step(&root.base, ended, |fiber| {
                fiber.state.load(Ordering::SeqCst) == state::PARKED
            });
            if parked == Some(false) || (parked.is_none() && ended != Ended::Left) {
                return false;
            }
        }
        true
    }

    /// Step 3, one pass: every base's owner, then the counts.
    fn walk(
        &self,
        roots: &[Arc<Root>],
        groups: &[&Arc<Group>],
        ended: Ended,
    ) -> Option<Vec<(u64, u64)>> {
        let bases = roots
            .iter()
            .map(|root| &root.base)
            .chain(groups.iter().flat_map(|group| group.members.iter()));
        let mut seen = Vec::new();
        for base in bases {
            match Self::step(base, ended, |fiber| read(fiber, Reads::StateFirst)) {
                Some(fiber) => seen.push(fiber?),
                None if ended == Ended::Left => {}
                None => return None,
            }
        }
        if !self.counts.quiet() {
            return None;
        }
        seen.sort_unstable();
        Some(seen)
    }

    fn deadlocks_sent(&self) -> usize {
        self.outcomes
            .lock()
            .unwrap()
            .iter()
            .filter(|outcome| **outcome == Outcome::Deadlocked)
            .count()
    }
}

/// Model (4) (`woken`) and model (7): `main` (R) returns while its detached
/// fiber F parks for good. In model 4, R waits for F first, and F wakes it
/// and then parks; in model 7, R runs from the start and F is parked. R's
/// base frame ends its base under the lock, reports `Returned`,
/// unregisters and retires (its block reused). The detector runs twice
/// meanwhile. No `Deadlocked` may be sent: R returned. Every read of R's
/// block is checked by loom against the retirement.
fn root_returns(ended: Ended, woken: bool) {
    explore(move || {
        let counts = Arc::new(Counts::default());
        let (r, f) = if woken {
            (Fiber::parked(1, SITE), Fiber::running(2))
        } else {
            (Fiber::running(1), Fiber::parked(2, SITE))
        };
        let (r, f) = (Arc::new(r), Arc::new(f));
        let r_base = Arc::new(Base::new(&r));
        let f_base = Arc::new(Base::new(&f));
        let root = Arc::new(Root::new(&r_base));
        let group = Arc::new(Group::new(vec![f_base]));
        let domain = Arc::new(Domain::new(vec![Arc::clone(&root)], vec![group], &counts));
        let detached = {
            let (r, f) = (Arc::clone(&r), Arc::clone(&f));
            thread::spawn(move || {
                if woken {
                    r.wake();
                    f.park_for_good(SITE);
                }
            })
        };
        let returning = {
            let domain = Arc::clone(&domain);
            thread::spawn(move || {
                if woken {
                    r.resume();
                    r.clear_wait();
                }
                // The base frame: end the base, then report and unregister,
                // then retire.
                r_base.end();
                domain.unregister(&root, Outcome::Returned);
                r_base.block.reuse();
                r.finish();
            })
        };
        let first = domain.detect(ended);
        let second = domain.detect(ended);
        detached.join().unwrap();
        returning.join().unwrap();
        assert!(
            first.list.is_none() && second.list.is_none() && domain.deadlocks_sent() == 0,
            "a deadlock was declared for a root that returned"
        );
        assert_eq!(
            domain.outcomes.lock().unwrap().first(),
            Some(&Outcome::Returned)
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_4_no_deadlock_for_a_root_that_returned() {
    root_returns(Ended::Busy, true);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared for a root that returned")]
fn model_4_twin_surveying_a_root_after_its_unregistration_reports_it() {
    root_returns(Ended::Left, true);
}

/// The status `run_main` fixes for a deadlock (D-724, D-859).
const DEADLOCK: i64 = 103;
/// The status of model 5's termination.
const TERMINATED: i64 = 3;
/// No status fixed yet.
const NO_STATUS: i64 = -1;

/// Whether model 5's action takes the `reported` flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    /// A compare-and-swap of `reported`, and candidates without a reported
    /// root (step 7, the runtime's).
    Once,
    /// Neither: model 5's twin.
    EveryTick,
}

/// The runtime state model 5's actors share.
#[derive(Debug)]
struct Halting {
    exit_status: AtomicI64,
    halting: AtomicBool,
}

impl Halting {
    /// `Shared::fix_status`: the first status wins.
    fn fix_status(&self, status: i64) -> i64 {
        match self.exit_status.compare_exchange(
            NO_STATUS,
            status,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => status,
            Err(fixed) => fixed,
        }
    }
}

/// Model (5): `main`'s domain is deadlocked (its survey confirms on every
/// tick: the model takes the confirmation as given); the monitor acts on
/// two ticks while another fiber's termination (status 3) races it, and
/// `run_main` takes the first outcome its channel holds: `Exited(3)`
/// returns 3, `Deadlocked` fixes 103 (or finds the termination's status)
/// and halts. At most one `Deadlocked` is sent, and the host returns the
/// first status fixed.
fn act_against_termination(act: Act) {
    explore(move || {
        let counts = Arc::new(Counts::default());
        let r = Arc::new(Fiber::parked(1, SITE));
        let root = Arc::new(Root::new(&Arc::new(Base::new(&r))));
        let domain = Arc::new(Domain::new(vec![Arc::clone(&root)], Vec::new(), &counts));
        let shared = Arc::new(Halting {
            exit_status: AtomicI64::new(NO_STATUS),
            halting: AtomicBool::new(false),
        });
        let monitor = {
            let (domain, shared) = (Arc::clone(&domain), Arc::clone(&shared));
            thread::spawn(move || {
                for _ in 0..2 {
                    if shared.halting.load(Ordering::SeqCst) {
                        return;
                    }
                    let roots: Vec<Arc<Root>> = domain
                        .roots
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|root| {
                            act == Act::EveryTick || !root.reported.load(Ordering::SeqCst)
                        })
                        .cloned()
                        .collect();
                    // The survey confirms; `act` checks the halt first.
                    if shared.halting.load(Ordering::SeqCst) {
                        return;
                    }
                    for root in roots {
                        let first = match act {
                            Act::Once => root
                                .reported
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok(),
                            Act::EveryTick => true,
                        };
                        if first {
                            domain.send(Outcome::Deadlocked);
                        }
                    }
                }
            })
        };
        let termination = {
            let (domain, shared) = (Arc::clone(&domain), Arc::clone(&shared));
            thread::spawn(move || {
                // `Shared::terminate`.
                let won = shared
                    .exit_status
                    .compare_exchange(NO_STATUS, TERMINATED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok();
                shared.halting.store(true, Ordering::SeqCst);
                if won {
                    let roots = domain.roots.lock().unwrap();
                    for _ in roots.iter() {
                        domain.send(Outcome::Exited(TERMINATED));
                    }
                }
            })
        };
        // `run_main`: the first outcome on its channel.
        let status = match domain.first_outcome() {
            Outcome::Exited(status) => status,
            Outcome::Deadlocked => {
                let status = shared.fix_status(DEADLOCK);
                // `deadlocked`: writes the report, then halts.
                shared.halting.store(true, Ordering::SeqCst);
                status
            }
            Outcome::Returned => panic!("a deadlocked root returned"),
        };
        monitor.join().unwrap();
        termination.join().unwrap();
        assert!(
            domain.deadlocks_sent() <= 1,
            "the host was sent a second deadlocked outcome"
        );
        assert_eq!(
            status,
            shared.exit_status.load(Ordering::SeqCst),
            "the host's status is not the first status fixed"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_5_one_outcome_reaches_the_host_and_the_first_status_wins() {
    act_against_termination(Act::Once);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the host was sent a second deadlocked outcome")]
fn model_5_twin_without_the_cas_reports_a_root_twice() {
    act_against_termination(Act::EveryTick);
}

/// One pass of model 6 over its fiber: the `(id, resumes)` list and, in
/// the twin's pass 2, the line.
type PassAndLine = (Vec<(u64, u64)>, Option<usize>);

/// Where model 6's lines are built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lines {
    /// In the collect step after the confirmation, with the state and the
    /// resume count read again after the stack (step 5, the runtime's).
    Collected,
    /// Inside pass 2, with no re-check: model 6's twin.
    InPassTwo,
}

/// The fences around model 6's stack reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fences {
    /// The runtime's: an acquire fence after the stack's reads, before the
    /// re-check (`deadlock::collect`, V6's answer to DLm-1), and none on the
    /// resume path (`Worker::run_fiber`).
    Reader,
    /// Also a release fence after the resume count, before the fiber runs:
    /// the seqlock pattern, which C11 needs for the doc's claim.
    Pair,
}

/// Model (6): X is parked at `SITE`, its stack holding `FRAME`; a stuck
/// fiber outside the survey (D-857) wakes it at any time, and X runs (its
/// stack holds `SCRATCH` meanwhile) and parks again at the same site with
/// the same stack. A report's line is the stack word read at the site;
/// every line of a report must read `FRAME`, from a stack that did not run.
fn collect_against_a_resume(lines: Lines, fences: Fences) {
    explore(move || {
        let counts = Arc::new(Counts::default());
        let x = Arc::new(Fiber::parked(1, SITE));
        let base = Arc::new(Base::new(&x));
        let stack = Arc::new(AtomicUsize::new(FRAME));
        let outsider = {
            let x = Arc::clone(&x);
            thread::spawn(move || x.wake())
        };
        let woken = {
            let (x, stack) = (Arc::clone(&x), Arc::clone(&stack));
            thread::spawn(move || {
                x.resume();
                if fences == Fences::Pair {
                    fence(Ordering::Release);
                }
                x.clear_wait();
                stack.store(SCRATCH, Ordering::Relaxed);
                stack.store(FRAME, Ordering::Relaxed);
                x.park_for_good(SITE);
            })
        };
        let one_pass = |line: bool| -> Option<PassAndLine> {
            let seen = base.step(|fiber| {
                let seen = read(fiber, Reads::StateFirst)?;
                Some((seen, line.then(|| stack.load(Ordering::Relaxed))))
            })??;
            counts.quiet().then(|| (vec![seen.0], seen.1))
        };
        let line = (|| {
            let (first, _) = one_pass(false)?;
            match lines {
                Lines::InPassTwo => {
                    let (second, line) = one_pass(true)?;
                    (second == first).then_some(line)?
                }
                Lines::Collected => {
                    let (second, _) = one_pass(false)?;
                    if second != first {
                        return None;
                    }
                    let &[(_, resumes)] = second.as_slice() else {
                        return None;
                    };
                    // Step 5: under the scope's lock, the state and the
                    // count again, the line, then both once more.
                    base.step(|fiber| {
                        let unchanged = || {
                            fiber.state.load(Ordering::SeqCst) == state::PARKED
                                && fiber.resumes.load(Ordering::Relaxed) == resumes
                        };
                        if !unchanged() || fiber.site.load(Ordering::Relaxed) == NO_SITE {
                            return None;
                        }
                        let line = stack.load(Ordering::Relaxed);
                        fence(Ordering::Acquire);
                        unchanged().then_some(line)
                    })?
                }
            }
        })();
        outsider.join().unwrap();
        woken.join().unwrap();
        if let Some(line) = line {
            assert_eq!(line, FRAME, "a line was built from a stack that ran");
        }
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_6_the_collect_step_gives_up_a_line_whose_fiber_ran() {
    collect_against_a_resume(Lines::Collected, Fences::Pair);
}

/// The collect step with the runtime's fences, the acquire half only: in
/// C11 a relaxed read of a word the resumed fiber wrote, then an acquire
/// fence, orders nothing after the resume without a release fence on the
/// resume path, so the `SeqCst` re-read of the state may still return
/// `PARKED` (loom finds it through the initial value, which is not a
/// `SeqCst` store). The runtime relies on the hardware instead: on
/// `AArch64` `run_fiber`'s `SeqCst` load of `life` orders the fiber's later
/// stores after its count, and x86-64 is TSO (V6's answer to DLm-1).
#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a line was built from a stack that ran")]
fn model_6_with_the_acquire_fence_alone_c11_lets_a_running_stack_through() {
    collect_against_a_resume(Lines::Collected, Fences::Reader);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a line was built from a stack that ran")]
fn model_6_twin_collecting_inside_pass_two_reads_a_running_stack() {
    collect_against_a_resume(Lines::InPassTwo, Fences::Pair);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_7_a_roots_block_is_read_only_before_its_base_ends() {
    root_returns(Ended::Busy, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "Causality violation")]
fn model_7_twin_reading_the_roots_block_after_end_base_races_its_retirement() {
    root_returns(Ended::ReadAfterEnd, false);
}

/// Model (8): an earlier entry's detached fiber S waits on the blocking
/// pool (marked, its job counted in the session's domain and tracked by
/// its group G). The host tears G down: the teardown marks G stuck, then
/// the host starts a later entry, whose root L parks in a wait for good.
/// Meanwhile the pool completes S's job (clear the mark, wake, release),
/// and S, unwinding, parks in a wait for good. The detector surveys the
/// session's domain once the later entry is registered: the job leaves the
/// pool count exactly once, and a report never lists S.
fn stuck_group(release: Release) {
    explore(move || {
        let counts = Arc::new(Counts::with(0, 1));
        let s = Arc::new(Fiber::parked(1, NO_SITE));
        s.blocking.store(true, Ordering::SeqCst);
        let token = Arc::new(PoolPending::new(&counts, release));
        let group = Arc::new(Group::new(vec![Arc::new(Base::new(&s))]));
        group.jobs.lock().unwrap().push(Arc::clone(&token));
        let l = Arc::new(Fiber::parked(2, SITE));
        let later = Arc::new(Root::new(&Arc::new(Base::new(&l))));
        let domain = Arc::new(Domain::new(Vec::new(), vec![Arc::clone(&group)], &counts));
        let host = {
            let domain = Arc::clone(&domain);
            thread::spawn(move || {
                group.mark_stuck();
                domain.register(later);
            })
        };
        let pool = {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                s.blocking.store(false, Ordering::SeqCst);
                assert!(s.try_wake(), "the pool's fiber was not parked");
                token.release();
            })
        };
        let stuck = {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                s.resume();
                s.clear_wait();
                s.park_for_good(SITE);
            })
        };
        // The monitor's survey of the later entry, once it is registered.
        domain.wait_for_a_root();
        let detected = domain.detect(Ended::Busy);
        host.join().unwrap();
        pool.join().unwrap();
        stuck.join().unwrap();
        if let Some(list) = detected.list {
            assert!(
                list.iter().all(|&(id, _)| id != s.id),
                "the survey listed a stuck fiber"
            );
        }
        assert_eq!(counts.pool.load(Ordering::SeqCst), 0);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn model_8_a_stuck_groups_job_leaves_the_count_once() {
    stuck_group(Release::Once);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a domain's pending pool count fell below 0")]
fn model_8_twin_lowering_at_the_mark_and_the_completion_counts_twice() {
    stuck_group(Release::EveryCaller);
}
