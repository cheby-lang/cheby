//! The fiber control block (FCB, D-685): `ExecCtx`, the hidden first
//! argument of every Cheby-ABI function (D-453). One per running entry
//! today; one per fiber once the scheduler runs fibers.
//!
//! # The generated-code view
//!
//! Generated code reads only the fields with an `abi::CTX_*` offset, which
//! are fixed (D-685): the stack limit at 0, the mode at 8, the unwind flag
//! at 12 (reserved whichever unwinding backend is chosen, D-696) and, for
//! the `try_call` prototype only, the exit record at 16 and 24. The
//! function-entry check compares the stack pointer with `stack_limit` and
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

use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use crate::abi::{self, UnwindBackend};
use crate::alloc::Domain;
use crate::consteval::ConstTable;
use crate::entry::{Callbacks, EntryMode, EntryOutcome};

/// The reasons for an interrupt (D-688), bits of the FCB's reason word.
pub mod reason {
    /// The scheduler asks the fiber to yield (D-028). Accepted and ignored
    /// until the scheduler exists.
    pub const YIELD: u32 = 1;
    /// The fiber's scope is cancelled (D-688). Accepted and ignored until
    /// cancellation exists.
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
    /// fiber; [`abi::unwind_flag::NONE`] with the default backend.
    pub unwind: AtomicU32,
    /// At [`abi::CTX_EXIT_FP`]: the `try_call` prototype's exit record, the
    /// frame pointer of the generated code that called out to C.
    /// GAP(try-call-exit-record).
    pub exit_fp: AtomicUsize,
    /// At [`abi::CTX_EXIT_PC`]: the return address of that call.
    /// GAP(try-call-exit-record).
    pub exit_pc: AtomicUsize,
    // The Rust part: generated code never reads it.
    /// Why an interrupt was requested: [`reason`] bits (D-688).
    reasons: AtomicU32,
    /// The unwinding backend the fiber's code was compiled for (D-696).
    unwind_backend: UnwindBackend,
    /// The stack limit when no interrupt is pending.
    soft_limit: usize,
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
    /// The accounting domain the fiber charges (D-564, D-691).
    domain: Arc<Domain>,
    /// The build's constants during constant evaluation (D-535).
    consts: Option<Arc<ConstTable>>,
    /// The trampolines kernels call closures through (D-526).
    callbacks: Callbacks,
    entry_mode: EntryMode,
    /// Where the outcome goes: the host waiting in `run_entry`.
    outcome: Option<Sender<EntryOutcome>>,
}

const _: () = assert!(offset_of!(ExecCtx, stack_limit) == 0 && abi::CTX_STACK_LIMIT == 0);
const _: () = assert!(offset_of!(ExecCtx, mode) == 8 && abi::CTX_MODE == 8);
const _: () = assert!(offset_of!(ExecCtx, unwind) == 12 && abi::CTX_UNWIND == 12);
const _: () = assert!(offset_of!(ExecCtx, exit_fp) == 16 && abi::CTX_EXIT_FP == 16);
const _: () = assert!(offset_of!(ExecCtx, exit_pc) == 24 && abi::CTX_EXIT_PC == 24);

/// What a new fiber control block starts with.
#[derive(Debug)]
pub(crate) struct FiberInit {
    /// The stack limit when no interrupt is pending.
    pub(crate) soft_limit: usize,
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
            exit_fp: AtomicUsize::new(0),
            exit_pc: AtomicUsize::new(0),
            reasons: AtomicU32::new(0),
            unwind_backend: UnwindBackend::Off,
            soft_limit: init.soft_limit,
            stack_top: init.stack_top,
            interrupts: AtomicU64::new(0),
            seed: AtomicU64::new(init.seed),
            budget: AtomicU64::new(init.budget),
            scratch_live: AtomicU64::new(0),
            scratch_peak: AtomicU64::new(0),
            shard: AtomicUsize::new(0),
            domain: init.domain,
            consts: init.consts,
            callbacks: init.callbacks,
            entry_mode: init.entry_mode,
            outcome: init.outcome,
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

    /// The stack limit when no interrupt is pending.
    #[must_use]
    pub fn real_stack_limit(&self) -> usize {
        self.soft_limit
    }

    /// The stack position where the entry started.
    #[must_use]
    pub(crate) fn stack_top(&self) -> usize {
        self.stack_top
    }

    /// What kind of entry is running.
    #[must_use]
    pub fn entry_mode(&self) -> &EntryMode {
        &self.entry_mode
    }

    /// The unwinding backend the fiber's code was compiled for (D-696):
    /// [`UnwindBackend::Off`] until the hosts select one.
    #[must_use]
    pub fn unwind_backend(&self) -> UnwindBackend {
        self.unwind_backend
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
    ctx.stack_limit.store(ctx.soft_limit, Ordering::SeqCst);
    ctx.reasons.swap(0, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::*;

    fn fcb() -> ExecCtx {
        ExecCtx::new(FiberInit {
            soft_limit: 0x1000,
            stack_top: 0x10_0000,
            entry_mode: EntryMode::Test,
            outcome: None,
            seed: 7,
            budget: u64::MAX,
            domain: Domain::new(),
            consts: None,
            callbacks: Callbacks::default(),
        })
    }

    #[test]
    fn the_abi_part_has_its_fixed_offsets() {
        assert_eq!(offset_of!(ExecCtx, stack_limit), 0);
        assert_eq!(offset_of!(ExecCtx, mode), 8);
        assert_eq!(offset_of!(ExecCtx, unwind), 12);
        assert_eq!(offset_of!(ExecCtx, exit_fp), 16);
        assert_eq!(offset_of!(ExecCtx, exit_pc), 24);
        let ctx = fcb();
        assert_eq!(ctx.unwind.load(Ordering::Relaxed), abi::unwind_flag::NONE);
        assert_eq!(ctx.unwind_backend(), UnwindBackend::Off);
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
        let deadline = Instant::now() + Duration::from_secs(120);
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
