//! A fiber's scheduling state (D-685, D-687, D-689): the part of its
//! control block the scheduler uses, how a fiber is laid out in its stack
//! slot, how it starts, and [`park`], the only way a fiber suspends.
//!
//! # States
//!
//! A fiber's state word moves through [`state`]'s values:
//!
//! ```text
//! RUNNABLE --worker resumes it--> RUNNING --park(Requeue)--> RUNNABLE
//! RUNNING --park(Park(lock) or ParkAll(locks))--> PARKING --worker, after the switch--> PARKED
//! PARKING --a waker that takes no lock--> PARKING_WOKEN --worker--> RUNNABLE
//! PARKED --wake--> RUNNABLE
//! RUNNING --park(Exit)--> DONE
//! RUNNING --park(Freeze), during a halt--> FROZEN
//! ```
//!
//! A halt (D-073, D-708) freezes every fiber that runs at its next entry
//! check: it switches away with [`AfterSwitch::Freeze`] and is never
//! resumed, requeued or retired; its slot, control block and wait nodes
//! stay as they are, since nothing runs afterwards.
//!
//! `PARKING` covers the moment between the fiber's decision to park and the
//! worker's post-switch step: the fiber's registers are not saved yet, so
//! it must not be resumed. A waker that holds the lock the fiber parks with
//! cannot see `PARKING`, since the lock is released after the switch; a
//! waker that takes no lock (timers, cancellation, the blocking pool) turns
//! it into `PARKING_WOKEN`, and the worker enqueues the fiber itself once
//! the switch is complete. A fiber that parks in a cancellable wait stores
//! `PARKING` earlier, with [`begin_park`], before the cancellation walk can
//! reach its wait node (survey-u5 §2.3, hazard 1), and `park` then leaves
//! the state as it finds it; [`cancel_park`] stores `RUNNING` back if the
//! fiber does not park after all.
//!
//! # Ownership of the cells
//!
//! The saved context, the slot and the start function are plain cells.
//! Exactly one thread uses them at a time: the fiber's own code while it
//! runs, and otherwise the worker that switched away from it or resumes
//! it. The hand-off between workers goes through the run queues and the
//! state word, which order every write before the next worker's read.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::lock_api::RawMutex as _;

use super::Shared;
use super::deadlock::DomainCounts;
use crate::ctx::{ExecCtx, FiberInit};
use crate::fiber::sanitizer::{self, SanFiber};
use crate::fiber::stack::WARM_BYTES;
use crate::fiber::{Saved, StackError, StackPool, StackSlot, switch};
use crate::panic::fatal;
use crate::tls;

/// The values of a fiber's state word (module documentation).
pub(crate) mod state {
    /// In a run queue, or about to be.
    pub(crate) const RUNNABLE: u8 = 0;
    /// Running on a worker.
    pub(crate) const RUNNING: u8 = 1;
    /// Switching away to park: not resumable yet.
    pub(crate) const PARKING: u8 = 2;
    /// Woken while switching away to park: the worker enqueues it.
    pub(crate) const PARKING_WOKEN: u8 = 3;
    /// Parked: a wake makes it runnable.
    pub(crate) const PARKED: u8 = 4;
    /// Finished: never runs again.
    pub(crate) const DONE: u8 = 5;
    /// Frozen by a halt: never runs again, and is never retired (D-708).
    pub(crate) const FROZEN: u8 = 6;
}

/// What a parked fiber waits in (D-858): the operation a deadlock report
/// names for it. A waiting kernel records it with [`FiberSched::note_wait`]
/// before it parks; the blocking pool's park records none (a pending job is
/// never blocked, D-857).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WaitOp {
    /// `channel::send`, at capacity.
    Send = 1,
    /// `channel::receive`, on an empty channel.
    Receive = 2,
    /// `fiber::join`, on a fiber that has not finished.
    Join = 3,
    /// `selector::select`, with no arm ready.
    Select = 4,
    /// The end of a `fiber::scope`, waiting for its members.
    Scope = 5,
    /// The end of a `fiber::timeout`'s timed scope.
    Timeout = 6,
}

