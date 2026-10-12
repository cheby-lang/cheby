//! The return-flag prototype's runtime half (D-696, D-685): the fiber's
//! unwind flag, the `u32` at [`crate::abi::CTX_UNWIND`] of its control
//! block. Generated code compiled for [`crate::abi::UnwindBackend::Flag`]
//! loads it after every C call that may unwind and tests it; the guards,
//! `raise_panic` and `cheby_rt_panic` set it when an unwinding leaves Rust
//! code under that backend, and catch points clear it.
//!
//! Only the owning fiber writes the flag (D-685), so these accessors need
//! no read-modify-write; the release store and the acquire load order the
//! unwind record written before the flag with readers that test the flag
//! first.

use std::sync::atomic::Ordering;

use crate::abi::unwind_flag;
use crate::ctx::ExecCtx;

/// Sets the unwind flag of `ctx` to `flag`, one of
/// [`crate::abi::unwind_flag`]'s values.
pub(crate) fn set(ctx: &ExecCtx, flag: u32) {
    ctx.unwind.store(flag, Ordering::Release);
}

/// The unwind flag of `ctx`: [`unwind_flag::NONE`] when no unwinding is in
/// flight.
#[expect(
    dead_code,
    reason = "the shared unwinding runtime (package U3) calls it in this wave"
)]
pub(crate) fn get(ctx: &ExecCtx) -> u32 {
    ctx.unwind.load(Ordering::Acquire)
}

/// Clears the unwind flag of `ctx`, at a catch point.
pub(crate) fn clear(ctx: &ExecCtx) {
    ctx.unwind.store(unwind_flag::NONE, Ordering::Release);
}
