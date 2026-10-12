//! The runtime half of unwinding by return-flag propagation (D-733,
//! ADR-0061, D-685): the fiber's unwind flag, the `u32` at
//! [`crate::abi::CTX_UNWIND`] of its control block. Generated code loads
//! it after every C call that may unwind and tests it; the guards and
//! `cheby_rt_panic` set it when an unwinding leaves Rust code, and catch
//! points clear it.
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "tests read it; the entry check's releasing slow path will (D-698)"
    )
)]
pub(crate) fn get(ctx: &ExecCtx) -> u32 {
    ctx.unwind.load(Ordering::Acquire)
}

/// Clears the unwind flag of `ctx`, at a catch point.
pub(crate) fn clear(ctx: &ExecCtx) {
    ctx.unwind.store(unwind_flag::NONE, Ordering::Release);
}
