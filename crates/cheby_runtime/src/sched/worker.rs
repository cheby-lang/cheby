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
//! 2. every 61st round, the global queue first, so it is never starved;
//! 3. its runnext slot: the fiber the running one woke just before it
//!    blocked, which keeps a capacity-0 ping-pong on one worker; it
//!    inherits the time slice, so the pair yields after one quantum and
//!    cannot starve the deque (Go's `inheritTime`);
//! 4. its local Chase-Lev deque;
//! 5. the global queue;
//! 6. as a searcher (at most half the workers search, Tokio's idle
//!    protocol): half the deque of each other worker, from a random one,
//!    then the global queue again.
//!
//! Then it parks until notified.

use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::thread::Thread;

use crossbeam_deque::{Steal, Stealer};
use parking_lot::Mutex;

use super::fiber::{self, AfterSwitch, FiberRef, state};
use super::{Shared, Where};
use crate::ctx::ExecCtx;
use crate::fiber::{Saved, StackPool, switch};
use crate::panic::fatal;
use crate::tls;

/// How often the global queue is checked first (Go's 61).
const GLOBAL_FIRST_EVERY: u64 = 61;

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
    /// The worker's thread, set by the thread itself before it can park.
    thread: OnceLock<Thread>,
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
}

// SAFETY: `local` and `saved` are used only by the worker's own thread
// (the worker loop, and a fiber while that thread runs it); every other
// field is atomic, locked or set once.
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
            thread: OnceLock::new(),
            saved: std::cell::UnsafeCell::new(Saved::EMPTY),
            running: AtomicPtr::new(ptr::null_mut()),
            resumes: AtomicU64::new(0),
            gate: Mutex::new(()),
            looking: AtomicBool::new(false),
        }
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

    fn take_next(&self) -> Option<FiberRef> {
        let old = self.runnext.swap(ptr::null_mut(), Ordering::AcqRel);
        ptr::NonNull::new(old).map(|old| {
            FiberRef::from_ctx(
                // SAFETY: a fiber in the runnext slot is not retired.
                unsafe { old.as_ref() },
            )
        })
    }

    /// Queues a fiber only this worker may run.
    pub(super) fn push_targeted(&self, fiber: FiberRef) {
        self.targeted.lock().push_back(fiber);
    }

    fn has_targeted(&self) -> bool {
        !self.targeted.lock().is_empty()
    }

    /// Wakes the worker's thread if it is parked in `std::thread::park`.
    pub(super) fn unpark(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
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
    // Before the worker can register as parked, so a notifier that finds
    // it parked can always unpark it.
    let _ = me.thread.set(std::thread::current());
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
        loop {
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
            if self.shared.stopping() {
                break;
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
        if self.tick.is_multiple_of(GLOBAL_FIRST_EVERY)
            && let Some(fiber) = retry(|| self.shared.injector.steal())
        {
            return Some(fiber);
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
        if !self.searching {
            self.searching = self.shared.idle.transition_worker_to_searching();
            if !self.searching {
                return None;
            }
        }
        let count = self.shared.workers.len();
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
            if self.me.has_targeted() || shared.stopping() {
                return !shared.idle.unpark_self(self.index);
            }
            std::thread::park();
            if !shared.idle.is_parked(self.index) {
                return true;
            }
        }
    }

    /// Runs `fiber` until it switches back, then runs its action.
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
            AfterSwitch::Exit => self.retire(fiber, false),
            AfterSwitch::Quarantine => self.retire(fiber, true),
        }
    }

    /// Ends a fiber that will never run again: drops its control block and
    /// gives its slot back, or quarantines it for an abandoned fiber.
    fn retire(&self, fiber: FiberRef, quarantine: bool) {
        // A monitor that read `running` before it was cleared has finished
        // its request: `run_fiber` waited for it.
        // SAFETY: the fiber switched away for good; this worker is its
        // last user.
        let ctx = unsafe { fiber.ctx() };
        ctx.sched.state().store(state::DONE, Ordering::SeqCst);
        // SAFETY: as above.
        let slot = unsafe { ctx.sched.take_slot() };
        let watermark = ctx.real_stack_limit();
        // SAFETY: the control block was written by `fiber::create` and is
        // dropped once, here; nothing refers to it any more: it is in no
        // queue, no monitor look that read it is in progress, and an
        // abandoned fiber's frames never run again.
        unsafe { ptr::drop_in_place(fiber.as_ptr()) };
        #[cfg(test)]
        self.shared.probe.retired(fiber.as_ptr());
        self.shared.fiber_retired();
        let Some(slot) = slot else {
            return;
        };
        let pool = StackPool::global();
        if quarantine {
            // SAFETY: the fiber never runs again and its control block is
            // dropped: nothing in the slot is live.
            unsafe { slot.release(slot.bottom(), slot.top()) };
            pool.quarantine(slot);
        } else {
            pool.give_back(slot, watermark);
        }
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
