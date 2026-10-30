//! The fiber side of the blocking pool (D-683, D-786): a fiber parks while
//! a pool thread runs its job, and the pool thread wakes it.
//!
//! # Protocol
//!
//! [`run_blocking`] on a fiber of a runtime:
//!
//! 1. reads its cancellation target (`cancel::check`): a fiber already
//!    being cancelled starts no job, so no line is consumed and no file is
//!    written (D-816);
//! 2. sets D-786's blocking mark (`FiberSched::set_blocking`), then stores
//!    `PARKING` (`begin_park`), then submits the job, then parks
//!    (`park(AfterSwitch::Park(None))`). It publishes no wait node: the
//!    pool is the only waker of a fiber parked on a job, and the
//!    cancellation walk reaches it only through its entry check, after the
//!    job returns. A job that completes before the switch finds the fiber
//!    `PARKING` and turns it into `PARKING_WOKEN`, and its worker requeues
//!    it after the switch (`Worker::parked`);
//! 3. on the pool thread, the job runs, then its completion stores the
//!    result, clears the mark (`set_blocking(false)`, a `SeqCst` store),
//!    and only then wakes the fiber, then counts the job as done
//!    ([`leave`]). A completion during a halt, or after a stop gave up on
//!    the pool, wakes nothing, and the result is dropped with the job
//!    (D-815): a stop releases the code tokens once
//!    its workers are joined, and a fiber parked on the pool holds no
//!    worker, so a later wake could resume code that was freed. Until it
//!    gives up, a stop keeps its workers while a fiber of the runtime
//!    waits on the pool ([`holds_stop`]), as it keeps them while a fiber
//!    is runnable (D-708): the completion's wake reaches a live worker,
//!    and a root that blocks while the host stops the runtime runs to its
//!    end and reports. The stop's join gives up ([`poll_stop`]) when two
//!    polls 5 ms apart find a fiber still waiting, every remaining worker
//!    idle and no job completed between them (D-804's shape): that fiber
//!    waits on input that may never end and is treated as one blocked
//!    inline (D-786); a blocking call after that runs inline;
//! 4. once resumed, reads its target again: if a cancellation arrived
//!    during the job, the Rust result is dropped before any Cheby value is
//!    made and the answer is `Err(Cancelled)`, which the kernel raises
//!    (D-670, D-683). The work finished first, and its result is released
//!    (§10.7, §12.7): a cancelled `read_line` consumed its line.
//!
//! [`run_blocking_detached`] (and [`run_blocking_claiming`], on which it
//! is built) is the detached completion of D-1065, for jobs whose effect a
//! cancelled fiber cannot miss: a refill of standard input's reader (D-823)
//! or of a `File`'s read-ahead buffer, a host-name lookup. Steps 1 and 3
//! are `run_blocking`'s, and so are the mark, the inline paths and the
//! stop, with these differences:
//!
//! 2. besides the mark and `PARKING`, the fiber publishes a wait header on
//!    the heap (`WaitHeader::on_pool`, an `Arc` the job's completion holds
//!    too) to the cancellation walk (`cancel::publish`) before it submits
//!    the job; a target already set refuses the publication, and the fiber
//!    returns `Err(Cancelled)` with no job started (D-816);
//! 3. the header is settled exactly once, as a wait node's is
//!    (`sched/wait.rs`): the job's completion claims it `Waiting ->
//!    Claimed` ([`Claim::claim`]; a refill claims it itself under the
//!    reader's lock, before it takes a line, so that a line is either
//!    handed to its read or left in the reader, never both), or the walk
//!    cancels it `Waiting -> Cancelled` (`cancel::reach`). Whoever wins
//!    clears the mark and wakes the fiber, once; the loser touches nothing
//!    of the fiber, which may have gone. A completion that lost drops the
//!    job's result on its pool thread (D-1065), and a job whose wait was
//!    cancelled before it started does not run. After a refused thread
//!    (D-817) the fiber claims its own wait before it runs the jobs
//!    inline, where nothing can abandon them: the walk then cannot clear
//!    the mark of a fiber blocked inline. A walk that won the header
//!    before that claim wakes the fiber only once it has released its
//!    locks, so the fiber parks until that wake before it runs the jobs
//!    (a fiber is woken once per park);
//! 4. once resumed, the fiber unpublishes the header and reads it: on
//!    `Claimed` it returns the result, also when a cancellation arrived
//!    after the claim (the operation finished first, as a channel `receive`
//!    whose sender won its node returns the value; the cancellation is
//!    delivered at the next entry check); on `Cancelled` it abandons the
//!    job: it takes the job out of its domain's pending count and out of
//!    the runtime's pool count itself, and returns `Err(Cancelled)` at
//!    once, while the job runs on to its end on its pool thread
//!    (D-1165: an abandoned job holds no stop, D-815,
//!    no leak check, D-710, and no deadlock report back, D-857, since no
//!    fiber waits for it any more).
//!
//! Every other pool job waits (D-670, as D-1065 keeps it): a caller must
//! know whether a write, an open, a close or a directory change happened.
//! The protocol's orderings are modelled in `sched/blocking_models.rs`.
//!
//! The mark stays set from before the fiber parks until the pool thread
//! clears it, right before the wake (D-814), so the
//! teardown's survey, D-804's two polls, the per-test leak check (D-833)
//! and `run_main`'s skip-and-count treat a fiber waiting on the pool as they
//! treat one blocked inline (D-786, D-710). The pool thread, not the fiber,
//! clears it: a woken fiber that waits in a run queue (behind a busy
//! worker, for more than the teardown's 5 ms poll) is then parked or
//! runnable and unmarked, never skipped as blocked.
//!
//! Without a runtime (an isolated entry: the REPL until it runs on a
//! runtime, the Windows thread backend, D-692, D-714) the job runs inline
//! on the calling thread with the mark set for its duration, as before
//! the pool. When the system refuses a pool thread and none runs, the job
//! runs inline on the worker with the mark and the hand-off
//! (`hand_off_before_blocking`, D-817).
//!
//! # For the deadlock detector (D-857, D-860)
//!
//! A job is pending in its fiber's domain from before the submit until its
//! completion has woken the fiber, or skipped the wake, or, for a detached
//! job, until its fiber abandoned it: `on_pool` takes a
//! `deadlock::PoolPending` token, which raises the domain's pool count and
//! is registered with the fiber's group, and the completion releases it
//! after the wake. A stuck group's mark releases its jobs' tokens instead,
//! each once (`sched/deadlock.rs`). A fiber with the blocking mark set (a
//! pending job, or a job running inline) is never deadlocked, and it needs
//! no location in a deadlock report.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::deadlock::{self, PoolPending};
use super::{
    AfterSwitch, CodeToken, FiberRef, Shared, WaitHeader, WaitState, begin_park, cancel_park,
    hand_off_before_blocking, park,
};
use crate::backtrace::Start;
use crate::ctx::ExecCtx;
use crate::ffi_pool::{self, FfiJob};
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::panic::{PanicReport, UNKNOWN_LOCATION, fatal};
use crate::pool::{self, Pool, Submitted};
use crate::unwind::guard::guarded;
use crate::unwind::raise::{cancel_from_runtime, raise_cancel, raise_from_runtime_at};

