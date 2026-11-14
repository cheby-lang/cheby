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
//! cargo nextest run -p cheby_runtime ffi::models
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

// Drop functions a pool thread leaves to the owner fiber (D-171, D-950) -----
//
// D-1307: a
// `cheby_ffi_drop` on a pool thread that releases the last reference to a
// value of a user external type runs its free there, but the type's drop
// function waits for the fiber the job acts for. The protocol, as `ffi.rs`,
// `ffi_pool.rs` and `sched/blocking.rs` implement it:
//
// - the release enters its job (the per-job lock and the mark, above); the
//   finalizer then pushes the drop function and its handle onto the job's
//   deferral list under that list's leaf lock (`ffi::finalize_external`,
//   `FfiJob::defer_drop`);
// - the job's completion wakes the owner fiber unless a give-up's flag is
//   set (`Completion::complete`, `wakes_nothing`);
// - the woken owner takes the list and runs each drop function on the
//   fiber (`FfiJob::take_deferred`, `ffi::run_deferred_drops`, called by
//   `cheby_rt_blocking_call` right after `wait_for_job`);
// - a give-up sets its flag, then marks the job (D-815, D-1269): its fiber
//   is never resumed, so what the job deferred stays in the list, unrun.
//
// Whatever the interleaving: no drop function runs on the pool thread, each
// runs at most once, every one the job deferred ran on the owner when the
// owner was woken, and none ran when the job was abandoned. The
// `#[should_panic]` twin runs the drop function where the release happens.
//
// | Model step              | Mirrors (path under `crates/cheby_runtime/src/`, function)              |
// | ----------------------- | ----------------------------------------------------------------------- |
// | `DeferJob::release`     | `ffi.rs` `cheby_ffi_drop`, `finalize_external`; `FfiJob::defer_drop`     |
// | `DeferJob::take`        | `ffi_pool.rs` `FfiJob::take_deferred`; `ffi.rs` `run_deferred_drops`     |
// | `defer_model` give-up   | `sched/blocking.rs` `poll_stop`, `abandon_ffi_jobs`; `FfiJob::abandon`  |
// | `defer_model` wake      | `sched/blocking.rs` `Completion::complete`, `cheby_rt_blocking_call`    |

/// Which variant of the deferral protocol a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferVariant {
    /// The runtime's protocol.
    Runtime,
    /// The drop function runs where the release happens.
    DropWhereReleased,
}

/// How many values of an external type the pool thread releases.
const DEFERRED: usize = 2;

/// One package `@blocking` call's job, the drop functions it defers, and
/// what ran.
struct DeferJob {
    variant: DeferVariant,
    state: Mutex<JobState>,
    /// The deferral list: the handles whose drop functions wait.
    deferred: Mutex<Vec<usize>>,
    /// The give-up's flag, set before its mark.
    given_up: AtomicUsize,
    /// How many releases entered the job.
    released: AtomicUsize,
    /// How many times each handle's drop function ran.
    runs: [AtomicUsize; DEFERRED],
}

impl DeferJob {
    /// Enters one call: false when the job is abandoned.
    fn enter(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.abandoned {
            return false;
        }
        state.in_call = state.in_call.saturating_add(1);
        true
    }

    /// Ends a call that entered.
    fn leave(&self) {
        let mut state = self.state.lock().unwrap();
        state.in_call = state.in_call.saturating_sub(1);
    }

    /// The give-up's mark.
    fn abandon(&self) {
        self.state.lock().unwrap().abandoned = true;
    }

