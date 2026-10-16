//! A worker: one OS thread of a runtime that runs fibers (D-689).
//!
//! The worker loop runs on the thread's own stack (Go's `g0`). It finds a
//! runnable fiber, switches to it, and when the fiber switches back runs
//! the fiber's [`AfterSwitch`] action there, after the fiber's registers
//! are saved. Where it looks, in order (architecture §3.5):
//!
//! 1. its targeted queue: fibers only it may run (pinned ones, D-687, and
//!    in the migration stress mode fibers moved off another worker); right
//!    after a pinned fiber yielded, this queue comes after step 5 instead,
//!    so the yield lets the worker's other fibers run;
//! 2. every 61st round, the due timers, then the global queue first, then
//!    the local deque, so neither is starved;
//! 3. its runnext slot: the fiber the running one woke just before it
//!    blocked, which keeps a capacity-0 ping-pong on one worker; it
//!    inherits the time slice, so the pair yields after one quantum (Go's
//!    `inheritTime`); a chain of spawns through the slot, which never
//!    yields between links, gives way to the deque at step 2;
//! 4. its local Chase-Lev deque;
//! 5. the global queue;
//! 6. the due timers (D-682), then the global queue again;
//! 7. as a searcher (at most half the workers search, Tokio's idle
//!    protocol): half the deque of each other worker, from a random one,
//!    then the global queue again.
//!
//! Then it parks until notified (`park`): in the runtime's poller if no
//! other worker holds it, with the earliest timer's deadline as the limit,
//! and otherwise on its own condition variable (D-680).
//!
//! Once a halt has started (D-073, D-708) the worker takes no fiber: its
//! loop ends before the next search, and a fiber that switches back with
//! `AfterSwitch::Freeze` is left frozen.
//!
//! # Started and retired workers (D-657, D-689, D-795)
//!
//! A runtime has [`MAX_THREADS`] worker slots, filled lazily: a slot gets
//! its [`WorkerShared`] when a worker first starts there, and keeps it for
//! the runtime's life, so the index a fiber last ran on (its
//! `sched.worker()`) always names a slot, also after that worker retired.
//! `set_thread_count` starts workers at once in free slots, lowest first,
//! and retires the live workers with the highest indexes. A slot's
//! [`life`] word moves so:
//!
//! ```text
//! LIVE --set_thread_count lowers the count--> RETIRING --raised again--> LIVE
//! RETIRING --the worker, at its next scheduling point--> LEAVING
//! LEAVING --its pinned fibers are gone, the count is still lower--> DEAD
//! LEAVING --the count went up meanwhile, or the runtime stops--> LIVE (the same thread rejoins)
//! DEAD --set_thread_count raises the count--> LIVE (a new thread)
//! LIVE or RETIRING --a stop, nothing runnable, its targeted queue empty--> DEAD (final)
//! ```
//!
//! The retiring worker's running fiber gets a yield request
//! ([`crate::ctx::reason::YIELD`], never `SAFEPOINT`, which is the halt's,
//! C-5), so even a lone fiber (D-737) gives the worker back at its next
//! entry check or suspension. The request comes from `retire_worker` if it
//! sees the fiber running, and otherwise from the worker itself when it
//! resumes a fiber while `RETIRING` (`run_fiber`), and again when a fiber
//! leaves its last kernel callback on a retiring or leaving worker
//! (`leave_callback`). Then, at the top of its loop, the worker
//! moves its runnext fiber and its deque to the global queue, as
//! `hand_off_before_blocking` does before a blocking call, ends its search,
//! leaves `Idle`, and wakes a parked worker for those fibers and, if it
//! held the poller, to hold it next. While fibers are pinned to it (a
//! kernel callback on their stacks, D-687) it keeps running them from its
//! targeted queue, parked aside from the poller in between, and takes no
//! other fiber. Then it marks its slot `DEAD` and its thread ends; the
//! runtime's stop and halt join it like every other worker (C-7). A stop
//! that is not a halt runs every runnable fiber first, so a worker that is
//! retiring when it begins stays, and one that is leaving rejoins; both end
//! as live workers do, once nothing is runnable: under the targeted
//! queue's lock with that queue empty, marking the slot `DEAD`, so a fiber
//! pushed there afterwards goes to the global queue, or, if pinned, is left
//! alone and counted (D-821).
//!
//! # The slot cache (D-825, D-677, D-749)
//!
//! A worker keeps up to four free stack slots for the fibers spawned by
//! the fibers it runs: `retire` puts the slot of a spawned fiber it retired
//! there (after D-749's deep release), and `fiber::create` takes from it
//! when a fiber running on this worker spawns. A root started by a host
//! takes its slot from the global pool and gives it back there. Only this worker's thread touches the cache, and
//! it gives every cached slot back to the global pool before its thread
//! ends and before its slot can be marked `DEAD`: at a halt, at a stop's
//! exit, and when it retires. A slot a teardown skipped or a halt froze is
//! never retired, so it never enters the cache (MEM-W1-1).

use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crossbeam_deque::{Steal, Stealer};
use parking_lot::Mutex;