/// Runs `job` on the blocking pool while `me` is parked (D-683), or inline
/// when `me` has no runtime (D-692) or the pool cannot start a thread
/// (D-817). `Err(Cancelled)` when a cancellation was
/// pending before the job (none ran, D-816) or
/// arrived during it (the result was dropped before any Cheby
/// allocation). The blocking mark is set before `begin_park` and cleared
/// by the pool thread before its wake (D-814), which
/// never happens during a halt or once a stop gave up on the pool; after
/// that the job runs inline (D-815). A fiber with
/// the mark set is a pending pool job: H1's deadlock rule reads that
/// (plan §2.6).
///
/// `me` is the fiber running on this thread. `job` owns Rust data only:
/// it never touches a Cheby object (D-191, D-683).
pub(crate) fn run_blocking<R: Send + 'static>(
    me: &ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
) -> Result<R, Cancelled> {
    run_blocking_in(Pool::global(), me, job, &Hooks::default())
}

/// What tests observe of, and force on, one blocking call; empty outside
/// tests.
#[derive(Debug, Default)]
pub(crate) struct Hooks {
    /// The test's probe.
    #[cfg(test)]
    pub(crate) probe: Option<Arc<Probe>>,
}

/// A test's view of one blocking call ([`Hooks`]).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Probe {
    /// Set by the test: after submitting, the fiber waits until the job
    /// has completed before it parks, so the wake finds it `PARKING`.
    pub(crate) complete_before_park: std::sync::atomic::AtomicBool,
    /// Set by the completion once it stored the result.
    pub(crate) completed: std::sync::atomic::AtomicBool,
    /// The fiber's state right before it parked (with
    /// `complete_before_park`).
    pub(crate) state_before_park: std::sync::atomic::AtomicU8,
    /// Completions that woke the fiber.
    pub(crate) woke: std::sync::atomic::AtomicUsize,
    /// Completions that found the runtime stopping and woke nothing.
    pub(crate) skipped: std::sync::atomic::AtomicUsize,
    /// Whether the mark was set when the completion woke the fiber.
    pub(crate) marked_at_wake: std::sync::atomic::AtomicBool,
    /// Completions of a detached job that found its wait cancelled and
    /// dropped the result on the pool thread (D-1065).
    pub(crate) abandoned: std::sync::atomic::AtomicUsize,
    /// Set by the test: after a refused thread (D-817), the detached wait
    /// holds until a cancellation walk has won its header, before the
    /// fiber's own claim, so that the walk wins that race (review finding
    /// SOUND-W4B-1).
    pub(crate) hold_refused: std::sync::atomic::AtomicBool,
}

