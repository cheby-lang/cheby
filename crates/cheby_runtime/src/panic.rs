//! Panics (F7, §11.6, §11.8) and the stack-limit slow path (§11.8.1).
//!
//! A panic records its report in the fiber (D-699) and unwinds by the
//! return flag to its catch point (D-733, ADR-0061), which prints the
//! report where D-701 says ([`crate::unwind`]): the entry's thread or
//! fiber then ends normally, and the cleanup pads have released its
//! objects (D-713). In code compiled without unwind edges
//! (`CHEBY_UNWIND_EDGES=off`, D-742) a panic ends the process instead
//! (GAP(edgeless-panics), [`crate::unwind::raise`]).
//!
//! What happens next is the host's policy: `cheby run` exits with 101
//! (D-182), `cheby test` records a failure and prints the report in its
//! failure block (D-541), the REPL continues, and constant evaluation
//! reports a compile error with the report's message and location in a
//! note (D-449, D-533).
//!
//! Report format (D-449), as rustc prints panics:
//!
//! ```text
//! panicked at src/main.cheby:4:7:
//! <message>
//! ```

use std::fmt;

use crate::abi::{PanicKind, Word};
use crate::ctx::{self, ExecCtx};
use crate::entry::EntryMode;
use crate::io;
use crate::sched::{self, AfterSwitch};
use crate::string;
use crate::unwind::guard::guarded;

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

/// Panics the running entry, called by generated code.
///
/// `kind` is a [`PanicKind`] number; `message` is a borrowed `String` or 0
/// for the default message; `location` is a static `String` holding
/// `path:line:column`, or 0.
///
/// It is the outermost Rust frame of the panic, so it hands the panic to
/// the return flag directly ([`crate::unwind::raise::raise_from_runtime`],
/// D-697, D-733): it records the report, sets the flag and returns, and
/// generated code runs its cleanup pad.
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
) {
    // SAFETY: forwarded from the caller.
    let report = unsafe { report_of(kind, message, location) };
    // SAFETY: forwarded from the caller.
    unsafe { crate::unwind::raise::raise_from_runtime(ctx, report) }
}

/// The report of a panic given as `cheby_rt_panic`'s words.
///
/// # Safety
///
/// `message` and `location` are 0 or live objects.
pub(crate) unsafe fn report_of(kind: u32, message: Word, location: Word) -> PanicReport {
    // SAFETY: forwarded from the caller.
    let message = (message != 0)
        .then(|| unsafe { string::to_rust_string(message) })
        .flatten();
    // SAFETY: as above.
    let location = unsafe { location_text(location) };
    match PanicKind::from_code(kind) {
        Some(kind) => PanicReport::new(kind, message.as_deref(), &location),
        None => PanicReport::new(
            PanicKind::Explicit,
            Some(&format!("internal error: unknown panic kind {kind}")),
            &location,
        ),
    }
}

/// The slow path of the function-entry stack check: called when the stack
/// pointer is below `ctx.stack_limit`. Handles the pending interrupt
/// reasons in the order of D-688 (safepoint, cancellation, deadline,
/// yield; the deadline and the yield have an effect so far), lowers the
/// soft limit of a fiber that goes deeper (D-677), and returns, or panics
/// with [`PanicKind::StackOverflow`] if the hard limit is reached.
///
/// # Safety
///
/// `ctx` is the running entry's context, and `location` is 0 or a live
/// `String` object naming the function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow(ctx: *mut ExecCtx, location: Word) {
    // SAFETY: forwarded from the caller.
    let overflowed = guarded(Some(ctx), || unsafe { stack_slow(ctx, 0) });
    if overflowed {
        // SAFETY: forwarded from the caller.
        unsafe { overflow(ctx, location) }
    }
}

/// The room below the stack position that the slow path of a large
/// function's entry check requires on top of its frame (D-679): the outer
/// function's check is `sp - frame_size - 64 KiB < stack_limit`.
pub const LARGE_FRAME_MARGIN: u64 = 64 << 10;

/// The slow path of a large function's entry check (D-679): like
/// [`cheby_rt_stack_slow`], but called by the small outer function of a
/// function whose frame of `frame_size` bytes exceeds 64 KiB, before that
/// frame is allocated. Handles a pending interrupt and returns, or panics
/// with [`PanicKind::StackOverflow`] when the stack position minus
/// `frame_size` minus [`LARGE_FRAME_MARGIN`] is below the real limit.
/// (D-743)
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_frame(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
) {
    let needed = frame_size.saturating_add(LARGE_FRAME_MARGIN);
    // SAFETY: forwarded from the caller.
    let overflowed = guarded(Some(ctx), || unsafe { stack_slow(ctx, needed) });
    if overflowed {
        // SAFETY: forwarded from the caller.
        unsafe { overflow(ctx, location) }
    }
}

/// Raises the stack overflow both slow paths found, from their outermost
/// frame, so that no Rust unwinding runs at the deepest point (D-700).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
unsafe fn overflow(ctx: *mut ExecCtx, location: Word) {
    // SAFETY: forwarded from the caller.
    let location = unsafe { location_text(location) };
    let report = PanicReport::new(PanicKind::StackOverflow, None, &location);
    // SAFETY: forwarded from the caller.
    unsafe { crate::unwind::raise::raise_from_runtime(ctx, report) }
}

/// The body of both slow paths: handles the pending interrupt, then
/// returns whether fewer than `needed` bytes are left above the real
/// limit, a stack overflow. Runs in the slow paths' guard: the deadline of
/// constant evaluation raises inside it.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
unsafe fn stack_slow(ctx: *mut ExecCtx, needed: u64) -> bool {
    // SAFETY: forwarded from the caller.
    let Some(context) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a stack check ran without an execution context"
        ));
    };
    // 1. Take the reasons (D-688), in the order of the interrupt protocol.
    let reasons = ctx::take(context);
    if reasons != 0 {
        context.count_interrupt();
    }
    // 2. A safepoint stops the fiber, and 3. a cancellation unwinds it;
    // both are accepted and ignored until the runtime stops fibers and
    // cancels scopes.
    // 4. The constant-evaluation deadline (D-195, D-536). A request that
    // finds the deadline not passed yet is ignored; the watchdog repeats
    // it.
    if reasons & ctx::reason::DEADLINE != 0
        && let EntryMode::ConstEval(limits) = context.entry_mode()
        && limits
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        crate::consteval::limit_exceeded(crate::consteval::Limit::Time);
    }
    // 5. A yield (D-688): a fiber of a runtime goes back to a run queue,
    // and continues here when it is resumed, maybe on another worker.
    // Isolated entries ignore it.
    if reasons & ctx::reason::YIELD != 0 && context.sched.runtime().is_some() {
        sched::park(context, AfterSwitch::Requeue);
    }
    // 6. The depth: below the soft limit, the limit goes down in doubling
    // steps (D-677); below the hard limit, the stack overflows.
    let needed = usize::try_from(needed).unwrap_or(usize::MAX);
    let low = stack_position().checked_sub(needed);
    match low {
        Some(low) if low >= context.hard_limit() => {
            if low < context.real_stack_limit() {
                context.lower_soft_limit(low);
            }
            false
        }
        _ => true,
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
