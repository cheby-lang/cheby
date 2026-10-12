//! Raising a Cheby panic from runtime code (D-697, D-699).
//!
//! What a raise does depends on the backend the running fiber's code was
//! compiled for ([`UnwindBackend`], D-696):
//!
//! - `Off` (the default): the slice's interim. The panic is reported and
//!   the entry or fiber is abandoned, as in M1 ([`crate::panic::abandon`],
//!   GAP(interim-panics)).
//! - `Flag` and `TryCall`: the panic is recorded in the fiber's unwind
//!   record when it starts (D-699), then the Rust frames between the raise
//!   and the entry point's guard unwind with Rust's own unwinder, carrying
//!   the zero-sized [`ChebyUnwind`] marker and running every `Drop` (I-4).
//!   The guard hands the unwinding to the backend ([`super::guard`]).
//!
//! Generated code's own raises (`cheby_rt_panic`, a stack overflow found
//! by the slow paths) have no Rust frame to unwind:
//! [`raise_from_runtime`] hands them to the backend directly.

use std::sync::atomic::Ordering;

use crate::abi::{PanicKind, UnwindBackend};
use crate::ctx::ExecCtx;
use crate::panic::{PanicReport, UNKNOWN_LOCATION, abandon, fatal};

use super::record;

/// The payload of a Cheby unwinding inside Rust (D-697). Zero-sized: the
/// cause is in the fiber's record, so the unwinding allocates nothing for
/// it, and `resume_unwind` runs no panic hook and prints nothing.
#[derive(Debug)]
pub(crate) struct ChebyUnwind;

/// Starts a Cheby panic from runtime code (D-697, D-699). Under
/// `UnwindBackend::Off` it is the slice's interim (S5): the panic is
/// reported and the fiber is abandoned, as in M1.
///
/// `ctx` is the running fiber's control block, or `None` (or null) for
/// code without one: the current fiber is used then. The location is
/// unknown. Outside any entry it ends the process.
pub(crate) fn raise_panic(ctx: Option<*mut ExecCtx>, kind: PanicKind, message: Option<&str>) -> ! {
    raise(ctx, PanicReport::new(kind, message, UNKNOWN_LOCATION))
}

/// [`raise_panic`] with a report of its own, for helpers that know the
/// location of the operation that panics.
pub(crate) fn raise(ctx: Option<*mut ExecCtx>, report: PanicReport) -> ! {
    let ctx = running(ctx).unwrap_or_else(|| {
        fatal(format_args!(
            "a `{:?}` panic happened outside of any entry",
            report.kind
        ))
    });
    // SAFETY: `running` returns the running fiber's control block, alive
    // while it runs; only shared references to it are made.
    let fiber = unsafe { &*ctx };
    match fiber.unwind_backend() {
        // SAFETY: `ctx` is the running fiber's.
        UnwindBackend::Off => unsafe { abandon(ctx, report) },
        UnwindBackend::Flag | UnwindBackend::TryCall => {
            record::start_panic(fiber, report);
            std::panic::resume_unwind(Box::new(ChebyUnwind))
        }
    }
}

/// Hands a panic raised by generated code's own call into the runtime
/// (`cheby_rt_panic`, the stack slow paths) to the backend, from the
/// outermost Rust frame, so no Rust unwinding runs (D-700): `Off` abandons
/// the entry and never returns; `Flag` records the report, sets the flag
/// and returns, and generated code runs its cleanup pad; `TryCall` records
/// the report and resumes at the nearest handler.
///
/// # Safety
///
/// `ctx` is null (outside any entry, which ends the process) or the
/// running fiber's control block, and the caller is the outermost Rust
/// frame below generated code: nothing above it needs dropping.
pub(crate) unsafe fn raise_from_runtime(ctx: *mut ExecCtx, report: PanicReport) {
    // SAFETY: forwarded from the caller.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        // SAFETY: null is allowed: it ends the process.
        unsafe { abandon(ctx, report) }
    };
    match fiber.unwind_backend() {
        // SAFETY: forwarded from the caller.
        UnwindBackend::Off => unsafe { abandon(ctx, report) },
        UnwindBackend::Flag => {
            record::start_panic(fiber, report);
            super::guard::set_flag(fiber);
        }
        UnwindBackend::TryCall => {
            record::start_panic(fiber, report);
            super::guard::set_flag(fiber);
            // SAFETY: `ctx` is the running fiber's, with the unwinding just
            // recorded; generated code called the caller through an exit
            // stub, which recorded the exit record; no value with a
            // destructor is alive in this frame or the caller's.
            unsafe { super::try_call::throw_from_runtime(ctx) }
        }
    }
}

/// The running fiber's control block: `ctx` when it is given and not
/// null, else the current fiber's, or `None` outside any entry.
pub(crate) fn running(ctx: Option<*mut ExecCtx>) -> Option<*mut ExecCtx> {
    ctx.filter(|ctx| !ctx.is_null())
        .or_else(crate::entry::current_ctx)
}

/// The `try_call` prototype's exit record (FCB offsets 16 and 24,
/// GAP(try-call-exit-record)), saved where Rust code calls generated code
/// and may raise afterwards: the callee's own exit stubs overwrite the
/// record, and a throw after the call must start from the caller's
/// (plan §4.2). Harmless under the other backends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SavedExit {
    fp: usize,
    pc: usize,
}

impl SavedExit {
    /// Saves `fiber`'s exit record.
    pub(crate) fn save(fiber: &ExecCtx) -> Self {
        Self {
            fp: fiber.exit_fp.load(Ordering::Relaxed),
            pc: fiber.exit_pc.load(Ordering::Relaxed),
        }
    }

    /// Puts the saved exit record back.
    pub(crate) fn restore(self, fiber: &ExecCtx) {
        fiber.exit_fp.store(self.fp, Ordering::Relaxed);
        fiber.exit_pc.store(self.pc, Ordering::Relaxed);
    }
}

/// Rethrows the unwinding in flight in `fiber` through the Rust frames
/// above, after a call into generated code (a trampoline, a forced
/// constant's thunk) returned while unwinding: clears the flag, which the
/// guard sets again, and resumes the marker. Returns when nothing is
/// unwinding.
pub(crate) fn rethrow_if_unwinding(fiber: &ExecCtx) {
    if record::unwinding(fiber) {
        super::flag::clear(fiber);
        std::panic::resume_unwind(Box::new(ChebyUnwind));
    }
}