/// The state a wake gives a fiber it finds `PARKING`, for the tests.
#[cfg(test)]
pub(crate) const PARKING_WOKEN: u8 = super::fiber::state::PARKING_WOKEN;

/// [`run_blocking`] on `pool`, with test hooks.
pub(crate) fn run_blocking_in<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
    hooks: &Hooks,
) -> Result<R, Cancelled> {
    cancel::check(me)?;
    let result = wait_for_job(pool, me, job, hooks);
    // The cancellation was delivered while the job ran: the work finished,
    // and its result is released here, before any Cheby value is made
    // (D-670, D-683).
    if cancel::check(me).is_err() {
        drop(result);
        return Err(Cancelled);
    }
    Ok(result)
}

/// The middle of [`run_blocking_in`], without its two cancellation checks:
/// runs `job` on `pool` while `me` is parked, or inline (D-692, D-815,
/// D-817), and returns its result.
fn wait_for_job<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
    hooks: &Hooks,
) -> R {
    match me.sched.runtime() {
        Some(runtime) if enter(runtime) => on_pool(pool, me, runtime, job, hooks),
        // A stop gave up on the pool (D-815): the
        // call runs inline with D-786's mark and the hand-off, as after a
        // refused pool thread, and the stop's join treats it as D-786 says.
        Some(_) | None => inline(me, job),
    }
}

/// Runs `job` inline on the calling thread with `me` marked as blocked in
/// it (D-786), for an isolated entry (D-692) and for a fiber of a runtime
/// whose stop gave up on the pool (D-815), after
/// handing off the fibers waiting on its worker.
fn inline<R>(me: &ExecCtx, job: impl FnOnce() -> R) -> R {
    /// Clears the mark, also if `job` unwinds.
    struct Mark<'a>(&'a ExecCtx);
    impl Drop for Mark<'_> {
        fn drop(&mut self) {
            self.0.sched.set_blocking(false);
        }
    }
    me.sched.set_blocking(true);
    let mark = Mark(me);
    hand_off_before_blocking(me);
    let result = job();
    drop(mark);
    result
}

/// Where a job leaves its result, and how its fiber is woken: on the heap,
/// shared by the fiber and the job (D-683).
struct Completion<R> {
    /// The job's result, once it returned.
    result: Mutex<Option<R>>,
    /// The fiber waiting for it.
    fiber: FiberRef,
    /// The fiber's runtime, kept alive by the job.
    runtime: Arc<Shared>,
    /// The job's place in its domain's pending count (D-857,
    /// `sched/deadlock.rs`), released after the wake or the skip.
    pending: Option<Arc<PoolPending>>,
    /// The test's probe.
    #[cfg(test)]
    probe: Option<Arc<Probe>>,
}

