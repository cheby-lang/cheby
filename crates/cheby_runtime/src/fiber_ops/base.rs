//! The base frame of a spawned fiber (D-694, architecture §5.4) and the
//! join cell its result goes to (D-707, architecture §7.6).
//!
//! [`fiber_main`] is the start function of every fiber `sys_spawn`
//! creates. It is a runtime base frame calling a closure, like an entry
//! thunk, not a kernel calling back into Cheby, so D-484 stays as it is
//! (D-675). It touches only its own control block (I-5), and in D-694's
//! order:
//!
//! 1. calls the spawned function through the trampoline `cb$tramp$0`
//!    (D-526), unless its scope had already failed when it was spawned
//!    (D-666), and then drops the function (D-670);
//! 2. stores the result in the join cell, marked shared (D-674, D-705), or
//!    drops it when no `Fiber` handle remains (D-670); a fiber that did
//!    not return marks the cell failed (D-668);
//! 3. after an unwinding, has taken its record and drops it, printing
//!    nothing (D-701): a panic was recorded in the fiber's scope when it
//!    started (D-666, `unwind::record::start_panic`), which reports it as
//!    its `Err`, and a cancellation just ends the fiber;
//! 4. drops its share of the join cell and every other reference its
//!    control block holds, so the fiber holds no Cheby object;
//! 5. only then leaves its scope's members, which may wake the scope's
//!    owner: its last touch of shared state, so a leak check after the
//!    owner wakes needs no global quiescence.
//!
//! Then the scheduler parks it for good (`park(Exit)`).

use std::sync::Arc;

use parking_lot::Mutex;

use super::scope::ScopeState;
use super::{Owned, needs_scheduler};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::HandlePayload;
use crate::panic::fatal;
use crate::rc;
use crate::sched::{AfterSwitch, FiberRef, Unlock, park, wake};
use crate::unwind::raise::ChebyUnwind;
use crate::unwind::record::{UnwindRecord, take_unwound};

/// Where a fiber's result goes (D-707): shared by the fiber and its
/// `Fiber` handle, and outliving both as long as either holds it.
#[derive(Debug, Default)]
pub(crate) struct JoinCell {
    state: Mutex<JoinState>,
}

/// The part of a join cell under its lock.
#[derive(Debug, Default)]
struct JoinState {
    /// How far the fiber has come.
    result: Outcome,
    /// Whether the fiber's `Fiber` handle is gone, so nobody can join it.
    handle_gone: bool,
    /// The fibers waiting in `join`.
    joiners: Vec<FiberRef>,
}

/// How far a fiber has come.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It has not ended.
    #[default]
    Running,
    /// It returned this value, shared, which the cell owns (one reference).
    Done(Word),
    /// It panicked or was cancelled, or its scope had failed before it
    /// started (D-666, D-668).
    Failed,
    /// It returned, but its value was released because no `Fiber` handle
    /// remained (D-670).
    Released,
}

/// What a joiner found in a join cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Joined {
    /// The fiber's value, dup'd for the joiner.
    Value(Word),
    /// The fiber failed.
    Failed,
}

impl JoinCell {
    /// Waits until the fiber has ended (D-668, D-707): its value, dup'd
    /// for the joiner, the same on every call; or `Failed`. Without a
    /// runtime, waiting ends the program (D-692).
    pub(crate) fn join(&self, me: &ExecCtx) -> Joined {
        loop {
            let mut state = self.state.lock();
            match state.result {
                Outcome::Done(value) => {
                    // SAFETY: the cell owns a reference to the live value,
                    // which the lock keeps from being released.
                    unsafe { rc::dup(value) };
                    return Joined::Value(value);
                }
                Outcome::Failed => return Joined::Failed,
                Outcome::Released => fatal(format_args!(
                    "a fiber was joined after its result was released"
                )),
                Outcome::Running => {}
            }
            if me.sched.runtime().is_none() {
                drop(state);
                needs_scheduler("fiber::join");
            }
            state.joiners.push(FiberRef::from_ctx(me));
            park(me, AfterSwitch::Park(Some(Unlock::new(state))));
        }
    }

    /// Ends the cell with the fiber's outcome, `Done` with an owned value
    /// already marked shared, or `Failed`, and wakes every joiner. A value
    /// nobody can join any more is handed back to be released by the
    /// caller, outside the lock.
    pub(super) fn finish(&self, outcome: Outcome) -> Option<Word> {
        let mut state = self.state.lock();
        let (result, released) = match outcome {
            Outcome::Done(value) if state.handle_gone => (Outcome::Released, Some(value)),
            other => (other, None),
        };
        state.result = result;
        for joiner in state.joiners.drain(..) {
            // SAFETY: a joiner registered under this lock and parks with
            // it, so it is parking or parked, not retired. The fiber is
            // about to end, so a joiner takes its runnext slot.
            unsafe { wake(joiner, true) };
        }
        released
    }

