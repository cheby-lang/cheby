//! The scope of `process::until_interrupted` (D-1073, ADR-0071): the kernel
//! `fiber::sys_arm_interrupt`. `fiber::interruptible` opens a scope with
//! `scope_run`, as `fiber::timeout` does, and its body calls this kernel
//! first, then `f`: the scope is armed from inside, so `scope_run`'s
//! lowering stays as it is. From then on the first SIGINT or SIGTERM
//! cancels the scope, with the scope as the target, as an expired limit
//! cancels a timed scope (D-790), and records no first cause, so the close
//! fails it as kind 1 with empty texts when `f` unwound for it, and
//! `interruptible` reads that as an interruption; an `f` that returned
//! before the cancellation reached it gives `Ok` (D-671's rule). The close
//! disarms the scope on every path out (`scope.rs`, step 2). The signal
//! path is `sched/signal.rs`. D-1298.

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::unwind::guard::guarded;

/// `fiber::sys_arm_interrupt`, package-visible through
/// `fiber::interruptible` (D-1073): arms the borrowed `Scope` handle
/// `owner`, which the running fiber opened and whose body runs, for SIGINT
/// and SIGTERM (`ScopeState::arm_interrupt`). Under `cheby test`, in the
/// REPL and on Windows it does nothing (`sched::signal::arm`). The result
/// is `Nil`, which is `CType::Void`, as `sys_cancel`'s.
///
/// # Safety
///
/// `ctx` is the running fiber's control block and `owner` a live `Scope`
/// handle, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_arm_interrupt(ctx: *mut ExecCtx, owner: Word) {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { super::running(ctx) };
        // SAFETY: a live `Scope` handle, borrowed for the call.
        let scope = unsafe { super::scope::state_of(owner) };
        scope.arm_interrupt(me);
    });
}