impl<R> Completion<R> {
    /// Stores `result`, clears the fiber's mark, then wakes the fiber
    /// unless its runtime halts or its stop gave up on the pool
    /// (D-814, D-815), and only
    /// then counts the job as done ([`leave`]). Runs on the pool thread
    /// with no lock held, or inline on the fiber's own worker after a
    /// refused thread.
    fn complete(&self, result: R) {
        *self.result.lock() = Some(result);
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe
                .completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: the fiber waits for this job, parked or parking (or
        // running it inline, after a refused thread), and only this
        // completion wakes it: it is not retired before the wake below.
        // Its control block, at the top of its stack slot, outlives its
        // runtime: a fiber that waits on a job is never cancelled to its
        // end before the job returns (D-683), and one that a teardown
        // skipped or a halt froze keeps its slot, which is never retired
        // or reused (D-708, D-786). So the store below, also after a stop
        // or a halt began, writes into a slot that cannot be reused
        // (D-814, which refines D-683's "must not write
        // into the fiber's slot" to that).
        let fiber = unsafe { self.fiber.ctx() };
        fiber.sched.set_blocking(false);
        if wakes_nothing(&self.runtime) {
            #[cfg(test)]
            if let Some(probe) = &self.probe {
                probe
                    .skipped
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.release_pending();
            leave(&self.runtime);
            return;
        }
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe
                .marked_at_wake
                .store(fiber.sched.blocking(), std::sync::atomic::Ordering::SeqCst);
            probe.woke.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: as above; nothing touches the control block after the
        // wake, which may let the fiber run and retire. A fiber running
        // the job inline is `RUNNING`, and the wake does nothing.
        unsafe { super::wake(self.fiber, false) };
        // After the wake: a deadlock survey that reads the domain's count
        // as 0 finds the fiber runnable or resumed (D-857, D-860).
        self.release_pending();
        // After the wake: a worker of a stop that reads the count as 0
        // finds the fiber queued when it looks again ([`holds_stop`]).
        leave(&self.runtime);
    }

    /// Takes the job out of its domain's pending count, unless its group's
    /// stuck mark did already (`deadlock::PoolPending::release`).
    fn release_pending(&self) {
        if let Some(pending) = &self.pending {
            pending.release();
        }
    }
}

/// Whether a completion of `runtime`'s pool jobs wakes nothing: during a
/// halt (D-073, D-708), or once its stop gave up on the pool
/// (D-815), when its workers may have ended.
fn wakes_nothing(runtime: &Shared) -> bool {
    runtime.halting() || runtime.pool_given_up.load(Ordering::SeqCst)
}

/// Counts a fiber of `runtime` as waiting on the pool, unless the
/// runtime's stop gave up on the pool (D-815): then
/// returns false, and the call runs inline (D-786).
fn enter(runtime: &Shared) -> bool {
    runtime.pool_waiting.fetch_add(1, Ordering::SeqCst);
    if runtime.pool_given_up.load(Ordering::SeqCst) {
        runtime.pool_waiting.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    true
}

/// Counts one completion of `runtime`'s pool jobs, once its wake (or its
/// skip) is done: its fiber is queued before the count falls.
fn leave(runtime: &Shared) {
    runtime.pool_done.fetch_add(1, Ordering::SeqCst);
    runtime.pool_waiting.fetch_sub(1, Ordering::SeqCst);
}

/// Whether a worker of `runtime` must not end at its stop yet because a
/// fiber of the runtime waits on the pool and the stop has not given up on
/// it (D-815, D-708: a stop runs every runnable
/// fiber to its end, and the completion makes this one runnable). A worker
/// that reads false looks for a fiber once more before it ends: a
/// completion queues its fiber before it lowers the count ([`leave`]), so
/// that look finds it.
pub(super) fn holds_stop(runtime: &Shared) -> bool {
    runtime.pool_waiting.load(Ordering::SeqCst) > 0 && !runtime.pool_given_up.load(Ordering::SeqCst)
}

/// How long apart the stop's two polls of the pool are: D-804's 5 ms.
const STOP_POLL: Duration = Duration::from_millis(5);

/// One poll of the host's stop while it joins its workers
/// (D-815): the stop gives up on the pool when two
/// polls [`STOP_POLL`] apart find a fiber still waiting on it, every
/// remaining worker `idle` (running no fiber) and no job completed between
/// them (D-804's shape). Such a fiber waits on input that may never end and
/// is treated as one blocked inline (D-786): its completion, if it ever
/// comes, wakes nothing, the workers end, and a later blocking call runs
/// inline. `seen` is the stop's state between polls.
pub(super) fn poll_stop(runtime: &Shared, idle: bool, seen: &mut Option<(Instant, u64)>) {
    if !idle || !holds_stop(runtime) {
        *seen = None;
        return;
    }
    let done = runtime.pool_done.load(Ordering::SeqCst);
    match *seen {
        Some((at, before)) if before == done => {
            #[cfg(test)]
            if runtime.probe.hold_pool.load(Ordering::SeqCst)
                && at.elapsed() < super::testing::hang()
            {
                return;
            }
            if at.elapsed() >= STOP_POLL {
                runtime.pool_given_up.store(true, Ordering::SeqCst);
                // D-950: the package calls still on the pool are abandoned.
                abandon_ffi_jobs(runtime);
            }
        }
        _ => *seen = Some((Instant::now(), done)),
    }
}

/// Runs `job` on `pool` while `me`, a fiber of `runtime`, is parked.
fn on_pool<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    runtime: &Arc<Shared>,
    job: impl FnOnce() -> R + Send + 'static,
    hooks: &Hooks,
) -> R {
    // D-857: the job is pending in the fiber's domain from before the
    // submit until its completion has woken the fiber (or skipped the
    // wake); a job of a stuck group is not counted (`sched/deadlock.rs`).
    let pending = deadlock::counts_of(me).map(|counts| {
        let pending = counts.pool_submitted();
        if let Some(group) = me.conc.group() {
            group.track_job(&pending);
        }
        pending
    });
    let completion = Arc::new(Completion {
        result: Mutex::new(None),
        fiber: FiberRef::from_ctx(me),
        runtime: Arc::clone(runtime),
        pending,
        #[cfg(test)]
        probe: hooks.probe.clone(),
    });
    #[cfg(not(test))]
    let _ = hooks;
    let done = Arc::clone(&completion);
    // `PARKING` first, then the mark, both before the submit: a completion
    // that comes before the switch finds the fiber `PARKING`; a survey that
    // sees it `PARKED` sees it marked (the mark comes before the switch),
    // and one that sees it `PARKING` and unmarked counts it as busy. A
    // stop's join never sees it `RUNNING` and marked, which is how it
    // tells a fiber blocked inline (D-786, `Shared::worker_blocked`) from
    // one parking on the pool, which it waits for
    // (D-815).
    begin_park(me);
    me.sched.set_blocking(true);
    match pool.submit(Box::new(move || {
        let result = job();
        done.complete(result);
    })) {
        Submitted::Queued => {
            #[cfg(test)]
            wait_for_completion(me, hooks);
            park(me, AfterSwitch::Park(None));
        }
        Submitted::Refused(jobs) => {
            // Not parking after all: no waker can have seen `PARKING`
            // except this fiber's own job, which runs below.
            cancel_park(me);
            hand_off_before_blocking(me);
            for job in jobs {
                pool::run_job(job);
            }
        }
    }
    // The job completed: its fiber was woken, or ran it.
    let Some(result) = completion.result.lock().take() else {
        fatal(format_args!(
            "a fiber waiting on the blocking pool resumed without its result"
        ));
    };
    result
}

/// A test hook: waits until the job has completed before the fiber parks,
/// so that the wake finds the fiber `PARKING` (survey-io §3.2).
#[cfg(test)]
fn wait_for_completion(me: &ExecCtx, hooks: &Hooks) {
    use std::sync::atomic::Ordering;
    let Some(probe) = &hooks.probe else {
        return;
    };
    if !probe.complete_before_park.load(Ordering::SeqCst) {
        return;
    }
    while !probe.completed.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    // The completion stores the result, clears the mark, then wakes: wait
    // for the wake, which finds the fiber `PARKING`.
    let state = me.sched.state();
    while state.load(Ordering::SeqCst) == super::fiber::state::PARKING {
        std::thread::yield_now();
    }
    probe
        .state_before_park
        .store(state.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// Runs `job` on the blocking pool while `me` waits for it, as
/// [`run_blocking`] does, except that a cancellation reaching `me` during
/// the wait is delivered at once (D-1065): `me` returns `Err(Cancelled)`
/// without waiting, and the job runs to its end on its pool thread,
/// keeping what it owns; its result is then dropped there. A cancellation
/// pending before the call starts no job (D-816). Without a runtime, or
/// when the pool cannot start a thread, the job runs inline and cannot be
/// abandoned (D-692, D-817). `job` owns Rust data only and never touches a
/// Cheby object (D-191, D-683). An abandoned job's pool token is released
/// when the fiber abandons it, and it holds no stop and no leak check
/// (D-1165).
///
/// Only jobs whose effect a cancelled fiber cannot miss are detached
/// (D-1065): refills and lookups. Writes, opens, closes, directory
/// changes and `@blocking` calls use [`run_blocking`], which waits (D-670).
pub(crate) fn run_blocking_detached<R: Send + 'static>(
    me: &ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
) -> Result<R, Cancelled> {
    run_blocking_detached_in(Pool::global(), me, job, &Hooks::default())
}

/// [`run_blocking_detached`] on `pool`, with test hooks.
pub(crate) fn run_blocking_detached_in<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
    hooks: &Hooks,
) -> Result<R, Cancelled> {
    run_blocking_claiming_in(pool, me, move |_| job(), hooks)
}

// The first callers of the detached completion are S2's lookups (M3 wave
// 5) and S3's `File` refills (wave 6); standard input's refill uses
// `run_blocking_claiming`. Until then these keep both entries compiled and
// linted as live code (D-1065).
const _: DetachedFn = run_blocking_detached;
const _: DetachedInFn = run_blocking_detached_in;

/// [`run_blocking_detached`]'s type at `()`.
type DetachedFn = fn(&ExecCtx, fn()) -> Result<(), Cancelled>;

/// [`run_blocking_detached_in`]'s type at `()`.
type DetachedInFn = fn(&'static Pool, &ExecCtx, fn(), &Hooks) -> Result<(), Cancelled>;

/// [`run_blocking_detached`] for a job that commits its effect only once
/// it has claimed its fiber's wait ([`Claim::claim`]): a refill of
/// standard input's reader claims the wait under the reader's lock before
/// it takes a line for its read, so that a cancelled read takes nothing
/// and its line stays in the reader for the next read (D-1065, D-548). A
/// job that does not claim is claimed by its completion when it returns.
/// When the claim fails the fiber has gone, or is going, and the job's
/// result is dropped on its pool thread.
pub(crate) fn run_blocking_claiming<R: Send + 'static>(
    me: &ExecCtx,
    job: impl FnOnce(&Claim) -> R + Send + 'static,
) -> Result<R, Cancelled> {
    run_blocking_claiming_in(Pool::global(), me, job, &Hooks::default())
}

/// [`run_blocking_claiming`] on `pool`, with test hooks.
pub(crate) fn run_blocking_claiming_in<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    job: impl FnOnce(&Claim) -> R + Send + 'static,
    hooks: &Hooks,
) -> Result<R, Cancelled> {
    cancel::check(me)?;
    match me.sched.runtime() {
        Some(runtime) if enter(runtime) => on_pool_detached(pool, me, runtime, job, hooks),
        // Inline (D-692, D-815), as `run_blocking_in`: the job runs on this
        // thread and its claim always holds, so its result is returned,
        // also when a cancellation came meanwhile: the line it took is
        // never lost (D-1065); the cancellation is delivered at the next
        // entry check.
        Some(_) | None => Ok(inline(me, move || job(&Claim { header: None }))),
    }
}

/// A detached job's hold on its fiber's wait ([`run_blocking_claiming`]).
#[derive(Debug)]
pub(crate) struct Claim {
    /// The fiber's published header; `None` for a job that runs inline,
    /// which nothing can abandon.
    header: Option<Arc<WaitHeader>>,
}

impl Claim {
    /// Settles the fiber's wait `Waiting -> Claimed` for the job: true when
    /// the fiber still waits and will take the job's result (also on a
    /// second call, once the job claimed it), false when the cancellation
    /// walk won the wait first and the fiber has gone or is going. Only the
    /// job, and then its completion, claim; a claim is never undone.
    pub(crate) fn claim(&self) -> bool {
        self.header.as_ref().is_none_or(|header| {
            header.try_settle_arm(WaitState::Claimed, 0) || header.state() == WaitState::Claimed
        })
    }
}

/// Where a detached job leaves its result, and how its fiber is woken:
/// on the heap, shared by the fiber and the job (D-683), with the wait
/// header the walk reaches the fiber through (D-1065).
struct Detached<R> {
    /// The job's result, once it returned and claimed the wait.
    result: Mutex<Option<R>>,
    /// The fiber's wait, settled once by the completion or the walk.
    header: Arc<WaitHeader>,
    /// The fiber's runtime, kept alive by the job.
    runtime: Arc<Shared>,
    /// The job's place in its domain's pending count (D-857), released
    /// after the completion's wake or skip, or by the fiber when it
    /// abandons the job (D-1165).
    pending: Option<Arc<PoolPending>>,
    /// The test's probe.
    #[cfg(test)]
    probe: Option<Arc<Probe>>,
}

impl<R> Detached<R> {
    /// The completion, on the pool thread (or inline after a refused
    /// thread): claims the wait unless the job did; when the walk won it,
    /// drops `result` here and touches nothing of the fiber, which took
    /// the job out of every count itself. Otherwise as
    /// [`Completion::complete`]: stores the result, clears the mark, wakes
    /// the fiber unless the runtime halts or its stop gave up on the pool
    /// (D-814, D-815), then releases the token and counts the job done.
    fn complete(&self, result: R) {
        if !(Claim {
            header: Some(Arc::clone(&self.header)),
        })
        .claim()
        {
            drop(result);
            // Counted after the drop, so a test that waits for the count
            // sees the abandoned result already dropped.
            #[cfg(test)]
            if let Some(probe) = &self.probe {
                probe
                    .abandoned
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            return;
        }
        *self.result.lock() = Some(result);
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe
                .completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: the claim won the header, so the fiber is parked or
        // parking on it (or running this job inline, after a refused
        // thread) and nothing else wakes it: it is not retired before the
        // wake below, and its slot is never reused after a stop or a halt
        // skipped it (D-708, D-786, D-814), as for `Completion::complete`.
        let fiber = unsafe { self.header.fiber().ctx() };
        fiber.sched.set_blocking(false);
        if wakes_nothing(&self.runtime) {
            #[cfg(test)]
            if let Some(probe) = &self.probe {
                probe
                    .skipped
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.release_pending();
            leave(&self.runtime);
            return;
        }
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe
                .marked_at_wake
                .store(fiber.sched.blocking(), std::sync::atomic::Ordering::SeqCst);
            probe.woke.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: as above; nothing touches the control block after the
        // wake. A fiber running the job inline is `RUNNING`, and the wake
        // does nothing.
        unsafe { super::wake(self.header.fiber(), false) };
        // After the wake (D-857, D-860), then the stop's count
        // ([`holds_stop`]), as `Completion::complete`.
        self.release_pending();
        leave(&self.runtime);
    }

    /// The fiber abandons the job, or never submitted it: takes it out of
    /// its domain's pending count and of the runtime's pool count, which
    /// its completion then leaves alone (D-1165).
    /// Called by the fiber while it runs, after the walk's wake.
    fn abandon(&self) {
        self.release_pending();
        leave(&self.runtime);
    }

    /// Takes the job out of its domain's pending count, unless its group's
    /// stuck mark did already.
    fn release_pending(&self) {
        if let Some(pending) = &self.pending {
            pending.release();
        }
    }
}

/// Runs `job` on `pool` while `me`, a fiber of `runtime`, is parked on a
/// wait the cancellation walk can win (module documentation, D-1065).
fn on_pool_detached<R: Send + 'static>(
    pool: &'static Pool,
    me: &ExecCtx,
    runtime: &Arc<Shared>,
    job: impl FnOnce(&Claim) -> R + Send + 'static,
    hooks: &Hooks,
) -> Result<R, Cancelled> {
    // D-857, as `on_pool`.
    let pending = deadlock::counts_of(me).map(|counts| {
        let pending = counts.pool_submitted();
        if let Some(group) = me.conc.group() {
            group.track_job(&pending);
        }
        pending
    });
    let header = Arc::new(WaitHeader::on_pool(me));
    let detached = Arc::new(Detached {
        result: Mutex::new(None),
        header: Arc::clone(&header),
        runtime: Arc::clone(runtime),
        pending,
        #[cfg(test)]
        probe: hooks.probe.clone(),
    });
    #[cfg(not(test))]
    let _ = hooks;
    // `PARKING`, then the mark, as `on_pool`; then the header goes to the
    // walk, before the submit, so a cancellation from now on either is
    // refused here or wins the header.
    begin_park(me);
    me.sched.set_blocking(true);
    // SAFETY: `header` lives in an `Arc` this frame holds until after
    // `unpublish` below.
    if unsafe { cancel::publish(me, &header) }.is_err() {
        // A target came before the publication (D-816): no job starts.
        cancel_park(me);
        me.sched.set_blocking(false);
        detached.abandon();
        return Err(Cancelled);
    }
    let done = Arc::clone(&detached);
    let claim = Claim {
        header: Some(Arc::clone(&header)),
    };
    match pool.submit(Box::new(move || {
        // A job whose fiber was cancelled before it started does not run:
        // nobody waits for it, and nothing of it is missed (D-1065). The
        // fiber took it out of every count itself.
        if done.header.state() == WaitState::Cancelled {
            return;
        }
        let result = job(&claim);
        done.complete(result);
    })) {
        Submitted::Queued => {
            #[cfg(test)]
            wait_for_completion(me, hooks);
            park(me, AfterSwitch::Park(None));
        }
        Submitted::Refused(jobs) => {
            #[cfg(test)]
            hold_refused(&header, hooks);
            // The jobs run inline on this worker (D-817), where nothing can
            // abandon them: the fiber claims its own wait first, while it
            // is still `PARKING`, so that a walk can no longer win it and
            // clear the mark of a fiber blocked inline (D-786: a teardown
            // skips it, and the stop's join takes its worker as blocked).
            // Only the fiber's own job, which has not run, could claim it
            // besides, so the claim fails only when the walk won.
            if header.try_settle_arm(WaitState::Claimed, 0) {
                // Not parking after all, as in `on_pool`: no walk won the
                // header, so nothing but the fiber's own completion, run
                // below on this thread, wakes the fiber, and that wake
                // finds it `RUNNING` and does nothing.
                cancel_park(me);
            } else {
                // The walk won the header (`cancel::reach`) and wakes the
                // fiber once it has released its locks (`cancel::cancel`),
                // maybe long after: the fiber parks until that wake, so it
                // never lands on a later park of the fiber, or on a fiber
                // that has ended (review finding SOUND-W4B-1). A wake that
                // came already found the fiber `PARKING`, and the worker
                // requeues it after the switch. The fiber's own job does
                // not run (above), and the fiber abandons it below; the
                // mark covers the others' jobs it runs.
                park(me, AfterSwitch::Park(None));
            }
            me.sched.set_blocking(true);
            hand_off_before_blocking(me);
            for job in jobs {
                pool::run_job(job);
            }
            me.sched.set_blocking(false);
        }
    }
    cancel::unpublish(me);
    // Woken by the completion (`Claimed`) or by the walk (`Cancelled`). A
    // header still `Waiting` cannot be seen here; it is cancelled all the
    // same, so a completion could never wake this fiber once it left.
    let claimed = match header.state() {
        WaitState::Claimed => true,
        // Lost only to a claim that came meanwhile.
        WaitState::Waiting => !header.try_cancel(),
        WaitState::Cancelled | WaitState::Closed => false,
    };
    if claimed {
        let Some(result) = detached.result.lock().take() else {
            fatal(format_args!(
                "a fiber waiting on the blocking pool resumed without its result"
            ));
        };
        return Ok(result);
    }
    // Abandoned (D-1065): the walk cleared the mark before its wake
    // (`cancel::reach`); the job runs on, and its completion drops its
    // result.
    me.sched.set_blocking(false);
    detached.abandon();
    Err(Cancelled)
}

/// A test hook: after a refused thread, holds until a cancellation walk
/// has won `header` (at most 10 s), so that the fiber's own claim loses
/// (`Probe::hold_refused`).
#[cfg(test)]
fn hold_refused(header: &WaitHeader, hooks: &Hooks) {
    use std::sync::atomic::Ordering;
    let Some(probe) = &hooks.probe else {
        return;
    };
    if !probe.hold_refused.load(Ordering::SeqCst) {
        return;
    }
    let started = Instant::now();
    while header.state() != WaitState::Cancelled && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Clones of `runtime`'s code tokens (D-708), taken under its leaf lock.
pub(crate) fn code_tokens(runtime: &Shared) -> Vec<CodeToken> {
    runtime.code.lock().clone()
}

/// The runtime entry of an `@blocking` call (D-818): copies
/// `nargs` words from `args`, clones the runtime's code tokens (on
/// `Shared`) into the job, so the JIT code `shim` lives in outlives the job
/// even if the runtime stops meanwhile, runs `shim` on the words through
/// `run_blocking`, and returns the result word unchanged; a cancellation
/// pending after the return raises (D-670, D-683) through the flag at
/// `ctx+12`, which the call site tests (D-734), never through the result,
/// which may be 0. Its body runs in `guarded(Some(ctx), …)`.
///
/// `shim` is the generated C-ABI shim of one `@blocking` external: it
/// calls the foreign function with the words at its first argument and
/// stores the result word at its second.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `args` points to `nargs` readable
/// words (or `nargs` is 0); `shim` is a generated shim whose code stays
/// valid while the runtime's code tokens live, and which only reads its
/// `nargs` words and writes its result word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_blocking_call(
    ctx: *mut ExecCtx,
    shim: unsafe extern "C" fn(*const u64, *mut u64),
    args: *const u64,
    nargs: usize,
) -> u64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { crate::fiber_ops::running(ctx) };
        // D-816: a fiber already being cancelled starts no job.
        if cancel::check(me).is_err() {
            raise_cancel(Some(ctx));
        }
        let words: Vec<u64> = if nargs == 0 || args.is_null() {
            Vec::new()
        } else {
            // SAFETY: the caller's `nargs` readable words, copied before
            // the fiber parks: the job owns Rust data only (D-683).
            unsafe { std::slice::from_raw_parts(args, nargs) }.to_vec()
        };
        // The job holds the runtime's code tokens (D-818,
        // refines D-708): a stop joins the workers, not the pool, and then
        // releases its tokens, so without these the code `shim` runs in
        // could be freed under the pool thread.
        let code = me.sched.runtime().map(|runtime| code_tokens(runtime));
        // D-950: the call's hold on its fiber's domain, through which the
        // pool thread acts for the parked fiber, registered with the
        // runtime so that a give-up or a halt can abandon it.
        let job = FfiJob::new(Arc::clone(me.domain()), std::ptr::from_ref(me));
        let runtime = me.sched.runtime().map(Arc::clone);
        if let Some(runtime) = &runtime {
            register_ffi_job(runtime, &job);
        }
        let pool_job = Arc::clone(&job);
        let call = move || {
            let mut out = 0_u64;
            // SAFETY: the caller's shim, given its own copy of the words
            // and a result cell; its code lives while `code` does.
            ffi_pool::run_as_job(&pool_job, || unsafe { shim(words.as_ptr(), &raw mut out) });
            if let Some(runtime) = runtime {
                unregister_ffi_job(&runtime, &pool_job);
            }
            drop(code);
            out
        };
        let word = wait_for_job(Pool::global(), me, call, &Hooks::default());
        // D-1268: a call that must unwind after its
        // job returns hands its result word back with the flag set, and
        // the wrapper's helper (`cheby_rt_ffi_value`, `cheby_rt_ffi_external`)
        // releases it on the fiber before the wrapper unwinds: an owned
        // value through the helper's cleanup, an external handle through
        // its type's drop function (D-950, D-171). A scalar result's call
        // tests the flag itself, with nothing to release.
        if job.failed() {
            // An allocation of the job failed on the pool thread (D-950):
            // the `out of memory` panic is raised on the fiber now (D-642,
            // D-946).
            let report = PanicReport::new(
                crate::abi::PanicKind::Explicit,
                Some(crate::alloc::OUT_OF_MEMORY),
                UNKNOWN_LOCATION,
            );
            // SAFETY: the running fiber's context; this is the outermost
            // Rust frame below the generated call site, and nothing unwinds
            // from here.
            unsafe { raise_from_runtime_at(ctx, report, Start::Unknown) };
            return word;
        }
        // The cancellation was delivered while the job ran (D-670, D-683):
        // the work finished, and its result goes back with the flag set.
        if cancel::check(me).is_err() {
            cancel_from_runtime(me);
        }
        word
    })
}