    /// What the cell holds now.
    #[cfg(test)]
    pub(crate) fn outcome(&self) -> Outcome {
        self.state.lock().result
    }
}

/// The payload of a `Fiber` handle (kind 42).
#[derive(Debug)]
pub(crate) struct FiberHandle(pub(crate) Arc<JoinCell>);

impl HandlePayload for FiberHandle {
    /// The last copy of the handle is gone: nobody can join the fiber any
    /// more, so its result is released now if it has one, or as soon as
    /// the fiber returns (D-670, D-707).
    fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word)) {
        let value = {
            let mut state = self.0.state.lock();
            state.handle_gone = true;
            match state.result {
                Outcome::Done(value) => {
                    state.result = Outcome::Released;
                    Some(value)
                }
                Outcome::Running | Outcome::Failed | Outcome::Released => None,
            }
        };
        if let Some(value) = value {
            release(value);
        }
    }
}

/// What a spawned fiber needs at its base (D-694).
#[derive(Debug)]
pub(crate) struct Base {
    /// The spawned function, `fn() -> T`: one reference, marked shared,
    /// which the fiber releases when the call ends.
    pub(crate) function: Word,
    /// Where the result goes.
    pub(crate) cell: Arc<JoinCell>,
    /// The scope the fiber is a member of.
    pub(crate) scope: Arc<ScopeState>,
    /// Its member slot.
    pub(crate) slot: usize,
    /// Whether its scope had failed when it was spawned, so the function is
    /// never called (D-666).
    pub(crate) cancelled: bool,
}

/// The start function of a spawned fiber (D-694; the module documentation
/// gives the order).
///
/// # Safety
///
/// `ctx` is the new fiber's control block, running on this thread; `base`
/// came from `sys_spawn`, which gave it one reference to the function.
pub(crate) unsafe fn fiber_main(ctx: *mut ExecCtx, base: Base) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { super::running(ctx) };
    me.conc.set_spawn_scope(Some(Arc::clone(&base.scope)));
    let Base {
        function,
        cell,
        scope,
        slot,
        cancelled,
    } = base;
    // SAFETY: the reference `sys_spawn` gave this fiber.
    let function = unsafe { Owned::new(function) };
    // 1. Call the function: its result, or the record of its unwinding
    // (`None` for a fiber that never called it). Then drop it.
    let ended: Result<Word, Option<UnwindRecord>> = if cancelled {
        Err(None)
    } else {
        let Some(tramp) = me.callbacks().t0 else {
            fatal(format_args!(
                "the running entry has no trampoline for a spawned function"
            ));
        };
        // SAFETY: the trampolines come from the root's configuration,
        // whose ABI the host guarantees; `ctx` is this fiber's; the
        // function is a live `fn() -> T`, borrowed by the call, and the
        // result is owned.
        let result = unsafe { tramp(ctx, function.word()) };
        // An unwinding returns 0 here, which owns nothing. Taking the
        // record clears the flag (D-699).
        take_unwound(me).map_or(Ok(result), |record| Err(Some(record)))
    };
    drop(function);
    // 2. Store or drop the result.
    let ended = ended.and_then(|result| share_result(me, result));
    let outcome = match ended {
        Ok(result) => Outcome::Done(result),
        Err(_) => Outcome::Failed,
    };
    if let Some(released) = cell.finish(outcome) {
        // SAFETY: the value the cell gave back, owned.
        unsafe { rc::drop(released) };
    }
    // 3. The record ends here, unprinted (D-701): a panic is in the
    // scope's first cause already, or was not the first (D-666); a
    // cancellation just ends the fiber.
    drop(ended);
    // 4. Hold no reference any more.
    drop(cell);
    me.conc.set_spawn_scope(None);
    // 5. Leave the scope: the last touch of shared state.
    scope.leave(slot);
}

/// Marks the fiber's result shared for its joiners (D-674, D-705) and
/// returns it. Growing the walk's worklist can raise `out of memory`
/// (D-642), and the raise must not unwind into the fiber's start function,
/// which is `extern "C"` (review finding MEM-1): it is caught here, the
/// result is released, and the fiber ends with that panic as if its
/// function had raised it. The function has returned, so the fiber has no
/// scope open: the panic was recorded in the scope it was spawned into
/// (D-666).
fn share_result(me: &ExecCtx, result: Word) -> Result<Word, Option<UnwindRecord>> {
    let marked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the fiber's own result: every object it reaches with a
        // positive count is confined to this fiber (I-1).
        unsafe { rc::mark_shared(result) };
    }));
    match marked {
        Ok(()) => Ok(result),
        Err(payload) if payload.is::<ChebyUnwind>() => {
            drop(payload);
            // SAFETY: the fiber owns its result; what the walk marked
            // before it raised is shared together with all it reaches, so
            // either path of the release is right.
            unsafe { rc::drop(result) };
            Err(take_unwound(me))
        }
        Err(_) => fatal(format_args!("a Rust panic inside the runtime")),
    }
}
