//! The blocking pool (D-683, D-690): one process-global pool of up to 512
//! threads, started on demand, each of which ends after 10 s without a
//! job, running blocking work (standard input, file IO and `@blocking`
//! externals) in first-in, first-out order while the fiber that asked for
//! it is parked.
//!
//! A job is a Rust closure over Rust data only: its arguments were copied
//! out of Cheby values on the fiber before it was submitted, its result is
//! Rust data, and its completion state lives in an `Arc` on the heap,
//! never on the fiber's stack (D-683). A pool thread never touches a Cheby
//! object, allocates one, runs Cheby code, flushes standard output or
//! ends the program (D-191, D-683, D-709); the fiber side
//! (`sched::blocking`) builds the Cheby values once it runs again, charged
//! to its own domain. A Rust panic inside a job is a broken invariant and
//! ends the process.
//!
//! The pool outlives every runtime: a job may complete after its fiber's
//! runtime has stopped (`sched::blocking` then wakes nothing). Its thread
//! names are `cheby-blocking`, with Rust's default stack size, which
//! `std::fs` and the foreign code of M3's `@blocking` externals fit in.
//!
//! When the system refuses a new thread and no pool thread runs, nothing
//! would ever take the queue: [`Pool::submit`] then hands every queued job
//! back to the caller, which runs them inline on its worker with D-786's
//! mark and the hand-off; with pool threads running, the job stays queued
//! for them (D-817).
//!
//! The queue's lock is a leaf (plan §0 'Locks'): no other lock is taken
//! under it, and no job runs under it.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::panic::fatal;

#[cfg(test)]
mod models;
#[cfg(test)]
mod tests;

/// The most threads the pool runs at once (D-683); later jobs queue.
pub(crate) const MAX_THREADS: usize = 512;

/// How long a pool thread waits for a job before it ends (D-683).
pub(crate) const IDLE_EXIT: Duration = Duration::from_secs(10);

/// The name of every pool thread.
pub(crate) const THREAD_NAME: &str = "cheby-blocking";

/// A job: Rust code over Rust data that delivers its own result.
pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// What [`Pool::submit`] did with a job.
#[must_use = "jobs handed back must run inline (D-817)"]
pub(crate) enum Submitted {
    /// A pool thread runs it, now or when one is free.
    Queued,
    /// The system refused a thread and none runs: every queued job, the
    /// submitted one among them, in queue order, for the caller to run
    /// inline (D-817).
    Refused(Vec<Job>),
}

impl std::fmt::Debug for Submitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => f.write_str("Queued"),
            Self::Refused(jobs) => write!(f, "Refused({} jobs)", jobs.len()),
        }
    }
}

/// What the queue's lock guards.
struct State {
    /// The jobs no thread has taken yet, oldest first.
    queue: VecDeque<Job>,
    /// Threads started and not ended.
    threads: usize,
    /// Threads waiting for a job.
    idle: usize,
}

/// A blocking pool: the process-global one ([`Pool::global`]), or a
/// private one in tests.
pub(crate) struct Pool {
    /// The queue and the thread counts; a leaf lock.
    state: Mutex<State>,
    /// Where idle threads wait for a job.
    work: Condvar,
    /// The most threads at once.
    max_threads: usize,
    /// How long a thread waits for a job before it ends.
    idle_exit: Duration,
    /// A test hook: the system refuses every new thread.
    #[cfg(test)]
    refuse: AtomicBool,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("Pool")
            .field("queued", &state.queue.len())
            .field("threads", &state.threads)
            .field("idle", &state.idle)
            .finish_non_exhaustive()
    }
}

/// The process-global pool (D-690).
static GLOBAL: Pool = Pool::new(MAX_THREADS, IDLE_EXIT);

impl Pool {
    /// A pool of at most `max_threads` threads (at least one), each ending
    /// after `idle_exit` without a job.
    const fn new(max_threads: usize, idle_exit: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                threads: 0,
                idle: 0,
            }),
            work: Condvar::new(),
            max_threads: if max_threads == 0 { 1 } else { max_threads },
            idle_exit,
            #[cfg(test)]
            refuse: AtomicBool::new(false),
        }
    }

    /// The process-global pool, shared by every runtime (D-690).
    pub(crate) fn global() -> &'static Self {
        &GLOBAL
    }

    /// A private pool for a test, with its own cap and idle timeout, so
    /// that its thread count is not shared with the tests that run beside
    /// it. It lives for the rest of the process.
    #[cfg(test)]
    pub(crate) fn private(max_threads: usize, idle_exit: Duration) -> &'static Self {
        Box::leak(Box::new(Self::new(max_threads, idle_exit)))
    }

    /// A test hook: while on, the system "refuses" every new thread.
    #[cfg(test)]
    pub(crate) fn refuse_threads(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::SeqCst);
    }

    /// The threads started and not ended.
    #[cfg(test)]
    pub(crate) fn threads(&self) -> usize {
        self.state.lock().threads
    }

    /// The jobs no thread has taken yet.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.state.lock().queue.len()
    }

    /// Queues `job`: an idle thread takes it, or a new thread starts for
    /// it while there are fewer than the cap, or it waits for a busy
    /// thread. When the system refuses a new thread and no thread runs,
    /// every queued job comes back (D-817).
    pub(crate) fn submit(&'static self, job: Job) -> Submitted {
        let mut state = self.state.lock();
        state.queue.push_back(job);
        if state.queue.len() <= state.idle {
            drop(state);
            self.work.notify_one();
            return Submitted::Queued;
        }
        if state.threads >= self.max_threads {
            return Submitted::Queued;
        }
        state.threads = state.threads.saturating_add(1);
        drop(state);
        if self.start_thread().is_ok() {
            return Submitted::Queued;
        }
        let mut state = self.state.lock();
        state.threads = state.threads.saturating_sub(1);
        if state.threads > 0 {
            // A running thread takes it when it is free.
            return Submitted::Queued;
        }
        Submitted::Refused(state.queue.drain(..).collect())
    }

    /// Starts one pool thread, already counted in `threads`.
    fn start_thread(&'static self) -> std::io::Result<()> {
        #[cfg(test)]
        if self.refuse.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("a refused thread (test hook)"));
        }
        std::thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || self.serve())
            .map(drop)
    }

    /// A pool thread's loop: takes jobs in queue order and runs them, and
    /// ends once it waited [`Pool::idle_exit`] with the queue empty.
    fn serve(&self) {
        let mut state = self.state.lock();
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                run_job(job);
                state = self.state.lock();
                continue;
            }
            state.idle = state.idle.saturating_add(1);
            let timed_out = self.work.wait_for(&mut state, self.idle_exit).timed_out();
            state.idle = state.idle.saturating_sub(1);
            // A job queued meanwhile is taken first, also after a timeout:
            // its submitter counted this thread as idle.
            if timed_out && state.queue.is_empty() {
                state.threads = state.threads.saturating_sub(1);
                return;
            }
        }
    }
}

/// Runs one job. A Rust panic in it is a broken invariant (no job raises
/// or runs Cheby code), so it ends the process rather than losing the
/// wake of the fiber that waits for it.
pub(crate) fn run_job(job: Job) {
    if catch_unwind(AssertUnwindSafe(job)).is_err() {
        fatal(format_args!("a blocking-pool job panicked"));
    }
}
