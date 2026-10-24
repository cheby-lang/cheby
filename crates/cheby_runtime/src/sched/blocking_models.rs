//! `loom` models of the detached completion's protocol (D-1065,
//! `sched/blocking.rs`, `run_blocking_claiming`): a fiber that waits on a
//! detached blocking-pool job, the job's completion that claims the wait,
//! and the cancellation walk that cancels it, with D-786's blocking mark
//! (D-814), the domain's pending pool count (D-857) and the runtime's pool
//! count (D-815), and standard input's refill, which claims the wait under
//! the reader's lock before it takes a line (D-548, D-823).
//!
//! Each model is the protocol, not the runtime's code, as in
//! `sched/io_models.rs`: `loom` atomics stand for the fiber's state word,
//! its blocking mark, the wait header, the domain's pending count and the
//! runtime's pool count; a `loom` mutex stands for the fiber lock, which
//! guards the target and the published header, and another for the
//! reader's lock. A thread plays one fiber and the worker that runs it: it
//! parks as the runtime parks (`PARKING`, then the worker's
//! compare-and-swap to `PARKED`, or a requeue when a waker found it
//! `PARKING`), and it resumes as `Worker::run_fiber` resumes it. The run
//! queue is a flag under a lock with a condition variable, which the other
//! threads also signal when they end, so that a fiber nobody woke reports
//! a lost wake instead of blocking. loom 0.7 needs no `--cfg loom`:
//!
//! ```text
//! cargo test -p cheby_runtime sched::blocking_models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step              | Mirrors (`crates/cheby_runtime/src/`, function)                        |
//! | ----------------------- | ----------------------------------------------------------------------- |
//! | `Fiber::wait`           | `sched/blocking.rs` `run_blocking_claiming_in`, `on_pool_detached`      |
//! | `wait_refused`          | `sched/blocking.rs` `on_pool_detached`, its `Submitted::Refused` branch |
//! | `Fiber::begin_park`     | `sched/fiber.rs` `begin_park`                                           |
//! | `Fiber::end_park`       | `sched/worker.rs` `Worker::parked`                                      |
//! | `Fiber::try_wake`       | `sched/mod.rs` `wake`                                                   |
//! | `Fiber::resume`         | `sched/worker.rs` `Worker::run_fiber`                                   |
//! | `complete`              | `sched/blocking.rs` `Detached::complete`, `Claim::claim`                |
//! | `walk`                  | `fiber_ops/cancel.rs` `reach` (`WaitHeader::try_cancel`), `cancel`      |
//! | `Shared::publish`       | `fiber_ops/cancel.rs` `publish`                                         |
//! | `Shared::abandon`       | `sched/blocking.rs` `Detached::abandon`                                 |
//! | `Pending::release`      | `sched/deadlock.rs` `PoolPending::release`                              |
//! | `Shared::leave`         | `sched/blocking.rs` `leave`                                             |
//! | `refill`                | `io.rs` `read_line_on`'s job (`fill_line`, the claim, `read_line`)      |
//!
//! The models, each with `#[should_panic]` twins that break one rule:
//!
//! 1. A fiber starts a detached wait (the entry check, the counts, the
//!    mark, `PARKING`, the publication, the submit and the park) while the
//!    job's completion and a cancellation's walk race for its header from
//!    the start: the walk may come before the publication (which refuses
//!    it, and no job runs), between it and the park, or after. Exactly one
//!    of the completion and the walk wakes the fiber, once, and only when
//!    it is parked or parking; no wake is lost; the fiber returns the
//!    result exactly when the completion won, and the cancellation
//!    otherwise; the mark is clear when it runs again; the job's result is
//!    dropped once (by the fiber when it won, on the pool thread when it
//!    lost), or never made (a job cancelled before it started does not
//!    run); the domain's pending count and the runtime's pool count fall
//!    exactly once and end at 0. Twins: a completion that does not claim
//!    the header (`run_blocking`'s, which wakes when the job ends) breaks
//!    a rule; a completion that lost but still leaves the runtime's pool
//!    count lowers it twice; a walk that leaves the mark set lets a woken
//!    fiber run marked (a teardown would count it as blocked); a wake that
//!    ignores a fiber still `PARKING` is lost.
//! 2. The completion comes after the fiber has gone: the walk won, the
//!    fiber abandoned the job and ended, and only then does the job
//!    return. The completion wakes nothing, drops the result on its own
//!    thread and leaves both counts alone. Twin: a completion that does not
//!    claim wakes the fiber that has gone.
//! 3. Standard input's refill: a read on a fiber waits for a line that
//!    arrives while a cancellation races for its wait; a second read asks
//!    after the refill took the reader's lock (or was skipped, cancelled
//!    before it started). The line goes to exactly one read, never to both
//!    and never to neither: to the first when its refill claimed the wait,
//!    else it stays in the reader (or in the input) and the second read
//!    takes it. Twin: a refill that takes the line before it claims loses
//!    the line when the walk won.
//! 4. After a refused thread (D-817) the header is already published, so
//!    the fiber's own claim, made before it runs the jobs inline, races
//!    the walk. When the claim wins, no walk wakes the fiber and the job's
//!    result is returned; when the walk won, the fiber parks until the
//!    walk's wake, which comes once the walk has released its locks, and
//!    the job never runs. The walk's wake finds the fiber parked or
//!    parking, never running on or ended. Twin: a fiber that stops parking
//!    before its claim and goes on whatever the claim found takes the
//!    walk's late wake while it runs, or after it ended (review finding
//!    SOUND-W4B-1).
//!
//! What the models leave out: the pool's queue and threads (the completion
//! thread is the pool thread once it took the job), the inline paths
//! without a runtime or after a stop gave up on the pool (no header is
//! published there), other fibers' jobs that a refused thread hands to
//! the fiber to run inline, a stop's and a halt's skip of the wake (`wakes_nothing`,
//! `blocking.rs`'s other tests), the deadlock survey (a marked fiber never
//! passes it, `deadlock_models`' model 2 covers the pool count's order),
//! and the fast path of a read (D-823), which a counted waiter turns off.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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

