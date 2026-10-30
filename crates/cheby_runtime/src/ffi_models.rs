//! `loom` models of drop functions on the free worklist (D-949, D-684,
//! D-951): a user drop function runs inline from the free worklist, may
//! release values with `cheby_ffi_drop` and may write to the standard
//! streams through the funnel.
//!
//! The protocol, as `ffi.rs`, `alloc.rs` and `io.rs` implement it:
//!
//! - while a drop function runs, its thread's mark is set and every
//!   `cheby_ffi_drop` queues the reference instead of freeing; the
//!   finalizer gives the queue to the running worklist once the drop
//!   function returns (`ffi::run_drop`), so a free never starts inside
//!   another free;
//! - a write takes its stream's lock, writes borrowed bytes and releases
//!   the lock before anything is freed (`io::write_routed`): the runtime
//!   never frees, and so never runs a drop function, while it holds a
//!   stream lock.
//!
//! Each model is the protocol, not the runtime's code: a `loom` mutex per
//! stream (non-reentrant here, although Rust's stream locks are reentrant:
//! the hazard is two threads and two streams), a free-in-progress flag per
//! thread, and a worklist of node indexes. Two threads free chains of
//! values whose drop functions write to one stream each and release the
//! next value; whatever the interleaving, both finish (no deadlock, which
//! `loom` reports), each drop function runs once, and no free nests.
//!
//! Two `#[should_panic]` twins show the models can fail: one whose
//! `cheby_ffi_drop` frees at once (a free nests inside the drop function's
//! free), and one that frees while it still holds the stream lock it wrote
//! under (the two threads would deadlock on the two streams, an
//! interleaving the model names with `try_lock` rather than hang in).
//!
//! ```text
//! cargo test -p cheby_runtime ffi::models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step          | Mirrors (path under `crates/cheby_runtime/src/`, function)        |
//! | ------------------- | ----------------------------------------------------------------- |
//! | `Thread::free`      | `alloc.rs` `free_in` and `drain` (the worklist)                   |
//! | `Thread::finalize`  | `ffi.rs` `finalize_external` and `run_drop` (mark, queue, release) |
//! | `Thread::ffi_drop`  | `ffi.rs` `cheby_ffi_drop` and `release`                           |
//! | `Thread::write`     | `io.rs` `write_routed` (the stream lock around borrowed bytes)    |

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// Which variant of the protocol a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    /// The runtime's protocol.
    Runtime,
    /// `cheby_ffi_drop` frees at once instead of queueing.
    FreeNested,
    /// The free runs while the stream lock it wrote under is still held.
    FreeUnderLock,
}

/// The two standard streams' locks.
struct Streams {
    locks: [Mutex<usize>; 2],
}

/// One thread of a model: a chain of values `0..len`, value `i` holding
/// value `i + 1`, whose drop functions write to `stream`.
struct Thread {
    variant: Variant,
    streams: Arc<Streams>,
    stream: usize,
    /// How many drop functions ran.
    dropped: Arc<AtomicUsize>,
    /// Set while this thread frees (the runtime's worklist is per thread).
    freeing: bool,
    /// Set while a drop function runs: the mark.
    mark: Option<Vec<usize>>,
    len: usize,
}

impl Thread {
    /// Frees value `first` and everything only it kept alive, iteratively
    /// (`alloc::drain`), running each drop function once.
    fn free(&mut self, first: usize) {
        assert!(!self.freeing, "a free started inside another free");
        self.freeing = true;
        let mut worklist = vec![first];
        while let Some(value) = worklist.pop() {
            self.finalize(value, &mut worklist);
        }
        self.freeing = false;
    }

    /// The finalizer of value `value`: its drop function under the mark,
    /// then the queued references onto the running worklist.
    fn finalize(&mut self, value: usize, worklist: &mut Vec<usize>) {
        self.mark = Some(Vec::new());
        self.drop_function(value);
        let queued = self.mark.take().unwrap_or_default();
        worklist.extend(queued);
    }

