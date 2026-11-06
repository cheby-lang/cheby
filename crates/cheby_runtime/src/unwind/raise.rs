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
//!   [`raise_from_runtime_at`] records the panic, sets the flag and returns
//!   (D-700).
//! - A cancellation (D-102, D-688) is raised the same two ways: by a
//!   kernel that would park, or a scope's close passing it through
//!   ([`raise_cancel`]), and by the slow path's step 3
//!   ([`cancel_from_runtime`]). Its record says `Cancel`, which no catch
//!   point reports as a panic (D-178), and the flag's value is the
//!   cancellation's.
//!
//! Code compiled without unwind edges (`CHEBY_UNWIND_EDGES=off`, D-742's
//! measurement switch) cannot unwind: a raise in such a fiber flushes
//! standard output, prints the report and ends the process
//! ([`edgeless`], D-770).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::abi::PanicKind;
use crate::backtrace::{self, Start};
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
/// unwind edges (D-770).
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

/// The raise of a waiting kernel's panic (D-1040's second start): `joined
/// fiber failed` (`fiber_ops/join.rs`) and `spawn into a scope that has
/// ended` (`fiber_ops/spawn.rs`), which read the control block's
/// wait-frame word (D-868) for the backtrace's walk. Otherwise as
/// [`raise_panic`], whose arguments it takes.
pub(crate) fn raise_panic_from_wait(
    ctx: Option<*mut ExecCtx>,
    kind: PanicKind,
    message: Option<&str>,
) -> ! {
    raise_at(
        ctx,
        PanicReport::new(kind, message, UNKNOWN_LOCATION),
        Some(backtrace::wait_start_of),
    )
}

/// [`raise_panic`] with a report of its own, for helpers that know the
/// location of the operation that panics. Its backtrace, when the variable
/// is on, is the no-frames line: no frame record of generated code is
/// known (D-1040).
pub(crate) fn raise(ctx: Option<*mut ExecCtx>, report: PanicReport) -> ! {
    raise_at(ctx, report, None)
}

/// [`raise`] with the backtrace's start (D-1040): `start` reads it from
/// the running fiber, `None` is no start.
fn raise_at(
    ctx: Option<*mut ExecCtx>,
    report: PanicReport,
    start: Option<fn(&ExecCtx) -> Start>,
) -> ! {
    let ctx = running(ctx).unwrap_or_else(|| {
        fatal(format_args!(
            "a `{:?}` panic happened outside of any entry",
            report.kind
        ))
    });
    // SAFETY: `running` returns the running fiber's control block, alive
    // while it runs; only shared references to it are made.
    let fiber = unsafe { &*ctx };
    let start = start.map_or(Start::Unknown, |start| start(fiber));
    let report = with_backtrace(fiber, report, start);
    if !fiber.unwind_edges() {
        edgeless(Some(&report));
    }
    record::start_panic(fiber, report);
    std::panic::resume_unwind(Box::new(ChebyUnwind))
}

/// `report` with the backtrace of `fiber` from `start` (D-1042), captured
/// while every frame is on the stack, before the panic is recorded and
/// before an edgeless fiber prints it; unchanged with the variable off or
/// in constant evaluation.
fn with_backtrace(fiber: &ExecCtx, report: PanicReport, start: Start) -> PanicReport {
    let backtrace = backtrace::capture(fiber, start, &report.location);
    report.with_backtrace(backtrace)
}

/// Delivers a cancellation from runtime code (D-102, D-688, D-706): records
/// it ([`record::start_cancel`], unless a record is in flight) and unwinds
/// the Rust frames above to the entry point's guard, which sets the flag to
/// its cancellation value (`unwind_flag::CANCEL`, D-733). Raised by a
/// kernel that would park (`send`, `receive`, `join`) and by a scope's
/// close that passes a cancellation through. In a fiber compiled without
/// unwind edges it ends the process with [`EDGELESS_FATAL`], which names an
/// unwinding, not a panic (D-770). Entered with no runtime
/// lock held.
pub(crate) fn raise_cancel(ctx: Option<*mut ExecCtx>) -> ! {
    let ctx = running(ctx)
        .unwrap_or_else(|| fatal(format_args!("a cancellation happened outside of any entry")));
    // SAFETY: `running` returns the running fiber's control block, alive
    // while it runs; only shared references to it are made.
    let fiber = unsafe { &*ctx };
    if !fiber.unwind_edges() {
        edgeless(None);
    }
    record::start_cancel(fiber);
    std::panic::resume_unwind(Box::new(ChebyUnwind))
}

/// Delivers a cancellation found by the stack slow path (D-688, step 3)
/// from its outermost Rust frame, as [`raise_from_runtime_at`] does a panic:
/// records it, sets the flag and returns, so no Rust unwinding runs at the
/// entry check (D-700), and generated code runs its entry pad. In a fiber
/// compiled without unwind edges it ends the process with
/// [`EDGELESS_FATAL`] (D-770).
pub(crate) fn cancel_from_runtime(fiber: &ExecCtx) {
    if !fiber.unwind_edges() {
        edgeless(None);
    }
    record::start_cancel(fiber);
    super::guard::set_flag(fiber);
}

/// Hands a panic raised by generated code's own call into the runtime
/// (`cheby_rt_panic`, the stack slow paths) to the return flag, from the
/// outermost Rust frame, so no Rust unwinding runs (D-700): records the
/// report, sets the flag and returns, and generated code runs its cleanup
/// pad (D-733). `start` is the backtrace's start: the frame record of the
/// generated code that called the raising entry, which its naked stub
/// passes (D-1040), or [`Start::Unknown`].
///
/// # Safety
///
/// `ctx` is null (outside any entry, which ends the process) or the
/// running fiber's control block, and the caller is the outermost Rust
/// frame below generated code: nothing above it needs dropping. `start` is
/// [`Start::Unknown`] or names a frame of the running fiber's generated
/// code that called the entry.
pub(crate) unsafe fn raise_from_runtime_at(ctx: *mut ExecCtx, report: PanicReport, start: Start) {
    // SAFETY: forwarded from the caller.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a `{:?}` panic happened outside of any entry",
            report.kind
        ))
    };
    let report = with_backtrace(fiber, report, start);
    if !fiber.unwind_edges() {
        edgeless(Some(&report));
    }
    record::start_panic(fiber, report);
    super::guard::set_flag(fiber);
}

/// Ends the process for a raise in code compiled without unwind edges
/// (D-770, refining D-742 and D-733): such code does not
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
    reason = "D-770: the process ends with the panic status, not a signal"
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
    // D-770 keeps this process exit, so a write that fails here ends the
    // process at once with status 1, as D-306 says, rather than halting a
    // runtime that this thread may not belong to. Both lines go to the real
    // stream, never to a capture that nobody would replay.
    let report = report.map_or_else(String::new, ToString::to_string);
    if io::write_last_words(&report).is_err()
        || io::write_last_words(&format!("fatal runtime error: {EDGELESS_FATAL}\n")).is_err()
    {
        std::process::exit(crate::abi::status::ERR)
    }
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
