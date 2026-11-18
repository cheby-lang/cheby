//! `fiber::raise` (D-1097): the kernel `sys_raise`, which panics on the
//! calling fiber with the message and location of a caught panic.
//!
//! It raises the panic as generated code's own `panic` does
//! (`crate::panic::cheby_rt_panic`, and `option::panic_at` in
//! `crate::option`): it records the report, sets the unwind flag and
//! returns, so the caller runs its cleanup pad and the fiber unwinds by the
//! return flag (D-699, D-733), with no Rust unwinding. The report is an
//! explicit panic with the message and the location as given: the two texts
//! of a `Panic`, which `take_failure` read from the scope that caught it,
//! so a scope that catches the raise gets a `Panic` that is `==` to the one
//! raised (D-648), and an uncaught one prints the original report (§11.6).
//!
//! Its entry is a naked stub that passes its caller's frame pointer: the
//! frame record of its `@external` function `sys_raise`, which
//! `fiber::raise` reaches by a tail call. The backtrace starts there as at
//! the frame that `fiber::join` publishes before it waits
//! (`crate::backtrace::Start::Named`, D-868, D-1040): `sys_raise` is not
//! shown, the first line is `fiber::raise` with no location, while the
//! report keeps the original location, and the next is the caller of
//! `raise`, at the call (§11.6, D-1039, D-1097).
//!
//! The kernel's C result is the uniform word of `Never`, which it returns
//! only to the caller's flag test (D-1293).

use std::ptr;

use crate::abi::{PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::panic::PanicReport;
use crate::string;

/// `fiber::raise`'s kernel (D-1097). D-1040: a naked stub that passes its
/// caller's frame pointer to [`raise_from`] as a fourth argument and jumps
/// to it. AAPCS64: the arguments are in `x0`-`x2`; `x29` goes to `x3`.
///
/// # Safety
///
/// `ctx` is the running fiber's control block, and `message` and
/// `location` are live `String`s, borrowed; the caller is generated code
/// whose frame record is at `x29`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_fiber_sys_raise(
    ctx: *mut ExecCtx,
    message: Word,
    location: Word,
) -> Word {
    core::arch::naked_asm!("mov x3, x29", "b {body}", body = sym raise_from)
}

/// As the AAPCS64 stub, on System V x86-64 (D-1040): the arguments are in
/// `rdi`, `rsi` and `rdx`; `rbp` goes to `rcx`.
///
/// # Safety
///
/// As for the AAPCS64 stub; the caller's frame record is at `rbp`.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_fiber_sys_raise(
    ctx: *mut ExecCtx,
    message: Word,
    location: Word,
) -> Word {
    core::arch::naked_asm!("mov rcx, rbp", "jmp {body}", body = sym raise_from)
}

/// The plain entry under Miri and on Windows until M6 (D-1040): its
/// backtrace has no start.
///
/// # Safety
///
/// `ctx` is the running fiber's control block, and `message` and
/// `location` are live `String`s, borrowed.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_fiber_sys_raise(
    ctx: *mut ExecCtx,
    message: Word,
    location: Word,
) -> Word {
    // SAFETY: forwarded from the caller; 0 names no frame.
    unsafe { raise_from(ctx, message, location, 0) }
}

/// The body of [`cheby_fiber_sys_raise`]: `fp` is the generated caller's
/// frame pointer, or 0. Raises the panic
/// from the outermost Rust frame below generated code, as `cheby_rt_panic`
/// does (D-700, D-733), and returns 0, which the caller never reads: it
/// tests the flag.
///
/// # Safety
///
/// As for [`cheby_fiber_sys_raise`]; `fp` is 0 or the caller's frame
/// record.
unsafe extern "C" fn raise_from(
    ctx: *mut ExecCtx,
    message: Word,
    location: Word,
    fp: usize,
) -> Word {
    // SAFETY: forwarded from the caller.
    let report = unsafe { report_of(message, location) };
    let ctx = crate::unwind::raise::running(Some(ctx)).unwrap_or(ptr::null_mut());
    // SAFETY: `running` gives the running fiber's control block, alive
    // while it runs, or null; only a shared reference is made.
    let start = unsafe { ctx.as_ref() }.map_or(crate::backtrace::Start::Unknown, |fiber| {
        let (_, top) = fiber.stack_bounds();
        crate::backtrace::named_start(fp, crate::panic::stack_position(), top, RAISE)
    });
    // SAFETY: `ctx` is the running fiber's control block or null, and this
    // is the outermost Rust frame below generated code; `start` is checked
    // against the fiber's stack.
    unsafe { crate::unwind::raise::raise_from_runtime_at(ctx, report, start) };
    0
}

/// The display name of `fiber::raise` (D-1041), the first line of the
/// backtrace of a raised panic (D-1097).
const RAISE: &str = "std::fiber::raise";

/// The report of `fiber::sys_raise(message, location)`: an explicit panic
/// with `message` as it is, located at `location` as it is, so it prints
/// as the panic it repeats did (§11.6, D-1097).
///
/// # Safety
///
/// `message` and `location` are live `String`s.
pub(crate) unsafe fn report_of(message: Word, location: Word) -> PanicReport {
    // SAFETY: forwarded from the caller.
    let message = unsafe { string::to_rust_string(message) }.unwrap_or_default();
    // SAFETY: as above.
    let location = unsafe { string::to_rust_string(location) }.unwrap_or_default();
    PanicReport::new(PanicKind::Explicit, Some(&message), &location)
}

#[cfg(test)]
mod tests {
    use super::report_of;
    use crate::{rc, values};

    /// D-1097: the report repeats the message and the location as given,
    /// with no prefix, so a raised `todo`'s message stays as it was.
    #[test]
    fn raise_reports_the_message_and_location_it_is_given() {
        let message = values::string("not implemented: later");
        let location = values::string("src/main.cheby:4:7");
        // SAFETY: both are live `String`s, released below.
        let report = unsafe { report_of(message, location) };
        // SAFETY: the test owns both.
        unsafe {
            rc::drop(message);
            rc::drop(location);
        }
        assert_eq!(report.kind, crate::abi::PanicKind::Explicit);
        assert_eq!(
            report.to_string(),
            "panicked at src/main.cheby:4:7:\nnot implemented: later\n"
        );
    }
}
