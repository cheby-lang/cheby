//! The execution context (D-453): the hidden first argument of every
//! Cheby-ABI function. One per running entry in M1; the fiber control block
//! in M2.
//!
//! Generated code reads only the fields with an `abi::CTX_*` offset. The
//! function-entry check compares the stack pointer with `stack_limit` and
//! calls `cheby_rt_stack_slow` when it is below. Setting `stack_limit` to
//! `usize::MAX` forces the next function entry into that slow path, which
//! is how the D-195 deadline (E6) and the D-028 yield (M2) interrupt
//! running code with the same single compare (D-124).

use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use crate::abi;
use crate::entry::{EntryMode, EntryOutcome};

/// The execution context of one running entry.
#[derive(Debug)]
#[repr(C)]
pub struct ExecCtx {
    /// At [`abi::CTX_STACK_LIMIT`]: the lowest stack address a
    /// function may start below without calling the slow path, or
    /// `usize::MAX` to request an interrupt.
    pub stack_limit: AtomicUsize,
    /// At [`abi::CTX_MODE`]: mode bits such as
    /// [`abi::CTX_MODE_CONST_EVAL`].
    pub mode: AtomicU32,
    inner: Box<Inner>,
}

const _: () = assert!(offset_of!(ExecCtx, stack_limit) == 0 && abi::CTX_STACK_LIMIT == 0);
const _: () = assert!(offset_of!(ExecCtx, mode) == 8 && abi::CTX_MODE == 8);

/// The part of the context only the runtime reads.
#[derive(Debug)]
struct Inner {
    /// The stack limit when no interrupt is pending.
    real_limit: usize,
    entry_mode: EntryMode,
    /// Where the outcome goes: the host waiting in `run_entry`.
    outcome: Option<Sender<EntryOutcome>>,
    /// Interrupts the slow path has handled.
    interrupts: AtomicU64,
}

impl ExecCtx {
    /// A context with the given real stack limit. A panic sends its report
    /// to `outcome`; without one, a panic ends the process.
    pub(crate) fn new(
        real_limit: usize,
        entry_mode: EntryMode,
        outcome: Option<Sender<EntryOutcome>>,
    ) -> Self {
        let mode = match entry_mode {
            EntryMode::ConstEval(_) => abi::CTX_MODE_CONST_EVAL,
            EntryMode::Run | EntryMode::Test | EntryMode::Repl => 0,
        };
        Self {
            stack_limit: AtomicUsize::new(real_limit),
            mode: AtomicU32::new(mode),
            inner: Box::new(Inner {
                real_limit,
                entry_mode,
                outcome,
                interrupts: AtomicU64::new(0),
            }),
        }
    }

    /// Asks the running code to enter the runtime at its next function
    /// entry. Callable from any thread.
    pub fn request_interrupt(&self) {
        self.stack_limit.store(usize::MAX, Ordering::Release);
    }

    /// Clears a pending interrupt request and reports whether there was
    /// one.
    pub(crate) fn take_interrupt(&self) -> bool {
        let taken = self
            .stack_limit
            .compare_exchange(
                usize::MAX,
                self.inner.real_limit,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok();
        if taken {
            self.inner.interrupts.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// The number of interrupts the slow path has handled.
    #[must_use]
    pub fn interrupts(&self) -> u64 {
        self.inner.interrupts.load(Ordering::Relaxed)
    }

    /// The stack limit when no interrupt is pending.
    #[must_use]
    pub fn real_stack_limit(&self) -> usize {
        self.inner.real_limit
    }

    /// What kind of entry is running.
    #[must_use]
    pub fn entry_mode(&self) -> &EntryMode {
        &self.inner.entry_mode
    }

    /// Where a panic reports to.
    pub(crate) fn outcome(&self) -> Option<&Sender<EntryOutcome>> {
        self.inner.outcome.as_ref()
    }
}