    /// A drop function, given its handle; `on_pool` says where it runs.
    fn drop_function(&self, handle: usize, on_pool: bool) {
        assert!(!on_pool, "a drop function ran on the pool thread");
        if let Some(runs) = self.runs.get(handle) {
            runs.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The pool thread releases the last reference of value `handle`: the
    /// free runs here, and the drop function is deferred (or, in the twin,
    /// runs here).
    fn release(&self, handle: usize) {
        self.released.fetch_add(1, Ordering::SeqCst);
        match self.variant {
            DeferVariant::Runtime => self.deferred.lock().unwrap().push(handle),
            DeferVariant::DropWhereReleased => self.drop_function(handle, true),
        }
    }

    /// The owner, resumed, takes the list and runs it on the fiber.
    fn take(&self) {
        let deferred = std::mem::take(&mut *self.deferred.lock().unwrap());
        for handle in deferred {
            self.drop_function(handle, false);
        }
    }

    /// How many drop functions ran.
    fn ran(&self) -> usize {
        self.runs
            .iter()
            .map(|runs| runs.load(Ordering::SeqCst))
            .sum()
    }
}

/// Runs the deferral model of `variant`: the pool thread releases
/// [`DEFERRED`] values, each in a call of its own, then completes; a
/// give-up races it; the owner drains the list if it is woken.
fn defer_model(variant: DeferVariant) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(move || {
        let job = Arc::new(DeferJob {
            variant,
            state: Mutex::new(JobState::default()),
            deferred: Mutex::new(Vec::new()),
            given_up: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
            runs: std::array::from_fn(|_| AtomicUsize::new(0)),
        });
        let pool = {
            let job = Arc::clone(&job);
            thread::spawn(move || {
                for handle in 0..DEFERRED {
                    if job.enter() {
                        job.release(handle);
                        job.leave();
                    }
                }
                // The completion: it wakes the owner unless the give-up's
                // flag is set.
                job.given_up.load(Ordering::SeqCst) == 0
            })
        };
        let give_up = {
            let job = Arc::clone(&job);
            thread::spawn(move || {
                job.given_up.store(1, Ordering::SeqCst);
                job.abandon();
            })
        };
        // The owner waits on the completion.
        let woken = pool.join().unwrap();
        if woken {
            job.take();
        }
        give_up.join().unwrap();
        for runs in &job.runs {
            assert!(
                runs.load(Ordering::SeqCst) <= 1,
                "a drop function ran twice"
            );
        }
        let released = job.released.load(Ordering::SeqCst);
        if woken {
            assert_eq!(job.ran(), released, "a deferred drop function never ran");
        } else {
            assert_eq!(job.ran(), 0, "an abandoned job's drop function ran");
            assert_eq!(
                job.deferred.lock().unwrap().len(),
                released,
                "an abandoned job's deferred handle was touched"
            );
        }
    });
}

/// D-1307: the runtime's protocol: every drop function a pool
/// thread's release defers runs once, on the owner fiber, when it is
/// woken, and none runs for an abandoned job.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
fn ffi_pool_model_deferred_drops_run_on_the_owner() {
    defer_model(DeferVariant::Runtime);
}

/// The twin that runs the drop function where the release happens: on the
/// pool thread, against D-171.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "a drop function ran on the pool thread")]
fn ffi_pool_model_a_drop_on_the_pool_thread_fails() {
    defer_model(DeferVariant::DropWhereReleased);
}

// A call inline on its own fiber after a racing give-up (D-815, D-1269) -----
//
// D-1308: registration
// marks a job only while the runtime halts, no longer once its stop gave
// up, and a job that a give-up marked while it ran inline on its own fiber
// leaves the abandoned list after it. The protocol:
//
// - the fiber registers its job under the registry's lock
//   (`blocking::register_ffi_job`);
// - a give-up sets its flag, then, under the registry's lock, marks each
//   registered job under the job's lock and pushes it onto the leaf
//   `ABANDONED` list (`poll_stop`, `abandon_ffi_jobs`, `FfiJob::abandon`);
// - the fiber's `enter` reads the flag: set, the call runs inline on its
//   own fiber, which records that the job ran inline (`run_as_job`); unset,
//   the job goes to a pool thread, whose calls enter the job;
// - when the shim returned, the job is unregistered under the registry's
//   lock (`unregister_ffi_job`), and only then, on the fiber, a job that
//   ran inline is taken off `ABANDONED` (`ffi_pool::release_if_ran_inline`):
//   a give-up that held the registry's lock has pushed it by then, and a
//   later one no longer finds it.
//
// Whatever the interleaving: a job that ran inline is not in `ABANDONED` at
// the end and no pool thread touched its domain, and a marked job that went
// to a pool thread stays there (D-950). The `#[should_panic]` twin releases
// before it unregisters, so a give-up can push the job after the release.
// The teardown half of the protocol is `pool_model`'s, whose twin
// `ffi_pool_model_freeing_an_abandoned_domain_fails` still fails.
//
// | Model step               | Mirrors (path under `crates/cheby_runtime/src/`, function)            |
// | ------------------------ | --------------------------------------------------------------------- |
// | `inline_model` register  | `sched/blocking.rs` `register_ffi_job`                                |
// | `inline_model` give-up   | `sched/blocking.rs` `poll_stop`, `abandon_ffi_jobs`; `FfiJob::abandon` |
// | `inline_model` entry     | `sched/blocking.rs` `enter`, `wait_for_job`; `ffi_pool.rs` `run_as_job` |
// | `release_inline`         | `ffi_pool.rs` `release_if_ran_inline`                                  |

/// Which variant of the inline-release protocol a model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineVariant {
    /// The runtime's protocol.
    Runtime,
    /// The release comes before the unregistration.
    ReleaseBeforeUnregister,
}

