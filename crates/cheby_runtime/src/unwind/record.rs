//! The record of the unwinding in flight in a fiber, and the catch points'
//! accessors (D-697, D-699, D-701).
//!
//! The cause of an unwinding lives in the fiber (D-699): a panic is
//! recorded when it starts ([`start_panic`]), so the unwinding allocates
//! nothing more, and its report is printed where the unwinding ends
//! (D-701). A fiber is unwinding while it holds a record. A catch point
//! takes the record: an entry's ([`finish_entry`]), a trampoline's caller
//! (`callback::call`, which rethrows), `fiber_main` and the scope
//! intrinsic ([`take_unwound`], [`take_record`]).
//!
//! A host decides whether an entry panicked from the record, through
//! [`finish_entry`], never from the entry thunk's result: under the flag
//! backend an unwinding thunk returns 0, but `main`'s thunk returns the
//! exit status, and 0 is success.

use parking_lot::Mutex;

use crate::abi::UnwindBackend;
use crate::ctx::ExecCtx;
use crate::entry::EntryMode;
use crate::io;
use crate::panic::PanicReport;

use super::guard::RecordKind;

/// The unwinding fields of a fiber control block (D-685, D-697): the
/// record of the unwinding in flight. Only the owning fiber touches it,
/// so the lock is never contended; it is never held across a call or a
/// fiber switch (I-5).
#[derive(Default, Debug)]
pub(crate) struct UnwindState {
    record: Mutex<Option<UnwindRecord>>,
}

/// What is unwinding a fiber.
#[derive(Debug)]
pub(crate) enum UnwindRecord {
    /// A panic, with its report.
    Panic(PanicReport),
    /// A cancellation; its target is the fiber's (U5).
    #[expect(dead_code, reason = "cancellation arrives with package U5")]
    Cancel,
}

/// Records the panic `report` as the fiber's unwinding when it starts
/// (D-699). If the fiber is already unwinding, the new panic is dropped:
/// no user code runs while a fiber unwinds (ADR-0029), so the first cause
/// stands.
pub(crate) fn start_panic(ctx: &ExecCtx, report: PanicReport) {
    let mut record = ctx.unwinding.record.lock();
    if record.is_none() {
        *record = Some(UnwindRecord::Panic(report));
    }
}

/// Whether the fiber is unwinding: it holds a record.
pub(crate) fn unwinding(ctx: &ExecCtx) -> bool {
    ctx.unwinding.record.lock().is_some()
}

/// The kind of the record in flight, without taking it.
pub(crate) fn peek_kind(ctx: &ExecCtx) -> Option<RecordKind> {
    ctx.unwinding.record.lock().as_ref().map(RecordKind::of)
}

/// At a catch point, after the callee returned while unwinding: clears the
/// flag and takes the record.
pub(crate) fn take_unwound(ctx: &ExecCtx) -> Option<UnwindRecord> {
    super::flag::clear(ctx);
    take_record(ctx)
}

/// The record without touching the flag (`scope_close(_, true)` after
/// `clear_unwind`).
///
/// Taking a record completes a catch: the reserve that out of memory
/// gives back to the system before it raises is set aside again (D-642),
/// now that the cleanup pads have released what the unwinding owned.
pub(crate) fn take_record(ctx: &ExecCtx) -> Option<UnwindRecord> {
    let record = ctx.unwinding.record.lock().take();
    if record.is_some() {
        crate::alloc::ensure_reserve();
    }
    record
}

/// At an entry's catch point (isolated entries, and the root runner in
/// `sched/`): takes the record, prints the report when the entry mode
/// prints it (D-701: `Run`, `Repl`), and returns it for the host's
/// `EntryOutcome::Panicked`. `None` when the entry returned normally.
///
/// Standard output is flushed first, as a panic always did (D-449); a
/// failed flush ends the program as a failed write does (D-306).
pub(crate) fn finish_entry(ctx: &ExecCtx) -> Option<PanicReport> {
    let report = match take_unwound(ctx)? {
        UnwindRecord::Panic(report) => report,
        // A cancellation unwinds only to its target scope (D-699), which an
        // entry's own frames hold; it never reaches the entry's catch
        // point.
        UnwindRecord::Cancel => crate::panic::fatal(format_args!(
            "a cancellation reached an entry's catch point"
        )),
    };
    if io::flush_stdout().is_err() {
        io::exit_after_write_failure();
    }
    match ctx.entry_mode() {
        EntryMode::Run | EntryMode::Repl => io::write_stderr(&report.to_string()),
        EntryMode::Test | EntryMode::ConstEval(_) => {}
    }
    Some(report)
}

/// The unwinding backend the fiber's code was compiled for (D-696).
pub(crate) fn backend(ctx: &ExecCtx) -> UnwindBackend {
    ctx.unwind_backend()
}
