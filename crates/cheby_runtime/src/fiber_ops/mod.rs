//! The runtime side of `std::fiber`, `std::channel` and `std::selector`
//! (D-646, D-692 to D-695, D-704, D-707): scopes, spawning, the base frame
//! of a spawned fiber, joining, channels and selectors.
//!
//! - [`scope`]: `cheby_rt_scope_open` and `cheby_rt_scope_close`, the two
//!   runtime helpers of the `scope_run` intrinsic (D-693), and the kernel
//!   `take_failure`.
//! - [`spawn`]: the kernel `sys_spawn` (architecture §5.4), and the stubs
//!   of the parts of `std::fiber` that later packages bring.
//! - [`base`]: `fiber_main`, the base frame of a spawned fiber (D-694), and
//!   the join cell its result goes to (D-707).
//! - [`detached`]: the kernel `sys_spawn_detached` and the base frame of a
//!   detached fiber, with its base scope (D-669, D-694).
//! - [`join`]: the kernel `sys_join` (D-668, D-707).
//! - [`channel`]: Go's `hchan` (D-704): the kernels `new`, `send` and
//!   `receive`, and the handles' finalizers (D-672).
//!
//! Every kernel here is `@std_context` (D-692): it gets the running fiber's
//! control block first, runs its whole body in U3's `guarded` and raises
//! only through `raise_panic` (D-697). Arguments are borrowed and results
//! owned (D-187).
//!
//! **Without a runtime** (D-692, architecture §5.1): an isolated entry (the
//! REPL, constant evaluation never gets here, the thread backend on
//! Windows) runs these kernels too. What completes without waiting works:
//! `channel::new`, a `send` with room, a `receive` of a buffered value or
//! of a closed channel, a `scope` whose body spawns nothing. Starting a
//! fiber or waiting ends the program with a fatal runtime error naming the
//! std function ([`needs_scheduler`]): a fatal error, not a panic, because
//! `scope` would catch a panic and mislead.
//!
//! - [`cancel`]: cancellation (D-667, D-706): the subtree walk, the check a
//!   fiber makes before it parks, and the publication of its wait node.
//! - [`select`]: the kernels of `std::selector` (D-673, D-695): building
//!   selectors, and the select wait over several channels and a timer.
//!
//! [`FiberConc`] is the structured-concurrency part of a fiber's control
//! block (D-685): the scope it was spawned into, the scopes it has open,
//! why the last one it closed failed, and the fiber lock (architecture
//! §3.1, D-706) over its cancellation target and its wait-node pointer. The
//! innermost of its open scopes is the one a panic of the fiber is recorded
//! in when it starts (D-666, `unwind::record::start_panic`).
//!
//! # Locks
//!
//! The runtime's locks, in the order they may be taken (architecture §7.5,
//! D-706); a lock is never taken while a later one in this list is held,
//! and none is held across a switch (I-5), except the one a parking fiber
//! hands to `park`, which its worker releases after the switch:
//!
//! 1. scope locks ([`scope::ScopeState`]), a parent's before a child's;
//! 2. or a channel lock ([`channel`]) or a join cell's lock
//!    ([`base::JoinCell`]): never held together with a scope lock. A
//!    `select` ([`select`]) holds several channel locks at once: it takes
//!    the channels of its receive arms in address order, each once (two
//!    arms on one channel share its lock), then the fiber lock to publish
//!    its wait, and never a scope lock with them; it parks with all of
//!    them (`AfterSwitch::ParkAll`) and, once woken, takes them again one
//!    at a time to unlink its nodes;
//! 3. the fiber lock ([`FiberConc`]): a leaf. Nothing else is locked while
//!    it is held, it is never held while user code runs, and `sched::wake`
//!    is never called under it (a wake can take a worker's queue lock);
//! 4. the timer heap's lock (`sched::timer`): a leaf. It may be taken
//!    under channel locks (a `select` arms its timer under them) and
//!    nothing is taken inside it; a timer entry's own lock is a leaf too,
//!    never taken under the heap's. A timer fires only after the heap's
//!    lock is released, with no runtime lock held, so a timer's target may
//!    wake a fiber or run the cancellation walk; a winning
//!    `TimerHandle::cancel` drops the target on the canceller's thread, so
//!    a target's `Drop` takes no runtime lock. (The poll driver's lock is
//!    taken only by an idle worker, with `try_lock`, and is out of reach
//!    of this module.)
//! 5. the unwind record's lock (`unwind::record`): a leaf too, taken with
//!    no other lock held;
//! 6. a detached group's lock (`sched::DetachedGroup`): a leaf, taken with
//!    no scope, channel or fiber lock held, and nothing is taken under it
//!    (a teardown cancels after releasing it); the runtime's registries of
//!    groups and of waiting root hosts (`sched`) are leaves too.
//!
//! A teardown, a halt and a termination (`sched::lifecycle`) are entered
//! with no runtime lock held, like `start_panic`, `start_cancel` and
//! [`cancel::cancel`] below.
//!
//! The fiber's own locks of [`FiberConc`] (its spawn scope, open scopes and
//! last failure) are taken only by the fiber itself and never with another
//! lock held.
//!
//! A raise takes a scope lock to record its panic (`start_panic`) and runs
//! the cancellation walk ([`cancel::cancel`]), and `parking_lot`'s mutexes
//! are not reentrant. So `unwind::record::start_panic`,
//! `unwind::record::start_cancel` and [`cancel::cancel`] are entered with
//! no runtime lock held (scope, channel, join cell, fiber, record); every
//! raise site of this module and of `sched` releases its locks first, and
//! [`scope::ScopeState::record_panic`] releases the scope's lock before it
//! calls [`cancel::cancel`]. The walk holds a scope's lock while it takes
//! each member's fiber lock, and never acts on a member after releasing
//! that scope's lock: a member can then leave, retire and have its slot
//! reused.