impl WaitOp {
    /// D-858's name: `channel::send`, `channel::receive`, `fiber::join`,
    /// `selector::select`, `fiber::scope` or `fiber::timeout`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Send => "channel::send",
            Self::Receive => "channel::receive",
            Self::Join => "fiber::join",
            Self::Select => "selector::select",
            Self::Scope => "fiber::scope",
            Self::Timeout => "fiber::timeout",
        }
    }

    /// The byte the wait site stores for the operation (never 0).
    const fn byte(self) -> u8 {
        match self {
            Self::Send => 1,
            Self::Receive => 2,
            Self::Join => 3,
            Self::Select => 4,
            Self::Scope => 5,
            Self::Timeout => 6,
        }
    }

    /// The operation stored as `byte`, or `None` for 0 (no wait) and any
    /// value that is not one.
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Send),
            2 => Some(Self::Receive),
            3 => Some(Self::Join),
            4 => Some(Self::Select),
            5 => Some(Self::Scope),
            6 => Some(Self::Timeout),
            _ => None,
        }
    }
}

/// What a fiber runs when it first starts: a function of its own control
/// block. When it returns, the fiber exits.
pub(crate) type StartFn = Box<dyn FnOnce(*mut ExecCtx) + Send>;

/// The scheduling fields of a fiber control block (D-685, D-689).
pub(crate) struct FiberSched {
    /// The fiber's number within its runtime; 0 for isolated entries.
    id: u64,
    /// The runtime the fiber belongs to; `None` for isolated entries
    /// (constant evaluation, the REPL, the thread backend, D-690, D-714).
    runtime: Option<Arc<Shared>>,
    /// One of [`state`]'s values.
    state: AtomicU8,
    /// The worker that runs it, or ran it last.
    worker: AtomicUsize,
    /// Kernel callbacks on its stack; above 0 the fiber is pinned to its
    /// worker (D-687).
    callback_depth: AtomicU32,
    /// Set while the fiber waits on a blocking-pool job (standard input,
    /// file IO, `@blocking`; from before it parks until the pool thread
    /// clears it, right before the wake) or runs such a call inline (no
    /// runtime, or a refused pool thread): a teardown and a halt cannot
    /// stop it, so they skip it and count it (D-786, D-683). A fiber with
    /// the mark set is a pending pool job (`sched/blocking.rs`).
    blocking: AtomicBool,
    /// How many times a worker resumed it: the worker that resumes it
    /// stores one more with `Relaxed` before the switch (D-860), the only
    /// writer. The deadlock survey reads it after the state (`SeqCst`), so
    /// a survey that sees a later `PARKED` sees the count of the resume
    /// before it (`sched/deadlock.rs`).
    resumes: AtomicU64,
    /// The operation of its current wait (D-858): 0 for none, else a
    /// [`WaitOp`]. Written by the fiber itself ([`FiberSched::note_wait`],
    /// [`FiberSched::clear_wait`]) and read by the detector, so atomic
    /// (critique RISK-1): a fiber read `PARKED` can be resumed at once.
    wait_op: AtomicU8,
    /// The published frame of its current wait, `ExecCtx::wait_frame()`
    /// at the park (D-856): where the report's walk starts.
    wait_frame: AtomicUsize,
    /// Its domain's pending counts in its runtime (`sched/deadlock.rs`),
    /// taken from the runtime's table on the first timer or pool job it
    /// counts. Only the fiber itself fills it.
    domain_counts: OnceLock<Arc<DomainCounts>>,
    /// Its registers while it is not running.
    saved: UnsafeCell<Saved>,
    /// The slot it lives in, taken by the worker that retires it.
    slot: UnsafeCell<Option<StackSlot>>,
    /// Whether a fiber running on a worker spawned it, so its slot may go
    /// to the cache of the worker that retires it; a root started by a
    /// host gives its slot back to the global pool (D-825,
    /// gate 3's MEM-W3-2).
    spawned: bool,
    /// What it runs first, taken when it starts.
    start: UnsafeCell<Option<StartFn>>,
    /// What the sanitizers know about it (D-721, `fiber/sanitizer.rs`);
    /// zero-sized without `--cfg cheby_sanitize`.
    san: SanFiber,
}

