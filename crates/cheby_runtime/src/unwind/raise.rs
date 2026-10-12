//! Raising a Cheby panic from runtime code (D-697, D-699, D-733).
//!
//! Every raise records the panic in the fiber's unwind record when it
//! starts (D-699) and unwinds by the return flag (D-733, ADR-0061):
//!
//! - A raise inside Rust code ([`raise`], [`raise_panic`]) unwinds the
//!   Rust frames between it and the entry point's guard with Rust's own
//!   unwinder, carrying the zero-sized [`ChebyUnwind`] marker and running
//!   every `Drop` (I-4). The guard sets the flag and returns its sentinel
//!   ([`super::guard`]).
//! - Generated code's own raises (`cheby_rt_panic`, a stack overflow found
//!   by the slow paths) have no Rust frame to unwind:
//!   [`raise_from_runtime`] records the panic, sets the flag and returns
//!   (D-700).
//!
//! Code compiled without unwind edges (`CHEBY_UNWIND_EDGES=off`, D-742's
//! measurement switch) cannot unwind: a raise in such a fiber flushes
//! standard output, prints the report and ends the process
//! ([`edgeless`], GAP(edgeless-panics)).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::abi::PanicKind;
use crate::ctx::ExecCtx;
use crate::io;
use crate::panic::{PanicReport, UNKNOWN_LOCATION, fatal};

use super::record;

/// The payload of a Cheby unwinding inside Rust (D-697). Zero-sized: the
/// cause is in the fiber's record, so the unwinding allocates nothing for
/// it, and `resume_unwind` runs no panic hook and prints nothing.
#[derive(Debug)]
pub(crate) struct ChebyUnwind;

/// The last words of a process whose fiber raised in code compiled without
/// unwind edges (GAP(edgeless-panics)).
pub(crate) const EDGELESS_FATAL: &str =
    "an unwinding in code compiled without unwind edges (CHEBY_UNWIND_EDGES=off)";

/// Starts a Cheby panic from runtime code (D-697, D-699): records it in
/// the running fiber and unwinds the Rust frames above to the entry
/// point's guard or the host's catch point (D-733).
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
    if !fiber.unwind_edges() {
        edgeless(Some(&report));
    }
    record::start_panic(fiber, report);
    std::panic::resume_unwind(Box::new(ChebyUnwind))
}

/// Hands a panic raised by generated code's own call into the runtime
/// (`cheby_rt_panic`, the stack slow paths) to the return flag, from the
/// outermost Rust frame, so no Rust unwinding runs (D-700): records the
/// report, sets the flag and returns, and generated code runs its cleanup
/// pad (D-733).
///
/// # Safety
///
/// `ctx` is null (outside any entry, which ends the process) or the
/// running fiber's control block, and the caller is the outermost Rust
/// frame below generated code: nothing above it needs dropping.
pub(crate) unsafe fn raise_from_runtime(ctx: *mut ExecCtx, report: PanicReport) {
    // SAFETY: forwarded from the caller.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a `{:?}` panic happened outside of any entry",
            report.kind
        ))
    };
    if !fiber.unwind_edges() {
        edgeless(Some(&report));
    }
    record::start_panic(fiber, report);
    super::guard::set_flag(fiber);
}

/// Ends the process for a raise in code compiled without unwind edges
/// (GAP(edgeless-panics), refining D-742 and D-733): such code does not
/// test the flag, so returning into it would go on as if nothing had
/// happened, or trap without a report. It flushes standard output, prints
/// the panic's `report` to standard error, whatever the entry's mode (a
/// cancellation has none), prints `fatal runtime error:` and
/// [`EDGELESS_FATAL`], and exits with the panic status 101 (D-182), not by
/// a signal: the tools that run such a build see an ordinary failure.
/// `CHEBY_UNWIND_EDGES=off` is a measurement switch, so the rule is only
/// that such a build never runs on silently.
///
/// The process ends once: when fibers on several workers raise at the same
/// time, only the first caller prints and exits, and every later one parks
/// its thread for good without printing, so the output is one report and
/// one fatal line at every thread count (plan §0 'Determinism'), and
/// `exit` is never called from two threads at once.
#[cold]
#[inline(never)]
#[expect(
    clippy::exit,
    reason = "GAP(edgeless-panics): the process ends with the panic status, not a signal"
)]
pub(crate) fn edgeless(report: Option<&PanicReport>) -> ! {
    /// Set by the first caller, which ends the process.
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        // Another thread is ending the process: this one waits for it.
        loop {
            std::thread::park();
        }
    }
    // The process ends below whether the flush worked.
    let _ = io::flush_stdout();
    if let Some(report) = report {
        io::write_stderr(&report.to_string());
    }
    io::write_stderr(&format!("fatal runtime error: {EDGELESS_FATAL}\n"));
    std::process::exit(crate::abi::status::PANIC)
}

/// The running fiber's control block: `ctx` when it is given and not
/// null, else the current fiber's, or `None` outside any entry.
pub(crate) fn running(ctx: Option<*mut ExecCtx>) -> Option<*mut ExecCtx> {
    ctx.filter(|ctx| !ctx.is_null())
        .or_else(crate::entry::current_ctx)
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