use super::fiber::{self, AfterSwitch, FiberRef, state};
use super::park::Parker;
use super::{MAX_THREADS, Shared, Where};
use crate::ctx::ExecCtx;
use crate::fiber::stack::{SlotCache, StackError, StackSlot};
use crate::fiber::{Saved, StackPool, switch};
use crate::panic::fatal;
use crate::tls;

/// How often the global queue is checked first (Go's 61).
const GLOBAL_FIRST_EVERY: u64 = 61;

/// The values of a worker slot's life word (module documentation).
pub(super) mod life {
    /// A worker takes fibers there.
    pub(in crate::sched) const LIVE: u8 = 0;
    /// Its worker retires at its next scheduling point, unless the thread
    /// count goes up first.
    pub(in crate::sched) const RETIRING: u8 = 1;
    /// Its worker has left `Idle` and runs only the fibers pinned to it.
    pub(in crate::sched) const LEAVING: u8 = 2;
    /// No worker runs there: never started, or its thread has ended or is
    /// ending and no longer uses the slot.
    pub(in crate::sched) const DEAD: u8 = 3;
}

/// A runtime's worker slots (D-795): [`MAX_THREADS`]
/// of them, each filled the first time a worker starts there and never
/// emptied, so an index stays valid for the runtime's life.
pub(super) struct Workers {
    slots: Box<[OnceLock<Box<WorkerShared>>]>,
    /// One more than the highest index ever filled.
    filled: AtomicUsize,
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workers")
            .field("filled", &self.filled())
            .finish_non_exhaustive()
    }
}

impl Workers {
    /// No slot filled yet.
    pub(super) fn new() -> Self {
        Self {
            slots: (0..MAX_THREADS).map(|_| OnceLock::new()).collect(),
            filled: AtomicUsize::new(0),
        }
    }

    /// The worker of slot `index`, if one ever started there.
    pub(super) fn get(&self, index: usize) -> Option<&WorkerShared> {
        self.slots.get(index)?.get().map(|worker| &**worker)
    }

    /// The worker of slot `index`, made now if the slot was never filled;
    /// `None` past [`MAX_THREADS`].
    pub(super) fn fill(&self, index: usize) -> Option<&WorkerShared> {
        let worker = self
            .slots
            .get(index)?
            .get_or_init(|| Box::new(WorkerShared::new()));
        self.filled
            .fetch_max(index.saturating_add(1), Ordering::SeqCst);
        Some(worker)
    }

    /// One more than the highest index ever filled: the range a search
    /// over the workers covers.
    pub(super) fn filled(&self) -> usize {
        self.filled.load(Ordering::SeqCst)
    }

    /// Every filled slot with its index, live or not.
    pub(super) fn indexed(&self) -> impl Iterator<Item = (usize, &WorkerShared)> {
        (0..self.filled()).filter_map(|index| self.get(index).map(|worker| (index, worker)))
    }

    /// Every filled slot's worker, live or not.
    pub(super) fn iter(&self) -> impl Iterator<Item = &WorkerShared> {
        self.indexed().map(|(_, worker)| worker)
    }

    /// The number of slots whose life is `LIVE`.
    pub(super) fn live(&self) -> usize {
        self.iter()
            .filter(|worker| worker.life() == life::LIVE)
            .count()
    }

    /// The first live slot after `index`, going round, other than `index`
    /// itself: where the migration stress mode moves a fiber (D-687).
    pub(super) fn next_live_after(&self, index: usize) -> Option<usize> {
        let count = self.filled();
        (1..count)
            .map(|step| index.wrapping_add(step).checked_rem(count).unwrap_or(0))
            .find(|&other| {
                other != index
                    && self
                        .get(other)
                        .is_some_and(|worker| worker.life() == life::LIVE)
            })
    }
}

/// A worker's part of the shared runtime.
pub(crate) struct WorkerShared {
    /// The fiber to run next on this worker, or null.
    runnext: AtomicPtr<ExecCtx>,
    /// The worker's local deque. Only the worker's own thread uses it:
    /// the worker loop, and a fiber while it runs on this worker.
    local: std::cell::UnsafeCell<crossbeam_deque::Worker<FiberRef>>,
    /// The other end of `local`, for thieves.
    stealer: Stealer<FiberRef>,
    /// Fibers only this worker may run, never stolen.
    targeted: Mutex<std::collections::VecDeque<FiberRef>>,
    /// Where the worker's thread blocks while idle: in the poller or on a
    /// condition variable (D-680).
    parker: Parker,
    /// The worker's own context while a fiber runs. Only its thread uses
    /// it: the worker loop and the fiber it runs.
    saved: std::cell::UnsafeCell<Saved>,
    /// The fiber it runs, or null; the monitor reads it.
    running: AtomicPtr<ExecCtx>,
    /// How many fibers it has resumed; the monitor compares samples.
    resumes: AtomicU64,
    /// Held by the monitor while it looks at this worker and interrupts its
    /// running fiber.
    gate: Mutex<()>,
    /// Set by the monitor, under the gate, while a look is in progress:
    /// from before it reads `running` until after its request. A worker
    /// that has cleared `running` and sees it set waits for the gate
    /// before the fiber can be queued, woken or retired
    /// ([`WorkerShared::wait_for_monitor`]), so the monitor never touches
    /// a retired control block, whichever worker retires it (I-8).
    looking: AtomicBool,
    /// One of [`life`]'s values.
    life: AtomicU8,
    /// The fibers pinned to it now (D-687): a retiring worker stays until
    /// none is left.
    pinned: AtomicUsize,
    /// Up to four free stack slots kept for this worker's own spawns
    /// (D-825): filled by `Worker::retire`, taken by
    /// `fiber::create` on this worker, and drained to the global pool
    /// before the worker's thread ends (before its slot can be marked
    /// `DEAD`, after which a new thread may start here) and at a halt. Only
    /// the worker's own thread uses it.
    cache: std::cell::UnsafeCell<SlotCache>,
}

