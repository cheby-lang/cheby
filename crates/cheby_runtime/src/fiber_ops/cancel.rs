//! Cancellation (D-666, D-667, D-688, D-706, architecture §3.1 and §7.5):
//! the fiber lock, the subtree walk that cancels a scope and everything
//! below it, the check a fiber makes before it parks, and the publication
//! of the wait node a parked fiber is reached through.
//!
//! # The fiber lock
//!
//! [`FiberLock`] guards a fiber's cancellation target and its wait-node
//! pointer, the only control-block state other fibers touch (architecture
//! §3.1). It is a leaf: it is taken after a scope lock (the walk) or a
//! channel or join cell lock (a waiter publishing its node), never held
//! while another lock is taken, across a switch (I-5) or while user code
//! runs, and `sched::wake` is never called under it.
//!
//! # Targets
//!
//! A fiber's target is the scope its cancellation unwinds to (D-667): the
//! outermost of the scopes whose cancellation reached it. It is set by the
//! walk ([`cancel`]) and by a spawn into a cancelled scope, and cleared only
//! by the close of the target itself ([`at_close`]) or when the fiber ends.
//! Every target is a scope the fiber's position is inside, so two targets
//! of one fiber are always on one chain of parents, and their depths tell
//! which is outer. Delivery is level-triggered from the target (D-688):
//! the walk sets the target before it requests CANCEL, under the fiber
//! lock, so the CANCEL reason only forces the next entry check into the
//! slow path, which decides from the target ([`check`]).
//!
//! # Wait nodes
//!
//! A fiber that parks in a cancellable wait (`send`, `receive`, `join`)
//! calls `sched::begin_park`, links its node under the channel's or join
//! cell's lock, then publishes it here under its fiber lock ([`publish`]),
//! which refuses if a target is set: the waiter then unlinks its node and
//! raises instead of parking. The walk, under the fiber lock, CASes a
//! published node `Waiting -> Cancelled` and wakes the fiber after
//! releasing its locks. The fiber clears the pointer under its fiber lock
//! once it runs again ([`unpublish`]), before it leaves the node's frame,
//! so the walk never touches a dead node.

use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;

use parking_lot::Mutex;

use super::scope::ScopeState;
use crate::ctx::{self, ExecCtx, reason};
use crate::sched::{FiberRef, WaitHeader, wake};

/// A fiber must unwind for a cancellation ([`check`], [`publish`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

/// The fiber lock (module documentation).
#[derive(Default)]
pub(crate) struct FiberLock(Mutex<Locked>);

impl fmt::Debug for FiberLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let locked = self.0.lock();
        f.debug_struct("FiberLock")
            .field("targeted", &locked.target.is_some())
            .field("waiting", &locked.wait.is_some())
            .finish()
    }
}

/// What the fiber lock guards.
#[derive(Default)]
struct Locked {
    /// The scope the fiber's cancellation unwinds to, if one reached it.
    target: Option<Arc<ScopeState>>,
    /// The header of the node the fiber is parked on in a cancellable wait.
    wait: Option<WaitPtr>,
}

/// A published wait node's header: on the waiting fiber's stack, which it
/// does not leave before it clears the pointer under its fiber lock.
struct WaitPtr(NonNull<WaitHeader>);

// SAFETY: the pointer is used only under the fiber lock, while the node it
// points to is live (module documentation); the header it reaches is made
// for other threads (an atomic state and a fiber reference).
unsafe impl Send for WaitPtr {}

/// What a scope's close finds as its owner's target (architecture §7.5,
/// step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtClose {
    /// No cancellation reached the owner.
    Untargeted,
    /// The closing scope was the target: it is cleared, with the CANCEL
    /// reason, and the scope returns per its first cause.
    This,
    /// A proper ancestor of the closing scope is the target: the
    /// cancellation passes through.
    Outer,
}

/// Sets `fiber`'s target to `target` if it has none or a deeper one (the
/// outermost wins, D-667). Called under the fiber lock.
fn retarget(locked: &mut Locked, target: &Arc<ScopeState>) {
    let replace = locked
        .target
        .as_ref()
        .is_none_or(|current| current.depth() > target.depth());
    if replace {
        locked.target = Some(Arc::clone(target));
    }
}

/// The walk's step for one fiber, a member or the owner of a scope in the
/// target's subtree, called under that scope's lock (scope lock, then fiber
/// lock): sets the target (the outermost wins), requests CANCEL, and CASes
/// a published wait node `Waiting -> Cancelled`. Returns whether it won the
/// node, in which case the caller wakes the fiber once its locks are
/// released: the fiber stays parked, and so alive, until then.
pub(super) fn reach(fiber: &ExecCtx, target: &Arc<ScopeState>) -> bool {
    let mut locked = fiber.conc.lock.0.lock();
    retarget(&mut locked, target);
    ctx::request(fiber, reason::CANCEL);
    locked.wait.as_ref().is_some_and(|node| {
        // SAFETY: a published node is live while its fiber's lock is held
        // (module documentation).
        unsafe { node.0.as_ref() }.try_cancel()
    })
}

