//! Detached fibers (D-669, D-694, D-701, D-708, architecture §5.4):
//! the kernel `sys_spawn_detached` of `std::fiber` and the base frame of a
//! detached fiber.
//!
//! A detached fiber belongs to no scope of its spawner: it joins its
//! spawner's group (`sched::DetachedGroup`, GAP(detached-groups)) and gets
//! a **base scope** of its own (GAP(detached-base-scope)): a parentless
//! scope (depth 0) owned by the fiber and set as its spawn scope. Every
//! scope the fiber opens hangs below it, so:
//!
//! - no cancellation of the spawner's scopes reaches the fiber, not a
//!   failing scope nor a `timeout` around the spawn (D-669, D-671): the
//!   base has no parent;
//! - a teardown is `cancel::cancel(&base)` (D-708, D-710): the walk reaches
//!   the fiber wherever it waits and every scope and fiber it started,
//!   D-769's chain ends at the base, which is failing only during the
//!   teardown, and every close passes the cancellation through (D-667), so
//!   no inner scope returns `Err` to user code;
//! - a panic at the fiber's top level is not recorded in the base and
//!   cancels nothing (D-669): the base frame prints it.
//!
//! `sys_spawn_detached(f)`, in order:
//!
//! 1. marks `f` shared (D-705), outside every lock;
//! 2. without a runtime, ends the program (D-692);
//! 3. reserves a slot in the spawner's group, so a teardown waits for the
//!    fiber from now on, and takes the fiber's reference to `f`;
//! 4. creates the fiber on the runtime's default fiber stack (D-659: a
//!    detached fiber always gets the default; D-660's `fiber-stack`); a
//!    stack region that cannot be mapped gives the slot back and panics
//!    with `out of memory` (D-677);
//! 5. makes its base scope, sets it as the fiber's spawn scope and records
//!    the fiber in its slot, which tells whether the group is closing: then
//!    the fiber gets its base as its target before it is first queued, so
//!    it starts cancelled and never calls `f`, and still leaves the group
//!    (GAP(detached-spawn-while-closing));
//! 6. queues it to the worker's runnext slot, as `sys_spawn` does.
//!
//! Every raise happens with no lock held.
//!
//! The base frame ([`detached_main`]), in D-694's order:
//!
//! 1. calls `f` through the trampoline `cb$tramp$0` (D-526), unless the
//!    fiber starts with a target; then drops `f` (D-670);
//! 2. drops the result at once: there is no join cell (D-669);
//! 3. after an unwinding, takes its record: a panic is printed as its
//!    report after standard output is flushed, the two lines in one write
//!    (D-669, D-701), as an entry's catch point does; a cancellation (a
//!    teardown) prints nothing;
//! 4. drops its target and its spawn scope, and ends the base scope under
//!    its lock, so a later teardown's walk skips it;
//! 5. last, leaves its group, which may wake a teardown: its last touch of
//!    shared state, so a leak check after the teardown needs no global
//!    quiescence.
//!
//! Then the scheduler parks it for good (`park(Exit)`).

use std::sync::Arc;

use super::scope::ScopeState;
use super::{Owned, cancel, needs_scheduler, running};
use crate::abi::{PanicKind, Word};
use crate::alloc::OUT_OF_MEMORY;
use crate::ctx::ExecCtx;
use crate::io;
use crate::panic::fatal;
use crate::rc;
use crate::sched::{self, DetachedGroup, SpawnError, StartFn};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_panic;
use crate::unwind::record::{UnwindRecord, take_unwound};

/// `fiber::sys_spawn_detached`, private, which `fiber::spawn_detached`
/// calls: starts a detached fiber that calls `f` (the module documentation
/// gives the steps) and returns `Nil`.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `f` a live `fn() -> Nil`
/// closure, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn_detached(ctx: *mut ExecCtx, f: Word) {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { spawn_detached(ctx, f) });
}

/// What a detached fiber needs at its base (D-694).
#[derive(Debug)]
struct Start {
    /// The spawned function, `fn() -> Nil`: one reference, marked shared,
    /// which the fiber releases when the call ends.
    function: Word,
    /// The group the fiber is a member of.
    group: Arc<DetachedGroup>,
    /// Its member slot.
    slot: usize,
}