// SAFETY: the cells are used by one thread at a time (module
// documentation): the fiber's own code while it runs, otherwise the worker
// that switched away from it or resumes it, with the hand-off ordered by
// the run queues and the state word. Every other field is atomic or
// immutable.
unsafe impl Sync for FiberSched {}

impl fmt::Debug for FiberSched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FiberSched")
            .field("id", &self.id)
            .field("scheduled", &self.runtime.is_some())
            .field("state", &self.state.load(Ordering::Relaxed))
            .field("worker", &self.worker.load(Ordering::Relaxed))
            .field(
                "callback_depth",
                &self.callback_depth.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl FiberSched {
    /// The fields of an isolated entry: no runtime, always running.
    pub(crate) fn isolated() -> Self {
        Self {
            id: 0,
            runtime: None,
            state: AtomicU8::new(state::RUNNING),
            worker: AtomicUsize::new(0),
            callback_depth: AtomicU32::new(0),
            blocking: AtomicBool::new(false),
            resumes: AtomicU64::new(0),
            wait_op: AtomicU8::new(0),
            wait_frame: AtomicUsize::new(0),
            domain_counts: OnceLock::new(),
            saved: UnsafeCell::new(Saved::EMPTY),
            slot: UnsafeCell::new(None),
            spawned: false,
            start: UnsafeCell::new(None),
            san: SanFiber::new(),
        }
    }

    /// The fields of a new fiber of `runtime`, runnable on `worker`;
    /// `spawned` when a fiber running on a worker of `runtime` spawns it.
    fn scheduled(
        id: u64,
        runtime: Arc<Shared>,
        worker: usize,
        spawned: bool,
        start: StartFn,
    ) -> Self {
        Self {
            id,
            runtime: Some(runtime),
            state: AtomicU8::new(state::RUNNABLE),
            worker: AtomicUsize::new(worker),
            callback_depth: AtomicU32::new(0),
            blocking: AtomicBool::new(false),
            resumes: AtomicU64::new(0),
            wait_op: AtomicU8::new(0),
            wait_frame: AtomicUsize::new(0),
            domain_counts: OnceLock::new(),
            saved: UnsafeCell::new(Saved::EMPTY),
            slot: UnsafeCell::new(None),
            spawned,
            start: UnsafeCell::new(Some(start)),
            san: SanFiber::new(),
        }
    }

    /// Whether a fiber running on a worker spawned this one: its slot may
    /// then go to the cache of the worker that retires it
    /// (D-825); a root's goes back to the global pool.
    pub(super) const fn spawned(&self) -> bool {
        self.spawned
    }

    /// The runtime the fiber belongs to, or `None` for an isolated entry.
    pub(crate) fn runtime(&self) -> Option<&Arc<Shared>> {
        self.runtime.as_ref()
    }

    /// The worker that runs the fiber, or ran it last.
    pub(crate) fn worker(&self) -> usize {
        self.worker.load(Ordering::Relaxed)
    }

    /// Records the worker that resumes the fiber.
    pub(super) fn set_worker(&self, worker: usize) {
        self.worker.store(worker, Ordering::Relaxed);
    }

    /// The state word.
    pub(super) fn state(&self) -> &AtomicU8 {
        &self.state
    }

    /// Whether a kernel callback is on the fiber's stack, which pins it to
    /// its worker (D-687).
    pub(crate) fn pinned(&self) -> bool {
        self.callback_depth.load(Ordering::Relaxed) > 0
    }

    /// A kernel starts calling a closure on this fiber (D-687): pins it to
    /// the worker it runs on, which counts it, so that the worker does not
    /// end while the fiber needs it (D-689). Only the fiber itself calls
    /// this, while it runs.
    pub(crate) fn enter_callback(&self) {
        let depth = self.callback_depth.load(Ordering::Relaxed);
        self.callback_depth
            .store(depth.saturating_add(1), Ordering::Relaxed);
        if depth == 0 {
            self.with_worker(super::worker::WorkerShared::pin);
        }
    }

    /// The closure a kernel called returned (D-687); the outermost one
    /// unpins the fiber. Returns whether the fiber was unpinned on a worker
    /// that is retiring or leaving: [`leave_callback`] then asks it to
    /// yield.
    #[must_use = "a fiber unpinned on a leaving worker must be asked to yield (`leave_callback`)"]
    fn unpin_callback(&self) -> bool {
        let depth = self.callback_depth.load(Ordering::Relaxed);
        self.callback_depth
            .store(depth.saturating_sub(1), Ordering::Relaxed);
        depth == 1
            && self
                .with_worker(super::worker::WorkerShared::unpin)
                .unwrap_or(false)
    }

    /// Runs `act` on the worker slot the fiber runs on, if it has a
    /// runtime.
    fn with_worker<R>(&self, act: impl FnOnce(&super::worker::WorkerShared) -> R) -> Option<R> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.worker(self.worker()))
            .map(act)
    }

    /// Whether the fiber waits on a blocking-pool job or is blocked
    /// inline in a system call (D-786, D-683). Callable from any thread
    /// that keeps the control block alive.
    pub(crate) fn blocking(&self) -> bool {
        self.blocking.load(Ordering::SeqCst)
    }

    /// Marks the fiber as blocked, or no longer blocked, in a system call
    /// (D-786, D-683). The fiber sets it before it parks on a pool job
    /// (or around a call it runs inline); the pool thread that ran the
    /// job clears it, right before its wake (`sched/blocking.rs`).
    pub(crate) fn set_blocking(&self, blocking: bool) {
        self.blocking.store(blocking, Ordering::SeqCst);
    }

    /// The fiber's number within its runtime, unique while the runtime
    /// lives (0 for isolated entries): the survey's key, which a reused
    /// slot address never aliases.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// How many times a worker resumed the fiber (D-860). Read by the
    /// detector after the state, which it reads with `SeqCst` first.
    pub(crate) fn resumes(&self) -> u64 {
        self.resumes.load(Ordering::Relaxed)
    }

    /// Counts one more resume: the worker that resumes the fiber, the only
    /// writer, before the switch (D-860). A plain load and store, both
    /// `Relaxed`: no read-modify-write on the park path.
    pub(super) fn count_resume(&self) {
        let resumes = self.resumes.load(Ordering::Relaxed);
        self.resumes
            .store(resumes.wrapping_add(1), Ordering::Relaxed);
    }

    /// Records what the fiber is about to wait in (D-858) and the
    /// published frame its report's walk starts at (`ExecCtx::wait_frame()`,
    /// D-856). Called by a waiting kernel before `begin_park`: both stores
    /// are `Relaxed`, ordered before the park by `begin_park`'s (or
    /// `park`'s) `SeqCst` store of the state, which the detector reads first.
    /// Only the fiber itself calls it, while it runs.
    pub(crate) fn note_wait(&self, op: WaitOp, frame: usize) {
        self.wait_frame.store(frame, Ordering::Relaxed);
        self.wait_op.store(op.byte(), Ordering::Relaxed);
    }

    /// Clears the wait site once the fiber resumed from its wait (or did not
    /// park after all): `Relaxed` stores. Only the fiber itself calls it.
    pub(crate) fn clear_wait(&self) {
        self.wait_op.store(0, Ordering::Relaxed);
        self.wait_frame.store(0, Ordering::Relaxed);
    }

    /// The operation and frame of the fiber's current wait, or `None`
    /// when it records none. Read by the detector after it has read the
    /// fiber's state `PARKED` (`SeqCst` first): `Relaxed` loads, so a value
    /// read while the fiber is resumed may be stale or torn between the
    /// two words; only the collect step, after the confirmation and with
    /// the state and resume count re-checked under the fiber's scope lock,
    /// acts on the frame (critique RISK-1, RISK-2).
    pub(crate) fn wait_site(&self) -> Option<(WaitOp, usize)> {
        let op = WaitOp::from_byte(self.wait_op.load(Ordering::Relaxed))?;
        Some((op, self.wait_frame.load(Ordering::Relaxed)))
    }

    /// The cell of the fiber's domain's pending counts
    /// (`deadlock::counts_of`).
    pub(super) fn domain_counts(&self) -> &OnceLock<Arc<DomainCounts>> {
        &self.domain_counts
    }

    /// The cell of the fiber's saved registers.
    pub(super) fn saved(&self) -> *mut Saved {
        self.saved.get()
    }

    /// Takes the slot out of a retired fiber.
    ///
    /// # Safety
    ///
    /// The caller is the worker that retires the fiber, which no longer
    /// runs.
    pub(super) unsafe fn take_slot(&self) -> Option<StackSlot> {
        // SAFETY: forwarded from the caller: only the retiring worker uses
        // the cell now.
        unsafe { (*self.slot.get()).take() }
    }

    /// What the sanitizers know about the fiber (`fiber/sanitizer.rs`).
    pub(super) fn san(&self) -> &SanFiber {
        &self.san
    }

    /// The fiber's stack slot as its lowest address and its size, or
    /// `(0, 0)` once it is taken, for the sanitizers' switch hooks.
    ///
    /// # Safety
    ///
    /// The caller is the worker that resumes or retires the fiber, which
    /// does not run now.
    pub(super) unsafe fn slot_bounds(&self) -> (usize, usize) {
        // SAFETY: forwarded from the caller: only that worker uses the
        // cell now.
        unsafe { (*self.slot.get()).as_ref() }.map_or((0, 0), |slot| (slot.bottom(), slot.size()))
    }
}

