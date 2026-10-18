//! The fiber control block (FCB, D-685): `ExecCtx`, the hidden first
//! argument of every Cheby-ABI function (D-453). One per running entry
//! today; one per fiber once the scheduler runs fibers.
//!
//! # The generated-code view
//!
//! Generated code touches only the fields with an `abi::CTX_*` offset,
//! which are fixed (D-685): it reads the stack limit at 0 (a `usize`), the
//! mode at 8 (a `u32`) and the unwind flag at 12 (a `u32`, D-733), and it
//! writes the wait-frame word at 16 (a `usize`, D-856;
//! GAP(loc-ctx-wait-frame)) right before it calls a waiting kernel or
//! `scope_close`. The view ends at byte 24, where the Rust part starts
//! with the interrupt reasons; offsets 16 and 24 once held the exit record
//! of a prototype that is gone (D-763), so nothing generated reads or
//! writes anything else. The function-entry check compares the stack pointer with `stack_limit` and
//! calls `cheby_rt_stack_slow` when it is below. Setting `stack_limit` to
//! `usize::MAX` forces the next function entry into that slow path, which
//! is how an interrupt reaches running code with the same single compare
//! (D-124).
//!
//! # Interrupt reasons
//!
//! Why the slow path was forced is a bit set of [`reason`]s in the Rust
//! part (D-688). [`request`] and [`take`] use only sequentially consistent
//! operations, which makes the protocol free of lost interrupts: in the
//! single total order of `SeqCst` operations, a request whose `fetch_or`
//! follows the handler's `swap` also stores `usize::MAX` after the
//! handler's store of the soft limit, so the next entry check takes the
//! slow path again and sees it.
//!
//! # The Rust part
//!
//! Everything M1 kept in thread-locals of the entry's thread lives here
//! inline: the hash seed, the build's constants, the trampolines, the
//! accounting domain, constant evaluation's memory budget, the scratch
//! counts, the stack top, the entry mode and where the outcome goes.
//! Fields that change while the fiber runs are atomics written with
//! relaxed ordering by the owning fiber only, so the block may be shared
//! with the threads that interrupt it. `tls.rs`'s current fiber points to
//! it while it runs (D-686).
//!
//! A fiber of a runtime has its block at the top of its stack slot (D-677)
//! with the scheduler's fields (`sched`), the structured-concurrency part
//! (`conc`: its scopes and the fiber lock over its cancellation target and
//! wait node, the only state other fibers touch, D-706) and the record of
//! the unwinding in flight (`unwinding`). An isolated entry's block is on
//! the heap and has no runtime.
//!
//! # Stack limits
//!
//! A fiber starts with a soft limit 128 KiB below the top of its slot.
//! When an entry check fails because of depth, not an interrupt, the slow
//! path lowers the soft limit in doubling steps down to the hard limit,
//! 256 KiB above the slot's bottom, which is where a stack overflow is
//! reported (D-677). The lowest soft limit is the watermark the slot is
//! given back with. An isolated entry's soft limit is its hard limit.

use std::cell::RefCell;
use std::mem::offset_of;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use crate::abi;
use crate::alloc::{BlockCache, Domain};
use crate::consteval::ConstTable;
use crate::entry::{Callbacks, EntryMode, EntryOutcome};
use crate::fiber_ops::FiberConc;
use crate::sched::FiberSched;
use crate::unwind::UnwindState;

/// The reasons for an interrupt (D-688), bits of the FCB's reason word.
pub mod reason {
    /// The scheduler asks the fiber to yield (D-028, D-688): the slow path
    /// puts a fiber of a runtime back in a run queue; an isolated entry
    /// ignores it.
    pub const YIELD: u32 = 1;
    /// A cancellation reached the fiber (D-688): the walk sets the fiber's
    /// target, then requests it, so the next entry check takes the slow
    /// path, whose step 3 delivers the cancellation from the target
    /// (`fiber_ops::cancel::check`). A scope's close clears it with its own
    /// target ([`super::clear_reason`]).
    pub const CANCEL: u32 = 2;
    /// Constant evaluation's deadline may have passed (D-195, D-536).
    pub const DEADLINE: u32 = 4;
    /// The runtime stops the fiber at a safepoint. Accepted and ignored
    /// until the runtime stops fibers.
    pub const SAFEPOINT: u32 = 8;
}