/// The body of [`cheby_fiber_sys_spawn_detached`].
///
/// # Safety
///
/// As for [`cheby_fiber_sys_spawn_detached`].
unsafe fn spawn_detached(ctx: *mut ExecCtx, function: Word) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // 1. Mark it shared: every object it reaches with a positive count is
    // confined to this fiber (I-1); if the walk raises, what it marked
    // stays shared together with all it reaches.
    // SAFETY: a live closure, borrowed.
    unsafe { rc::mark_shared(function) };
    // 2. The no-runtime rule.
    let Some(runtime) = me.sched.runtime() else {
        needs_scheduler("fiber::spawn_detached")
    };
    // 3. Reserve a slot and take the fiber's reference to the function.
    let group = me
        .conc
        .group()
        .unwrap_or_else(|| Arc::clone(runtime.default_group()));
    let slot = group.reserve();
    // SAFETY: a live closure, borrowed.
    unsafe { rc::dup(function) };
    // 4. Create the fiber, on the default stack (D-659).
    let start = Start {
        function,
        group: Arc::clone(&group),
        slot,
    };
    let start: StartFn = Box::new(move |ctx| {
        // SAFETY: `ctx` is the new fiber's control block, running on this
        // thread, and `start` holds the reference taken above.
        unsafe { detached_main(ctx, start) }
    });
    match sched::create_fiber(me, runtime.fiber_stack(), start) {
        Ok(fiber) => {
            // 5. Its base scope; then record it in its slot.
            let base = Arc::new(ScopeState::new_base(fiber));
            // SAFETY: the fiber was just created and is not queued yet, so
            // it cannot have run, let alone retired.
            let new = unsafe { fiber.ctx() };
            new.conc.set_spawn_scope(Some(Arc::clone(&base)));
            if group.fill(slot, fiber, Arc::clone(&base)) {
                // A teardown of the group is running: the fiber starts
                // cancelled (GAP(detached-spawn-while-closing)).
                cancel::set_target(new, &base);
            }
            drop(base);
            // 6. Queue it.
            sched::schedule_new(fiber, true);
        }
        Err(error) => {
            // The start function was dropped without running: give back
            // the reference it held, and the slot.
            // SAFETY: the reference taken in step 3.
            unsafe { rc::drop(function) };
            group.leave(slot);
            match error {
                SpawnError::NoRuntime => needs_scheduler("fiber::spawn_detached"),
                SpawnError::Stack(_) => {
                    raise_panic(Some(ctx), PanicKind::Explicit, Some(OUT_OF_MEMORY))
                }
            }
        }
    }
}

/// The start function of a detached fiber (D-694; the module
/// documentation gives the order).
///
/// # Safety
///
/// `ctx` is the new fiber's control block, running on this thread; `start`
/// came from `sys_spawn_detached`, which gave it one reference to the
/// function.
unsafe fn detached_main(ctx: *mut ExecCtx, start: Start) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    let Start {
        function,
        group,
        slot,
    } = start;
    let Some(base) = me.conc.spawn_scope() else {
        fatal(format_args!(
            "a detached fiber started without its base scope"
        ));
    };
    // SAFETY: the reference `sys_spawn_detached` gave this fiber.
    let function = unsafe { Owned::new(function) };
    // 1. Call the function, unless the fiber starts cancelled: spawned into
    // a closing group, or reached by a teardown before it started.
    let record: Option<UnwindRecord> = if cancel::check(me).is_err() {
        None
    } else {
        let Some(tramp) = me.callbacks().t0 else {
            fatal(format_args!(
                "the running entry has no trampoline for a spawned function"
            ));
        };
        // SAFETY: the trampolines come from the root's configuration,
        // whose ABI the host guarantees; `ctx` is this fiber's; the
        // function is a live `fn() -> Nil`, borrowed by the call, and the
        // result is owned.
        let result = unsafe { tramp(ctx, function.word()) };
        // An unwinding returns 0 here, which owns nothing. Taking the
        // record clears the flag (D-699).
        let record = take_unwound(me);
        if record.is_none() {
            // 2. Nobody can join a detached fiber: its result goes at once
            // (D-669).
            // SAFETY: the owned result of the call.
            unsafe { rc::drop(result) };
        }
        record
    };
    drop(function);
    // 3. Report a panic; a cancellation ends the fiber quietly.
    if let Some(UnwindRecord::Panic(report)) = &record {
        if io::flush_stdout().is_err() {
            sched::terminate(crate::abi::status::ERR);
        }
        io::write_stderr(&report.to_string());
    }
    drop(record);
    // 4. Hold no scope any more, and end the base: a teardown's walk that
    // comes later skips it and never reaches this fiber once it retires.
    cancel::clear_at_end(me);
    me.conc.set_spawn_scope(None);
    base.end_base();
    drop(base);
    // 5. Leave the group: the last touch of shared state.
    group.leave(slot);
}