    /// A user drop function: writes its line, then releases the next value.
    fn drop_function(&mut self, value: usize) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        self.write();
        let next = value.saturating_add(1);
        if next < self.len {
            self.ffi_drop(next);
        }
    }

    /// `cheby_ffi_drop`: queued under the mark (the runtime), or freed at
    /// once (the twin).
    fn ffi_drop(&mut self, value: usize) {
        match (&mut self.mark, self.variant) {
            (Some(queue), Variant::Runtime | Variant::FreeUnderLock) => queue.push(value),
            _ => self.free(value),
        }
    }

    /// One write through the funnel: the stream lock around borrowed bytes.
    /// In the twin `FreeUnderLock` the lock is still held when the write's
    /// caller frees: here the write releases a value of the other thread's
    /// stream kind, which writes to the other stream under this lock.
    fn write(&mut self) {
        let streams = Arc::clone(&self.streams);
        let mut guard = streams.locks[self.stream].lock().unwrap();
        *guard = guard.saturating_add(1);
        if self.variant == Variant::FreeUnderLock {
            // A free under the lock whose drop function writes to the
            // other stream: it needs the other lock while holding this
            // one. Where the other thread holds it the same way, both
            // would wait for ever; `try_lock` names the interleaving
            // instead of hanging the model.
            let other = usize::from(self.stream == 0);
            let Ok(held) = streams.locks[other].try_lock() else {
                panic!("a free under a stream lock waits for the other stream's lock: a deadlock");
            };
            drop(held);
        }
        drop(guard);
    }
}

/// Runs the model of `variant`: two threads each free a chain of three
/// values, one writing to standard output, the other to standard error.
fn model(variant: Variant) {
    loom::model(move || {
        let streams = Arc::new(Streams {
            locks: [Mutex::new(0), Mutex::new(0)],
        });
        let dropped = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..2)
            .map(|stream| {
                let mut thread = Thread {
                    variant,
                    streams: Arc::clone(&streams),
                    stream,
                    dropped: Arc::clone(&dropped),
                    freeing: false,
                    mark: None,
                    len: 3,
                };
                thread::spawn(move || thread.free(0))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            6,
            "a drop function ran twice or not at all"
        );
        let writes: usize = streams.locks.iter().map(|lock| *lock.lock().unwrap()).sum();
        assert_eq!(writes, 6);
    });
}

/// The runtime's protocol: no deadlock, each drop function once, no
/// nested free.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
fn ffi_models_drop_functions_queue_and_never_free_under_a_stream_lock() {
    model(Variant::Runtime);
}

/// The twin whose `cheby_ffi_drop` frees at once: a free nests.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "a free started inside another free")]
fn ffi_models_freeing_at_once_nests_a_free() {
    model(Variant::FreeNested);
}

/// The twin that frees under the stream lock: the two threads deadlock on
/// the two streams, which loom reports.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "deadlock")]
fn ffi_models_freeing_under_a_stream_lock_deadlocks() {
    model(Variant::FreeUnderLock);
}