/// Registers `job`, a package `@blocking` call's, with `runtime`, so that a
/// give-up or a halt abandons it (D-950); when the runtime halts or gave up
/// already, marks it at once. Under the registry's lock, which the
/// abandoning side takes after it set its flag, so the job is either
/// marked by [`abandon_ffi_jobs`] or sees the flag here.
fn register_ffi_job(runtime: &Shared, job: &Arc<FfiJob>) {
    let mut jobs = runtime.ffi_jobs.lock();
    jobs.push(Arc::clone(job));
    if wakes_nothing(runtime) {
        job.abandon();
    }
}

/// Forgets `job` once its shim returned: no `cheby_ffi_` call of it is left.
fn unregister_ffi_job(runtime: &Shared, job: &Arc<FfiJob>) {
    let mut jobs = runtime.ffi_jobs.lock();
    if let Some(at) = jobs.iter().position(|known| Arc::ptr_eq(known, job)) {
        jobs.swap_remove(at);
    }
}

/// Abandons every package `@blocking` call of `runtime` still running (a
/// give-up, D-815, or a halt, D-073): each is marked under its per-job lock
/// (D-950, `ffi_pool.rs`). Called after the flag that [`wakes_nothing`]
/// reads was set, with no other runtime lock held. Lock order: the
/// registry, then each job's lock.
pub(super) fn abandon_ffi_jobs(runtime: &Shared) {
    let jobs = runtime.ffi_jobs.lock();
    for job in jobs.iter() {
        job.abandon();
    }
}