/// Cancels `target`'s subtree, parent first (D-667, D-706). For each scope,
/// under its lock: skips it if it has ended; marks it cancelled; for each
/// member and the owner, under that fiber's lock (scope lock, then fiber
/// lock), sets the target (the outermost wins), requests CANCEL and CASes
/// a parked waiter's node `Waiting -> Cancelled`; keeps only the fibers
/// whose node it won and the nested scopes (as upgraded `Arc`s). Then
/// releases the scope lock, wakes the won fibers and goes on with the
/// nested scopes. Never acts on a member after releasing its scope's lock
/// (a member can retire). An explicit worklist: no recursion in Rust or
/// over the scope tree (D-700's headroom). A detached fiber belongs to no
/// scope of its spawner, only to its own parentless base scope, so only a
/// teardown's walk of that base reaches it (D-669,
/// D-798). Entered with no runtime lock held.
pub(crate) fn cancel(target: &Arc<ScopeState>) {
    let mut scopes = vec![Arc::clone(target)];
    let mut won: Vec<FiberRef> = Vec::new();
    while let Some(scope) = scopes.pop() {
        scope.cancel_step(target, &mut won, &mut scopes);
        // The scope's lock is released: wake outside every lock.
        for fiber in won.drain(..) {
            // SAFETY: the walk won the fiber's node, so the fiber is parked
            // or parking on it and nothing else wakes it: it cannot run, so
            // cannot retire, before this wake.
            unsafe { wake(fiber, false) };
        }
    }
}

/// Whether the running fiber must unwind for a cancellation: its target,
/// read under its fiber lock (level-triggered, D-688). A CANCEL reason
/// without a target is stale (the fiber's own close cleared both, or is
/// about to) and is not a cancellation.
pub(crate) fn check(me: &ExecCtx) -> Result<(), Cancelled> {
    if me.conc.lock.0.lock().target.is_some() {
        Err(Cancelled)
    } else {
        Ok(())
    }
}

/// Whether the running fiber is being cancelled, decided from its position
/// as well as its target (D-666, D-667, D-668): the walk sets targets one
/// scope at a time, parent first, so a fiber deeper in a cancelled subtree
/// can learn of the cancellation's effects (a joined fiber that failed for
/// it, a channel that its unwinding closed) before the walk reaches that
/// fiber. Yet the cancellation reached the whole subtree at the moment the
/// scope failed (D-667), so the outcome must not depend on how far the walk
/// has come, which would make it depend on the thread count (D-232).
///
/// Walks from `from` up through the parents, taking each scope's lock in
/// turn and releasing it before the next, and finds the outermost scope
/// that has failed or is being cancelled. If there is one, the running
/// fiber takes it as its target ([`set_target`]: the outermost wins) and
/// the answer is [`Cancelled`]; else the answer is [`check`]'s. The walk
/// marks a scope under its lock, parent first, before it reaches any of
/// that scope's fibers, and a first cause is recorded before its walk
/// starts, so every effect of a cancellation is preceded by the mark of a
/// scope on the chain of every fiber it reaches. Every scope on the chain
/// is open: a fiber's open scopes and the scope it was spawned into, and
/// their ancestors, cannot end before it leaves them (I-7).
///
/// `from` is the running fiber's responsible scope (a joiner, a waiter
/// woken by a close) or the parent of the scope it closes (step 3 of a
/// close). Entered with no runtime lock held.
///
/// (D-769) A fiber is being cancelled when it has a target or when a scope
/// on its chain is marked. This is decided where a cancellation's effects
/// can reach a fiber the walk has not reached yet: a join that finds the
/// fiber failed (D-668), a `send`, `receive` or `select` woken by the
/// channel's other end going away (it unwinds and takes nothing, as a
/// cancelled wait does, D-670), step 3 of a scope's close (D-667), and a
/// spawn (D-666). Every other check reads the target alone.
pub(crate) fn check_chain(me: &ExecCtx, from: Option<Arc<ScopeState>>) -> Result<(), Cancelled> {
    match outermost_failing(from) {
        Some(scope) => {
            set_target(me, &scope);
            Err(Cancelled)
        }
        None => check(me),
    }
}