/// A wait header's states (`WaitState`).
mod header {
    pub(super) const WAITING: u8 = 0;
    pub(super) const CLAIMED: u8 = 1;
    pub(super) const CANCELLED: u8 = 3;
}

/// The prefix of every broken rule, so that a twin that may break more
/// than one rule names the first one it meets.
const BROKEN: &str = "the detached wait broke a rule";

/// The other threads of a model, besides the fiber, that may wake it.
const WAKERS: usize = 2;

/// One fiber's scheduling words, its mark and its run queue.
#[derive(Debug)]
struct Fiber {
    state: AtomicU8,
    /// D-786's blocking mark.
    marked: AtomicBool,
    /// Whether a wake that ignores a `PARKING` fiber is used (twin).
    lose_parking_wakes: bool,
    /// The run queue's flag, and how many other threads have ended.
    queue: Mutex<(bool, usize)>,
    pushed: Condvar,
}

impl Fiber {
    fn new(lose_parking_wakes: bool) -> Self {
        Self {
            state: AtomicU8::new(state::RUNNING),
            marked: AtomicBool::new(false),
            lose_parking_wakes,
            queue: Mutex::new((false, 0)),
            pushed: Condvar::new(),
        }
    }

    /// `begin_park`.
    fn begin_park(&self) {
        self.state.store(state::PARKING, Ordering::SeqCst);
    }