/// The closure a kernel called on fiber `me` returned (D-687); the
/// outermost one unpins it. Only the fiber itself calls this, while it
/// runs.
///
/// If its worker is retiring or leaving, the fiber is asked to yield
/// (never to freeze, C-5): unpinned now, it must give that worker back at
/// its next suspension point (D-657), and nothing else would ask a fiber
/// that computes alone (D-737; review finding CONC3-2 of gate 3). It then
/// goes on on a live worker.
pub(crate) fn leave_callback(me: &ExecCtx) {
    if me.sched.unpin_callback() {
        crate::ctx::request(me, crate::ctx::reason::YIELD);
    }
}

/// A fiber of a runtime: a pointer to its control block, which lives at
/// the top of its stack slot until a worker retires it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FiberRef(NonNull<ExecCtx>);

// SAFETY: a `FiberRef` is a handle that moves between run queues and
// threads; the control block it points to is `Sync`, and the fields that
// are not atomic are used under the ownership rules of the module
// documentation.
unsafe impl Send for FiberRef {}
// SAFETY: as above.
unsafe impl Sync for FiberRef {}

impl FiberRef {
    /// The fiber whose control block is `ctx`.
    pub(crate) fn from_ctx(ctx: &ExecCtx) -> Self {
        Self(NonNull::from(ctx))
    }