pub(crate) mod base;
pub(crate) mod cancel;
pub(crate) mod channel;
pub(crate) mod detached;
pub(crate) mod join;
pub(crate) mod scope;
pub(crate) mod select;
pub(crate) mod spawn;
#[cfg(test)]
pub(crate) mod testing;

use std::sync::Arc;

use parking_lot::Mutex;

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::panic::fatal;
use crate::rc;
use crate::sched::DetachedGroup;

use self::scope::ScopeState;

/// The structured-concurrency fields of a fiber control block (D-685).
///
/// The fiber lock ([`cancel::FiberLock`], architecture §3.1, D-706) guards
/// the only state other fibers touch: the cancellation target and the
/// wait-node pointer, which the cancellation walk writes and reads. Every
/// other field is read and written by the owning fiber only, so its lock
/// is always free when it is taken; those locks make the block `Sync`,
/// which it must be for the scheduler, at the cost of one uncontended
/// atomic each. No guard is held across a switch (I-5).
#[derive(Debug, Default)]
pub(crate) struct FiberConc {
    /// The fiber lock: the cancellation target and the wait node.
    pub(crate) lock: cancel::FiberLock,
    /// The scope the fiber was spawned into; `None` for a root fiber and an
    /// isolated entry.
    spawn_scope: Mutex<Option<Arc<ScopeState>>>,
    /// The scopes the fiber's frames have open, innermost last
    /// (architecture §7.5).
    open_scopes: Mutex<Vec<Arc<ScopeState>>>,
    /// Why the scope that `scope_run` last closed on this fiber failed, for
    /// `take_failure` (architecture §5.3).
    last_failure: Mutex<Option<Failure>>,
    /// The value the last `select` chose and the random state that orders
    /// a select's arms (D-673, D-695).
    pub(crate) select: select::SelectState,
    /// The group the detached fibers it spawns join
    /// (GAP(detached-groups)): a root's is given by its host
    /// (`Runtime::run_root_in`, the runtime's default group for
    /// `run_root`), and a spawned fiber inherits its spawner's, as it
    /// inherits its domain (D-691). Set before the fiber is first queued;
    /// `None` for an isolated entry.
    group: Mutex<Option<Arc<DetachedGroup>>>,
}

