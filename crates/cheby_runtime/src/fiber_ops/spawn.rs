//! Starting a fiber (D-666, D-670, D-677, architecture §5.4): the kernel
//! `sys_spawn` of `std::fiber`, and the stubs of plan §7 UQ-2's answer.
//!
//! `sys_spawn(owner, f)`, in order:
//!
//! 1. allocates the `Fiber` handle and its join cell, and marks `f` shared,
//!    outside every lock (D-705), so that running out of memory raises
//!    before a member slot is reserved (D-642; review finding MEM-1: a
//!    raise after the reservation left the scope waiting for a fiber that
//!    never existed);
//! 2. joins the scope `owner` under its lock: a scope that has ended makes
//!    the spawn panic with `spawn into a scope that has ended` (D-666,
//!    D-732); otherwise a member slot is reserved, so the scope waits for
//!    the fiber from now on, and a scope that has failed makes the fiber
//!    start cancelled, never calling `f` (D-666);
//! 3. without a runtime, ends the program (D-692);
//! 4. takes a reference to `f` for the fiber;
//! 5. creates the fiber on a slot of the default 8 MiB (D-125, D-677),
//!    inheriting the spawner's domain, trampolines, hash seed, entry mode
//!    and unwind-edge setting (D-742); a stack region that cannot be
//!    mapped gives the slot back and panics with `out of memory` (D-677);
//!
//! Every raise here happens with no lock held (plan §0 'Locks'): `join`
//! and `leave` release the scope's lock before they return, and a raise
//! takes the spawner's own responsible scope's lock to record its panic
//! (D-666).
//! 6. records the fiber in its slot and queues it to the worker's runnext
//!    slot, so it runs when the spawner blocks (architecture §3.5).

use std::sync::Arc;

use super::base::{Base, FiberHandle, JoinCell, fiber_main};
use super::scope::{Membership, state_of};
use super::{Owned, needs_scheduler, running};
use crate::abi::{PanicKind, Word};
use crate::alloc::OUT_OF_MEMORY;
use crate::ctx::ExecCtx;
use crate::entry::DEFAULT_STACK_SIZE;
use crate::handles::{self, HandleKind};
use crate::panic::fatal;
use crate::rc;
use crate::sched::{self, SpawnError, StartFn};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_panic;

/// `fiber::sys_spawn`, private, which `fiber::spawn` calls: starts a fiber
/// in the scope `owner` that calls `f` (the module documentation gives the
/// steps), and returns its `Fiber` handle, owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context, `owner` a live `Scope` handle and
/// `f` a live `fn() -> T` closure, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn(ctx: *mut ExecCtx, owner: Word, f: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        spawn(ctx, owner, f, DEFAULT_STACK_SIZE)
    })
}

/// The body of [`cheby_fiber_sys_spawn`] with a stack of `stack_size`
/// bytes.
///
/// # Safety
///
/// As for [`cheby_fiber_sys_spawn`].
unsafe fn spawn(ctx: *mut ExecCtx, owner: Word, function: Word, stack_size: usize) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // SAFETY: a live `Scope` handle, borrowed.
    let scope = unsafe { state_of(owner) };
    // 1. The handle and its join cell.
    let cell = Arc::new(JoinCell::default());
    // SAFETY: `ctx` is the running fiber's; a fresh handle, owned here
    // until it is returned.
    let handle = unsafe {
        Owned::new(handles::new_handle(
            ctx,
            HandleKind::Fiber,
            Box::new(FiberHandle(Arc::clone(&cell))),
        ))
    };
    // SAFETY: a live closure, borrowed; every object it reaches with a
    // positive count is confined to this fiber (I-1). If the walk raises,
    // what it marked stays shared together with all it reaches.
    unsafe { rc::mark_shared(function) };
    // 2. Join the scope.
    let (slot, cancelled) = match scope.join() {
        Membership::Ended => {
            drop(handle);
            raise_panic(
                Some(ctx),
                PanicKind::Explicit,
                Some("spawn into a scope that has ended"),
            )
        }
        Membership::Member { index, cancelled } => (index, cancelled),
    };
    // 3. The no-runtime rule.
    if me.sched.runtime().is_none() {
        needs_scheduler("fiber::spawn");
    }
    // 4. Take the fiber's reference to the function, marked shared in
    // step 1.
    // SAFETY: a live closure, borrowed.
    unsafe { rc::dup(function) };
    // 5. Create the fiber.
    let base = Base {
        function,
        cell,
        scope: Arc::clone(&scope),
        slot,
        cancelled,
    };
    let start: StartFn = Box::new(move |ctx| {
        // SAFETY: `ctx` is the new fiber's control block, running on this
        // thread, and `base` holds the reference taken above.
        unsafe { fiber_main(ctx, base) }
    });
    match sched::create_fiber(me, stack_size, start) {
        Ok(fiber) => {
            // 6. Record and queue it.
            scope.fill(slot, fiber);
            sched::schedule_new(fiber, true);
            handle.into_word()
        }
        Err(error) => {
            // The start function was dropped without running: give back
            // the reference it held, and the slot.
            // SAFETY: the reference taken in step 4.
            unsafe { rc::drop(function) };
            scope.leave(slot);
            drop(handle);
            match error {
                SpawnError::NoRuntime => needs_scheduler("fiber::spawn"),
                SpawnError::Stack(_) => {
                    raise_panic(Some(ctx), PanicKind::Explicit, Some(OUT_OF_MEMORY))
                }
            }
        }
    }
}

/// `fiber::sys_spawn_with_stack`, private, which `fiber::spawn_with_stack`
/// calls. A stub until package L1 (D-754): it ends the program with
/// `fatal runtime error: fiber::spawn_with_stack is not supported yet`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; the other arguments are borrowed
/// and not read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn_with_stack(
    ctx: *mut ExecCtx,
    owner: Word,
    stack_bytes: i64,
    f: Word,
) -> Word {
    guarded(Some(ctx), || -> Word {
        let _ = (owner, stack_bytes, f);
        fatal(format_args!("fiber::spawn_with_stack is not supported yet"))
    })
}

/// `fiber::sys_spawn_detached`, private, which `fiber::spawn_detached`
/// calls. A stub until package L1 (D-754): it ends the program with
/// `fatal runtime error: fiber::spawn_detached is not supported yet`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `f` is borrowed and not read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn_detached(ctx: *mut ExecCtx, f: Word) {
    guarded(Some(ctx), || -> () {
        let _ = f;
        fatal(format_args!("fiber::spawn_detached is not supported yet"))
    });
}