    /// The control block's address.
    pub(crate) fn as_ptr(self) -> *mut ExecCtx {
        self.0.as_ptr()
    }

    /// The control block.
    ///
    /// # Safety
    ///
    /// The fiber is not retired yet, and the reference is not used after
    /// it is.
    pub(crate) unsafe fn ctx<'a>(self) -> &'a ExecCtx {
        // SAFETY: forwarded from the caller.
        unsafe { self.0.as_ref() }
    }
}

/// What the worker does once a parking fiber's registers are saved
/// (architecture §3.4): Go's `gopark(unlockf)`. It travels to the worker as
/// the switch's message word.
#[derive(Debug)]
pub(crate) enum AfterSwitch<'a> {
    /// Yield: the fiber goes to the global queue, or back to its worker's
    /// queue of pinned fibers while it is pinned.
    Requeue,
    /// Park, then release the lock, if any, that the fiber held to
    /// register itself as a waiter: wakers that take that lock see the
    /// fiber parked.
    Park(Option<Unlock<'a>>),
    /// Park, then release every lock the fiber held to register itself as
    /// a waiter in several places (a `select` over several channels), in
    /// the reverse of the order they were given.
    ParkAll(Unlocks<'a>),
    /// The fiber finished, returned or unwound: its slot goes back to the
    /// pool.
    Exit,
    /// A halt froze the fiber (D-073, D-708): it is never resumed,
    /// requeued or retired, and nothing of it is released.
    Freeze,
}

/// Several mutexes given up like [`Unlock`], released by the worker after
/// the switch.
#[derive(Debug, Default)]
pub(crate) struct Unlocks<'a>(Vec<Unlock<'a>>);