/// Why a scope failed (architecture §5.3), as `take_failure` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    /// 0 for a panic, 1 for an expired limit.
    pub(crate) kind: i64,
    /// The panic's message; empty for an expired limit.
    pub(crate) message: String,
    /// The panic's location; empty for an expired limit.
    pub(crate) location: String,
}

impl Failure {
    /// The failure of a panic, with its message and location.
    pub(crate) fn panic(message: String, location: String) -> Self {
        Self {
            kind: 0,
            message,
            location,
        }
    }
}

impl FiberConc {
    /// The scope the fiber was spawned into.
    pub(crate) fn spawn_scope(&self) -> Option<Arc<ScopeState>> {
        self.spawn_scope.lock().clone()
    }

    /// Records the scope the fiber was spawned into; `fiber_main` does it
    /// before the spawned function runs.
    pub(crate) fn set_spawn_scope(&self, scope: Option<Arc<ScopeState>>) {
        *self.spawn_scope.lock() = scope;
    }

    /// The innermost scope the fiber has open.
    pub(crate) fn innermost_scope(&self) -> Option<Arc<ScopeState>> {
        self.open_scopes.lock().last().cloned()
    }

    /// The scope responsible for the fiber's position (D-666, architecture
    /// §6.6 and §7.5): its innermost open scope, else the scope it was
    /// spawned into; `None` for a root fiber or an isolated entry outside
    /// every scope. A new scope's parent, and where a panic is recorded.
    pub(crate) fn responsible_scope(&self) -> Option<Arc<ScopeState>> {
        self.innermost_scope().or_else(|| self.spawn_scope())
    }

    /// Pushes a scope the fiber opened.
    pub(crate) fn push_scope(&self, scope: Arc<ScopeState>) {
        self.open_scopes.lock().push(scope);
    }

    /// Pops the innermost open scope, which must be `scope`; a mismatch is
    /// a broken invariant of the `scope_run` intrinsic (I-7).
    pub(crate) fn pop_scope(&self, scope: &Arc<ScopeState>) {
        let popped = self.open_scopes.lock().pop();
        if !popped.is_some_and(|popped| Arc::ptr_eq(&popped, scope)) {
            fatal(format_args!(
                "a scope was closed that is not the fiber's innermost open scope"
            ));
        }
    }

    /// The number of scopes the fiber has open.
    #[cfg(test)]
    pub(crate) fn open_scope_count(&self) -> usize {
        self.open_scopes.lock().len()
    }

    /// Stores why the scope just closed failed.
    pub(crate) fn set_last_failure(&self, failure: Failure) {
        *self.last_failure.lock() = Some(failure);
    }

    /// Takes why the last scope closed on this fiber failed.
    pub(crate) fn take_last_failure(&self) -> Option<Failure> {
        self.last_failure.lock().take()
    }

    /// The group the detached fibers the fiber spawns join.
    pub(crate) fn group(&self) -> Option<Arc<DetachedGroup>> {
        self.group.lock().clone()
    }

    /// Records the fiber's group, before it is first queued.
    pub(crate) fn set_group(&self, group: Option<Arc<DetachedGroup>>) {
        *self.group.lock() = group;
    }
}

/// The std modules whose every kernel is registered here (plan §2.5).
pub(crate) const COMPLETE: &[&str] = &["channel", "fiber", "selector"];