/// The fiber control block (D-685).
#[derive(Debug)]
#[repr(C)]
pub struct ExecCtx {
    /// At [`abi::CTX_STACK_LIMIT`]: the lowest stack address a function
    /// may start below without calling the slow path, or `usize::MAX` to
    /// request an interrupt.
    pub stack_limit: AtomicUsize,
    /// At [`abi::CTX_MODE`]: mode bits such as
    /// [`abi::CTX_MODE_CONST_EVAL`].
    pub mode: AtomicU32,
    /// At [`abi::CTX_UNWIND`]: the unwind flag, one of
    /// [`abi::unwind_flag`]'s values (D-685). Written only by the owning
    /// fiber; [`abi::unwind_flag::NONE`] while nothing unwinds.
    pub unwind: AtomicU32,
    /// At [`abi::CTX_WAIT_FRAME`]: the frame pointer of the std function
    /// that last called a waiting kernel or `scope_close` (D-856). Written
    /// only by the owning fiber's generated code, right before the call,
    /// and read by that fiber's kernels ([`ExecCtx::wait_frame`]); 0 until
    /// the first such call.
    wait_frame: AtomicUsize,
    // The Rust part, from byte 24: generated code never reads it.
    /// Why an interrupt was requested: [`reason`] bits (D-688).
    reasons: AtomicU32,
    /// Whether the fiber's code was compiled with unwind edges (D-742).
    unwind_edges: bool,
    /// The stack limit when no interrupt is pending; only lowered, by the
    /// fiber itself (module documentation).
    soft_limit: AtomicUsize,
    /// The lowest the soft limit goes: below it the stack overflows.
    hard_limit: usize,
    /// The stack position where the entry started.
    stack_top: usize,
    /// Interrupts the slow path has handled.
    interrupts: AtomicU64,
    /// The hash seed new maps and sets take (D-538); changed by
    /// [`crate::entry::with_seed`] while a constant is forced.
    seed: AtomicU64,
    /// Constant evaluation's memory budget (D-536, D-617): the most live
    /// bytes the domain may hold, or `u64::MAX` for none.
    budget: AtomicU64,
    /// The bytes of scratch capacity the fiber holds
    /// ([`crate::alloc::scratch`]).
    scratch_live: AtomicU64,
    /// The most bytes of scratch the fiber held since the last reset.
    scratch_peak: AtomicU64,
    /// The shard of the domain's counters this fiber charges: its
    /// thread's, set whenever it becomes the thread's current fiber
    /// (D-691).
    shard: AtomicUsize,
    /// The block cache of the thread the fiber runs on, set with the shard
    /// whenever it becomes the thread's current fiber and cleared when it
    /// stops being current (`tls::set_current_fiber`); null when it is
    /// current nowhere or the thread's cache is gone. The allocator reads
    /// it instead of the thread-local, which saves a `tls.rs` accessor
    /// call on the paths that know their fiber (GATE1-1, D-686).
    block_cache: AtomicPtr<RefCell<BlockCache>>,
    /// The accounting domain the fiber charges (D-564, D-691).
    domain: Arc<Domain>,
    /// The build's constants during constant evaluation (D-535).
    consts: Option<Arc<ConstTable>>,
    /// The trampolines kernels call closures through (D-526).
    callbacks: Callbacks,
    entry_mode: EntryMode,
    /// Where the outcome goes: the host waiting in `run_entry` or
    /// `Runtime::run_root`; `None` for a fiber spawned by another.
    outcome: Option<Sender<EntryOutcome>>,
    /// The scheduler's fields (D-689).
    pub(crate) sched: FiberSched,
    /// Structured concurrency: the fiber's scopes, and the fiber lock over
    /// its cancellation target and wait node (D-706). After the
    /// generated-code view, like every field added since: no `abi::CTX_*`
    /// offset depends on it.
    pub(crate) conc: FiberConc,
    /// The unwinding in flight (plan §2.4).
    pub(crate) unwinding: UnwindState,
}

const _: () = assert!(offset_of!(ExecCtx, stack_limit) == 0 && abi::CTX_STACK_LIMIT == 0);
const _: () = assert!(offset_of!(ExecCtx, mode) == 8 && abi::CTX_MODE == 8);
const _: () = assert!(offset_of!(ExecCtx, unwind) == 12 && abi::CTX_UNWIND == 12);
const _: () = assert!(offset_of!(ExecCtx, wait_frame) == 16 && abi::CTX_WAIT_FRAME == 16);
// The generated-code view ends at 24: the Rust part starts there.
const _: () = assert!(offset_of!(ExecCtx, reasons) == 24);