/// A package `@blocking` call's job in the inline-release model.
struct InlineJob {
    state: Mutex<JobState>,
    /// Set by `run_as_job` on the job's own fiber.
    ran_inline: AtomicUsize,
    /// Set when a pool thread's call entered the job and charged its domain.
    pool_touched: AtomicUsize,
}

impl InlineJob {
    /// A job not marked, not run.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(JobState::default()),
            ran_inline: AtomicUsize::new(0),
            pool_touched: AtomicUsize::new(0),
        })
    }
}

/// The registry and the abandoned list of the inline-release model.
type Jobs = Arc<Mutex<Vec<Arc<InlineJob>>>>;

/// `ffi_pool::release_if_ran_inline`: takes a job that ran inline off the
/// abandoned list.
fn release_inline(abandoned: &Jobs, job: &Arc<InlineJob>) {
    if job.ran_inline.load(Ordering::SeqCst) != 0 {
        abandoned
            .lock()
            .unwrap()
            .retain(|known| !Arc::ptr_eq(known, job));
    }
}

/// Takes `job` out of the registry.
fn unregister(registry: &Jobs, job: &Arc<InlineJob>) {
    registry
        .lock()
        .unwrap()
        .retain(|known| !Arc::ptr_eq(known, job));
}

/// Runs the inline-release model of `variant`: a fiber registers its job
/// and enters while a give-up races it.
fn inline_model(variant: InlineVariant) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(move || {
        let job = InlineJob::new();
        let registry: Jobs = Arc::new(Mutex::new(Vec::new()));
        let abandoned: Jobs = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicUsize::new(0));
        let give_up = {
            let (registry, abandoned, flag) = (
                Arc::clone(&registry),
                Arc::clone(&abandoned),
                Arc::clone(&flag),
            );
            thread::spawn(move || {
                flag.store(1, Ordering::SeqCst);
                let jobs = registry.lock().unwrap();
                for known in jobs.iter() {
                    let first = {
                        let mut state = known.state.lock().unwrap();
                        let first = !state.abandoned;
                        state.abandoned = true;
                        first
                    };
                    if first {
                        abandoned.lock().unwrap().push(Arc::clone(known));
                    }
                }
            })
        };
        // The fiber: registration, then `enter`'s test of the give-up.
        registry.lock().unwrap().push(Arc::clone(&job));
        let inline = flag.load(Ordering::SeqCst) != 0;
        if inline {
            // `run_as_job` on the job's own fiber: the shim's calls act for
            // the fiber itself, and no pool thread sees the job.
            job.ran_inline.store(1, Ordering::SeqCst);
            if variant == InlineVariant::ReleaseBeforeUnregister {
                release_inline(&abandoned, &job);
            }
            unregister(&registry, &job);
            if variant == InlineVariant::Runtime {
                release_inline(&abandoned, &job);
            }
        } else {
            let pool = {
                let (job, registry) = (Arc::clone(&job), Arc::clone(&registry));
                thread::spawn(move || {
                    let entered = {
                        let mut state = job.state.lock().unwrap();
                        if state.abandoned {
                            false
                        } else {
                            state.in_call = state.in_call.saturating_add(1);
                            true
                        }
                    };
                    if entered {
                        job.pool_touched.store(1, Ordering::SeqCst);
                        let mut state = job.state.lock().unwrap();
                        state.in_call = state.in_call.saturating_sub(1);
                    }
                    unregister(&registry, &job);
                })
            };
            pool.join().unwrap();
            // A marked job's completion wakes nothing: the fiber is never
            // resumed and releases nothing (D-815).
        }
        give_up.join().unwrap();
        let kept = abandoned
            .lock()
            .unwrap()
            .iter()
            .any(|known| Arc::ptr_eq(known, &job));
        if inline {
            assert!(!kept, "an inline job stayed in the abandoned list");
            assert_eq!(
                job.pool_touched.load(Ordering::SeqCst),
                0,
                "a pool thread touched an inline job's domain"
            );
        } else if job.state.lock().unwrap().abandoned {
            assert!(kept, "a marked pool job left the abandoned list");
        }
    });
}

/// D-1308: the runtime's protocol: a job that a racing
/// give-up marked and that then ran inline leaves the abandoned list, and
/// a marked job on a pool thread stays in it.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
fn ffi_pool_model_an_inline_job_after_a_racing_give_up_is_released() {
    inline_model(InlineVariant::Runtime);
}

/// The twin that releases before it unregisters: a give-up that still
/// finds the job registered pushes it after the release, and it stays.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom switches stacks; Windows support is M6 (D-545)"
)]
#[should_panic(expected = "an inline job stayed in the abandoned list")]
fn ffi_pool_model_a_release_before_unregistering_fails() {
    inline_model(InlineVariant::ReleaseBeforeUnregister);
}
