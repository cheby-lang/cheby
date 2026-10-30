//! A pool thread acting for its parked fiber during a package's
//! `@blocking` foreign call (D-950, §12.7).
//!
//! A package's `@blocking` foreign function runs on the blocking pool
//! through its `cb$blk$` shim (D-683, D-818) while its fiber is parked, and
//! it may call `cheby.h`'s functions there (D-945). For the length of the
//! call the pool thread acts for the fiber through its host slot
//! (`tls.rs`'s `Host::job`, D-686): [`run_as_job`] installs the call's
//! [`FfiJob`] and makes the fiber's accounting domain the host's current
//! one, so every `cheby_ffi_` value the call builds or frees is charged to
//! the fiber's domain (`alloc::set_current_domain`), and restores both
//! once the shim returns.
//!
//! - A failed allocation on the pool thread is recorded in the job
//!   ([`FfiJob::fail`], from `alloc::out_of_memory`) and unwinds to the
//!   `cheby_ffi_` function's guard, which returns NULL; the job's later
//!   allocations return NULL without allocating, as a fiber's do under its
//!   flag (D-946). The fiber raises the `out of memory` panic when it
//!   resumes (`sched::blocking`'s `cheby_rt_blocking_call`).
//! - GAP(ffi-job-lock): each `cheby_ffi_` call on a pool thread that
//!   changes the runtime's state (an allocation, `cheby_ffi_dup`,
//!   `cheby_ffi_drop`, an output function) first enters its job under the
//!   job's short lock ([`FfiJob::enter`]): the lock covers the test of the
//!   abandoned mark and the count of calls in flight, never the call
//!   itself, so a drop function or a write that blocks never holds it. A
//!   give-up (D-815) or a halt marks every job of the runtime under the
//!   same lock ([`FfiJob::abandon`]); from then on the job's calls return
//!   NULL (allocations), do nothing (`cheby_ffi_drop`, `cheby_ffi_dup`,
//!   which returns its argument uncounted) or return -1 (output). Readers
//!   (`cheby_ffi_string_bytes`, `cheby_ffi_list_get`, …) never enter: the
//!   call's borrowed arguments stay readable.
//! - Teardown never frees a domain that an abandoned job names: the mark
//!   moves a reference to the job, and so to its domain, into a list kept
//!   until the process exits ([`ABANDONED`]), because a call that entered
//!   before the mark may still charge it, and C code may still read the
//!   borrowed arguments the parked fiber holds (D-950).
//!
//! A call that runs inline on its own fiber (no runtime, a refused pool
//! thread, a stop that gave up, D-692, D-815, D-817) needs no job: its
//! fiber is current there, and `cheby.h` acts for it directly.
//! GAP(blocking-package-shims): after a refused pool thread, a fiber runs
//! every queued job inline (D-817), other fibers' among them. For another
//! fiber's job, [`run_as_job`] makes the running fiber stop being current
//! for the length of the shim and installs the job, so the thread acts for
//! the job's parked fiber exactly as a pool thread would: its values are
//! charged to that fiber's domain, and a failed allocation is recorded in
//! the job and raised on that fiber when it resumes, never on the fiber
//! that ran it.
//! The lock, the mark and teardown are modelled in loom in `ffi_models.rs`
//! (`ffi_pool_model_*`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::alloc::{self, Domain};
use crate::ctx::ExecCtx;
use crate::tls;

/// One package `@blocking` call's hold on its parked fiber's domain, from
/// its submit until its shim returned (module documentation).
#[derive(Debug)]
pub(crate) struct FfiJob {
    /// The per-job lock (D-950): the abandoned mark and the calls in
    /// flight. Nothing else is taken under it but [`ABANDONED`]'s leaf lock
    /// after it is released.
    state: Mutex<JobState>,
    /// The parked fiber's accounting domain, charged by the job's
    /// `cheby_ffi_` calls.
    domain: Arc<Domain>,
    /// The address of the fiber's control block, which only identifies it
    /// ([`run_as_job`]: is the fiber running the job its own?); 0 for none.
    owner: usize,
    /// Set by the pool thread when an allocation of the job failed
    /// (D-946); read by the fiber after its wake.
    failed: AtomicBool,
}

/// What the per-job lock guards.
#[derive(Debug, Default)]
struct JobState {
    /// Set once by a give-up or a halt (D-815, D-950).
    abandoned: bool,
    /// `cheby_ffi_` calls of the job entered and not left yet.
    in_call: u32,
}

/// The jobs a give-up or a halt abandoned, kept with their domains until
/// the process exits (D-950: teardown never frees a domain that an
/// abandoned job names). A leaf lock.
static ABANDONED: Mutex<Vec<Arc<FfiJob>>> = Mutex::new(Vec::new());