// A pool thread acting for its parked fiber (D-950, `ffi_pool.rs`) ---------
//
// A package's `@blocking` call runs its shim on a pool thread, whose
// `cheby_ffi_` calls charge the parked fiber's domain. The protocol, as
// `ffi_pool.rs`, `ffi.rs` and `sched/blocking.rs` implement it:
//
// - the fiber registers the call's job with its runtime before it submits
//   it (`blocking::register_ffi_job`);
// - each `cheby_ffi_` call of the job that changes state enters the job
//   under its short per-job lock: an abandoned job's call does nothing,
//   otherwise the call is counted in flight, released from the lock, does
//   its work on the domain, and leaves under the lock (`FfiJob::enter`,
//   `FfiJob::leave`, `ffi::ffi_job_guarded`);
// - a give-up or a halt sets its flag, then, under the registry's lock,
//   marks every registered job under its per-job lock
//   (`blocking::abandon_ffi_jobs`, `FfiJob::abandon`);
// - teardown frees the domain only when no abandoned job names it: an
//   abandoned job keeps it until the process exits (`ffi_pool::ABANDONED`),
//   since a call that entered before the mark may still be working on it.
//
// Whatever the interleaving: no call touches a freed domain, no call
// enters after the mark, and the domain of an abandoned job is never freed.
// Three `#[should_panic]` twins show the model can fail: teardown frees the
// abandoned job's domain (a call in flight touches it); a call that never
// tests the mark (it enters after the mark); and the rejected unlocked mark
// (D-950: "invalidating the host slot without a lock") that frees the
// domain when it sees no call in flight, which a call between its test of
// the mark and its count slips past.
//
// | Model step           | Mirrors (path under `crates/cheby_runtime/src/`, function)         |
// | -------------------- | ------------------------------------------------------------------ |
// | `PoolJob::enter`     | `ffi_pool.rs` `FfiJob::enter`; `ffi.rs` `ffi_job_guarded`          |
// | `PoolJob::leave`     | `ffi_pool.rs` `FfiJob::leave`                                      |
// | `PoolJob::abandon`   | `ffi_pool.rs` `FfiJob::abandon`; `sched/blocking.rs` `abandon_ffi_jobs` |
// | `pool_model` teardown| `ffi_pool.rs` `ABANDONED` (the domain kept)                        |

/// Which variant of the pool protocol a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PoolVariant {
    /// The runtime's protocol.
    Runtime,
    /// Teardown frees the domain of an abandoned job too.
    FreeAbandoned,
    /// A call enters without testing the mark.
    UncheckedEntry,
    /// The mark is an atomic outside any lock, and teardown frees the
    /// domain when it sees no call in flight.
    UnlockedFreeIdle,
}

/// What the per-job lock guards: the mark and the calls in flight.
#[derive(Default)]
struct JobState {
    abandoned: bool,
    in_call: u32,
}

/// One package `@blocking` call's job and the domain it charges.
struct PoolJob {
    variant: PoolVariant,
    state: Mutex<JobState>,
    /// The twin `UnlockedFreeIdle`'s mark and count, outside the lock.
    unlocked_abandoned: AtomicUsize,
    unlocked_in_call: AtomicUsize,
    /// Whether teardown freed the fiber's domain.
    freed: AtomicUsize,
    /// Set by the halt inside the mark's critical section.
    marked: AtomicUsize,
}

impl PoolJob {
    /// Enters one call: false when the job is abandoned.
    fn enter(&self) -> bool {
        match self.variant {
            PoolVariant::Runtime | PoolVariant::FreeAbandoned => {
                let mut state = self.state.lock().unwrap();
                if state.abandoned {
                    return false;
                }
                assert_eq!(
                    self.marked.load(Ordering::SeqCst),
                    0,
                    "a call entered after the mark"
                );
                state.in_call = state.in_call.saturating_add(1);
                true
            }
            PoolVariant::UncheckedEntry => {
                let mut state = self.state.lock().unwrap();
                assert_eq!(
                    self.marked.load(Ordering::SeqCst),
                    0,
                    "a call entered after the mark"
                );
                state.in_call = state.in_call.saturating_add(1);
                true
            }
            PoolVariant::UnlockedFreeIdle => {
                if self.unlocked_abandoned.load(Ordering::SeqCst) != 0 {
                    return false;
                }
                self.unlocked_in_call.fetch_add(1, Ordering::SeqCst);
                true
            }
        }
    }

