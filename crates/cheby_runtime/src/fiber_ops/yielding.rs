//! `fiber::yield_now` (D-1102): the kernel `sys_yield_now`.
//!
//! The kernel puts the calling fiber behind the fibers that can run: it
//! goes back to a run queue, as the yield of the entry check's slow path
//! does (`crate::panic`, step 5), and resumes there, maybe on another
//! worker. On native the entry checks already keep a fiber from holding a
//! thread (§10.2, D-028), so the kernel only makes the switch happen at
//! once. A pending cancellation is delivered there (§10.7): one that reached
//! the fiber before the call, and one that reached it while it was queued.
//! An entry that runs no scheduler (D-692) has no other fiber to let run,
//! so there the kernel only delivers a cancellation.

use crate::ctx::ExecCtx;
use crate::sched::{AfterSwitch, park};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;

/// `fiber::sys_yield_now`, private, which `fiber::yield_now` calls
/// (D-1102): requeues the running fiber once, delivering a pending
/// cancellation before and after the switch. The result is `Nil`, which
/// is `CType::Void`, as `sys_cancel`'s.
///
/// # Safety
///
/// `ctx` is the running fiber's control block.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_fiber_sys_yield_now(ctx: *mut ExecCtx) {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { super::running(ctx) };
        deliver_cancel(ctx, me);
        if me.sched.runtime().is_some() {
            park(me, AfterSwitch::Requeue);
            deliver_cancel(ctx, me);
        }
    });
}

/// Unwinds the running fiber `me` when a cancellation has reached it
/// (D-102, D-688): read from its target, as the entry check's slow path
/// reads it (`crate::panic`, step 3).
fn deliver_cancel(ctx: *mut ExecCtx, me: &ExecCtx) {
    if super::cancel::check(me).is_err() {
        raise_cancel(Some(ctx));
    }
}
