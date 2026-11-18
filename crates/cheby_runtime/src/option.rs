//! The `std::option` kernel (D-1647): `panic_at`, through which
//! `option::expect` and `result::expect` end the calling fiber with a panic
//! located at the place of their call.
//!
//! `cheby_option_panic_at` raises the panic as generated code's own
//! `panic` does (`crate::panic::cheby_rt_panic`): it records the report
//! with the message and the location `path:line:column` that it is given,
//! sets the unwind flag and returns, so the caller runs its cleanup pad
//! and the fiber unwinds by the return flag (D-699, D-733), with no Rust
//! unwinding. The report, the exit status and the unwinding are those of a
//! `panic` written at that location (§11.6).
//!
//! Its entry is a naked stub that passes its caller's frame pointer: the
//! frame record of the std constructor (`option::expect_at` or
//! `result::expect_at`). A backtrace starts there as at the frame that
//! `fiber::join` publishes before it waits ([`crate::backtrace::wait_start`],
//! D-868, D-1040): the constructor is not shown, and the first line is its
//! caller, at the call that reached it. A caller that reached the
//! constructor by a tail call has no line, as everywhere (§11.6).
//!
//! The kernel's C result is the uniform word of `Never`, which it returns
//! only to the caller's flag test (D-1293).

use std::ptr;

use crate::abi::{PanicKind, Word};
use crate::panic::PanicReport;
use crate::string;

/// Appends every `std::option` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["option"];

/// `option::panic_at`'s kernel (D-1647). D-1040: a naked stub that passes
/// its caller's frame pointer to [`panic_at_from`] as a fifth argument and
/// jumps to it. AAPCS64: the arguments are in `x0`-`x3`; `x29` goes to
/// `x4`.
///
/// # Safety
///
/// `message` and `path` are live `String`s, borrowed; the caller is
/// generated code whose frame record is at `x29`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_option_panic_at(
    message: Word,
    path: Word,
    line: i64,
    column: i64,
) -> Word {
    core::arch::naked_asm!("mov x4, x29", "b {body}", body = sym panic_at_from)
}

/// As the AAPCS64 stub, on System V x86-64 (D-1040): the arguments are in
/// `rdi`, `rsi`, `rdx` and `rcx`; `rbp` goes to `r8`.
///
/// # Safety
///
/// As for the AAPCS64 stub; the caller's frame record is at `rbp`.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_option_panic_at(
    message: Word,
    path: Word,
    line: i64,
    column: i64,
) -> Word {
    core::arch::naked_asm!("mov r8, rbp", "jmp {body}", body = sym panic_at_from)
}

/// The plain entry under Miri and on Windows until M6 (D-1040): its
/// backtrace has no start.
///
/// # Safety
///
/// `message` and `path` are live `String`s, borrowed.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_option_panic_at(
    message: Word,
    path: Word,
    line: i64,
    column: i64,
) -> Word {
    // SAFETY: forwarded from the caller; 0 names no frame.
    unsafe { panic_at_from(message, path, line, column, 0) }
}

/// The body of [`cheby_option_panic_at`]: `fp` is the generated caller's
/// frame pointer, or 0. Raises the panic from the outermost Rust frame
/// below generated code, as `cheby_rt_panic` does (D-700, D-733), and
/// returns 0, which the caller never reads: it tests the flag.
///
/// # Safety
///
/// As for [`cheby_option_panic_at`]; `fp` is 0 or the caller's frame
/// record.
unsafe extern "C" fn panic_at_from(
    message: Word,
    path: Word,
    line: i64,
    column: i64,
    fp: usize,
) -> Word {
    // SAFETY: forwarded from the caller.
    let report = unsafe { report_at(message, path, line, column) };
    let ctx = crate::unwind::raise::running(None).unwrap_or(ptr::null_mut());
    // SAFETY: `running` gives the running fiber's control block, alive
    // while it runs, or null; only a shared reference is made.
    let start = unsafe { ctx.as_ref() }.map_or(crate::backtrace::Start::Unknown, |fiber| {
        let (_, top) = fiber.stack_bounds();
        crate::backtrace::wait_start(fp, crate::panic::stack_position(), top)
    });
    // SAFETY: `ctx` is the running fiber's control block or null, and this
    // is the outermost Rust frame below generated code; `start` is
    // checked against the fiber's stack.
    unsafe { crate::unwind::raise::raise_from_runtime_at(ctx, report, start) };
    0
}

/// The report of `option::panic_at(message, path, line, column)`: an
/// explicit panic with `message`, located at `path:line:column` (§11.6).
///
/// # Safety
///
/// `message` and `path` are live `String`s.
pub(crate) unsafe fn report_at(message: Word, path: Word, line: i64, column: i64) -> PanicReport {
    // SAFETY: forwarded from the caller.
    let message = unsafe { string::to_rust_string(message) }.unwrap_or_default();
    // SAFETY: as above.
    let path = unsafe { string::to_rust_string(path) }.unwrap_or_default();
    PanicReport::new(
        PanicKind::Explicit,
        Some(&message),
        &format!("{path}:{line}:{column}"),
    )
}

/// A kernel: its symbol, its address, its C parameter types, and its C
/// result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::option` kernel.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 1] {
    [(
        "cheby_option_panic_at",
        cheby_option_panic_at as *const u8,
        &["Word", "Word", "i64", "i64"],
        "Word",
    )]
}

/// The C signatures of the kernels, for the cross-check against `std/` in
/// `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::report_at;
    use crate::{rc, values};

    /// D-1647: the report is an explicit panic with the message as given,
    /// located at `path:line:column`, so it prints as a `panic` written
    /// at that place does (§11.6).
    #[test]
    fn panic_at_reports_its_message_at_the_given_place() {
        let message = values::string("no sixth element");
        let path = values::string("src/main.cheby");
        // SAFETY: both are live `String`s, released below.
        let report = unsafe { report_at(message, path, 16, 8) };
        // SAFETY: the test owns both.
        unsafe {
            rc::drop(message);
            rc::drop(path);
        }
        assert_eq!(report.kind, crate::abi::PanicKind::Explicit);
        assert_eq!(
            report.to_string(),
            "panicked at src/main.cheby:16:8:\nno sixth element\n"
        );
    }
}
