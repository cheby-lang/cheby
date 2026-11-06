//! Panics (F7, §11.6, §11.8) and the stack-limit slow path (§11.8.1).
//!
//! A panic records its report in the fiber (D-699) and unwinds by the
//! return flag to its catch point (D-733, ADR-0061), which prints the
//! report where D-701 says ([`crate::unwind`]): the entry's thread or
//! fiber then ends normally, and the cleanup pads have released its
//! objects (D-713). In code compiled without unwind edges
//! (`CHEBY_UNWIND_EDGES=off`, D-742) a panic ends the process instead
//! (D-770, [`crate::unwind::raise`]).
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
    /// The backtrace captured at the raise (D-1042): `backtrace:` and its
    /// lines, each ending in a newline; `None` with `CHEBY_BACKTRACE` off
    /// (D-1038), in constant evaluation, and for a report made by hand.
    backtrace: Option<Box<str>>,
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
            backtrace: None,
        }
    }

    /// The backtrace captured at the raise (D-1039, D-1042), which the
    /// report's text prints after its two lines; `None` when none was.
    #[must_use]
    pub fn backtrace(&self) -> Option<&str> {
        self.backtrace.as_deref()
    }

    /// The report with `backtrace`, as a raise captures it.
    pub(crate) fn with_backtrace(self, backtrace: Option<Box<str>>) -> Self {
        Self { backtrace, ..self }
    }
}

impl fmt::Display for PanicReport {
    /// The report as printed, ending with a newline: its two lines (D-449),
    /// then its backtrace when it has one, so every host prints both in
    /// one write (D-669, D-1042).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "panicked at {}:", self.location)?;
        writeln!(f, "{}", self.message)?;
        match &self.backtrace {
            Some(backtrace) => f.write_str(backtrace),
            None => Ok(()),
        }
    }
}

/// Panics the running entry, called by generated code.
///
/// `kind` is a [`PanicKind`] number; `message` is a borrowed `String` or 0
/// for the default message; `location` is a static `String` holding
/// `path:line:column`, or 0.
///
/// It is the outermost Rust frame of the panic, so it hands the panic to
/// the return flag directly ([`crate::unwind::raise::raise_from_runtime_at`],
/// D-697, D-733): it records the report, sets the flag and returns, and
/// generated code runs its cleanup pad.
///
/// D-1040: a naked stub that passes its caller's frame pointer and return
/// address to [`panic_from`] as two more arguments and jumps to it, so the
/// backtrace starts at the generated code that panicked. AAPCS64: the
/// arguments are in `x0`-`x3`; `x29` goes to `x4`, `x30` to `x5`.
///
/// # Safety
///
/// `ctx` is the running entry's context (or null outside any entry, which
/// ends the process), and `message` and `location` are 0 or live objects;
/// the caller is generated code whose frame record is at `x29`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_panic(
    ctx: *mut ExecCtx,
    kind: u32,
    message: Word,
    location: Word,
) {
    core::arch::naked_asm!(
        "mov x4, x29",
        "mov x5, x30",
        "b {body}",
        body = sym panic_from,
    )
}

/// D-1040: as the AAPCS64 stub, on System V x86-64: the arguments are in
/// `rdi`, `esi`, `rdx` and `rcx`; `rbp` goes to `r8` and the return
/// address, the word at `[rsp]`, to `r9`.
///
/// # Safety
///
/// As for the AAPCS64 stub; the caller's frame record is at `rbp`.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_panic(
    ctx: *mut ExecCtx,
    kind: u32,
    message: Word,
    location: Word,
) {
    core::arch::naked_asm!(
        "mov r8, rbp",
        "mov r9, qword ptr [rsp]",
        "jmp {body}",
        body = sym panic_from,
    )
}

/// The plain entry under Miri and on Windows until M6 (D-1040): its
/// backtrace has no start.
///
/// # Safety
///
/// `ctx` is the running entry's context (or null outside any entry, which
/// ends the process), and `message` and `location` are 0 or live objects.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_panic(
    ctx: *mut ExecCtx,
    kind: u32,
    message: Word,
    location: Word,
) {
    // SAFETY: forwarded from the caller; 0 and 0 name no frame.
    unsafe { panic_from(ctx, kind, message, location, 0, 0) }
}