impl FfiJob {
    /// A job of the fiber `owner` (null for none), whose accounting domain
    /// is `domain`.
    pub(crate) fn new(domain: Arc<Domain>, owner: *const ExecCtx) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(JobState::default()),
            domain,
            owner: owner.addr(),
            failed: AtomicBool::new(false),
        })
    }

    /// The parked fiber's accounting domain.
    pub(crate) fn domain(&self) -> &Arc<Domain> {
        &self.domain
    }

    /// Enters one `cheby_ffi_` call of the job under the per-job lock:
    /// false when the job is abandoned, and then the call does nothing.
    /// A call that entered calls [`FfiJob::leave`] when it is done.
    pub(crate) fn enter(&self) -> bool {
        let mut state = self.state.lock();
        if state.abandoned {
            return false;
        }
        state.in_call = state.in_call.saturating_add(1);
        true
    }

    /// Ends a call that [`FfiJob::enter`] let in.
    pub(crate) fn leave(&self) {
        let mut state = self.state.lock();
        state.in_call = state.in_call.saturating_sub(1);
    }

    /// Marks the job abandoned under the per-job lock (a give-up or a halt,
    /// D-815, D-950) and keeps it, with its domain, until the process
    /// exits. Returns the number of the job's calls in flight at the mark,
    /// or `None` when it was marked already.
    pub(crate) fn abandon(self: &Arc<Self>) -> Option<u32> {
        let in_flight = {
            let mut state = self.state.lock();
            if state.abandoned {
                return None;
            }
            state.abandoned = true;
            state.in_call
        };
        ABANDONED.lock().push(Arc::clone(self));
        Some(in_flight)
    }

    /// Whether a give-up or a halt abandoned the job.
    #[cfg(test)]
    pub(crate) fn abandoned(&self) -> bool {
        self.state.lock().abandoned
    }

    /// Records that an allocation of the job failed (D-946).
    pub(crate) fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
    }

    /// Whether an allocation of the job failed: its later allocations
    /// return NULL, and the fiber raises `out of memory` when it resumes.
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

/// Runs `call`, a package `@blocking` call's shim, with this thread acting
/// for the job's parked fiber (module documentation): `job` in the host
/// slot and its domain as the host's current domain, both restored after,
/// also if `call` unwinds. On a thread where the job's own fiber is
/// current (the call runs inline, D-692, D-815, D-817) it runs `call` as
/// it is: `cheby.h` finds that fiber itself. Where another fiber is
/// current (it runs the queued jobs after a refused thread, D-817), that
/// fiber stops being current until `call` returns, so the thread acts for
/// the job's fiber as a pool thread does (GAP(blocking-package-shims)).
pub(crate) fn run_as_job<R>(job: &Arc<FfiJob>, call: impl FnOnce() -> R) -> R {
    /// Restores the host slot's job and domain, then the fiber that was
    /// current.
    struct Restore {
        job: Option<Arc<FfiJob>>,
        domain: Option<Arc<Domain>>,
        fiber: *mut ExecCtx,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let job = self.job.take();
            replace_job(job);
            if let Some(domain) = self.domain.take() {
                alloc::set_current_domain(domain);
            }
            if !self.fiber.is_null() {
                // SAFETY: the fiber that was current when `run_as_job`
                // began: this code runs on its behalf on this thread, so
                // it is live, and no other thread uses it meanwhile; the
                // previous value, null, needs nothing.
                unsafe { tls::set_current_fiber(self.fiber) };
            }
        }
    }
    let current = tls::current_fiber();
    if !current.is_null() && current.addr() == job.owner {
        return call();
    }
    let fiber = if current.is_null() {
        current
    } else {
        // SAFETY: null is always allowed, and the previous fiber,
        // `current`, is live: this code runs on its behalf on this thread.
        unsafe { tls::set_current_fiber(std::ptr::null_mut()) }
    };
    let previous = replace_job(Some(Arc::clone(job)));
    let domain = alloc::set_current_domain(Arc::clone(job.domain()));
    let restore = Restore {
        job: previous,
        domain: Some(domain),
        fiber,
    };
    let result = call();
    drop(restore);
    result
}

/// The job this thread acts for, if it is a pool thread running a
/// package's `@blocking` call ([`run_as_job`]).
pub(crate) fn current_job() -> Option<Arc<FfiJob>> {
    tls::with_host(|host| host.job.try_borrow().ok().and_then(|job| job.clone())).flatten()
}

/// Puts `job` in this thread's host slot and returns the one it held. The
/// slot's cell is borrowed only for the moment of a read or this swap, never
/// across a call, so the swap always finds it free.
fn replace_job(job: Option<Arc<FfiJob>>) -> Option<Arc<FfiJob>> {
    tls::with_host(|host| {
        host.job
            .try_borrow_mut()
            .map(|mut slot| std::mem::replace(&mut *slot, job))
            .ok()
            .flatten()
    })
    .flatten()
}

/// Whether an abandoned job keeps `domain` for the rest of the process
/// (tests only).
#[cfg(test)]
pub(crate) fn keeps_domain(domain: &Arc<Domain>) -> bool {
    ABANDONED
        .lock()
        .iter()
        .any(|job| Arc::ptr_eq(job.domain(), domain))
}

#[cfg(test)]
#[path = "ffi_pool_tests.rs"]
mod tests;
