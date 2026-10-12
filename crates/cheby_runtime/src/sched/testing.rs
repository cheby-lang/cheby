//! Test helpers that run Rust closures as fibers of a [`Runtime`], for the
//! scheduler's tests and those of the fiber and channel runtime: runtimes
//! with a fixed thread count, root fibers, spawned fibers, a one-word
//! mailbox built on [`park`] and [`wake`], and the function-entry check
//! generated code performs.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use super::{
    AfterSwitch, FiberRef, Runtime, RuntimeConfig, Tuning, Unlock, park, spawn_fiber, wake,
};
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, MIN_STACK_SIZE};
use crate::panic::{cheby_rt_stack_slow, stack_position};

/// How long a test waits for an outcome before it calls it a hang.
pub(crate) const HANG: Duration = Duration::from_secs(60);

/// Whether the tests run under an emulator, where only correctness is
/// asserted (`CHEBY_RT_TEST_EMULATED=1`, set by the container and Rosetta
/// runs).
pub(crate) fn emulated() -> bool {
    std::env::var_os("CHEBY_RT_TEST_EMULATED").is_some_and(|value| value == "1")
}

/// A runtime of `threads` workers with the migration stress mode off.
pub(crate) fn runtime(threads: usize) -> Runtime {
    tuned(threads, false, true)
}

/// A runtime of `threads` workers with the migration stress mode `stress`;
/// without `rescue`, the monitor never wakes a worker for a queued fiber,
/// so a lost wakeup hangs.
pub(crate) fn tuned(threads: usize, stress: bool, rescue: bool) -> Runtime {
    Runtime::tuned(
        &RuntimeConfig {
            threads: Some(threads),
            ..RuntimeConfig::default()
        },
        Tuning {
            stress_migrate: Some(stress),
            rescue,
        },
    )
    .unwrap()
}

/// An entry configuration for Rust-level roots in test mode.
pub(crate) fn config(stack_size: usize) -> EntryConfig {
    EntryConfig {
        stack_size,
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// Starts `body` as a root fiber with a stack of `stack_size` bytes; the
/// receiver gets its word, or its panic.
pub(crate) fn start_root(
    runtime: &Runtime,
    stack_size: usize,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> mpsc::Receiver<EntryOutcome> {
    runtime
        .spawn_root(
            &config(stack_size),
            Box::new(move |ctx| {
                // SAFETY: `ctx` is the root fiber's live control block.
                body(unsafe { &*ctx })
            }),
        )
        .unwrap()
}

/// Starts `body` as a root fiber with the entry configuration `config`
/// (its stack, trampolines, domain and backend); the receiver gets its
/// word, or its panic.
pub(crate) fn start_root_with(
    runtime: &Runtime,
    config: &EntryConfig,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> mpsc::Receiver<EntryOutcome> {
    runtime
        .spawn_root(
            config,
            Box::new(move |ctx| {
                // SAFETY: `ctx` is the root fiber's live control block.
                body(unsafe { &*ctx })
            }),
        )
        .unwrap()
}

/// Runs `body` as a root fiber on the smallest stack and returns its word;
/// panics if it panics or hangs.
pub(crate) fn root(runtime: &Runtime, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) -> u64 {
    match start_root(runtime, MIN_STACK_SIZE, body).recv_timeout(HANG) {
        Ok(EntryOutcome::Returned(word)) => word,
        other => panic!("the root fiber did not return: {other:?}"),
    }
}

/// Spawns `body` as a fiber of `spawner`'s runtime on the smallest stack,
/// in the runnext slot with `next`.
pub(crate) fn spawn(spawner: &ExecCtx, next: bool, body: impl FnOnce(&ExecCtx) + Send + 'static) {
    spawn_fiber(
        spawner,
        MIN_STACK_SIZE,
        Box::new(move |ctx| {
            // SAFETY: `ctx` is the fiber's live control block.
            body(unsafe { &*ctx });
        }),
        next,
    )
    .unwrap();
}

/// A one-word mailbox: what a channel of capacity 1 does with a single
/// receiver, built on `park` and `wake` as the channel kernels will be.
#[derive(Default)]
pub(crate) struct Mailbox {
    state: parking_lot::Mutex<(Option<u64>, Option<FiberRef>)>,
}

impl Mailbox {
    /// Waits for a word.
    pub(crate) fn receive(&self, me: &ExecCtx) -> u64 {
        loop {
            let mut state = self.state.lock();
            if let Some(word) = state.0.take() {
                return word;
            }
            state.1 = Some(FiberRef::from_ctx(me));
            park(me, AfterSwitch::Park(Some(Unlock::new(state))));
        }
    }

    /// Puts a word in the empty mailbox, waking its receiver; with
    /// `from_running`, the receiver takes the waker's runnext slot.
    pub(crate) fn send(&self, word: u64, from_running: bool) {
        let mut state = self.state.lock();
        assert!(state.0.is_none(), "the mailbox is full");
        state.0 = Some(word);
        if let Some(waiter) = state.1.take() {
            // SAFETY: the waiter registered under this lock and parks with
            // it, so it is parking or parked, not retired.
            assert!(unsafe { wake(waiter, from_running) });
        }
    }

    /// The word in the mailbox, if any, without waiting.
    pub(crate) fn peek(&self) -> Option<u64> {
        self.state.lock().0
    }
}

/// Test hooks of one runtime's monitor: a delay between reading a worker's
/// running fiber and requesting a yield from it, which widens the window
/// in which that fiber could be retired, and a count of requests aimed at
/// a retired control block (which must stay 0, I-8).
#[derive(Debug, Default)]
pub(crate) struct MonitorProbe {
    /// Milliseconds the monitor sleeps before each request.
    pub(crate) delay_ms: std::sync::atomic::AtomicU64,
    /// The control blocks this runtime retired and has not reused.
    retired: parking_lot::Mutex<std::collections::HashSet<usize>>,
    /// Requests the monitor was about to make on a retired control block.
    pub(crate) stale: std::sync::atomic::AtomicUsize,
}

impl MonitorProbe {
    /// A control block was written at `fcb`.
    pub(crate) fn created(&self, fcb: *mut ExecCtx) {
        self.retired.lock().remove(&fcb.addr());
    }

    /// The control block at `fcb` was dropped.
    pub(crate) fn retired(&self, fcb: *mut ExecCtx) {
        self.retired.lock().insert(fcb.addr());
    }

    /// The monitor is about to request a yield from `fcb`.
    pub(crate) fn before_request(&self, fcb: *mut ExecCtx) {
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        if self.retired.lock().contains(&fcb.addr()) {
            self.stale.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// The function-entry check generated code performs; returns whether the
/// slow path raised (a stack overflow, a deadline), after which generated
/// code would run its cleanup pad and return (D-733).
pub(crate) fn entry_check(ctx: &ExecCtx) -> bool {
    if stack_position() < ctx.stack_limit.load(Ordering::Relaxed) {
        // SAFETY: `ctx` is the running fiber's.
        unsafe { cheby_rt_stack_slow(std::ptr::from_ref(ctx).cast_mut(), 0) };
    }
    crate::unwind::flag::get(ctx) != crate::abi::unwind_flag::NONE
}