/// What a new fiber control block starts with.
#[derive(Debug)]
pub(crate) struct FiberInit {
    /// The first stack limit when no interrupt is pending.
    pub(crate) soft_limit: usize,
    /// The lowest stack limit, at or below `soft_limit`.
    pub(crate) hard_limit: usize,
    /// The stack position where the entry starts.
    pub(crate) stack_top: usize,
    /// What kind of entry runs.
    pub(crate) entry_mode: EntryMode,
    /// Where a panic reports to; without one, a panic ends the process.
    pub(crate) outcome: Option<Sender<EntryOutcome>>,
    /// The hash seed.
    pub(crate) seed: u64,
    /// The memory budget, `u64::MAX` for none.
    pub(crate) budget: u64,
    /// The accounting domain.
    pub(crate) domain: Arc<Domain>,
    /// The build's constants.
    pub(crate) consts: Option<Arc<ConstTable>>,
    /// The trampolines.
    pub(crate) callbacks: Callbacks,
    /// Whether its code was compiled with unwind edges: false only under
    /// `CHEBY_UNWIND_EDGES=off` (D-742), where a raise ends the process
    /// (D-770).
    pub(crate) unwind_edges: bool,
    /// The scheduler's fields: [`FiberSched::isolated`] outside a runtime.
    pub(crate) sched: FiberSched,
}

impl FiberInit {
    /// An isolated entry's fields with no stack, no outcome, the process
    /// seed, no budget, a fresh domain, no constants or trampolines, the
    /// return flag (D-733) and unwind edges, to be completed with
    /// struct-update syntax.
    pub(crate) fn placeholder() -> Self {
        Self {
            soft_limit: 0,
            hard_limit: 0,
            stack_top: 0,
            entry_mode: EntryMode::Run,
            outcome: None,
            seed: 0,
            budget: u64::MAX,
            domain: Domain::new(),
            consts: None,
            callbacks: Callbacks::default(),
            unwind_edges: true,
            sched: FiberSched::isolated(),
        }
    }
}

impl ExecCtx {
    /// A fiber control block with nothing pending.
    pub(crate) fn new(init: FiberInit) -> Self {
        let mode = match init.entry_mode {
            EntryMode::ConstEval(_) => abi::CTX_MODE_CONST_EVAL,
            EntryMode::Run | EntryMode::Test | EntryMode::Repl => 0,
        };
        Self {
            stack_limit: AtomicUsize::new(init.soft_limit),
            mode: AtomicU32::new(mode),
            unwind: AtomicU32::new(abi::unwind_flag::NONE),
            wait_frame: AtomicUsize::new(0),
            reasons: AtomicU32::new(0),
            unwind_edges: init.unwind_edges,
            soft_limit: AtomicUsize::new(init.soft_limit),
            hard_limit: init.hard_limit.min(init.soft_limit),
            stack_top: init.stack_top,
            interrupts: AtomicU64::new(0),
            seed: AtomicU64::new(init.seed),
            budget: AtomicU64::new(init.budget),
            scratch_live: AtomicU64::new(0),
            scratch_peak: AtomicU64::new(0),
            shard: AtomicUsize::new(0),
            block_cache: AtomicPtr::new(ptr::null_mut()),
            domain: init.domain,
            consts: init.consts,
            callbacks: init.callbacks,
            entry_mode: init.entry_mode,
            outcome: init.outcome,
            sched: init.sched,
            conc: FiberConc::default(),
            unwinding: UnwindState::default(),
        }
    }

    /// Asks the running code to enter the runtime at its next function
    /// entry with the reason [`reason::YIELD`]. Callable from any thread.
    pub fn request_interrupt(&self) {
        request(self, reason::YIELD);
    }

    /// The number of interrupts the slow path has handled.
    #[must_use]
    pub fn interrupts(&self) -> u64 {
        self.interrupts.load(Ordering::Relaxed)
    }

    /// Counts an interrupt the slow path handled.
    pub(crate) fn count_interrupt(&self) {
        self.interrupts.fetch_add(1, Ordering::Relaxed);
    }

    /// The stack limit when no interrupt is pending: the soft limit, the
    /// lowest one so far for a fiber (module documentation).
    #[must_use]
    pub fn real_stack_limit(&self) -> usize {
        self.soft_limit.load(Ordering::Relaxed)
    }

    /// The lowest stack limit: a frame below it overflows the stack.
    #[must_use]
    pub(crate) fn hard_limit(&self) -> usize {
        self.hard_limit
    }