// SAFETY: `local`, `saved` and `cache` are used only by the worker's own
// thread (the worker loop, and a fiber while that thread runs it), and a
// thread that later starts in the slot finds `cache` drained before the
// slot was marked `DEAD`; every other field is atomic, locked or set once.
unsafe impl Sync for WorkerShared {}

impl std::fmt::Debug for WorkerShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerShared")
            .field("running", &self.running.load(Ordering::Relaxed))
            .field("resumes", &self.resumes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl WorkerShared {
    /// A worker with empty queues.
    pub(super) fn new() -> Self {
        let local = crossbeam_deque::Worker::new_fifo();
        let stealer = local.stealer();
        Self {
            runnext: AtomicPtr::new(ptr::null_mut()),
            local: std::cell::UnsafeCell::new(local),
            stealer,
            targeted: Mutex::new(std::collections::VecDeque::new()),
            parker: Parker::default(),
            saved: std::cell::UnsafeCell::new(Saved::EMPTY),
            running: AtomicPtr::new(ptr::null_mut()),
            resumes: AtomicU64::new(0),
            gate: Mutex::new(()),
            looking: AtomicBool::new(false),
            life: AtomicU8::new(life::DEAD),
            pinned: AtomicUsize::new(0),
            cache: std::cell::UnsafeCell::new(SlotCache::new()),
        }
    }

    /// A stack slot of `bytes`' class for a fiber this worker's running
    /// fiber spawns: from the worker's cache first, else from the global
    /// pool (D-825).
    ///
    /// # Errors
    ///
    /// As [`StackPool::take`].
    ///
    /// # Safety
    ///
    /// Called on the worker's own thread, by the fiber it runs.
    pub(super) unsafe fn take_cached_slot(&self, bytes: usize) -> Result<StackSlot, StackError> {
        // SAFETY: forwarded from the caller: only this thread uses the
        // cell, and the borrow ends with this call, before any switch.
        let cache = unsafe { &mut *self.cache.get() };
        StackPool::global().take_cached(cache, bytes)
    }

    /// Keeps the slot of a fiber this worker retired in its cache, after
    /// D-749's deep release, or gives it back to the global pool when the
    /// cache is full (D-825). Only a slot that may be
    /// reused comes here (MEM-W1-1): never one a teardown skipped or a
    /// halt froze, which is never retired.
    ///
    /// # Safety
    ///
    /// Called on the worker's own thread, by its loop.
    unsafe fn cache_slot(&self, slot: StackSlot, watermark: usize) {
        // SAFETY: forwarded from the caller, as in `take_cached_slot`.
        let cache = unsafe { &mut *self.cache.get() };
        StackPool::global().give_back_cached(cache, slot, watermark);
    }

    /// Gives every slot of the worker's cache back to the global pool.
    ///
    /// # Safety
    ///
    /// Called on the worker's own thread, by its loop, before its slot can
    /// be marked `DEAD`.
    unsafe fn drain_slot_cache(&self) {
        // SAFETY: forwarded from the caller, as in `take_cached_slot`.
        let cache = unsafe { &mut *self.cache.get() };
        StackPool::global().drain_cache(cache);
    }

    /// The number of slots in the worker's cache (for tests).
    ///
    /// # Safety
    ///
    /// Called on the worker's own thread, or once no thread runs in the
    /// slot and its last thread's writes are visible (it was joined, or
    /// its slot was seen `DEAD`).
    #[cfg(test)]
    pub(super) unsafe fn cached_slots(&self) -> usize {
        // SAFETY: forwarded from the caller: no other thread uses the cell
        // while this shared borrow lives.
        unsafe { &*self.cache.get() }.len()
    }

    /// The slot's life: one of [`life`]'s values.
    pub(super) fn life(&self) -> u8 {
        self.life.load(Ordering::SeqCst)
    }

    /// The slot's life read `Relaxed`, by the worker's own loop at its top
    /// (S7's K1): a hint, never an exact answer. A retirement that races
    /// the next resume is caught by `run_fiber`'s sequentially consistent
    /// load after it stores `running` (D-795's Dekker pair with
    /// `Shared::retire_worker`), which asks the fiber to yield. That load
    /// comes before this one on the same thread, so this one cannot read
    /// an older value than it did (read-read coherence): a `RETIRING` this
    /// read misses costs at most one more fiber slice, whose own resume
    /// sees it and asks again. A parked worker looks with `life` (`park`).
    fn life_hint(&self) -> u8 {
        self.life.load(Ordering::Relaxed)
    }

    /// Marks a free slot live for a worker about to start there. Under the
    /// runtime's thread registry lock, on a `DEAD` slot.
    pub(super) fn set_live(&self) {
        self.life.store(life::LIVE, Ordering::SeqCst);
    }

    /// Asks a live worker to retire; false if it was not live.
    pub(super) fn request_retire(&self) -> bool {
        self.life
            .compare_exchange(
                life::LIVE,
                life::RETIRING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Takes back a retirement the worker has not acted on yet; false if
    /// it was not retiring.
    pub(super) fn cancel_retire(&self) -> bool {
        self.life
            .compare_exchange(
                life::RETIRING,
                life::LIVE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// The worker acts on its retirement; false if it was taken back.
    fn begin_leaving(&self) -> bool {
        self.life
            .compare_exchange(
                life::RETIRING,
                life::LEAVING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Whether a fiber running on this worker may queue a fiber in its
    /// runnext slot or deque: not once it has drained them to leave.
    pub(super) fn takes_local(&self) -> bool {
        matches!(self.life(), life::LIVE | life::RETIRING)
    }

    /// A fiber running on this worker became pinned (D-687).
    pub(super) fn pin(&self) {
        self.pinned.fetch_add(1, Ordering::SeqCst);
    }

    /// A fiber pinned to this worker, and running on it, is no longer
    /// pinned. Returns whether the worker is retiring or leaving: the fiber
    /// must then be asked to yield, since nothing else moves a fiber that
    /// computes alone (D-737) off a worker that must end (D-657).
    pub(super) fn unpin(&self) -> bool {
        let _ = self
            .pinned
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            });
        matches!(self.life(), life::RETIRING | life::LEAVING)
    }

    /// The number of fibers pinned to it now.
    pub(super) fn pinned(&self) -> usize {
        self.pinned.load(Ordering::SeqCst)
    }

    /// The cell of the worker's own context.
    pub(super) fn saved(&self) -> *mut Saved {
        self.saved.get()
    }

    /// The worker's local deque.
    ///
    /// # Safety
    ///
    /// Called on the worker's own thread: by the worker loop, or by a fiber
    /// running on this worker, and the reference is dropped before the
    /// fiber suspends.
    pub(super) unsafe fn local(&self) -> &crossbeam_deque::Worker<FiberRef> {
        // SAFETY: forwarded from the caller: only this thread uses it.
        unsafe { &*self.local.get() }
    }

    /// Puts `fiber` in the runnext slot and returns the fiber it held.
    pub(super) fn put_next(&self, fiber: FiberRef) -> Option<FiberRef> {
        let old = self.runnext.swap(fiber.as_ptr(), Ordering::AcqRel);
        ptr::NonNull::new(old).map(|old| {
            FiberRef::from_ctx(
                // SAFETY: a fiber in the runnext slot is not retired.
                unsafe { old.as_ref() },
            )
        })
    }

    /// Empties the runnext slot and returns the fiber it held.
    pub(super) fn take_next(&self) -> Option<FiberRef> {
        let old = self.runnext.swap(ptr::null_mut(), Ordering::AcqRel);
        ptr::NonNull::new(old).map(|old| {
            FiberRef::from_ctx(
                // SAFETY: a fiber in the runnext slot is not retired.
                unsafe { old.as_ref() },
            )
        })
    }

    /// Queues a fiber only this worker may run, or gives it back if the
    /// worker has ended (`DEAD`, decided under the same lock as its last
    /// look at the queue).
    pub(super) fn push_targeted(&self, fiber: FiberRef) -> Result<(), FiberRef> {
        let mut targeted = self.targeted.lock();
        if self.life() == life::DEAD {
            return Err(fiber);
        }
        targeted.push_back(fiber);
        Ok(())
    }

    /// Marks the slot `DEAD` under the targeted queue's lock and returns
    /// what that queue still held: after this no fiber is queued here.
    pub(super) fn end(&self) -> Vec<FiberRef> {
        let mut targeted = self.targeted.lock();
        self.life.store(life::DEAD, Ordering::SeqCst);
        targeted.drain(..).collect()
    }

    /// A live worker's end at a stop (D-821, D-795,
    /// D-687): under the targeted queue's lock, marks the slot `DEAD` and
    /// returns true if that queue is empty; otherwise returns false, and
    /// the worker runs what it holds first. A push decides under the same
    /// lock, so after this returns true every push here is rejected and
    /// goes to the global queue (`Shared::push_targeted`), which the
    /// pushing worker, still running, looks at before it can end. `DEAD`
    /// stays: once stopping, `set_thread_count` only records the count (one
    /// that looked at the flag just before the stop may still start a new
    /// thread here, under the thread registry's lock, which the stop's join
    /// then waits for; that thread ends here too).
    pub(super) fn end_if_idle(&self) -> bool {
        let targeted = self.targeted.lock();
        if !targeted.is_empty() {
            return false;
        }
        self.life.store(life::DEAD, Ordering::SeqCst);
        true
    }

    fn has_targeted(&self) -> bool {
        !self.targeted.lock().is_empty()
    }

    /// Wakes the worker's thread: from the poller, from its condition
    /// variable, or, if it is not blocked, by making its next park return
    /// at once.
    pub(super) fn unpark(&self, shared: &Shared) {
        self.parker.unpark(&shared.driver);
    }

    /// The worker's parker (for tests).
    #[cfg(test)]
    pub(super) fn parker(&self) -> &Parker {
        &self.parker
    }

    /// Whether the worker's deque has fibers to steal.
    pub(super) fn has_stealable(&self) -> bool {
        !self.stealer.is_empty()
    }

    /// Whether its runnext slot or targeted queue holds a fiber.
    pub(super) fn has_waiting(&self) -> bool {
        !self.runnext.load(Ordering::Acquire).is_null() || self.has_targeted()
    }

    /// The fiber it runs, or null.
    pub(super) fn running(&self) -> *mut ExecCtx {
        self.running.load(Ordering::SeqCst)
    }

    /// How many fibers it has resumed.
    pub(super) fn resumes(&self) -> u64 {
        self.resumes.load(Ordering::Relaxed)
    }

    /// The monitor's gate.
    pub(super) fn gate(&self) -> &Mutex<()> {
        &self.gate
    }

    /// Marks a monitor look as in progress or over; the monitor calls it
    /// under the gate, `true` before it reads `running` and `false` after
    /// its request.
    pub(super) fn set_looking(&self, looking: bool) {
        self.looking.store(looking, Ordering::SeqCst);
    }

    /// Waits for a monitor look that may have read the fiber `running` held
    /// before this worker cleared it. Called after the clearing store and
    /// before the fiber can be queued, woken or retired.
    ///
    /// Dekker's pattern, every access sequentially consistent: the monitor
    /// stores `looking = true` and then loads `running`; the worker stores
    /// `running = null` and then loads `looking`. So either the monitor's
    /// load sees null (or a later fiber), or this load sees its flag: still
    /// `true`, and the gate, which the monitor holds until after its
    /// request, is taken and released; or the `false` it stored after its
    /// request, which that store publishes. The common case is one load,
    /// with no lock.
    fn wait_for_monitor(&self) {
        if self.looking.load(Ordering::SeqCst) {
            drop(self.gate.lock());
        }
    }
}

/// The state of a worker loop, on its own thread.
struct Worker<'a> {
    shared: &'a Shared,
    me: &'a WorkerShared,
    index: usize,
    tick: u64,
    searching: bool,
    /// A xorshift state for choosing the first victim.
    random: u64,
    /// Whether the fiber that just switched back was pinned and yielded:
    /// the next search then takes the targeted queue after the runnext
    /// slot, the local deque and the global queue, not first.
    pinned_yielded: bool,
    /// Whether the last fiber found came from the runnext slot: it then
    /// continues the time slice of the fiber that woke it (Go's
    /// `inheritTime`, [`Worker::run_fiber`]).
    from_runnext: bool,
}

/// The body of worker `index`'s thread.
pub(super) fn main(shared: &Shared, index: usize) {
    let Some(me) = shared.worker(index) else {
        return;
    };
    let seed = u64::try_from(index)
        .unwrap_or(0)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        | 1;
    let mut worker = Worker {
        shared,
        me,
        index,
        tick: 0,
        searching: false,
        random: seed,
        pinned_yielded: false,
        from_runnext: false,
    };
    worker.run();
}

impl Worker<'_> {
    fn run(&mut self) {
        // Whether the worker has looked for a fiber once more, at a stop,
        // since it last saw a fiber waiting on the blocking pool.
        let mut looked_after_stop = false;
        loop {
            // A halt (D-073, D-708): take no fiber any more, not even a
            // runnable one, so a requeued spinner is never resumed.
            if self.shared.halting() {
                self.drain_slot_cache();
                break;
            }
            // `set_thread_count` retires this worker at its next
            // scheduling point (D-657, D-689): here, between two fibers.
            // Not once the runtime stops: a stop runs every runnable fiber
            // to its end (`Runtime::shutdown`), and a worker that left now
            // could hand fibers to the global queue after the live workers
            // ended (review finding CONC3-3 of gate 3). A halt ended the
            // loop above. A `Relaxed` read (`WorkerShared::life_hint`):
            // `run_fiber`'s exact load covers the race with the next resume.
            if self.me.life_hint() == life::RETIRING && !self.shared.stopping() {
                if self.leave() {
                    break;
                }
                continue;
            }
            self.tick = self.tick.wrapping_add(1);
            if let Some(fiber) = self.find_runnable() {
                if self.searching {
                    self.searching = false;
                    if self.shared.idle.transition_worker_from_searching() {
                        self.shared.notify_parked();
                    }
                }
                self.run_fiber(fiber);
                continue;
            }
            // A stop (fixer-w3-1 of checkpoint 3): the worker ends only
            // with its targeted queue empty, and marks its slot `DEAD` under
            // that queue's lock, so no fiber is queued here after it ended
            // (in the migration stress mode another worker sends its fibers
            // here, D-687); otherwise it runs what arrived. A worker that was
            // retiring when the stop began, or leaving and rejoined, ends
            // here too. It does not end while a fiber of the runtime waits
            // on the blocking pool, whose completion makes it runnable,
            // unless the stop gave up on the pool; and once none waits, it
            // looks once more: a pool thread queues its fiber, from outside
            // the workers, before it lowers the count, so that fiber may
            // have come after the look above (`blocking::holds_stop`,
            // D-815).
            if self.shared.stopping() {
                if super::blocking::holds_stop(self.shared) {
                    looked_after_stop = false;
                    std::thread::yield_now();
                    continue;
                }
                if !looked_after_stop {
                    looked_after_stop = true;
                    continue;
                }
                // Before the slot can be `DEAD`: a thread `set_thread_count`
                // starts there afterwards would use the cache.
                self.drain_slot_cache();
                if self.me.end_if_idle() {
                    #[cfg(test)]
                    self.shared.probe.stop_exit(self.index);
                    break;
                }
                continue;
            }
            self.searching = self.park();
        }
    }

    fn next_random(&mut self) -> u64 {
        let mut x = self.random;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.random = x;
        x
    }

    fn find_runnable(&mut self) -> Option<FiberRef> {
        // A pinned fiber that yielded is back in the targeted queue; taking
        // that queue first would resume it at once, and a preemption
        // (D-688) would let nothing else run on this worker while a kernel
        // callback loops (D-687; review finding CONC2-2). So for this one
        // search the queue comes after this worker's other runnable fibers
        // and the global queue, as Go puts a yielded goroutine at the
        // global queue's tail.
        let yielded = std::mem::take(&mut self.pinned_yielded);
        self.from_runnext = false;
        if !yielded && let Some(fiber) = self.me.targeted.lock().pop_front() {
            return Some(fiber);
        }
        if self.tick.is_multiple_of(GLOBAL_FIRST_EVERY) {
            // Due timers too, so that a busy worker fires them (D-682):
            // the fibers they wake go to the global queue.
            self.shared.fire_if_due();
            if let Some(fiber) = retry(|| self.shared.injector.steal()) {
                return Some(fiber);
            }
            // The local deque before the runnext slot too: a chain of
            // fibers that each spawn their successor into the slot and end
            // never yields between links, so the slot alone would starve
            // the deque on one worker (review finding ROB2-3 of gate 2).
            // SAFETY: the worker loop runs on the worker's own thread, and
            // the reference ends with this statement.
            if let Some(fiber) = unsafe { self.me.local() }.pop() {
                return Some(fiber);
            }
        }
        if let Some(fiber) = self.me.take_next() {
            self.from_runnext = true;
            return Some(fiber);
        }
        // SAFETY: the worker loop runs on the worker's own thread, and the
        // reference ends with this function.
        let local = unsafe { self.me.local() };
        if let Some(fiber) = local.pop() {
            return Some(fiber);
        }
        if let Some(fiber) = retry(|| self.shared.injector.steal_batch_and_pop(local)) {
            return Some(fiber);
        }
        if yielded && let Some(fiber) = self.me.targeted.lock().pop_front() {
            return Some(fiber);
        }
        // Before it searches or parks, the worker fires the timers that are
        // due (D-682; one atomic read when none is armed). The fibers they
        // wake go to the global queue, or to their worker if pinned.
        if self.shared.fire_if_due()
            && let Some(fiber) = retry(|| self.shared.injector.steal_batch_and_pop(local))
        {
            return Some(fiber);
        }
        if !self.searching {
            self.searching = self.shared.idle.transition_worker_to_searching();
            if !self.searching {
                return None;
            }
        }
        // The steal range covers the filled slots; a worker that left has
        // emptied its deque and is skipped (D-795).
        let count = self.shared.workers.filled();
        let start = usize::try_from(self.next_random())
            .unwrap_or(0)
            .checked_rem(count)
            .unwrap_or(0);
        for step in 0..count {
            let victim = start.wrapping_add(step).checked_rem(count).unwrap_or(0);
            if victim == self.index {
                continue;
            }
            let Some(other) = self.shared.worker(victim) else {
                continue;
            };
            if !other.takes_local() {
                continue;
            }
            if let Some(fiber) = retry(|| other.stealer.steal_batch_and_pop(local)) {
                return Some(fiber);
            }
        }
        retry(|| self.shared.injector.steal_batch_and_pop(local))
    }

    /// Parks the worker's thread until it is notified or the runtime
    /// stops. Returns whether it wakes up searching.
    fn park(&mut self) -> bool {
        let shared = self.shared;
        if shared
            .idle
            .transition_worker_to_parked(self.index, self.searching)
            && shared.work_pending()
        {
            shared.notify_parked();
        }
        loop {
            // Work only this worker may run can arrive after its last look
            // and before it registered as parked; the pusher then found it
            // unparked and woke nobody (module `idle`).
            // A retirement wakes the worker without taking it out of the
            // parked list: it takes itself out and retires.
            if self.me.has_targeted() || shared.stopping() || self.me.life() == life::RETIRING {
                return !shared.idle.unpark_self(self.index);
            }
            // In the poller, if no other worker holds it, until a wake or
            // the earliest timer, which it then fires; it stays registered
            // as parked meanwhile, so a notifier can pick it (D-680).
            self.me.parker.park(shared);
            if !shared.idle.is_parked(self.index) {
                return true;
            }
        }
    }

    /// Retires this worker (module documentation; D-689): hands its runnext
    /// fiber and deque to the global queue, ends its search, leaves
    /// `Idle`, runs the fibers pinned to it until none is left, then ends.
    /// Returns whether the thread ends; false if the retirement was taken
    /// back, or the thread count went up meanwhile and the worker rejoins.
    /// Cold and never inlined (S7's K3), so `run_fiber` is inlined into
    /// the loop alone.
    #[cold]
    #[inline(never)]
    fn leave(&mut self) -> bool {
        if !self.me.begin_leaving() {
            return false;
        }
        let shared = self.shared;
        // 1. The fibers waiting here go where every live worker finds
        //    them, as before a blocking call (`hand_off_before_blocking`).
        let mut moved = false;
        if let Some(next) = self.me.take_next() {
            shared.injector.push(next);
            moved = true;
        }
        // SAFETY: the worker loop runs on the worker's own thread, and the
        // reference ends with this block.
        let local = unsafe { self.me.local() };
        while let Some(fiber) = local.pop() {
            shared.injector.push(fiber);
            moved = true;
        }
        // 2. Out of the idle protocol: no longer searching, no longer
        //    counted, never notified again.
        if self.searching {
            self.searching = false;
            shared.idle.transition_worker_from_searching();
        }
        shared.idle.remove_worker(self.index);
        // 3. A live worker takes the moved fibers, and, if nobody polls
        //    (this one held the poller, and the wake that retires it took
        //    it out), a parked one wakes to hold it: it parks again and
        //    finds the poller free (D-680).
        if moved || shared.work_pending() {
            shared.notify_parked();
        }
        if !shared.driver.polling()
            && let Some(index) = shared.idle.any_parked()
            && let Some(other) = shared.worker(index)
        {
            other.unpark(shared);
        }
        // 4. The fibers pinned here run only here (D-687).
        loop {
            if shared.halting() {
                // The halt joins this thread; its fibers stay frozen.
                self.drain_slot_cache();
                return true;
            }
            let next = self.me.targeted.lock().pop_front();
            if let Some(fiber) = next {
                // A new time slice for the monitor's samples (D-688).
                self.tick = self.tick.wrapping_add(1);
                self.from_runnext = false;
                self.run_fiber(fiber);
                continue;
            }
            if self.me.pinned() == 0 || shared.stopping() {
                break;
            }
            self.me.parker.park_aside();
        }
        // 5. End, or rejoin if the count went up while it was leaving. The
        //    cached slots go back first: once the slot is `DEAD`, a new
        //    thread may start there and use the cache.
        self.drain_slot_cache();
        shared.leave_or_rejoin(self.index)
    }

    /// Runs `fiber` until it switches back, then runs its action. Inlined
    /// into the worker loop (S7's K3: outlined, with two call sites, it
    /// cost a call and its spills on every resume); `leave`, its other
    /// caller, is cold.
    #[expect(
        clippy::inline_always,
        reason = "the resume path: MA's attribution (S7's K3) measured the outlined call"
    )]
    #[inline(always)]
    fn run_fiber(&mut self, fiber: FiberRef) {
        // SAFETY: a fiber in a run queue is not retired; only this worker
        // resumes it now.
        let ctx = unsafe { fiber.ctx() };
        ctx.sched.state().store(state::RUNNING, Ordering::SeqCst);
        ctx.sched.set_worker(self.index);
        // SAFETY: the control block is live until this worker retires it,
        // which happens after it stops being current, below.
        unsafe { tls::set_current_fiber(fiber.as_ptr()) };
        // A fiber from the runnext slot continues the time slice of the
        // fiber that woke it (Go's `inheritTime`): the count the monitor
        // samples stays as it was, so a chain of hand-offs through the
        // slot, such as a capacity-0 ping-pong, counts as one fiber
        // running and is asked to yield after one quantum (D-688). The
        // yield puts it on the global queue, which breaks the chain, so
        // the local deque runs: without this, a ping-pong starved the deque
        // forever on one worker (review finding CONC3-1; §10.1 allows
        // starvation only on JS).
        if !self.from_runnext {
            self.me.resumes.store(self.tick, Ordering::Relaxed);
        }
        self.me.running.store(fiber.as_ptr(), Ordering::SeqCst);
        // A retirement that came after the loop's look at the life word
        // (review finding CONC3-1 of gate 3). Dekker's pattern, every
        // access sequentially consistent: this worker stores `running` and
        // then loads its life; `Shared::retire_worker` stores `RETIRING`
        // and then loads `running`. So either that look sees this fiber and
        // asks it to yield, or this load sees `RETIRING` and asks here: the
        // fiber gives the worker back at its next entry check even when it
        // runs alone (D-737). Not during a stop, when a retiring worker
        // stays (`run`).
        if self.me.life() == life::RETIRING && !self.shared.stopping() {
            crate::ctx::request(ctx, crate::ctx::reason::YIELD);
        }
        self.shared.monitor.kick();
        // SAFETY: the fiber's context was saved by its last park (or made
        // by `prepare`) and nothing else resumes it: it was taken from a
        // queue, which hands it to exactly one worker. The worker's own
        // context goes to its cell, which only this thread uses; the fiber
        // switches back to it. The fiber cannot use anything this frame
        // borrows from a thread-local or lock (I-5).
        let message = unsafe {
            let to = (*ctx.sched.saved()).take();
            switch::switch(self.me.saved(), to, 0)
        };
        self.me.running.store(ptr::null_mut(), Ordering::SeqCst);
        // Before another worker can run the fiber, and so retire it, or
        // this one retires it (review finding CONC2-1).
        self.me.wait_for_monitor();
        // SAFETY: null is always allowed.
        unsafe { tls::set_current_fiber(ptr::null_mut()) };
        // SAFETY: `message` is the word the fiber's `park` passed, read
        // once, now, before the fiber can run again.
        let after = unsafe { fiber::after_switch(message) };
        match after {
            AfterSwitch::Requeue => {
                // A pinned fiber goes back to this worker's targeted queue.
                self.pinned_yielded = ctx.sched.pinned();
                ctx.sched.state().store(state::RUNNABLE, Ordering::SeqCst);
                self.shared.schedule(fiber, Some(self.index), Where::Global);
            }
            AfterSwitch::Park(unlock) => {
                // SAFETY: the fiber locked it on this thread and handed it
                // over; this is its only release.
                unsafe { fiber::release(unlock) };
                self.parked(fiber);
            }
            AfterSwitch::ParkAll(unlocks) => {
                // SAFETY: as above, for each lock.
                unsafe { fiber::release_all(unlocks) };
                self.parked(fiber);
            }
            AfterSwitch::Exit => self.retire(fiber),
            AfterSwitch::Freeze => {
                // D-708: never resumed, requeued or retired; its slot,
                // control block and wait nodes stay as they are.
                ctx.sched.state().store(state::FROZEN, Ordering::SeqCst);
                #[cfg(test)]
                self.shared.probe.froze();
            }
        }
    }

    /// Ends a park once the fiber's locks are released: `PARKING` becomes
    /// `PARKED`, or a fiber a lockless waker found `PARKING` is queued.
    fn parked(&self, fiber: FiberRef) {
        // SAFETY: the fiber has just switched away and is parking; it is
        // not retired.
        let ctx = unsafe { fiber.ctx() };
        match ctx.sched.state().compare_exchange(
            state::PARKING,
            state::PARKED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(state::PARKING_WOKEN) => {
                ctx.sched.state().store(state::RUNNABLE, Ordering::SeqCst);
                self.shared.schedule(fiber, Some(self.index), Where::Local);
            }
            Err(other) => fatal(format_args!("a parking fiber was in the state {other}")),
        }
    }

    /// Ends a fiber that will never run again, returned or unwound (D-713):
    /// drops its control block and gives its slot back.
    fn retire(&self, fiber: FiberRef) {
        // A monitor that read `running` before it was cleared has finished
        // its request: `run_fiber` waited for it.
        // SAFETY: the fiber switched away for good; this worker is its
        // last user.
        let ctx = unsafe { fiber.ctx() };
        ctx.sched.state().store(state::DONE, Ordering::SeqCst);
        // SAFETY: as above.
        let slot = unsafe { ctx.sched.take_slot() };
        let watermark = ctx.real_stack_limit();
        let spawned = ctx.sched.spawned();
        // SAFETY: the control block was written by `fiber::create` and is
        // dropped once, here; nothing refers to it any more: it is in no
        // queue, no monitor look that read it is in progress, and the
        // fiber's frames have all returned or unwound.
        unsafe { ptr::drop_in_place(fiber.as_ptr()) };
        #[cfg(test)]
        self.shared.probe.retired(fiber.as_ptr());
        self.shared.fiber_retired();
        let Some(slot) = slot else {
            return;
        };
        if !spawned {
            // A root started by a host, on the main stack's class or
            // another: the cache is for this worker's spawns, so its slot
            // goes back to the global pool (D-825, gate
            // 3's MEM-W3-2).
            StackPool::global().give_back(slot, watermark);
            return;
        }
        // SAFETY: the worker loop runs on the worker's own thread.
        unsafe { self.me.cache_slot(slot, watermark) };
    }

    /// Gives the worker's cached slots back to the global pool
    /// (D-825): before its thread ends, and before its
    /// slot can be marked `DEAD`, since a new thread may start there once
    /// it is. So no slot stays in a cache across a stop, a halt or a
    /// retirement.
    fn drain_slot_cache(&self) {
        // SAFETY: the worker loop runs on the worker's own thread, and the
        // slot is not `DEAD` yet.
        unsafe { self.me.drain_slot_cache() };
    }
}

/// Repeats a steal until it succeeds or finds nothing.
fn retry(mut steal: impl FnMut() -> Steal<FiberRef>) -> Option<FiberRef> {
    loop {
        match steal() {
            Steal::Success(fiber) => return Some(fiber),
            Steal::Empty => return None,
            Steal::Retry => std::hint::spin_loop(),
        }
    }
}