impl<'a> Unlocks<'a> {
    /// No mutex yet.
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// Gives up `guard`: the mutex stays locked until the worker that
    /// parks the fiber releases it, after the switch.
    pub(crate) fn push<T: ?Sized>(&mut self, guard: parking_lot::MutexGuard<'a, T>) {
        self.0.push(Unlock::new(guard));
    }

    /// Releases every mutex, the last given first.
    ///
    /// # Safety
    ///
    /// As for [`Unlock::unlock`], for each.
    unsafe fn unlock(self) {
        for unlock in self.0.into_iter().rev() {
            // SAFETY: forwarded from the caller.
            unsafe { unlock.unlock() };
        }
    }
}

/// A locked `parking_lot` mutex whose guard was given up so that the
/// worker can release it after the switch (I-5).
pub(crate) struct Unlock<'a>(&'a parking_lot::RawMutex);

impl fmt::Debug for Unlock<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Unlock").finish_non_exhaustive()
    }
}

impl<'a> Unlock<'a> {
    /// Gives up `guard`: the mutex stays locked until the worker that
    /// parks the fiber releases it.
    pub(crate) fn new<T: ?Sized>(guard: parking_lot::MutexGuard<'a, T>) -> Self {
        let mutex = parking_lot::MutexGuard::mutex(&guard);
        // SAFETY: the raw mutex is used only to unlock it once, below,
        // after the guard is forgotten, so it is never unlocked twice.
        let raw = unsafe { mutex.raw() };
        std::mem::forget(guard);
        Self(raw)
    }

    /// Releases the mutex.
    ///
    /// # Safety
    ///
    /// Called once, on the thread that locked it: the worker that ran the
    /// fiber, right after the fiber switched to it.
    unsafe fn unlock(self) {
        // SAFETY: the mutex is locked (its guard was forgotten in `new`),
        // and this is its only release.
        unsafe { self.0.unlock() };
    }
}

/// Makes PARKING visible before a wait node is published (survey-u5 §2.3,
/// hazard 1): a fiber about to park in a cancellable wait calls it before
/// it links its node where the cancellation walk or a lockless waker can
/// reach it, so that a wake that wins the node finds the fiber `PARKING`
/// (and turns it into `PARKING_WOKEN`) and is never lost. A fiber that
/// then decides not to park calls [`cancel_park`], which stores `RUNNING`
/// back. Only the fiber itself calls it, while it runs.
pub(crate) fn begin_park(me: &ExecCtx) {
    me.sched.state.store(state::PARKING, Ordering::SeqCst);
}

/// Undoes [`begin_park`] for a fiber that did not park after all, because
/// it found itself cancelled when it tried to publish its node: stores
/// `RUNNING` back. No waker can have seen the fiber `PARKING` meanwhile: a
/// lockless waker reaches a fiber only through a published node, and a
/// waker that takes the channel's or join cell's lock waits for the lock
/// the fiber still holds.
pub(crate) fn cancel_park(me: &ExecCtx) {
    me.sched.state.store(state::RUNNING, Ordering::SeqCst);
}