    /// Ends a call that entered.
    fn leave(&self) {
        if self.variant == PoolVariant::UnlockedFreeIdle {
            self.unlocked_in_call.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.in_call = state.in_call.saturating_sub(1);
    }

    /// The work of a call that entered: an allocation charged to the
    /// fiber's domain.
    fn work(&self) {
        assert_eq!(
            self.freed.load(Ordering::SeqCst),
            0,
            "a call touched a freed domain"
        );
    }

    /// The halt's mark; returns whether teardown may free the domain.
    fn abandon(&self) -> bool {
        match self.variant {
            PoolVariant::Runtime | PoolVariant::FreeAbandoned | PoolVariant::UncheckedEntry => {
                let mut state = self.state.lock().unwrap();
                state.abandoned = true;
                self.marked.store(1, Ordering::SeqCst);
                drop(state);
                // The runtime keeps an abandoned job's domain.
                self.variant == PoolVariant::FreeAbandoned
            }
            PoolVariant::UnlockedFreeIdle => {
                self.unlocked_abandoned.store(1, Ordering::SeqCst);
                self.unlocked_in_call.load(Ordering::SeqCst) == 0
            }
        }
    }
}

/// Runs the pool model of `variant`: the fiber's job, registered, makes two
/// `cheby_ffi_` calls on its pool thread and then ends; a halt marks the
/// registered jobs and tears down.
fn pool_model(variant: PoolVariant) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(move || {
        let job = Arc::new(PoolJob {
            variant,
            state: Mutex::new(JobState::default()),
            unlocked_abandoned: AtomicUsize::new(0),
            unlocked_in_call: AtomicUsize::new(0),
            freed: AtomicUsize::new(0),
            marked: AtomicUsize::new(0),
        });
        // The registry: the job is registered before its submit.
        let registry = Arc::new(Mutex::new(vec![Arc::clone(&job)]));
        let pool = {
            let (job, registry) = (Arc::clone(&job), Arc::clone(&registry));
            thread::spawn(move || {
                for _ in 0..2 {
                    if job.enter() {
                        job.work();
                        job.leave();
                    }
                }
                // The shim returned: the job is forgotten.
                registry
                    .lock()
                    .unwrap()
                    .retain(|known| !Arc::ptr_eq(known, &job));
            })
        };
        let halt = {
            let registry = Arc::clone(&registry);
            let job = Arc::clone(&job);
            thread::spawn(move || {
                let jobs = registry.lock().unwrap();
                let named = jobs.iter().any(|known| Arc::ptr_eq(known, &job));
                let free = if named { job.abandon() } else { true };
                drop(jobs);
                // Teardown: a domain no abandoned job keeps goes.
                if free {
                    job.freed.store(1, Ordering::SeqCst);
                }
                named
            })
        };
        pool.join().unwrap();
        let abandoned = halt.join().unwrap();
        if abandoned && variant == PoolVariant::Runtime {
            assert_eq!(
                job.freed.load(Ordering::SeqCst),
                0,
                "teardown freed a domain an abandoned job names"
            );
        }
    });
}

/// D-950: the runtime's protocol: no call touches a freed domain, none
/// enters after the mark, and an abandoned job's domain is kept.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
fn ffi_pool_model_the_lock_the_mark_and_teardown() {
    pool_model(PoolVariant::Runtime);
}

/// The twin whose teardown frees an abandoned job's domain: a call that
/// entered before the mark touches it.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "a call touched a freed domain")]
fn ffi_pool_model_freeing_an_abandoned_domain_fails() {
    pool_model(PoolVariant::FreeAbandoned);
}

/// The twin whose calls never test the mark: a call enters after it.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "a call entered after the mark")]
fn ffi_pool_model_an_unchecked_entry_fails() {
    pool_model(PoolVariant::UncheckedEntry);
}

/// The twin of the rejected unlocked mark (D-950), which frees the domain
/// when it sees no call in flight: a call between its test of the mark and
/// its count touches the freed domain.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "a call touched a freed domain")]
fn ffi_pool_model_an_unlocked_mark_fails() {
    pool_model(PoolVariant::UnlockedFreeIdle);
}