/// The body of [`cheby_rt_panic`]: `fp` and `ra` are the generated
/// caller's frame pointer and return address, or 0 and 0 (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_panic`]; `fp` and `ra` are 0 or name the caller's
/// frame record and the address it returns to.
unsafe extern "C" fn panic_from(
    ctx: *mut ExecCtx,
    kind: u32,
    message: Word,
    location: Word,
    fp: usize,
    ra: usize,
) {
    // SAFETY: forwarded from the caller.
    let report = unsafe { report_of(kind, message, location) };
    let start = crate::backtrace::start_at(fp, ra);
    // SAFETY: forwarded from the caller.
    unsafe { crate::unwind::raise::raise_from_runtime_at(ctx, report, start) }
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

/// What the body of the slow paths found ([`stack_slow`]); 0, the guard's
/// sentinel, when a raise inside it (the deadline) set the flag already.
mod found {
    /// Nothing more to do: return to the function.
    pub(super) const NOTHING: u8 = 0;
    /// The stack overflowed.
    pub(super) const OVERFLOW: u8 = 1;
    /// The fiber is cancelled (D-688, step 3).
    pub(super) const CANCEL: u8 = 2;
}

/// The slow path of the function-entry stack check: called when the stack
/// pointer is below `ctx.stack_limit`. Handles the pending interrupt
/// reasons in the order of D-688 (safepoint, cancellation, deadline,
/// yield; the safepoint has no effect yet), lowers the soft limit of a
/// fiber that goes deeper (D-677), and returns; or delivers a cancellation
/// or panics with [`PanicKind::StackOverflow`] if the hard limit is
/// reached, both by setting the unwind flag and returning (D-700, D-733).
///
/// # Safety
///
/// `ctx` is the running entry's context, and `location` is 0 or a live
/// `String` object naming the function.
///
/// D-1040: a naked stub that passes its caller's frame pointer and return
/// address to [`stack_slow_from`] and jumps to it, so a stack overflow's
/// backtrace starts at the function whose entry check found it. AAPCS64:
/// the arguments are in `x0` and `x1`; `x29` goes to `x2`, `x30` to `x3`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow(ctx: *mut ExecCtx, location: Word) {
    core::arch::naked_asm!(
        "mov x2, x29",
        "mov x3, x30",
        "b {body}",
        body = sym stack_slow_from,
    )
}

/// The entry check's slow path (above), as a System V x86-64 stub (D-1040): the
/// arguments are in `rdi` and `rsi`; `rbp` goes to `rdx` and the return
/// address, the word at `[rsp]`, to `rcx`.
///
/// # Safety
///
/// As for the AAPCS64 stub.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow(ctx: *mut ExecCtx, location: Word) {
    core::arch::naked_asm!(
        "mov rdx, rbp",
        "mov rcx, qword ptr [rsp]",
        "jmp {body}",
        body = sym stack_slow_from,
    )
}

/// The entry check's slow path (above), a plain function under Miri and on
/// Windows until M6 (D-1040): a stack overflow's backtrace has no start.
///
/// # Safety
///
/// As above.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow(ctx: *mut ExecCtx, location: Word) {
    // SAFETY: forwarded from the caller; 0 and 0 name no frame.
    unsafe { stack_slow_from(ctx, location, 0, 0) }
}

/// The body of [`cheby_rt_stack_slow`]: `fp` and `ra` are the generated
/// caller's frame pointer and return address, or 0 and 0 (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`]; `fp` and `ra` are 0 or name the
/// caller's frame record and the address it returns to.
unsafe extern "C" fn stack_slow_from(ctx: *mut ExecCtx, location: Word, fp: usize, ra: usize) {
    // SAFETY: forwarded from the caller.
    unsafe { stack_slow_at(ctx, location, crate::backtrace::start_at(fp, ra)) }
}

/// [`cheby_rt_stack_slow`]'s work, with the backtrace's start: also the
/// releasing slow path's (`crate::unwind::entry_slow`), whose own stub
/// finds the start (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`]; `start` is [`crate::backtrace::Start::Unknown`]
/// or names the frame of the generated code whose entry check this is.
pub(crate) unsafe fn stack_slow_at(
    ctx: *mut ExecCtx,
    location: Word,
    start: crate::backtrace::Start,
) {
    // SAFETY: forwarded from the caller.
    let found = guarded(Some(ctx), || unsafe { stack_slow(ctx, 0) });
    // SAFETY: forwarded from the caller.
    unsafe { act(ctx, location, found, start) }
}

/// Acts on what the body of a slow path found, from the slow path's
/// outermost frame, so that no Rust unwinding runs at the entry check
/// (D-700). `start` is a stack overflow's backtrace's start (D-1040).
///
/// # Safety
///
/// As for [`stack_slow_at`].
unsafe fn act(ctx: *mut ExecCtx, location: Word, found: u8, start: crate::backtrace::Start) {
    match found {
        // SAFETY: forwarded from the caller.
        found::OVERFLOW => unsafe { overflow(ctx, location, start) },
        found::CANCEL => {
            // SAFETY: forwarded from the caller; the slow path's body found
            // the context, so it is not null.
            if let Some(fiber) = unsafe { ctx.as_ref() } {
                crate::unwind::raise::cancel_from_runtime(fiber);
            }
        }
        _ => {}
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
///
/// D-1040: a naked stub, as [`cheby_rt_stack_slow`]'s, that passes its
/// caller's frame pointer and return address to [`stack_slow_frame_from`]
/// and jumps to it. AAPCS64: the arguments are in `x0`-`x2`; `x29` goes to
/// `x3`, `x30` to `x4`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_frame(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
) {
    core::arch::naked_asm!(
        "mov x3, x29",
        "mov x4, x30",
        "b {body}",
        body = sym stack_slow_frame_from,
    )
}

/// A large function's slow path (above), as a System V x86-64 stub (D-1040):
/// the arguments are in `rdi`, `rsi` and `rdx`; `rbp` goes to `rcx` and the
/// return address, the word at `[rsp]`, to `r8`.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_frame(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
) {
    core::arch::naked_asm!(
        "mov rcx, rbp",
        "mov r8, qword ptr [rsp]",
        "jmp {body}",
        body = sym stack_slow_frame_from,
    )
}

