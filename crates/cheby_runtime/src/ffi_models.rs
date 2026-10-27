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
