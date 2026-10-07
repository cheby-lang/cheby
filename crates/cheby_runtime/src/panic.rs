//! Panics (F7, §11.6, §11.8) and the stack-limit slow path (§11.8.1).
//!
//! A panic flushes standard output, prints its report to standard error
//! (except during constant evaluation, which captures it), and then
//! abandons the entry: it hands the report to the host waiting in
//! `run_entry` and parks the entry's thread forever. It never returns and
//! never unwinds (D-283; unwinding is M2). What happens next is the host's
//! policy: `cheby run` exits with 101 (D-182), `cheby test` records a
//! failure, the REPL continues, and constant evaluation reports a compile
//! error.
//!
//! Report format (D-449), as rustc prints panics:
//!
//! ```text
//! panicked at src/main.cheby:4:7:
//! <message>
//! ```

use std::fmt;

use crate::abi::{PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::entry::{EntryMode, EntryOutcome};
use crate::io;
use crate::string;

/// A panic as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicReport {
    /// What panicked.
    pub kind: PanicKind,
    /// The message: the program's, or the kind's default.
    pub message: String,
    /// `path:line:column` where the panic started (§11.6, D-421).
    pub location: String,
}

/// The location shown when generated code passes none.
pub const UNKNOWN_LOCATION: &str = "<unknown location>";

impl PanicReport {
    /// A report with the kind's default message when `message` is `None`.
    /// A `todo` with a message reads `not implemented: <message>`.
    #[must_use]
    pub fn new(kind: PanicKind, message: Option<&str>, location: &str) -> Self {
        let message = match (kind, message) {
            (PanicKind::Todo, Some(message)) => format!("{}: {message}", kind.default_message()),
            (_, Some(message)) => message.to_owned(),
            (_, None) => kind.default_message().to_owned(),
        };
        Self {
            kind,
            message,
            location: location.to_owned(),
        }
    }
}

impl fmt::Display for PanicReport {
    /// The report as printed, ending with a newline.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "panicked at {}:", self.location)?;
        writeln!(f, "{}", self.message)
    }
}

/// Panics the running entry.
///
/// `kind` is a [`PanicKind`] number; `message` is a borrowed `String` or 0
/// for the default message; `location` is a static `String` holding
/// `path:line:column`, or 0.
///
/// # Safety
///
/// `ctx` is the running entry's context (or null outside any entry, which
/// ends the process), and `message` and `location` are 0 or live objects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_panic(
    ctx: *mut ExecCtx,
    kind: u32,
    message: Word,
    location: Word,
) -> ! {
    // SAFETY: forwarded from the caller.
    let message = (message != 0)
        .then(|| unsafe { string::to_rust_string(message) })
        .flatten();
    // SAFETY: as above.
    let location = unsafe { location_text(location) };
    let report = match PanicKind::from_code(kind) {
        Some(kind) => PanicReport::new(kind, message.as_deref(), &location),
        None => PanicReport::new(
            PanicKind::Explicit,
            Some(&format!("internal error: unknown panic kind {kind}")),
            &location,
        ),
    };
    // SAFETY: forwarded from the caller.
    unsafe { abandon(ctx, report) }
}

/// The slow path of the function-entry stack check: called when the stack
/// pointer is below `ctx.stack_limit`. Handles a pending interrupt and
/// returns, or panics with [`PanicKind::StackOverflow`] if the real limit
/// is reached.
///
/// # Safety
///
/// `ctx` is the running entry's context, and `location` is 0 or a live
/// `String` object naming the function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow(ctx: *mut ExecCtx, location: Word) {
    // SAFETY: forwarded from the caller.
    let Some(context) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a stack check ran without an execution context"
        ));
    };
    // The interrupt handlers go here: the constant-evaluation deadline
    // (D-195, E6) and the scheduler's yield (D-028, M2).
    let _ = context.take_interrupt();
    if stack_position() < context.real_stack_limit() {
        // SAFETY: forwarded from the caller.
        let location = unsafe { location_text(location) };
        let report = PanicReport::new(PanicKind::StackOverflow, None, &location);
        // SAFETY: forwarded from the caller.
        unsafe { abandon(ctx, report) }
    }
}

/// An address in the current frame, at or below the caller's stack
/// pointer.
#[inline(never)]
pub(crate) fn stack_position() -> usize {
    let marker = 0_u8;
    std::hint::black_box(std::ptr::from_ref(&marker)).addr()
}

/// The text of a location word.
///
/// # Safety
///
/// `location` is 0 or a live object.
unsafe fn location_text(location: Word) -> String {
    if location == 0 {
        return UNKNOWN_LOCATION.to_owned();
    }
    // SAFETY: forwarded from the caller.
    unsafe { string::to_rust_string(location) }.unwrap_or_else(|| UNKNOWN_LOCATION.to_owned())
}

/// Ends the entry with a panic: flushes standard output, prints the report
/// unless the entry captures it, hands it to the host and parks forever.
///
/// # Safety
///
/// `ctx` is null or the running entry's context.
unsafe fn abandon(ctx: *mut ExecCtx, report: PanicReport) -> ! {
    if io::flush_stdout().is_err() {
        io::exit_after_write_failure();
    }
    // SAFETY: forwarded from the caller.
    let context = unsafe { ctx.as_ref() };
    let captured =
        context.is_some_and(|context| matches!(context.entry_mode(), EntryMode::ConstEval(_)));
    if !captured {
        io::write_stderr(&report.to_string());
    }
    let Some(outcome) = context.and_then(ExecCtx::outcome) else {
        fatal(format_args!("a panic happened outside of any entry"));
    };
    // If the host has gone there is nobody to tell; park anyway.
    outcome.send(EntryOutcome::Panicked(report)).ok();
    loop {
        std::thread::park();
    }
}

/// Reports a broken runtime invariant and aborts the process. This is an
/// internal error, never a Cheby panic: it means generated code or the
/// runtime broke the ABI.
pub(crate) fn fatal(message: fmt::Arguments<'_>) -> ! {
    io::write_stderr(&format!("fatal runtime error: {message}\n"));
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_render_like_rustc() {
        let report = PanicReport::new(
            PanicKind::Explicit,
            Some("computation failed"),
            "src/main.cheby:4:7",
        );
        assert_eq!(
            report.to_string(),
            "panicked at src/main.cheby:4:7:\ncomputation failed\n"
        );
    }

    #[test]
    fn default_messages_fill_in() {
        let report = PanicReport::new(PanicKind::Overflow, None, "a.cheby:1:1");
        assert_eq!(report.message, "integer overflow");
        let report = PanicReport::new(PanicKind::Todo, None, "a.cheby:1:1");
        assert_eq!(report.message, "not implemented");
        let report = PanicReport::new(PanicKind::Todo, Some("area of a dot"), "a.cheby:1:1");
        assert_eq!(report.message, "not implemented: area of a dot");
    }
}