/// A large function's slow path (above), a plain function under Miri and
/// on Windows until M6 (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_frame(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
) {
    // SAFETY: forwarded from the caller; 0 and 0 name no frame.
    unsafe { stack_slow_frame_from(ctx, location, frame_size, 0, 0) }
}

/// The body of [`cheby_rt_stack_slow_frame`]: `fp` and `ra` as for
/// [`stack_slow_from`].
///
/// # Safety
///
/// As for [`stack_slow_from`].
unsafe extern "C" fn stack_slow_frame_from(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    fp: usize,
    ra: usize,
) {
    let start = crate::backtrace::start_at(fp, ra);
    // SAFETY: forwarded from the caller.
    unsafe { stack_slow_frame_at(ctx, location, frame_size, start) }
}

/// [`cheby_rt_stack_slow_frame`]'s work, with the backtrace's start, as
/// [`stack_slow_at`] is [`cheby_rt_stack_slow`]'s.
///
/// # Safety
///
/// As for [`stack_slow_at`].
pub(crate) unsafe fn stack_slow_frame_at(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    start: crate::backtrace::Start,
) {
    let needed = frame_size.saturating_add(LARGE_FRAME_MARGIN);
    // SAFETY: forwarded from the caller.
    let found = guarded(Some(ctx), || unsafe { stack_slow(ctx, needed) });
    // SAFETY: forwarded from the caller.
    unsafe { act(ctx, location, found, start) }
}

/// Raises the stack overflow both slow paths found, from their outermost
/// frame, so that no Rust unwinding runs at the deepest point (D-700),
/// with its backtrace from `start`, captured here in the slow path's
/// reserved room (D-679, D-1042).
///
/// # Safety
///
/// As for [`stack_slow_at`].
unsafe fn overflow(ctx: *mut ExecCtx, location: Word, start: crate::backtrace::Start) {
    // SAFETY: forwarded from the caller.
    let location = unsafe { location_text(location) };
    let report = PanicReport::new(PanicKind::StackOverflow, None, &location);
    // SAFETY: forwarded from the caller.
    unsafe { crate::unwind::raise::raise_from_runtime_at(ctx, report, start) }
}

/// The body of both slow paths: handles the pending interrupt, then
/// returns what it found: a cancellation to deliver, or whether fewer than
/// `needed` bytes are left above the real limit, a stack overflow. Runs in
/// the slow paths' guard: the deadline of constant evaluation raises inside
/// it.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`].
unsafe fn stack_slow(ctx: *mut ExecCtx, needed: u64) -> u8 {
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
    // 2. A halt (D-073, D-708) freezes the fiber here, whatever else is
    // pending: `SAFEPOINT` forced this slow path, and the halt flag is read
    // too, so a fiber resumed just before the halt began freezes at its
    // next entry check. It is never resumed.
    if let Some(runtime) = context.sched.runtime()
        && runtime.halting()
    {
        sched::park(context, AfterSwitch::Freeze);
        fatal(format_args!("a frozen fiber was resumed"));
    }
    // 3. A cancellation unwinds it (D-688, D-706), skipping the later
    // steps. It is level-triggered from the target, read under the fiber
    // lock whether or not CANCEL was among the reasons: the walk sets the
    // target before it requests CANCEL, so a reason lost in a race cannot
    // drop it. A fiber that is unwinding already goes on with its record.
    if crate::fiber_ops::cancel::check(context).is_err()
        && !crate::unwind::record::unwinding(context)
    {
        return found::CANCEL;
    }
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
            found::NOTHING
        }
        _ => found::OVERFLOW,
    }
}

/// An address in the current frame, at or below the caller's stack
/// pointer.
#[cfg(not(all(
    cheby_sanitize = "address",
    not(windows),
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
#[inline(never)]
pub(crate) fn stack_position() -> usize {
    let marker = 0_u8;
    std::hint::black_box(std::ptr::from_ref(&marker)).addr()
}

/// An address in the current frame, at or below the caller's stack
/// pointer: the stack pointer itself.
///
/// D-878: the address sanitizer's detection of uses
/// after return moves a local whose address escapes to a fake stack on the
/// heap, so the address of a local is no stack position there.
#[cfg(all(
    cheby_sanitize = "address",
    not(windows),
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
#[inline(never)]
pub(crate) fn stack_position() -> usize {
    let sp: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: copies the stack pointer into a register; it reads and writes
    // no memory, leaves the stack and the flags alone.
    unsafe {
        std::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: as above, with `rsp`.
    unsafe {
        std::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
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
    // The process aborts next whether the message could be written; it
    // goes to the real stream, never to a capture nobody would replay.
    let _ = io::write_last_words(&format!("fatal runtime error: {message}\n"));
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