    /// The frame pointer the fiber's generated code stored at
    /// [`abi::CTX_WAIT_FRAME`] right before its last call of a waiting
    /// kernel or `scope_close` (D-856): the frame record of the std function
    /// that made the call, where a deadlock report's walk starts
    /// (`locate::locate`). 0 before the first such call. Read by the
    /// fiber's own kernels, before they park.
    #[must_use]
    pub fn wait_frame(&self) -> usize {
        self.wait_frame.load(Ordering::Relaxed)
    }

    /// Stores `frame` as generated code does, for tests that have no JIT.
    #[cfg(test)]
    pub(crate) fn set_wait_frame(&self, frame: usize) {
        self.wait_frame.store(frame, Ordering::Relaxed);
    }

    /// The stack's lowest usable address and its top: the hard limit and
    /// the position where the entry started (`hard_limit`, `stack_top`).
    /// Every frame record of the fiber's Cheby code lies between them, so a
    /// frame walk checks each frame pointer against them (D-856,
    /// `locate::locate`). Both are fixed when the block is made.
    #[must_use]
    pub(crate) fn stack_bounds(&self) -> (usize, usize) {
        (self.hard_limit, self.stack_top)
    }

    /// Lowers the soft limit so that `low` lies above it: from its distance
    /// to the top in doubling steps, down to the hard limit at most (D-677).
    /// Called by the fiber itself in the slow path, after it took the
    /// pending reasons; a request made since keeps its forced limit.
    pub(crate) fn lower_soft_limit(&self, low: usize) {
        let old = self.soft_limit.load(Ordering::Relaxed);
        let mut limit = old;
        while limit > low && limit > self.hard_limit {
            let distance = self.stack_top.saturating_sub(limit).max(1);
            limit = self
                .stack_top
                .saturating_sub(distance.saturating_mul(2))
                .max(self.hard_limit);
        }
        if limit == old {
            return;
        }
        self.soft_limit.store(limit, Ordering::SeqCst);
        // A forced limit (`usize::MAX`) stays: the next check takes the
        // slow path, whose `take` stores the new soft limit.
        let _ = self
            .stack_limit
            .compare_exchange(old, limit, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// What kind of entry is running.
    #[must_use]
    pub fn entry_mode(&self) -> &EntryMode {
        &self.entry_mode
    }

    /// Whether the fiber's code was compiled with unwind edges: false only
    /// under `CHEBY_UNWIND_EDGES=off`, D-742's measurement switch, where a
    /// raise ends the process instead of unwinding (D-770).
    #[must_use]
    pub fn unwind_edges(&self) -> bool {
        self.unwind_edges
    }

    /// Where a panic reports to.
    pub(crate) fn outcome(&self) -> Option<&Sender<EntryOutcome>> {
        self.outcome.as_ref()
    }

    /// The hash seed new maps and sets take.
    pub(crate) fn seed(&self) -> u64 {
        self.seed.load(Ordering::Relaxed)
    }

    /// Replaces the hash seed and returns the previous one.
    pub(crate) fn replace_seed(&self, seed: u64) -> u64 {
        self.seed.swap(seed, Ordering::Relaxed)
    }

    /// The trampolines kernels call closures through.
    pub(crate) fn callbacks(&self) -> Callbacks {
        self.callbacks
    }

    /// The build's constants during constant evaluation.
    pub(crate) fn consts(&self) -> Option<&ConstTable> {
        self.consts.as_deref()
    }

    /// The accounting domain the fiber charges.
    pub(crate) fn domain(&self) -> &Arc<Domain> {
        &self.domain
    }

    /// The shard of the domain's counters the fiber charges.
    pub(crate) fn shard(&self) -> usize {
        self.shard.load(Ordering::Relaxed)
    }

    /// Makes the fiber charge `shard`: its thread's.
    pub(crate) fn set_shard(&self, shard: usize) {
        self.shard.store(shard, Ordering::Relaxed);
    }

    /// The block cache of the thread the fiber is current on, or null (see
    /// the field). Valid only while the fiber is this thread's current
    /// fiber, and only for code that does not suspend (D-686).
    pub(crate) fn block_cache(&self) -> *const RefCell<BlockCache> {
        self.block_cache.load(Ordering::Relaxed)
    }

    /// Records the block cache of the thread the fiber becomes current on,
    /// or null when it stops being current (`tls::set_current_fiber`).
    pub(crate) fn set_block_cache(&self, cache: *const RefCell<BlockCache>) {
        self.block_cache.store(cache.cast_mut(), Ordering::Relaxed);
    }

    /// The memory budget, `u64::MAX` for none.
    pub(crate) fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Replaces the memory budget.
    pub(crate) fn set_budget(&self, budget: u64) {
        self.budget.store(budget, Ordering::Relaxed);
    }

    /// The fiber's scratch counters: live bytes and their peak.
    pub(crate) fn scratch(&self) -> (&AtomicU64, &AtomicU64) {
        (&self.scratch_live, &self.scratch_peak)
    }
}

/// Requests an interrupt of the fiber `ctx` for `reason` (D-688): adds the
/// reason, then forces the next function entry into the slow path.
/// Callable from any thread.
pub fn request(ctx: &ExecCtx, reason: u32) {
    ctx.reasons.fetch_or(reason, Ordering::SeqCst);
    ctx.stack_limit.store(usize::MAX, Ordering::SeqCst);
}

/// Takes every pending reason of the fiber `ctx` (D-688): restores the
/// soft stack limit, then empties the reason word and returns what it
/// held. Called by the fiber itself, in the slow path.
pub(crate) fn take(ctx: &ExecCtx) -> u32 {
    ctx.stack_limit
        .store(ctx.soft_limit.load(Ordering::Relaxed), Ordering::SeqCst);
    ctx.reasons.swap(0, Ordering::SeqCst)
}

/// Clears the pending `reason` bit of the fiber `ctx` without taking the
/// others (D-688): a `fetch_and` of its complement. A scope's close calls
/// it for [`reason::CANCEL`] once it has cleared its own cancellation
/// target (D-706, architecture §7.5 step 3), so that a request the walk
/// made is not delivered again after the cancellation it stood for has
/// ended. The forced stack limit, if any, stays: the next entry check then
/// takes the slow path once and finds nothing to do.
pub(crate) fn clear_reason(ctx: &ExecCtx, reason: u32) {
    ctx.reasons.fetch_and(!reason, Ordering::SeqCst);
}

/// Whether the reason bit `reason` is pending for the fiber `ctx`, without
/// taking it.
#[cfg(test)]
pub(crate) fn pending(ctx: &ExecCtx, reason: u32) -> bool {
    ctx.reasons.load(Ordering::SeqCst) & reason != 0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::*;

    fn fcb() -> ExecCtx {
        ExecCtx::new(FiberInit {
            soft_limit: 0x1000,
            hard_limit: 0x1000,
            stack_top: 0x10_0000,
            entry_mode: EntryMode::Test,
            seed: 7,
            ..FiberInit::placeholder()
        })
    }

    /// D-677: the soft limit goes down in doubling steps from the top, to
    /// the hard limit at most, and a forced limit stays forced.
    #[test]
    fn the_soft_limit_goes_down_in_doubling_steps() {
        let ctx = ExecCtx::new(FiberInit {
            soft_limit: 0x10_0000 - 0x1000,
            hard_limit: 0x8_0000,
            stack_top: 0x10_0000,
            ..FiberInit::placeholder()
        });
        ctx.lower_soft_limit(0x10_0000 - 0x1800);
        assert_eq!(ctx.real_stack_limit(), 0x10_0000 - 0x2000);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), 0x10_0000 - 0x2000);
        // Several steps at once.
        ctx.lower_soft_limit(0x10_0000 - 0x7000);
        assert_eq!(ctx.real_stack_limit(), 0x10_0000 - 0x8000);
        // Never below the hard limit.
        ctx.lower_soft_limit(0x100);
        assert_eq!(ctx.real_stack_limit(), 0x8_0000);
        assert_eq!(ctx.hard_limit(), 0x8_0000);
        // A request in flight keeps its forced limit.
        let ctx = ExecCtx::new(FiberInit {
            soft_limit: 0x10_0000 - 0x1000,
            hard_limit: 0x8_0000,
            stack_top: 0x10_0000,
            ..FiberInit::placeholder()
        });
        request(&ctx, reason::YIELD);
        ctx.lower_soft_limit(0x10_0000 - 0x1800);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), usize::MAX);
        assert_eq!(take(&ctx), reason::YIELD);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), 0x10_0000 - 0x2000);
    }

    #[test]
    fn the_abi_part_has_its_fixed_offsets() {
        assert_eq!(offset_of!(ExecCtx, stack_limit), 0);
        assert_eq!(offset_of!(ExecCtx, mode), 8);
        assert_eq!(offset_of!(ExecCtx, unwind), 12);
        assert_eq!(offset_of!(ExecCtx, wait_frame), 16);
        assert_eq!(offset_of!(ExecCtx, reasons), 24);
        let ctx = fcb();
        assert_eq!(ctx.unwind.load(Ordering::Relaxed), abi::unwind_flag::NONE);
        assert!(ctx.unwind_edges());
    }

    /// D-856: the wait-frame word starts at 0 and reads what generated code
    /// stored; the stack bounds are the hard limit and the top.
    #[test]
    fn the_wait_frame_and_the_stack_bounds() {
        let ctx = fcb();
        assert_eq!(ctx.wait_frame(), 0);
        ctx.set_wait_frame(0x8_0040);
        assert_eq!(ctx.wait_frame(), 0x8_0040);
        assert_eq!(ctx.stack_bounds(), (0x1000, 0x10_0000));
        let ctx = ExecCtx::new(FiberInit {
            soft_limit: 0x10_0000 - 0x1000,
            hard_limit: 0x8_0000,
            stack_top: 0x10_0000,
            ..FiberInit::placeholder()
        });
        assert_eq!(ctx.stack_bounds(), (0x8_0000, 0x10_0000));
    }

    #[test]
    fn requests_force_the_slow_path_and_take_restores_the_limit() {
        let ctx = fcb();
        assert_eq!(take(&ctx), 0);
        request(&ctx, reason::DEADLINE);
        request(&ctx, reason::YIELD);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), usize::MAX);
        assert_eq!(take(&ctx), reason::DEADLINE | reason::YIELD);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), 0x1000);
        assert_eq!(take(&ctx), 0);
        ctx.request_interrupt();
        assert_eq!(take(&ctx), reason::YIELD);
    }

    /// D-688: one thread requests, one after another, 100,000 interrupts
    /// with rotating reasons; the other runs entry checks and takes what
    /// they find, and also takes at random moments when no check fails,
    /// which races with the requester's two stores. Every request must be
    /// seen: a lost one would leave the requester waiting forever.
    #[test]
    fn no_request_is_lost_between_two_threads() {
        const ROUNDS: u64 = 100_000;
        let ctx = Arc::new(fcb());
        let done = Arc::new(AtomicBool::new(false));
        let taken = Arc::new(AtomicU64::new(0));
        let handler = {
            let (ctx, done, taken) = (Arc::clone(&ctx), Arc::clone(&done), Arc::clone(&taken));
            std::thread::spawn(move || {
                let mut seen = [0_u64; 4];
                let mut tick = 0_u64;
                while !done.load(Ordering::SeqCst) {
                    tick = tick.wrapping_add(1);
                    // The entry check, and now and then a spurious take.
                    let forced = ctx.stack_limit.load(Ordering::SeqCst) == usize::MAX;
                    if forced || tick.is_multiple_of(7) {
                        let bits = take(&ctx);
                        for (bit, count) in seen.iter_mut().enumerate() {
                            if bits & (1 << bit) != 0 {
                                *count = count.wrapping_add(1);
                                taken.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    std::hint::spin_loop();
                }
                seen
            })
        };
        let reasons = [
            reason::YIELD,
            reason::CANCEL,
            reason::DEADLINE,
            reason::SAFEPOINT,
        ];
        // GAP(runtime-test-hang-scale): the hang bound scales.
        let deadline = Instant::now() + crate::sched::testing::scaled(Duration::from_secs(120));
        for round in 0..ROUNDS {
            let reason = reasons[usize::try_from(round % 4).unwrap()];
            request(&ctx, reason);
            while taken.load(Ordering::SeqCst) <= round {
                assert!(
                    Instant::now() < deadline,
                    "request {round} was lost: limit {:#x}, reasons {:#x}",
                    ctx.stack_limit.load(Ordering::SeqCst),
                    ctx.reasons.load(Ordering::SeqCst)
                );
                std::hint::spin_loop();
            }
        }
        done.store(true, Ordering::SeqCst);
        let seen = handler.join().unwrap();
        assert_eq!(seen, [ROUNDS / 4; 4]);
        assert_eq!(taken.load(Ordering::SeqCst), ROUNDS);
        // Nothing is left pending. The last request's store of the forcing
        // limit may follow a spurious take of its reason, so take once more
        // before reading the limit.
        assert_eq!(take(&ctx), 0);
        assert_eq!(ctx.stack_limit.load(Ordering::SeqCst), 0x1000);
    }
}
