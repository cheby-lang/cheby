//! Scopes (D-693, D-706): the open and close that the compiler-generated
//! body of the `scope_run` intrinsic calls, with the body called in
//! between and no Cheby call around it.
//!
//! Both are stubs until the scheduler and the scope tree exist: calling
//! one ends the process.

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::panic::fatal;

/// Opens a scope for the running fiber and returns its handle, owned.
/// `timed` gives the scope a deadline `seconds` and `nanoseconds` from now
/// (D-693). It may raise only out of memory, before anything is open.
///
/// Not implemented yet: ends the process.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_scope_open(
    ctx: *mut ExecCtx,
    timed: bool,
    seconds: i64,
    nanoseconds: i64,
) -> Word {
    let _ = (ctx, timed, seconds, nanoseconds);
    fatal(format_args!("not implemented yet: cheby_rt_scope_open"))
}

/// Closes the scope `scope` that [`cheby_rt_scope_open`] opened, consuming
/// the handle: waits for its members, then decides what the body's
/// failure becomes (D-693). `unwound` tells whether the body ended by
/// unwinding. Returns true when the scope caught a failure, so that
/// `scope_run` returns the static `Err(Nil)`.
///
/// Not implemented yet: ends the process.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `scope` the handle its
/// `scope_open` returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_scope_close(
    ctx: *mut ExecCtx,
    scope: Word,
    unwound: bool,
) -> bool {
    let _ = (ctx, scope, unwound);
    fatal(format_args!("not implemented yet: cheby_rt_scope_close"))
}