    /// `cancel_park`: the fiber did not park after all.
    fn cancel_park(&self) {
        self.state.store(state::RUNNING, Ordering::SeqCst);
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
                self.push();
            }
            Err(other) => panic!("{BROKEN}: a parking fiber was in the state {other}"),
        }
    }

    /// `sched::wake`, by a waker that won the fiber's header: the fiber
    /// must be parked or parking, or the wake is a second one, or comes
    /// after the fiber has gone.
    fn wake(&self) {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                state::PARKED => state::RUNNABLE,
                state::PARKING if self.lose_parking_wakes => return,
                state::PARKING => state::PARKING_WOKEN,
                _ => panic!("{BROKEN}: a wake found the fiber neither parked nor parking"),
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
    }

    fn push(&self) {
        self.queue.lock().unwrap().0 = true;
        self.pushed.notify_all();
    }

    /// Another thread of the model has ended: a fiber that waits to be
    /// queued looks again.
    fn waker_ended(&self) {
        let mut queue = self.queue.lock().unwrap();
        queue.1 = queue.1.saturating_add(1);
        drop(queue);
        self.pushed.notify_all();
    }

    /// `Worker::run_fiber`: waits until queued, then runs; a fiber still
    /// not queued once every other thread has ended was never woken.
    fn resume(&self) {
        let mut queue = self.queue.lock().unwrap();
        while !queue.0 {
            assert!(
                queue.1 < WAKERS,
                "{BROKEN}: a wake was lost, and the fiber parked for good"
            );
            queue = self.pushed.wait(queue).unwrap();
        }
        queue.0 = false;
        drop(queue);
        self.state.store(state::RUNNING, Ordering::SeqCst);
    }
}

/// The job's result: counts its drops, and whether the pool thread
/// dropped it.
#[derive(Debug)]
struct Res {
    drops: Arc<AtomicUsize>,
    on_pool: Arc<AtomicBool>,
    pool_thread: bool,
}

impl Drop for Res {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        self.on_pool.store(self.pool_thread, Ordering::SeqCst);
    }
}

/// How a completion that lost its header behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    /// The runtime's: claims first; a loser drops the result and leaves.
    Claims,
    /// `run_blocking`'s completion: no claim, it always wakes (twin).
    NoClaim,
    /// Claims, but a loser still lowers the runtime's pool count (twin).
    LoserLeaves,
}

/// What the walk does once it won the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    /// The runtime's: clears the mark under the fiber lock, then wakes.
    ClearsMark,
    /// Wakes the fiber with its mark still set (twin).
    KeepsMark,
}

/// What a model's threads share: the fiber, its lock's state, its header,
/// the counts and the result.
#[derive(Debug)]
struct Shared {
    fiber: Fiber,
    /// The fiber lock: (the target is set, the header is published).
    lock: Mutex<(bool, bool)>,
    header: AtomicU8,
    /// The domain's pending pool count, and the job's token in it.
    pending: AtomicUsize,
    counted: AtomicBool,
    /// The runtime's count of fibers waiting on the pool.
    pool_waiting: AtomicUsize,
    /// Whether the fiber submitted the job: `None` until it decided.
    submitted: Mutex<Option<bool>>,
    decided: Condvar,
    /// The job's result, once the completion stored it.
    result: Mutex<Option<Res>>,
    drops: Arc<AtomicUsize>,
    on_pool: Arc<AtomicBool>,
}

