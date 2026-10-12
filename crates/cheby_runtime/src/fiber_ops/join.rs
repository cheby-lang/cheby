//! Joining a fiber (D-177, D-668, D-707, architecture §7.6): the kernel
//! `sys_join` of `std::fiber`.

use std::sync::Arc;

use super::base::{FiberHandle, Joined};
use super::running;
use crate::abi::{PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_panic;

/// `fiber::sys_join`, private, which `fiber::join` calls: waits until the
/// fiber of the borrowed handle `fiber` has ended and returns its value,
/// owned, the same value on every call by any fiber (D-668, D-707).
///
/// If the fiber failed, a joiner that is being cancelled unwinds, which
/// package U5 brings; until then every joiner of a failed fiber panics with
/// `joined fiber failed`, at `<unknown location>` (D-668, D-732), raised
/// after the join cell's lock is released. Without a runtime, waiting ends
/// the program (D-692).
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
        match cell.join(me) {
            Joined::Value(value) => value,
            Joined::Failed => {
                drop(cell);
                raise_panic(Some(ctx), PanicKind::Explicit, Some("joined fiber failed"))
            }
        }
    })
}
