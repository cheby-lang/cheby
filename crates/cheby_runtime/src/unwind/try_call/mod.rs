//! The `try_call` prototype (D-696, D-702): the runtime side of the
//! unwinding backend that resumes at Cranelift `try_call` handlers. Only
//! the symbol generated code calls exists so far, as a stub.

use crate::ctx::ExecCtx;
use crate::panic::fatal;

/// Resumes the unwinding in flight in the running fiber from a generated
/// cleanup pad: transfers control to the next handler up the stack.
///
/// Not implemented yet: ends the process.
///
/// # Safety
///
/// `ctx` is the running fiber's context, with an unwinding in flight.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_unwind_resume(ctx: *mut ExecCtx) {
    let _ = ctx;
    fatal(format_args!("not implemented yet: cheby_rt_unwind_resume"))
}
