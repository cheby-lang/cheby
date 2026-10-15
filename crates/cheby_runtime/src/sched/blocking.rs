//! The fiber side of the blocking pool (D-683, D-786): a fiber parks while
//! a pool thread runs its job, and the pool thread wakes it.
//!
//! # Protocol
//!
//! [`run_blocking`] on a fiber of a runtime:
//!
//! 1. reads its cancellation target (`cancel::check`): a fiber already
//!    being cancelled starts no job, so no line is consumed and no file is
//!    written (GAP(pool-check-before-submit));
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
//!    (GAP(pool-wakes-only-running)): a stop releases the code tokens once
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
//! The mark stays set from before the fiber parks until the pool thread
//! clears it, right before the wake (GAP(pool-in-the-survey)), so the
//! teardown's survey, D-804's two polls, D-805's later-check skips and
//! `run_main`'s skip-and-count treat a fiber waiting on the pool as they
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
//! (`hand_off_before_blocking`, GAP(pool-thread-refused)).
//!
//! # For the deadlock detector (H1)
//!
//! D-724's "no blocking-pool job pending" is "no fiber of the runtime has
//! the blocking mark set" (`FiberSched::blocking`): a fiber with the mark
//! set is a pending pool job (or a job running inline), never deadlocked,
//! and it needs no location in a deadlock report.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::{
    AfterSwitch, CodeToken, FiberRef, Shared, begin_park, cancel_park, hand_off_before_blocking,
    park,
};
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::panic::fatal;
use crate::pool::{self, Pool, Submitted};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;

/// Runs `job` on the blocking pool while `me` is parked (D-683), or inline
/// when `me` has no runtime (D-692) or the pool cannot start a thread
/// (GAP(pool-thread-refused)). `Err(Cancelled)` when a cancellation was
/// pending before the job (none ran, GAP(pool-check-before-submit)) or
/// arrived during it (the result was dropped before any Cheby
/// allocation). The blocking mark is set before `begin_park` and cleared
/// by the pool thread before its wake (GAP(pool-in-the-survey)), which
/// never happens during a halt or once a stop gave up on the pool; after
/// that the job runs inline (GAP(pool-wakes-only-running)). A fiber with
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
    let result = match me.sched.runtime() {
        Some(runtime) if enter(runtime) => on_pool(pool, me, runtime, job, hooks),
        // A stop gave up on the pool (GAP(pool-wakes-only-running)): the
        // call runs inline with D-786's mark and the hand-off, as after a
        // refused pool thread, and the stop's join treats it as D-786 says.
        Some(_) | None => inline(me, job),
    };
    // The cancellation was delivered while the job ran: the work finished,
    // and its result is released here, before any Cheby value is made
    // (D-670, D-683).
    if cancel::check(me).is_err() {
        drop(result);
        return Err(Cancelled);
    }
    Ok(result)
}

/// Runs `job` inline on the calling thread with `me` marked as blocked in
/// it (D-786), for an isolated entry (D-692) and for a fiber of a runtime
/// whose stop gave up on the pool (GAP(pool-wakes-only-running)), after
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
    /// The test's probe.
    #[cfg(test)]
    probe: Option<Arc<Probe>>,
}

impl<R> Completion<R> {
    /// Stores `result`, clears the fiber's mark, then wakes the fiber
    /// unless its runtime halts or its stop gave up on the pool
    /// (GAP(pool-in-the-survey), GAP(pool-wakes-only-running)), and only
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
        // (GAP(pool-in-the-survey), which refines D-683's "must not write
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
        // After the wake: a worker of a stop that reads the count as 0
        // finds the fiber queued when it looks again ([`holds_stop`]).
        leave(&self.runtime);
    }
}

/// Whether a completion of `runtime`'s pool jobs wakes nothing: during a
/// halt (D-073, D-708), or once its stop gave up on the pool
/// (GAP(pool-wakes-only-running)), when its workers may have ended.
fn wakes_nothing(runtime: &Shared) -> bool {
    runtime.halting() || runtime.pool_given_up.load(Ordering::SeqCst)
}

/// Counts a fiber of `runtime` as waiting on the pool, unless the
/// runtime's stop gave up on the pool (GAP(pool-wakes-only-running)): then
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
/// it (GAP(pool-wakes-only-running), D-708: a stop runs every runnable
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
/// (GAP(pool-wakes-only-running)): the stop gives up on the pool when two
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
            if runtime.probe.hold_pool.load(Ordering::SeqCst) && at.elapsed() < super::testing::HANG
            {
                return;
            }
            if at.elapsed() >= STOP_POLL {
                runtime.pool_given_up.store(true, Ordering::SeqCst);
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
    let completion = Arc::new(Completion {
        result: Mutex::new(None),
        fiber: FiberRef::from_ctx(me),
        runtime: Arc::clone(runtime),
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
    // (GAP(pool-wakes-only-running)).
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

/// Clones of `runtime`'s code tokens (D-708), taken under its leaf lock.
pub(crate) fn code_tokens(runtime: &Shared) -> Vec<CodeToken> {
    runtime.code.lock().clone()
}

/// The runtime entry of an `@blocking` call (GAP(blocking-shim)): copies
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
        let words: Vec<u64> = if nargs == 0 || args.is_null() {
            Vec::new()
        } else {
            // SAFETY: the caller's `nargs` readable words, copied before
            // the fiber parks: the job owns Rust data only (D-683).
            unsafe { std::slice::from_raw_parts(args, nargs) }.to_vec()
        };
        // The job holds the runtime's code tokens (GAP(blocking-shim),
        // refines D-708): a stop joins the workers, not the pool, and then
        // releases its tokens, so without these the code `shim` runs in
        // could be freed under the pool thread.
        let code = me.sched.runtime().map(|runtime| code_tokens(runtime));
        let call = move || {
            let mut out = 0_u64;
            // SAFETY: the caller's shim, given its own copy of the words
            // and a result cell; its code lives while `code` does.
            unsafe { shim(words.as_ptr(), &raw mut out) };
            drop(code);
            out
        };
        match run_blocking(me, call) {
            Ok(word) => word,
            Err(Cancelled) => raise_cancel(Some(ctx)),
        }
    })
}