/// A kernel: its symbol, its address, its C parameter types after the
/// context pointer, and its C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every kernel of `std::channel`, `std::fiber` and `std::selector` (plan
/// §2.5, and the stubs of plan §7 UQ-2's answer).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 13] {
    [
        (
            "cheby_selector_sys_new",
            select::cheby_selector_sys_new as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_selector_sys_add_receive",
            select::cheby_selector_sys_add_receive as *const u8,
            &["Word", "Word", "Word"],
            "Word",
        ),
        (
            "cheby_selector_sys_add_after",
            select::cheby_selector_sys_add_after as *const u8,
            &["Word", "i64", "i64", "Word"],
            "Word",
        ),
        (
            "cheby_selector_sys_wait",
            select::cheby_selector_sys_wait as *const u8,
            &["Word"],
            "Word",
        ),
        (
            "cheby_selector_take_selected",
            select::cheby_selector_take_selected as *const u8,
            &["Word"],
            "Word",
        ),
        (
            "cheby_channel_new",
            channel::cheby_channel_new as *const u8,
            &["i64"],
            "Word",
        ),
        (
            "cheby_channel_send",
            channel::cheby_channel_send as *const u8,
            &["Word", "Word"],
            "Word",
        ),
        (
            "cheby_channel_receive",
            channel::cheby_channel_receive as *const u8,
            &["Word"],
            "Word",
        ),
        (
            "cheby_fiber_sys_spawn",
            spawn::cheby_fiber_sys_spawn as *const u8,
            &["Word", "Word"],
            "Word",
        ),
        (
            "cheby_fiber_sys_join",
            join::cheby_fiber_sys_join as *const u8,
            &["Word"],
            "Word",
        ),
        (
            "cheby_fiber_take_failure",
            scope::cheby_fiber_take_failure as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_fiber_sys_spawn_with_stack",
            spawn::cheby_fiber_sys_spawn_with_stack as *const u8,
            &["Word", "i64", "Word"],
            "Word",
        ),
        (
            "cheby_fiber_sys_spawn_detached",
            detached::cheby_fiber_sys_spawn_detached as *const u8,
            &["Word"],
            "()",
        ),
    ]
}

/// Appends every `std::channel`, `std::fiber` and `std::selector` kernel
/// symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The C signatures of the kernels, without the leading context pointer
/// every one of them takes (`@std_context`, D-692), for the cross-check
/// against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

/// Ends the program because `operation`, a std function's path such as
/// `channel::receive`, would start a fiber or wait in an entry that runs no
/// scheduler (D-692, architecture §5.1). On Windows, where every entry is
/// such an entry until M6, a note says so (D-714).
pub(crate) fn needs_scheduler(operation: &str) -> ! {
    if cfg!(windows) {
        fatal(format_args!(
            "{operation} needs the fiber scheduler, which this entry does not run\n\
             note: fibers on Windows arrive in M6"
        ))
    }
    fatal(format_args!(
        "{operation} needs the fiber scheduler, which this entry does not run"
    ))
}

/// The running fiber's control block, given to a kernel as its context
/// argument.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block, which lives while
/// the kernel runs.
pub(crate) unsafe fn running<'a>(ctx: *mut ExecCtx) -> &'a ExecCtx {
    // SAFETY: forwarded from the caller.
    match unsafe { ctx.as_ref() } {
        Some(ctx) => ctx,
        None => fatal(format_args!(
            "a fiber or channel kernel ran without an execution context"
        )),
    }
}

/// One owned reference to a Cheby value, released when the guard is
/// dropped, also when a raise unwinds the kernel's Rust frames (D-697,
/// I-4).
#[derive(Debug)]
pub(crate) struct Owned(Word);

impl Owned {
    /// Takes ownership of one reference to `word`.
    ///
    /// # Safety
    ///
    /// `word` is a tagged integer, 0, or a live value of which the caller
    /// gives up one reference it owns.
    pub(crate) unsafe fn new(word: Word) -> Self {
        Self(word)
    }

    /// The word, still owned by the guard.
    pub(crate) fn word(&self) -> Word {
        self.0
    }

    /// Gives the reference up to the caller.
    pub(crate) fn into_word(self) -> Word {
        let word = self.0;
        std::mem::forget(self);
        word
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the guard owns one reference to a live value (the
        // contract of `new`), released once, here.
        unsafe { rc::drop(self.0) };
    }
}

#[cfg(test)]
mod cancel_races;
#[cfg(test)]
mod cancel_tests;
#[cfg(test)]
pub(crate) mod detached_tests;
#[cfg(test)]
mod models;
#[cfg(test)]
mod select_models;
#[cfg(test)]
mod select_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod timeout_tests;
