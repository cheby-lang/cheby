//! Joining a fiber (D-177, D-668, D-707, architecture §7.6): the kernel
//! `sys_join` of `std::fiber`.

use std::sync::Arc;

use super::base::{FiberHandle, Joined};
use super::{cancel, running};
use crate::abi::{PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind};
use crate::unwind::guard::guarded;
use crate::unwind::raise::{raise_cancel, raise_panic};

/// `fiber::sys_join`, private, which `fiber::join` calls: waits until the
/// fiber of the borrowed handle `fiber` has ended and returns its value,
/// owned, the same value on every call by any fiber (D-668, D-707).
///
/// If the fiber panicked or was cancelled, a joiner that is being cancelled
/// unwinds, which is always the case for a member of that fiber's scope:
/// the scope's cancellation reached it when the failing fiber's panic
/// started, before that fiber ended (D-666, D-668). A joiner is being
/// cancelled when it has a target or when a scope on its chain has failed
/// or is being cancelled (`cancel::check_chain`), so the answer does not
/// depend on how far the walk has come. Any other joiner panics
/// with `joined fiber failed`, at `<unknown location>` (D-668, D-732). A
/// joiner that is cancelled while it waits, or before, unwinds without
/// waiting and takes nothing (D-670). Every raise happens after the join
/// cell's lock is released. Without a runtime, waiting ends the program
/// (D-692).
///
/// # Safety
///
/// `ctx` is the running fiber's context and `fiber` a live `Fiber` handle,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_join(ctx: *mut ExecCtx, fiber: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: a live `Fiber` handle, borrowed for the call. The cell is
        // cloned so that nothing borrowed from the handle is held while
        // the joiner waits.
        let cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(fiber, HandleKind::Fiber) }.0);
        let joined = cell.join(me);
        drop(cell);
        match joined {
            Ok(Joined::Value(value)) => value,
            // GAP(chain-cancellation). Being cancelled is decided from the
            // joiner's position too: a fiber that failed because an
            // enclosing scope of the joiner was cancelled can end before
            // the walk reaches the joiner (review finding CONC-W2-1).
            Ok(Joined::Failed) if cancel::check_chain(me, me.conc.responsible_scope()).is_err() => {
                raise_cancel(Some(ctx))
            }
            Ok(Joined::Failed) => {
                raise_panic(Some(ctx), PanicKind::Explicit, Some("joined fiber failed"))
            }
            Err(cancel::Cancelled) => raise_cancel(Some(ctx)),
        }
    })
}