/// Suspends the running fiber `me` and runs `after` on its worker once its
/// registers are saved (D-687, architecture §3.4). Returns when the fiber
/// is resumed, maybe on another worker, except after [`AfterSwitch::Exit`],
/// which never returns.
///
/// `me` must be the fiber running on this thread. No lock guard, `RefCell`
/// borrow or thread-local-derived pointer may be held across the call
/// (I-5): a lock is handed over in [`AfterSwitch::Park`] (several in
/// [`AfterSwitch::ParkAll`]) instead. Ends the
/// process for an isolated entry, which cannot suspend.
pub(crate) fn park(me: &ExecCtx, after: AfterSwitch<'_>) {
    let Some(shared) = me.sched.runtime() else {
        fatal(format_args!("a fiber without a runtime tried to suspend"));
    };
    let Some(worker) = shared.worker(me.sched.worker()) else {
        fatal(format_args!("a fiber's worker does not exist"));
    };
    let ends = matches!(after, AfterSwitch::Exit | AfterSwitch::Freeze);
    // A frozen fiber keeps its fake stack: it is never resumed, but it
    // does not end (D-708).
    let exiting = matches!(after, AfterSwitch::Exit);
    if matches!(after, AfterSwitch::Park(_) | AfterSwitch::ParkAll(_)) {
        // `RUNNING -> PARKING`, unless `begin_park` stored `PARKING`
        // already and a lockless waker may have turned it into
        // `PARKING_WOKEN` since: that wake must stand, and the worker
        // requeues the fiber after the switch.
        let _ = me.sched.state.compare_exchange(
            state::RUNNING,
            state::PARKING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
    let after = ManuallyDrop::new(after);
    let message = ptr::from_ref::<AfterSwitch<'_>>(&after).expose_provenance();
    // SAFETY: `me` runs on this thread, on the worker `worker`, which
    // switched to it and saved its own context in `worker.saved` (only
    // this thread uses that cell now). The fiber's own cell receives its
    // registers; it stays valid while the fiber is suspended, since the
    // control block lives at the top of its slot until it is retired. The
    // worker reads the message, which points to `after` on this stack,
    // before the fiber can be resumed or retired, and takes ownership of
    // it (hence `ManuallyDrop`). Nothing borrowed from a thread-local or a
    // lock guard is held here (I-5).
    unsafe {
        let to = (*worker.saved()).take();
        sanitizer::before_park(&me.sched.san, exiting);
        switch::switch(me.sched.saved(), to, message);
    }
    sanitizer::after_park(&me.sched.san);
    if ends {
        fatal(format_args!("a finished or frozen fiber was resumed"));
    }
    if cfg!(debug_assertions) && tls::current_fiber() != ptr::from_ref(me).cast_mut() {
        fatal(format_args!(
            "a fiber resumed without being the thread's current fiber (I-5)"
        ));
    }
}

/// Reads the action a fiber passed to its worker with [`park`].
///
/// # Safety
///
/// `message` is the word [`park`] passed to the switch that returned it,
/// read once, before the fiber can be resumed or retired.
pub(super) unsafe fn after_switch(message: usize) -> AfterSwitch<'static> {
    if message == 0 {
        fatal(format_args!(
            "a fiber switched to its worker without an action"
        ));
    }
    let after = ptr::with_exposed_provenance::<AfterSwitch<'static>>(message);
    // SAFETY: forwarded from the caller: `message` is the exposed address
    // of the `ManuallyDrop<AfterSwitch>` on the suspended fiber's stack,
    // read once, which moves it out. Its lifetime only covers the lock it
    // names, which the worker releases before the fiber can be resumed.
    unsafe { after.read() }
}

/// Runs the action of [`AfterSwitch::Park`]'s lock.
///
/// # Safety
///
/// As for [`Unlock::unlock`].
pub(super) unsafe fn release(unlock: Option<Unlock<'_>>) {
    if let Some(unlock) = unlock {
        // SAFETY: forwarded from the caller.
        unsafe { unlock.unlock() };
    }
}

/// Runs the action of [`AfterSwitch::ParkAll`]'s locks.
///
/// # Safety
///
/// As for [`Unlock::unlock`], for each lock.
pub(super) unsafe fn release_all(unlocks: Unlocks<'_>) {
    // SAFETY: forwarded from the caller.
    unsafe { unlocks.unlock() };
}

/// What a new fiber's control block starts with, besides its stack.
#[derive(Debug)]
pub(crate) struct FiberParams {
    /// Everything but the stack fields, which [`create`] fills in.
    pub(crate) init: FiberInit,
    /// The size of its stack slot in bytes.
    pub(crate) stack_size: usize,
}

/// Creates a runnable fiber of `runtime` that runs `start`: takes a slot,
/// places the control block at its top with a first soft limit
/// [`WARM_BYTES`] below the top (D-677), and prepares its first frame. The
/// fiber is not in any queue yet. `on_worker` is the worker of `runtime`
/// the calling thread is, when a fiber running there spawns: the slot then
/// comes from that worker's cache first (D-825). A root
/// started by a host passes `None` and takes its slot from the global
/// pool.
pub(super) fn create(
    runtime: &Arc<Shared>,
    worker: usize,
    on_worker: Option<usize>,
    params: FiberParams,
    start: StartFn,
) -> Result<FiberRef, StackError> {
    #[cfg(test)]
    if runtime.fail_stack.swap(false, Ordering::SeqCst) {
        // The test hook of a stack region that cannot be mapped (D-677).
        return Err(StackError::OutOfMemory {
            bytes: params.stack_size,
        });
    }
    let pool = StackPool::global();
    let cache_of = on_worker.and_then(|index| runtime.worker(index));
    let spawned = cache_of.is_some();
    let mut slot = match cache_of {
        // SAFETY: `on_worker` is the calling thread's worker (its running
        // fiber spawns), the only thread that uses its cache.
        Some(cache_of) => unsafe { cache_of.take_cached_slot(params.stack_size) }?,
        None => pool.take(params.stack_size)?,
    };
    let place = match slot.reserve_top(size_of::<ExecCtx>(), align_of::<ExecCtx>()) {
        Ok(place) => place,
        Err(error) => {
            pool.give_back(slot, usize::MAX);
            return Err(error);
        }
    };
    let hard_limit = slot.hard_limit();
    let soft_limit = slot.top().saturating_sub(WARM_BYTES).max(hard_limit);
    let stack_top = slot.stack_top();
    let id = runtime.next_fiber_id();
    let init = FiberInit {
        soft_limit,
        hard_limit,
        stack_top,
        sched: FiberSched::scheduled(id, Arc::clone(runtime), worker, spawned, start),
        ..params.init
    };
    let fcb = place.cast::<ExecCtx>();
    // SAFETY: `place` is `size_of::<ExecCtx>()` bytes aligned for it at the
    // top of a slot this function owns, which nothing else uses.
    unsafe { fcb.as_ptr().write(ExecCtx::new(init)) };
    // SAFETY: the block was written above and nothing else knows it yet.
    sanitizer::on_create(&unsafe { fcb.as_ref() }.sched.san);
    // SAFETY: `stack_top` lies in `slot` below the control block and above
    // its hard limit; nothing else uses the stack below it until the fiber
    // ends; `fiber_start` never returns and uses `arg` only as the control
    // block written above.
    let saved = unsafe { switch::prepare(&slot, stack_top, fiber_start, fcb.as_ptr().cast()) };
    // SAFETY: the block was written above and nothing else knows it yet.
    let ctx = unsafe { fcb.as_ref() };
    // SAFETY: as above: this thread is the only user of the cells.
    unsafe {
        *ctx.sched.saved.get() = saved;
        *ctx.sched.slot.get() = Some(slot);
    }
    #[cfg(test)]
    runtime.probe.created(fcb.as_ptr());
    runtime.fiber_created();
    Ok(FiberRef(fcb))
}

/// Where a fiber's stack starts: runs its start function, then exits.
///
/// # Safety
///
/// `arg` is the fiber's control block, made by [`create`].
unsafe extern "C" fn fiber_start(arg: *mut c_void) -> ! {
    let fcb = arg.cast::<ExecCtx>();
    // SAFETY: `arg` is the control block `create` placed at the top of the
    // slot; it lives until the worker retires the fiber after `Exit`.
    let ctx = unsafe { &*fcb };
    sanitizer::on_fiber_start(&ctx.sched.san);
    // SAFETY: the start function is set before the fiber is first
    // scheduled, and only the fiber itself, here, takes it.
    let start = unsafe { (*ctx.sched.start.get()).take() };
    if let Some(start) = start {
        start(fcb);
    }
    park(ctx, AfterSwitch::Exit);
    fatal(format_args!("a finished fiber was resumed"))
}