impl Shared {
    fn new(lose_parking_wakes: bool) -> Self {
        Self {
            fiber: Fiber::new(lose_parking_wakes),
            lock: Mutex::new((false, false)),
            header: AtomicU8::new(header::WAITING),
            pending: AtomicUsize::new(0),
            counted: AtomicBool::new(false),
            pool_waiting: AtomicUsize::new(0),
            submitted: Mutex::new(None),
            decided: Condvar::new(),
            result: Mutex::new(None),
            drops: Arc::new(AtomicUsize::new(0)),
            on_pool: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `cancel::publish`: refused when a target is set.
    fn publish(&self) -> bool {
        let mut lock = self.lock.lock().unwrap();
        if lock.0 {
            return false;
        }
        lock.1 = true;
        true
    }

    /// `PoolPending::release`: once.
    fn release(&self) {
        if self.counted.swap(false, Ordering::SeqCst) {
            assert!(
                self.pending.fetch_sub(1, Ordering::SeqCst) != 0,
                "{BROKEN}: a domain's pending pool count fell below 0"
            );
        }
    }

    /// `leave`: one fiber less waits on the pool.
    fn leave(&self) {
        assert!(
            self.pool_waiting.fetch_sub(1, Ordering::SeqCst) != 0,
            "{BROKEN}: the runtime's pool count fell below 0"
        );
    }

    /// `Detached::abandon`.
    fn abandon(&self) {
        self.release();
        self.leave();
    }

    fn decide(&self, submitted: bool) {
        *self.submitted.lock().unwrap() = Some(submitted);
        self.decided.notify_all();
    }

    /// The pool thread: waits until the fiber decided, and returns whether
    /// it submitted the job.
    fn job_submitted(&self) -> bool {
        let mut submitted = self.submitted.lock().unwrap();
        loop {
            if let Some(submitted) = *submitted {
                return submitted;
            }
            submitted = self.decided.wait(submitted).unwrap();
        }
    }

    /// `Claim::claim`.
    fn claim(&self) -> bool {
        self.header
            .compare_exchange(
                header::WAITING,
                header::CLAIMED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
            || self.header.load(Ordering::SeqCst) == header::CLAIMED
    }

    fn result(&self, pool_thread: bool) -> Res {
        Res {
            drops: Arc::clone(&self.drops),
            on_pool: Arc::clone(&self.on_pool),
            pool_thread,
        }
    }
}

/// How the fiber's wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Refused before any job: a target came first (D-816).
    NoJob,
    /// The completion won: the fiber took the result.
    Result,
    /// The walk won: the fiber abandoned the job.
    Cancelled,
}

/// The fiber's side (`run_blocking_claiming_in`, `on_pool_detached`).
fn wait(shared: &Shared) -> Outcome {
    // The entry check (D-816).
    if shared.lock.lock().unwrap().0 {
        shared.decide(false);
        return Outcome::NoJob;
    }
    // The counts: the domain's token, then `enter`.
    shared.pending.fetch_add(1, Ordering::SeqCst);
    shared.counted.store(true, Ordering::SeqCst);
    shared.pool_waiting.fetch_add(1, Ordering::SeqCst);
    // `PARKING`, the mark, the publication.
    shared.fiber.begin_park();
    shared.fiber.marked.store(true, Ordering::SeqCst);
    if !shared.publish() {
        shared.fiber.cancel_park();
        shared.fiber.marked.store(false, Ordering::SeqCst);
        shared.abandon();
        shared.decide(false);
        return Outcome::NoJob;
    }
    // The submit, then the park.
    shared.decide(true);
    shared.fiber.end_park();
    shared.fiber.resume();
    assert!(
        !shared.fiber.marked.load(Ordering::SeqCst),
        "{BROKEN}: a woken fiber still carried the blocking mark"
    );
    // `unpublish`, then the header.
    shared.lock.lock().unwrap().1 = false;
    if shared.header.load(Ordering::SeqCst) == header::CLAIMED {
        let result = shared.result.lock().unwrap().take();
        let Some(mut result) = result else {
            panic!("{BROKEN}: the fiber resumed without its result");
        };
        // Dropped here, on the fiber.
        result.pool_thread = false;
        drop(result);
        return Outcome::Result;
    }
    shared.fiber.marked.store(false, Ordering::SeqCst);
    shared.abandon();
    Outcome::Cancelled
}

/// The fiber's thread: the wait, then the fiber ends (its slot may be
/// reused from now on, so no waker may touch it).
fn fiber_thread(shared: &Shared) -> Outcome {
    let outcome = wait(shared);
    shared.fiber.state.store(state::DONE, Ordering::SeqCst);
    outcome
}

/// The pool thread takes the job (`on_pool_detached`'s job): a job whose
/// wait was cancelled before it started does not run; otherwise it runs
/// and completes.
fn run_job(shared: &Shared, kind: Completion) {
    if kind == Completion::Claims && shared.header.load(Ordering::SeqCst) == header::CANCELLED {
        return;
    }
    complete(shared, kind);
}

/// The completion, on the pool thread, once the job returned
/// (`Detached::complete`).
fn complete(shared: &Shared, kind: Completion) {
    let result = shared.result(true);
    let won = kind == Completion::NoClaim || shared.claim();
    if !won {
        drop(result);
        if kind == Completion::LoserLeaves {
            shared.release();
            shared.leave();
        }
        return;
    }
    *shared.result.lock().unwrap() = Some(result);
    shared.fiber.marked.store(false, Ordering::SeqCst);
    shared.fiber.wake();
    shared.release();
    shared.leave();
}

/// The walk (`cancel::reach`, then `cancel`'s wake after the locks).
fn walk(shared: &Shared, kind: Walk) {
    let won = {
        let mut lock = shared.lock.lock().unwrap();
        lock.0 = true;
        let won = lock.1
            && shared
                .header
                .compare_exchange(
                    header::WAITING,
                    header::CANCELLED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok();
        if won && kind == Walk::ClearsMark {
            shared.fiber.marked.store(false, Ordering::SeqCst);
        }
        won
    };
    if won {
        shared.fiber.wake();
    }
}

/// At most three preemptions per execution, as the other models bound
/// them.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(f);
}

/// The checks every model ends with, once every thread has ended.
fn check_end(shared: &Shared, outcome: Outcome) {
    assert_eq!(
        shared.pending.load(Ordering::SeqCst),
        0,
        "{BROKEN}: a pending pool count was left"
    );
    assert_eq!(
        shared.pool_waiting.load(Ordering::SeqCst),
        0,
        "{BROKEN}: a pool count was left"
    );
    let drops = shared.drops.load(Ordering::SeqCst);
    match outcome {
        Outcome::NoJob => assert_eq!(drops, 0, "{BROKEN}: a refused wait ran its job"),
        Outcome::Result => {
            assert_eq!(drops, 1, "{BROKEN}: the result was not dropped once");
            assert!(
                !shared.on_pool.load(Ordering::SeqCst),
                "{BROKEN}: the result the fiber took was dropped on the pool"
            );
        }
        // A job cancelled before it started never ran, and made no result.
        Outcome::Cancelled => {
            assert!(drops <= 1, "{BROKEN}: the result was dropped twice");
            assert!(
                drops == 0 || shared.on_pool.load(Ordering::SeqCst),
                "{BROKEN}: an abandoned job's result was not dropped on the pool"
            );
        }
    }
}

/// Model (1): the wait against its completion and a cancellation's walk.
fn cancel_against_completion(completion: Completion, walk_kind: Walk, lose_parking_wakes: bool) {
    explore(move || {
        let shared = Arc::new(Shared::new(lose_parking_wakes));
        let pool = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                if shared.job_submitted() {
                    run_job(&shared, completion);
                }
                shared.fiber.waker_ended();
            })
        };
        let canceller = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                walk(&shared, walk_kind);
                shared.fiber.waker_ended();
            })
        };
        let outcome = fiber_thread(&shared);
        pool.join().unwrap();
        canceller.join().unwrap();
        let claimed = shared.header.load(Ordering::SeqCst) == header::CLAIMED;
        match outcome {
            Outcome::Result => assert!(claimed, "{BROKEN}: a result without a claim"),
            Outcome::Cancelled => assert!(!claimed, "{BROKEN}: a claimed wait was cancelled"),
            Outcome::NoJob => {}
        }
        check_end(&shared, outcome);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn blocking_model_1_cancel_and_completion_wake_once_and_count_once() {
    cancel_against_completion(Completion::Claims, Walk::ClearsMark, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the detached wait broke a rule")]
fn blocking_model_1_twin_a_completion_that_does_not_claim_breaks_a_rule() {
    cancel_against_completion(Completion::NoClaim, Walk::ClearsMark, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the runtime's pool count fell below 0")]
fn blocking_model_1_twin_a_losing_completion_that_leaves_lowers_the_count_twice() {
    cancel_against_completion(Completion::LoserLeaves, Walk::ClearsMark, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a woken fiber still carried the blocking mark")]
fn blocking_model_1_twin_a_walk_that_keeps_the_mark_wakes_a_marked_fiber() {
    cancel_against_completion(Completion::Claims, Walk::KeepsMark, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a wake was lost, and the fiber parked for good")]
fn blocking_model_1_twin_a_wake_that_ignores_a_parking_fiber_is_lost() {
    cancel_against_completion(Completion::Claims, Walk::ClearsMark, true);
}

/// Model (2): the walk wins, the fiber abandons the job and ends, and only
/// then does the job return.
fn completion_after_the_fiber_has_gone(completion: Completion) {
    explore(move || {
        let shared = Arc::new(Shared::new(false));
        let canceller = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                // The walk waits until the fiber has published its header,
                // so that it wins it.
                while !shared.lock.lock().unwrap().1 {
                    thread::yield_now();
                }
                walk(&shared, Walk::ClearsMark);
                shared.fiber.waker_ended();
            })
        };
        let fiber = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || fiber_thread(&shared))
        };
        let outcome = fiber.join().unwrap();
        canceller.join().unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(shared.job_submitted());
        let pool = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || complete(&shared, completion))
        };
        pool.join().unwrap();
        assert_eq!(
            shared.fiber.state.load(Ordering::SeqCst),
            state::DONE,
            "{BROKEN}: a completion touched a fiber that has gone"
        );
        check_end(&shared, outcome);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn blocking_model_2_a_completion_after_the_fiber_has_gone_wakes_nothing() {
    completion_after_the_fiber_has_gone(Completion::Claims);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a wake found the fiber neither parked nor parking")]
fn blocking_model_2_twin_a_completion_that_does_not_claim_wakes_a_fiber_that_has_gone() {
    completion_after_the_fiber_has_gone(Completion::NoClaim);
}

/// How the refill takes its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refill {
    /// The runtime's: claims the wait, then takes the line.
    ClaimsFirst,
    /// Takes the line, then claims; drops it when the claim fails (twin).
    TakesFirst,
}

/// A flag one thread sets and another waits for.
#[derive(Debug, Default)]
struct Flag {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Flag {
    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut set = self.set.lock().unwrap();
        while !*set {
            set = self.changed.wait(set).unwrap();
        }
    }
}

/// What the reader's lock guards: the lines it holds, and (standing for
/// the input under it) the lines not read from the input yet.
#[derive(Debug)]
struct Reader {
    lines: usize,
    input: usize,
}

/// Model (3): a read's refill races a cancellation's walk for the read's
/// wait while its line arrives; a second read asks once the refill holds
/// the reader's lock (or was skipped). Lines are counted: the input holds
/// one.
fn refill_against_cancel(refill: Refill) {
    explore(move || {
        let shared = Arc::new(Shared::new(false));
        let reader = Arc::new(Mutex::new(Reader { lines: 0, input: 1 }));
        let locked = Arc::new(Flag::default());
        // The first read's line, once its fiber has it.
        let first = Arc::new(AtomicUsize::new(0));
        let pool = {
            let (shared, reader, locked) = (
                Arc::clone(&shared),
                Arc::clone(&reader),
                Arc::clone(&locked),
            );
            thread::spawn(move || {
                // A refill cancelled before it started does not run, and
                // the line stays in the input.
                let skipped = refill == Refill::ClaimsFirst
                    && shared.header.load(Ordering::SeqCst) == header::CANCELLED;
                if shared.job_submitted() && !skipped {
                    // The job (`fill_line`, the claim, `read_line`), under
                    // the reader's lock: the line moves from the input
                    // into the reader.
                    let mut lines = reader.lock().unwrap();
                    locked.set();
                    lines.lines = lines.lines.saturating_add(lines.input);
                    lines.input = 0;
                    let taken = match refill {
                        Refill::ClaimsFirst => shared.claim().then(|| {
                            lines.lines = lines.lines.saturating_sub(1);
                        }),
                        Refill::TakesFirst => {
                            lines.lines = lines.lines.saturating_sub(1);
                            shared.claim().then_some(())
                        }
                    };
                    drop(lines);
                    // The completion: a refill that claimed hands the line
                    // over and wakes the fiber.
                    if taken.is_some() {
                        *shared.result.lock().unwrap() = Some(shared.result(true));
                        shared.fiber.marked.store(false, Ordering::SeqCst);
                        shared.fiber.wake();
                        shared.release();
                        shared.leave();
                    }
                } else {
                    locked.set();
                }
                shared.fiber.waker_ended();
            })
        };
        let canceller = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                walk(&shared, Walk::ClearsMark);
                shared.fiber.waker_ended();
            })
        };
        let second = {
            let (reader, locked) = (Arc::clone(&reader), Arc::clone(&locked));
            thread::spawn(move || {
                // Asks after the refill took the lock: waits for it.
                locked.wait();
                // Its own refill: a line the reader holds, else one read
                // from the input.
                let mut lines = reader.lock().unwrap();
                if lines.lines == 0 {
                    lines.lines = lines.input;
                    lines.input = 0;
                }
                let taken = lines.lines.min(1);
                lines.lines = lines.lines.saturating_sub(taken);
                taken
            })
        };
        let outcome = fiber_thread(&shared);
        if outcome == Outcome::Result {
            first.store(1, Ordering::SeqCst);
        }
        pool.join().unwrap();
        canceller.join().unwrap();
        let second = second.join().unwrap();
        let left = {
            let lines = reader.lock().unwrap();
            lines.lines.saturating_add(lines.input)
        };
        let got = first.load(Ordering::SeqCst);
        let all = got.saturating_add(second).saturating_add(left);
        assert!(all >= 1, "{BROKEN}: a line was lost");
        assert_eq!(all, 1, "{BROKEN}: a line was read twice");
        if outcome != Outcome::Result {
            assert_eq!(got, 0, "{BROKEN}: a cancelled read took its line");
        }
        assert_eq!(
            shared.pending.load(Ordering::SeqCst),
            0,
            "{BROKEN}: a pending pool count was left"
        );
        assert_eq!(
            shared.pool_waiting.load(Ordering::SeqCst),
            0,
            "{BROKEN}: a pool count was left"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn blocking_model_3_a_cancelled_refill_leaves_its_line_for_the_next_read() {
    refill_against_cancel(Refill::ClaimsFirst);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a line was lost")]
fn blocking_model_3_twin_a_refill_that_takes_before_it_claims_loses_the_line() {
    refill_against_cancel(Refill::TakesFirst);
}

/// How the fiber goes on after a refused thread (D-817).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// The runtime's: claims its own wait while still `PARKING`; when the
    /// walk won it first, parks until the walk's wake.
    WaitsForTheWalk,
    /// `cancel_park` first, then the claim, and goes on whatever it found:
    /// the code before review finding SOUND-W4B-1 (twin).
    GoesOn,
}

/// The fiber's side after a refused thread (`on_pool_detached`'s
/// `Submitted::Refused` branch): the start of [`wait`], then the fiber's
/// own claim and the job run inline on its own thread.
fn wait_refused(shared: &Shared, kind: Refused) -> Outcome {
    // The entry check (D-816).
    if shared.lock.lock().unwrap().0 {
        return Outcome::NoJob;
    }
    // The counts, `PARKING`, the mark, the publication, as in `wait`.
    shared.pending.fetch_add(1, Ordering::SeqCst);
    shared.counted.store(true, Ordering::SeqCst);
    shared.pool_waiting.fetch_add(1, Ordering::SeqCst);
    shared.fiber.begin_park();
    shared.fiber.marked.store(true, Ordering::SeqCst);
    if !shared.publish() {
        shared.fiber.cancel_park();
        shared.fiber.marked.store(false, Ordering::SeqCst);
        shared.abandon();
        return Outcome::NoJob;
    }
    // The submit is refused. The fiber's own claim: only the walk can
    // have settled the header, since the job has not run.
    let own_claim = || {
        shared
            .header
            .compare_exchange(
                header::WAITING,
                header::CLAIMED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    };
    match kind {
        Refused::WaitsForTheWalk => {
            if own_claim() {
                shared.fiber.cancel_park();
            } else {
                // The walk won: wait for its wake.
                shared.fiber.end_park();
                shared.fiber.resume();
                assert!(
                    !shared.fiber.marked.load(Ordering::SeqCst),
                    "{BROKEN}: a woken fiber still carried the blocking mark"
                );
            }
        }
        Refused::GoesOn => {
            shared.fiber.cancel_park();
            let _ = own_claim();
        }
    }
    // The jobs, inline, under the mark. The fiber's own job does not run
    // once the walk won; otherwise its completion stores the result and
    // clears the mark, and its wake finds the fiber `RUNNING` on this very
    // thread and does nothing (`sched::wake` returns false), so the model
    // leaves it out.
    shared.fiber.marked.store(true, Ordering::SeqCst);
    if shared.header.load(Ordering::SeqCst) != header::CANCELLED {
        assert!(shared.claim(), "{BROKEN}: an inline job lost its claim");
        *shared.result.lock().unwrap() = Some(shared.result(false));
        shared.fiber.marked.store(false, Ordering::SeqCst);
        shared.release();
        shared.leave();
    }
    shared.fiber.marked.store(false, Ordering::SeqCst);
    // `unpublish`, then the header, as in `wait`.
    shared.lock.lock().unwrap().1 = false;
    if shared.header.load(Ordering::SeqCst) == header::CLAIMED {
        let Some(result) = shared.result.lock().unwrap().take() else {
            panic!("{BROKEN}: the fiber resumed without its result");
        };
        drop(result);
        return Outcome::Result;
    }
    shared.abandon();
    Outcome::Cancelled
}

/// Model (4): after a refused thread, the fiber's own claim races a
/// cancellation's walk for the published header. The walk's wake must
/// find the fiber parked or parking, never running on (where it would
/// land on the fiber's next park) or ended.
fn refused_against_cancel(kind: Refused) {
    explore(move || {
        let shared = Arc::new(Shared::new(false));
        // The pool, which starts no thread here, never wakes the fiber.
        shared.fiber.waker_ended();
        let canceller = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                walk(&shared, Walk::ClearsMark);
                shared.fiber.waker_ended();
            })
        };
        let outcome = wait_refused(&shared, kind);
        // The fiber ends: no wake may reach it from now on.
        shared.fiber.state.store(state::DONE, Ordering::SeqCst);
        canceller.join().unwrap();
        let claimed = shared.header.load(Ordering::SeqCst) == header::CLAIMED;
        match outcome {
            Outcome::Result => assert!(claimed, "{BROKEN}: a result without a claim"),
            Outcome::Cancelled => {
                assert!(!claimed, "{BROKEN}: a claimed wait was cancelled");
                assert_eq!(
                    shared.drops.load(Ordering::SeqCst),
                    0,
                    "{BROKEN}: a job the walk cancelled ran inline"
                );
            }
            Outcome::NoJob => {}
        }
        check_end(&shared, outcome);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn blocking_model_4_after_a_refused_thread_the_fiber_waits_for_the_walks_wake() {
    refused_against_cancel(Refused::WaitsForTheWalk);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a wake found the fiber neither parked nor parking")]
fn blocking_model_4_twin_a_fiber_that_goes_on_takes_a_late_wake() {
    refused_against_cancel(Refused::GoesOn);
}
