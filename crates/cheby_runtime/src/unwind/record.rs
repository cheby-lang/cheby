//! The record of the unwinding in flight in a fiber, and the catch points'
//! accessors (D-697, D-699, D-701).
//!
//! The cause of an unwinding lives in the fiber (D-699): a panic is
//! recorded when it starts ([`start_panic`]), so the unwinding allocates
//! nothing more, and its report is printed where the unwinding ends
//! (D-701). A fiber is unwinding while it holds a record. A catch point
//! takes the record: an entry's ([`finish_entry`]), a trampoline's caller
//! (`callback::call`, which rethrows), `fiber_main` and the scope
//! intrinsic ([`take_unwound`], [`take_record`]). A panic is also recorded
//! in the fiber's responsible scope when it starts (D-666, [`start_panic`]).
//!
//! A host decides whether an entry panicked from the record, through
//! [`finish_entry`], never from the entry thunk's result: with the return
//! flag (D-733) an unwinding thunk returns 0, but `main`'s thunk returns
//! the exit status, and 0 is success.

use parking_lot::Mutex;

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
    /// A cancellation; its target is the fiber's, under its fiber lock
    /// (D-706, `fiber_ops::cancel`).
    Cancel,
}

/// Records the panic `report` as the fiber's unwinding when it starts
/// (D-699), then in the fiber's responsible scope (D-666, architecture
/// §6.6): the innermost scope the fiber has open, a root's `main` or test
/// body inside `scope` included, else the scope it was spawned into. The
/// scope keeps it as its first cause if it has none and is not cancelled
/// from outside, so the first panic by start wins, whoever ends first, and
/// then cancels itself before this fiber unwinds (D-666, D-667,
/// `ScopeState::record_panic`). A fiber with neither scope (a root outside
/// every scope, an isolated entry) records nothing in a scope: its catch
/// point reports the panic.
///
/// If the fiber is already unwinding, the new panic is dropped and
/// recorded nowhere: no user code runs while a fiber unwinds (ADR-0029),
/// so the first cause stands.
///
/// Entered with no runtime lock held (scope, channel, join cell, fiber,
/// record): the scope's lock is taken after the record's is released, as
/// the lock order wants (architecture §7.5: the record's lock is a leaf).
/// So that the scope can read the report once the record's lock is
/// released, a copy of it is kept for the call, and the scope copies its
/// texts into its first cause only when it records: the scope's `Failure`
/// owns strings of its own, never shared with the record. D-642's reserve
/// has been released before an out-of-memory report is built, so the
/// copies have room.
pub(crate) fn start_panic(ctx: &ExecCtx, report: PanicReport) {
    let scope = ctx.conc.responsible_scope();
    let copy = {
        let mut record = ctx.unwinding.record.lock();
        if record.is_some() {
            return;
        }
        let copy = scope.as_ref().map(|_| report.clone());
        *record = Some(UnwindRecord::Panic(report));
        copy
    };
    if let (Some(scope), Some(copy)) = (scope, copy) {
        scope.record_panic(&copy);
    }
}

/// Records a cancellation as the fiber's unwinding when it is delivered
/// (D-688, D-699), unless a record is already in flight: a fiber that is
/// unwinding goes on unwinding with its first cause. A cancellation is
/// recorded in no scope: its target is the fiber's, under its fiber lock
/// (D-706). A cancelled fiber is never reported as panicked (D-178): its
/// record says `Cancel`, which no catch point prints. Entered with no
/// runtime lock held.
pub(crate) fn start_cancel(ctx: &ExecCtx) {
    let mut record = ctx.unwinding.record.lock();
    if record.is_none() {
        *record = Some(UnwindRecord::Cancel);
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