/// The outermost scope from `from` up through its parents that has failed
/// or is being cancelled ([`check_chain`]), each scope's lock taken in turn
/// and released before the next. Entered with no runtime lock held.
pub(crate) fn outermost_failing(from: Option<Arc<ScopeState>>) -> Option<Arc<ScopeState>> {
    let mut outermost = None;
    let mut next = from;
    while let Some(scope) = next {
        next = scope.parent().cloned();
        if scope.failing() {
            outermost = Some(scope);
        }
    }
    outermost
}

/// Publishes the running fiber's wait node, already linked under the lock
/// of the channel or join cell the fiber parks with, after `begin_park`:
/// under the fiber lock, refuses with [`Cancelled`] if a target is set,
/// else stores the pointer, so that a walk that sets a target later finds
/// the node (survey-u5 §2.3, hazard 1).
///
/// # Safety
///
/// `node` is the header of the caller's pinned node, which stays live until
/// the caller has called [`unpublish`].
pub(crate) unsafe fn publish(me: &ExecCtx, node: &WaitHeader) -> Result<(), Cancelled> {
    let mut locked = me.conc.lock.0.lock();
    if locked.target.is_some() {
        return Err(Cancelled);
    }
    locked.wait = Some(WaitPtr(NonNull::from(node)));
    Ok(())
}

/// Clears the running fiber's wait-node pointer under its fiber lock, once
/// the fiber runs again, before it leaves the node's frame.
pub(crate) fn unpublish(me: &ExecCtx) {
    me.conc.lock.0.lock().wait = None;
}

/// Gives `fiber` the target `target` unless it has an outer one, and
/// requests CANCEL: a fiber spawned into a cancelled scope (D-666,
/// survey-u5 §2.3 hazard 2), and the test hook. Takes only the fiber lock.
pub(crate) fn set_target(fiber: &ExecCtx, target: &Arc<ScopeState>) {
    let mut locked = fiber.conc.lock.0.lock();
    retarget(&mut locked, target);
    ctx::request(fiber, reason::CANCEL);
}

/// Step 3 of a scope's close (architecture §7.5, D-706): reads the owner's
/// target under its fiber lock and, if it is `scope` itself, clears it and
/// the CANCEL reason under that lock, so that a concurrent cancellation of
/// an outer scope is ordered before (and passes through) or after (and
/// sets a new target, delivered at the next entry check), and never lost.
pub(crate) fn at_close(me: &ExecCtx, scope: &Arc<ScopeState>) -> AtClose {
    let cleared = {
        let mut locked = me.conc.lock.0.lock();
        match &locked.target {
            None => return AtClose::Untargeted,
            Some(target) if Arc::ptr_eq(target, scope) => {
                ctx::clear_reason(me, reason::CANCEL);
                locked.target.take()
            }
            Some(_) => return AtClose::Outer,
        }
    };
    // The scope's `Arc` is released outside the lock.
    drop(cleared);
    AtClose::This
}

/// Drops the running fiber's target as it ends (`fiber_main`), outside the
/// fiber lock: the fiber holds no scope once it has left.
pub(super) fn clear_at_end(me: &ExecCtx) {
    let target = me.conc.lock.0.lock().target.take();
    drop(target);
}

#[cfg(test)]
pub(crate) mod hooks {
    //! What the crate's tests read of the fiber lock.

    use std::sync::Arc;

    use super::super::scope::ScopeState;
    use crate::ctx::ExecCtx;

    /// The fiber's target.
    pub(crate) fn target(fiber: &ExecCtx) -> Option<Arc<ScopeState>> {
        fiber.conc.lock.0.lock().target.clone()
    }

    /// Whether the fiber has published a wait node (it is parked, or about
    /// to park, in a cancellable wait).
    pub(crate) fn published(fiber: &ExecCtx) -> bool {
        fiber.conc.lock.0.lock().wait.is_some()
    }

    /// Clears the fiber's target, as its target's close would.
    pub(crate) fn clear_target(fiber: &ExecCtx) {
        let target = fiber.conc.lock.0.lock().target.take();
        drop(target);
    }

    /// Runs only the first step of the walk that cancels `target`: marks
    /// it, reaches its members and its owner, and wakes the fibers whose
    /// node it won, leaving the nested scopes unvisited, as the walk leaves
    /// them while it wakes (review finding CONC-W2-1). Entered with no
    /// runtime lock held.
    pub(crate) fn cancel_first_step(target: &Arc<ScopeState>) {
        let mut won = Vec::new();
        let mut nested = Vec::new();
        target.cancel_step(target, &mut won, &mut nested);
        drop(nested);
        for fiber in won {
            // SAFETY: as in `cancel`: the step won the fiber's node, so it
            // is parked or parking on it and nothing else wakes it.
            unsafe { crate::sched::wake(fiber, false) };
        }
    }
}
