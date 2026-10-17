//! Scopes (D-666, D-693, architecture §5.3 and §7.5): the open and close
//! that the compiler-generated body of the `scope_run` intrinsic calls,
//! with the body called in between and no Cheby call around it, and the
//! kernel `take_failure` that tells `std::fiber::scope` why a scope failed.
//!
//! A scope's state ([`ScopeState`]) is shared by its `Scope` handle (kind
//! 43), the fibers spawned into it and the scopes nested in it. It knows
//! its parent, its owner (the fiber that opened it) and its depth; its lock
//! guards the members, the nested scopes, the first cause and the
//! `cancelled` and `ended` flags. Lock order (architecture §7.5, the
//! module documentation of `fiber_ops`): a parent's lock before a child's,
//! then a fiber lock; a scope lock is never held together with a channel
//! lock, and never across a switch (I-5): the owner waiting in close hands
//! it to `park`.
//!
//! A scope's first cause is the first panic, by start, of a fiber
//! responsible to it: a member, or the body of the fiber that opened it
//! (D-666). The panic is recorded when it starts
//! ([`ScopeState::record_panic`], called by `unwind::record::start_panic`),
//! before the panicking fiber unwinds, so a panic that starts later never
//! replaces it, whichever fiber ends first; at that moment the scope is
//! cancelled (`cancel::cancel`, D-667), which reaches every other member,
//! the owner and every scope nested inside. A scope cancelled from outside
//! records no cause: its fibers' panics are caused by the cancellation
//! (D-178).
//!
//! A **timed scope** is the scope of `fiber::timeout` (D-671): its open
//! arms a timer ([`ScopeTimer`]) whose firing cancels the scope, with the
//! scope as the target, and nothing else. The firing records no first
//! cause (D-790). `TimedOut` is decided by the close, from
//! the body's unwinding: a body that unwound for this scope's own
//! cancellation, with no first cause, fails as an expired limit (kind 1,
//! empty texts). So an `f` that returns before the cancellation reaches it
//! gives `Ok`, even when the limit has passed by then, and one that panics
//! first gives its panic (D-671, spec §10.8). The close cancels the timer
//! on every path (step 2), and a firing that loses the race to the scope's
//! end does nothing, since the walk skips an ended scope.

use std::sync::{Arc, Weak};
use std::time::Instant;

use parking_lot::Mutex;

use super::cancel::{self, AtClose};
use super::{Failure, Owned, needs_scheduler, running};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind, HandlePayload};
use crate::panic::{PanicReport, fatal};
use crate::sched::{
    AfterSwitch, Deadline, FiberRef, TimerHandle, TimerTarget, Unlock, WaitOp, deadline_after,
    park, wake,
};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::unwind::record::{UnwindRecord, take_record};
use crate::values;

/// The state of one scope (architecture §7.5).
#[derive(Debug)]
pub(crate) struct ScopeState {
    /// Everything other fibers change.
    inner: Mutex<ScopeInner>,
    /// The scope enclosing the opening fiber's position when it opened:
    /// that fiber's innermost open scope, else the scope it was spawned
    /// into.
    parent: Option<Arc<ScopeState>>,
    /// The fiber that opened the scope and closes it. It cannot end before
    /// the scope has ended (I-7), so the walk may reach it while it holds
    /// the scope's lock and finds the scope not ended.
    owner: FiberRef,
    /// The number of proper ancestors: 0 for a scope without a parent. Of
    /// two scopes on one chain of parents, the outer has the smaller depth,
    /// which is how a fiber's outermost target is chosen (D-667).
    depth: u32,
    /// The scope's slot among its parent's nested scopes, set by the open
    /// once it is linked; only the fiber that opened the scope reads it,
    /// at the close.
    nested_slot: Mutex<Option<usize>>,
    /// The timer of a timed scope (D-671), set by the open once the scope
    /// is linked and cancelled by the close (step 2); only the fiber that
    /// opened the scope touches it.
    timer: Mutex<Option<TimerHandle>>,
    /// Whether the scope was opened timed, by `fiber::timeout` (D-671),
    /// whatever its deadline: its close's wait is named `fiber::timeout` in
    /// a deadlock report, any other's `fiber::scope` (D-858).
    of_timeout: bool,
    /// Whether this is a detached fiber's base scope
    /// (D-798): parentless, owned by the detached fiber,
    /// its spawn scope, the root of every scope it opens; it never has
    /// members, records no first cause (D-669) and is ended by the
    /// fiber's base frame ([`ScopeState::end_base`]). A root's base scope
    /// (D-859, [`open_root_base`]) is one too.
    base: bool,
    /// Whether this is a root's base scope (D-859, [`open_root_base`]):
    /// a base scope owned by a root, which only a deadlock's cancellation
    /// targets (`cancel::cancel_deadlocked_root`), ended by the root's
    /// base frame ([`close_root_base`]).
    root: bool,
}

/// The target of a timed scope's timer (D-671): it holds the scope weakly,
/// so a timer still in the heap keeps no scope alive, and when it fires it
/// cancels the scope, if the scope still exists, with the scope as the
/// target (`cancel::cancel`, D-667), and does nothing else.
/// D-790: it records no first cause, so `TimedOut` comes
/// from the close (module documentation). It runs with no runtime lock
/// held (`TimerTarget`), and a scope that has ended is skipped by the walk
/// (`ScopeState::cancel_step`), so a firing that loses the race to the
/// close is harmless.
#[derive(Debug)]
struct ScopeTimer(Weak<ScopeState>);

impl TimerTarget for ScopeTimer {
    fn fire(&self) {
        if let Some(scope) = self.0.upgrade() {
            cancel::cancel(&scope);
        }
    }
}

/// The part of a scope under its lock.
#[derive(Debug, Default)]
struct ScopeInner {
    /// The fibers spawned into the scope that have not left it yet.
    members: Members,
    /// The scopes opened inside its region that have not closed yet, for
    /// cancellation's walk of a subtree.
    nested: Nested,
    /// The first panic of the scope (D-666).
    first_cause: Option<Failure>,
    /// Set by the cancellation walk, or inherited from the parent when the
    /// scope links (survey-u5 §2.3, hazard 3): the scope's fibers are being
    /// cancelled, so a spawn into it starts cancelled, and a panic in it is
    /// not its first cause unless it was the cause of the cancellation.
    cancelled: bool,
    /// Set by the close once every member has left: spawning panics from
    /// then on (D-666).
    ended: bool,
    /// The owner waiting in close for the last member.
    waiter: Option<FiberRef>,
}

/// The members of a scope: a slab, so that joining and leaving are O(1)
/// whatever the number of members, and a member knows its slot.
#[derive(Debug, Default)]
struct Members {
    /// The slots, by index.
    slots: Vec<Slot>,
    /// The free slots.
    free: Vec<usize>,
    /// The slots that are not free.
    count: usize,
}

/// A member slot of a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Free.
    Free,
    /// Reserved by a spawn that has not created its fiber yet.
    Reserved,
    /// Held by a member fiber, which cancellation reaches through it.
    Member(FiberRef),
}

impl Members {
    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Reserves a slot for a fiber about to be spawned: from now on the
    /// scope waits for it.
    fn reserve(&mut self) -> usize {
        self.count = self.count.saturating_add(1);
        if let Some(index) = self.free.pop()
            && let Some(slot) = self.slots.get_mut(index)
        {
            *slot = Slot::Reserved;
            return index;
        }
        self.slots.push(Slot::Reserved);
        self.slots.len().saturating_sub(1)
    }

    /// Records the fiber of a reserved slot.
    fn fill(&mut self, index: usize, fiber: FiberRef) {
        match self.slots.get_mut(index) {
            Some(slot @ Slot::Reserved) => *slot = Slot::Member(fiber),
            _ => fatal(format_args!(
                "a scope member filled a slot it did not reserve"
            )),
        }
    }

    /// Frees a slot: its fiber has left, or was never created.
    fn leave(&mut self, index: usize) {
        match self.slots.get_mut(index) {
            Some(slot @ (Slot::Reserved | Slot::Member(_))) => *slot = Slot::Free,
            _ => fatal(format_args!("a scope member left a slot it did not hold")),
        }
        self.free.push(index);
        self.count = self.count.saturating_sub(1);
    }
}

/// The nested scopes of a scope: a slab, like [`Members`], so that linking
/// and unlinking a scope cost O(1) however many siblings are open (review
/// finding R3-ROB-1: scanning them made closing k open siblings O(k²),
/// under the parent's lock; `fiber::scope` costs O(1) plus the wait).
#[derive(Debug, Default)]
struct Nested {
    /// The slots, by index; `None` is free.
    slots: Vec<Option<Weak<ScopeState>>>,
    /// The free slots.
    free: Vec<usize>,
}

impl Nested {
    /// Links `scope` and returns its slot.
    fn insert(&mut self, scope: Weak<ScopeState>) -> usize {
        if let Some(index) = self.free.pop()
            && let Some(slot) = self.slots.get_mut(index)
        {
            *slot = Some(scope);
            return index;
        }
        self.slots.push(Some(scope));
        self.slots.len().saturating_sub(1)
    }

    /// Unlinks the scope at `state` from its slot `index`.
    fn remove(&mut self, index: usize, state: *const ScopeState) {
        match self.slots.get_mut(index) {
            Some(slot @ Some(_))
                if slot
                    .as_ref()
                    .is_some_and(|nested| std::ptr::eq(nested.as_ptr(), state)) =>
            {
                *slot = None;
            }
            _ => fatal(format_args!("a scope left a nested slot it did not hold")),
        }
        self.free.push(index);
    }

    /// How many scopes are linked.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len().saturating_sub(self.free.len())
    }
}

/// What a spawn learned when it joined a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Membership {
    /// The scope has ended: the spawn panics (D-666).
    Ended,
    /// A reserved member slot. Whether the new fiber starts cancelled is
    /// learned when the slot is filled ([`ScopeState::fill`]).
    Member(usize),
}

impl ScopeState {
    /// A scope opened by `owner` inside `parent`.
    pub(crate) fn new(parent: Option<Arc<Self>>, owner: FiberRef) -> Self {
        let depth = parent
            .as_ref()
            .map_or(0, |parent| parent.depth.saturating_add(1));
        Self {
            inner: Mutex::new(ScopeInner::default()),
            parent,
            owner,
            depth,
            nested_slot: Mutex::new(None),
            timer: Mutex::new(None),
            of_timeout: false,
            base: false,
            root: false,
        }
    }

    /// The base scope of the detached fiber `owner`
    /// (D-798): no parent, so depth 0, and no
    /// cancellation of the spawner's scopes reaches it (D-669, D-671).
    /// Every scope the fiber opens hangs below it, so a teardown cancels
    /// the fiber and everything it started with `cancel::cancel(&base)`
    /// (D-708, D-710).
    pub(crate) fn new_base(owner: FiberRef) -> Self {
        Self {
            base: true,
            ..Self::new(None, owner)
        }
    }

    /// Ends a detached fiber's base scope, under its lock: its base frame
    /// calls it before it leaves its group and retires, so a teardown's
    /// walk that comes later skips the scope (`cancel_step`) and never
    /// reaches the retired owner. A base scope has no members.
    pub(crate) fn end_base(&self) {
        let mut inner = self.inner.lock();
        if !self.base || !inner.members.is_empty() {
            drop(inner);
            fatal(format_args!(
                "a scope that is not an empty base scope was ended as one"
            ));
        }
        inner.ended = true;
    }

    /// The base scope of the root `owner` (D-859, D-798's form): a base
    /// scope ([`ScopeState::new_base`]) that is also marked as a root's.
    fn new_root_base(owner: FiberRef) -> Self {
        Self {
            root: true,
            ..Self::new_base(owner)
        }
    }

    /// Whether this is a root's base scope ([`open_root_base`]).
    pub(crate) fn is_root_base(&self) -> bool {
        self.root
    }

    /// Stores `report` on the root `root` that owns this root base scope,
    /// under the scope's lock, unless the scope has ended (D-859): then the
    /// root has left its body, and may have retired, so nothing is stored
    /// and the result is false. While the scope has not ended under its
    /// lock, its owner is not retired: the root's base frame ends it
    /// ([`close_root_base`]) before the root reports, unregisters and
    /// retires. The report's lock is a leaf (the module documentation of
    /// `fiber_ops`). A second report for one root is a broken invariant:
    /// the detector reports a root once.
    pub(crate) fn store_deadlock_report(&self, root: FiberRef, report: String) -> bool {
        let inner = self.inner.lock();
        if !self.root || self.owner != root {
            drop(inner);
            fatal(format_args!(
                "a deadlock was reported on a scope that is not the root's base scope"
            ));
        }
        if inner.ended {
            return false;
        }
        // SAFETY: the owner of a root base scope that has not ended under
        // this lock is not retired (above); the reference is not used after
        // the lock is released.
        let owner = unsafe { root.ctx() };
        let mut slot = owner.conc.deadlock_report.lock();
        if slot.is_some() {
            drop(slot);
            drop(inner);
            fatal(format_args!("a root was reported deadlocked twice"));
        }
        *slot = Some(report);
        true
    }

    /// The number of proper ancestors of the scope.
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    /// The scope enclosing the opening fiber's position when it opened.
    pub(crate) fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    /// Whether the scope has failed or is being cancelled, read under its
    /// lock: it has a first cause, whose walk is about to run or running,
    /// or a walk has marked it (`cancel::check_chain`).
    pub(crate) fn failing(&self) -> bool {
        let inner = self.inner.lock();
        inner.cancelled || inner.first_cause.is_some()
    }

    /// Links the new scope `state` into its parent's nested scopes. Under
    /// the parent's lock (then the new scope's, parent before child) the
    /// new scope inherits whether the parent is being cancelled (survey-u5
    /// §2.3, hazard 3): a walk that visited the parent before the link set
    /// the opener's target already, and the new scope must not accept
    /// spawns as if it were not cancelled; a walk after the link finds it
    /// among the parent's nested scopes.
    pub(crate) fn link(state: &Arc<Self>) {
        if let Some(parent) = &state.parent {
            let index = {
                let mut outer = parent.inner.lock();
                let index = outer.nested.insert(Arc::downgrade(state));
                if outer.cancelled || outer.first_cause.is_some() {
                    state.inner.lock().cancelled = true;
                }
                index
            };
            *state.nested_slot.lock() = Some(index);
        }
    }

    /// Cancellation's walk at this scope (`cancel::cancel`, D-667, D-706),
    /// under its lock: does nothing if the scope has ended; otherwise marks
    /// it cancelled, reaches every member and the owner under their fiber
    /// locks (`cancel::reach`), adds to `won` the fibers whose wait node the
    /// walk won, and adds every nested scope still alive to `nested`. The
    /// caller wakes `won` after this returns, with no lock held.
    pub(crate) fn cancel_step(
        &self,
        target: &Arc<Self>,
        won: &mut Vec<FiberRef>,
        nested: &mut Vec<Arc<Self>>,
    ) {
        let mut inner = self.inner.lock();
        if inner.ended {
            return;
        }
        inner.cancelled = true;
        let members = inner.members.slots.iter().filter_map(|slot| match slot {
            Slot::Member(fiber) => Some(*fiber),
            Slot::Free | Slot::Reserved => None,
        });
        for fiber in members.chain(std::iter::once(self.owner)) {
            // SAFETY: a member that has not left is not retired, since it
            // leaves under this lock before it ends; the owner closes the
            // scope, which has not ended under this lock, before it can
            // end (I-7). Neither is used after the lock is released.
            if cancel::reach(unsafe { fiber.ctx() }, target) {
                won.push(fiber);
            }
        }
        nested.extend(
            inner
                .nested
                .slots
                .iter()
                .flatten()
                .filter_map(Weak::upgrade),
        );
    }

    /// The teardown's survey at this scope (D-786, `sched::lifecycle`),
    /// under its lock, with no other lock held: returns false, doing
    /// nothing, if the scope has ended; otherwise calls `look` on the owner
    /// and on every member, then adds every nested scope still alive to
    /// `nested`. `look`'s flag says whether a walk from a base scope
    /// reaches the fiber here first: true for the members and for a base
    /// scope's owner, false for the owner of any other scope, which is a
    /// member of an enclosing scope or that base's owner. `look` may read
    /// only atomics of the control block and takes no lock: a member that
    /// has not left is not retired, and the owner cannot end before the
    /// scope has ended (I-7).
    pub(crate) fn survey_step(
        &self,
        look: &mut dyn FnMut(&ExecCtx, bool),
        nested: &mut Vec<Arc<Self>>,
    ) -> bool {
        let inner = self.inner.lock();
        if inner.ended {
            return false;
        }
        // SAFETY: as in `cancel_step`: neither the owner nor a member that
        // has not left can be retired while this lock is held and the scope
        // has not ended, and neither reference outlives the lock.
        look(unsafe { self.owner.ctx() }, self.base);
        for slot in &inner.members.slots {
            if let Slot::Member(fiber) = slot {
                // SAFETY: as above.
                look(unsafe { fiber.ctx() }, true);
            }
        }
        nested.extend(
            inner
                .nested
                .slots
                .iter()
                .flatten()
                .filter_map(Weak::upgrade),
        );
        true
    }

    /// Unlinks the closing scope from its parent's nested scopes.
    fn unlink(&self) {
        if let Some(parent) = &self.parent
            && let Some(index) = self.nested_slot.lock().take()
        {
            parent.inner.lock().nested.remove(index, self);
        }
    }

    /// How many nested scopes are linked to the scope.
    #[cfg(test)]
    pub(crate) fn nested_count(&self) -> usize {
        self.inner.lock().nested.len()
    }

    /// Reserves a member slot for a fiber about to be spawned (D-666): the
    /// scope accepts spawns while its body runs and while it waits in
    /// close, until it has ended.
    pub(crate) fn join(&self) -> Membership {
        let mut inner = self.inner.lock();
        if inner.ended {
            return Membership::Ended;
        }
        Membership::Member(inner.members.reserve())
    }

    /// Records `report` as the scope's first cause if it has none and is
    /// not cancelled from outside (D-666, architecture §6.6), under the
    /// scope's lock, then, after releasing that lock, cancels the scope
    /// (`cancel::cancel`, D-667) at that moment, before the panicking fiber
    /// unwinds: so a member that joins the panicking fiber is always being
    /// cancelled by then (D-668). Returns whether it recorded. Called by
    /// `record::start_panic` after the record's lock is released, for the
    /// top of `open_scopes` (a root's body inside `scope` included), else
    /// the spawn scope; a fiber with neither records nothing. Entered with
    /// no runtime lock held (plan §0 'Locks'). It allocates only the copy
    /// of the report's two texts and the walk's worklists, and only when it
    /// records.
    pub(crate) fn record_panic(self: &Arc<Self>, report: &PanicReport) -> bool {
        // D-669: a panic at a detached fiber's top level is reported by its
        // base frame, not recorded in its base scope, and cancels nothing
        // (D-798).
        if self.base {
            return false;
        }
        {
            let mut inner = self.inner.lock();
            if inner.first_cause.is_some() || inner.cancelled {
                return false;
            }
            inner.first_cause = Some(Failure::panic(
                report.message.clone(),
                report.location.clone(),
            ));
        }
        cancel::cancel(self);
        true
    }

    /// Records the fiber of the reserved slot `index`, and returns whether
    /// the scope has failed or is being cancelled, read under the same lock
    /// (survey-u5 §2.3, hazard 2): a walk that ran since the reservation
    /// found the slot without its fiber, so the spawner gives the new fiber
    /// its target before it is first scheduled (D-666), and its function
    /// is never called.
    pub(crate) fn fill(&self, index: usize, fiber: FiberRef) -> bool {
        let mut inner = self.inner.lock();
        inner.members.fill(index, fiber);
        inner.cancelled || inner.first_cause.is_some()
    }

    /// Leaves the member slot `index`: the member has ended, or its spawn
    /// failed before the fiber existed. The last member to leave wakes the
    /// owner waiting in close. For a fiber this is its last touch of shared
    /// state (D-694), so everything it released before happens before the
    /// owner wakes.
    pub(crate) fn leave(&self, index: usize) {
        let mut inner = self.inner.lock();
        inner.members.leave(index);
        if inner.members.is_empty()
            && let Some(owner) = inner.waiter.take()
        {
            // SAFETY: the owner registered as the waiter under this lock
            // and parks with it, so it is parking or parked, not retired.
            // The member is about to end, so the owner takes its runnext
            // slot.
            unsafe { wake(owner, true) };
        }
    }

    /// Whether the scope has ended.
    #[cfg(test)]
    pub(crate) fn ended(&self) -> bool {
        self.inner.lock().ended
    }

    /// Whether the scope holds a timer its close has not cancelled yet.
    #[cfg(test)]
    pub(crate) fn timed(&self) -> bool {
        self.timer.lock().is_some()
    }

    /// Fires a timed scope's timer target for `scope` by hand, as the heap
    /// would, whether or not a timer is armed (`timeout_tests`).
    #[cfg(test)]
    pub(crate) fn fire_timer_target(scope: &Arc<Self>) {
        ScopeTimer(Arc::downgrade(scope)).fire();
    }
}

/// The payload of a `Scope` handle (kind 43).
#[derive(Debug)]
pub(crate) struct ScopeHandle(pub(crate) Arc<ScopeState>);

impl HandlePayload for ScopeHandle {
    /// Releases the handle's share of the scope state (architecture §7.1).
    fn finalize(self: Box<Self>, _release: &mut dyn FnMut(Word)) {}
}

/// The scope state of the borrowed `Scope` handle `word`.
///
/// # Safety
///
/// `word` is a live `Scope` handle.
pub(crate) unsafe fn state_of(word: Word) -> Arc<ScopeState> {
    // SAFETY: forwarded from the caller.
    Arc::clone(&unsafe { handles::payload::<ScopeHandle>(word, HandleKind::Scope) }.0)
}

/// Opens the root's parentless base scope (D-859, D-798's form) and returns
/// it: the "own root scope" of spec §13.5.4 and §13.6. The root's base
/// frame (`sched`) calls it when the root starts, before its body runs, for
/// `main`, a test and a REPL entry. The scope is owned by the root and set
/// as its spawn scope, so the root's own scopes take it as their parent
/// (one more level of depth, which only orders targets, D-667) and
/// `FiberConc::spawn_scope` returns it. Like a detached fiber's base it has
/// no parent, never has members and records no first cause (a panic of
/// the root is reported by its catch point, D-701); only a deadlock's
/// cancellation targets it (`cancel::cancel_deadlocked_root`), and it is
/// the root of the detector's survey. A root that has a spawn scope
/// already is a broken invariant.
pub(crate) fn open_root_base(ctx: &ExecCtx) -> Arc<ScopeState> {
    let base = Arc::new(ScopeState::new_root_base(FiberRef::from_ctx(ctx)));
    {
        let mut spawn_scope = ctx.conc.spawn_scope.lock();
        if spawn_scope.is_some() {
            drop(spawn_scope);
            fatal(format_args!(
                "a root's base scope was opened on a fiber that has a scope"
            ));
        }
        *spawn_scope = Some(Arc::clone(&base));
    }
    base
}

/// Ends the root's base scope that [`open_root_base`] opened, under its
/// lock, as [`ScopeState::end_base`] does for a detached fiber (D-859,
/// D-798): the root's base frame calls it after the body returned or
/// unwound, the deadlock's cancellation included, and before the root
/// reports its outcome, unregisters and retires. From then on a
/// cancellation's walk and a survey skip the scope (`cancel_step`,
/// `survey_step`), so neither reaches the retired root, and a late
/// deadlock report is not stored ([`ScopeState::store_deadlock_report`]).
/// It also drops the root's cancellation target, as a fiber's end does. A
/// second call, a root without a base scope, and a base scope with a
/// member are broken invariants (`fatal`).
pub(crate) fn close_root_base(ctx: &ExecCtx) {
    let Some(base) = ctx.conc.spawn_scope.lock().take() else {
        fatal(format_args!(
            "a root's base scope was closed twice, or never opened"
        ));
    };
    if !base.is_root_base() || base.owner != FiberRef::from_ctx(ctx) {
        fatal(format_args!(
            "a root's base scope was closed that is not its own"
        ));
    }
    cancel::clear_at_end(ctx);
    base.end_base();
}

/// Opens a scope for the running fiber and returns its handle, owned
/// (D-693, architecture §7.5): its parent is the fiber's innermost open
/// scope, else the scope the fiber was spawned into; it is linked into the
/// parent's nested scopes and pushed on the fiber's open scopes. It may
/// raise only out of memory, when the handle cannot be allocated, which
/// happens before anything is open.
///
/// `timed` gives the scope a deadline `seconds` and `nanoseconds` from
/// now (D-652's reading, `sched::deadline_after`), which only
/// `fiber::timeout` asks for (D-671): once the scope is linked, a timer is
/// armed whose firing cancels it ([`ScopeTimer`]). A deadline past the
/// clock's range never comes and arms nothing, so the scope is untimed. A
/// limit of zero or less never gets here, because `timeout` returns
/// `Err(TimedOut)` without calling `f`; if one did, it would be a deadline
/// of now. Without a runtime (D-692, architecture §5.1) a scope whose
/// timer would be armed ends the program with a fatal runtime error naming
/// `fiber::timeout`, before anything is open, while one that never times
/// out opens untimed and runs `f`.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_scope_open(
    ctx: *mut ExecCtx,
    timed: bool,
    seconds: i64,
    nanoseconds: i64,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        let deadline = if timed {
            match deadline_after(seconds, nanoseconds) {
                Deadline::At(at) => Some(at),
                Deadline::Elapsed => Some(Instant::now()),
                Deadline::Never => None,
            }
        } else {
            None
        };
        let runtime = me.sched.runtime();
        if deadline.is_some() && runtime.is_none() {
            needs_scheduler("fiber::timeout");
        }
        let parent = me.conc.responsible_scope();
        let state = Arc::new(ScopeState {
            of_timeout: timed,
            ..ScopeState::new(parent, FiberRef::from_ctx(me))
        });
        // SAFETY: `ctx` is the running fiber's. If the allocation raises,
        // nothing is open yet and the payload box is dropped.
        let handle = unsafe {
            handles::new_handle(
                ctx,
                HandleKind::Scope,
                Box::new(ScopeHandle(Arc::clone(&state))),
            )
        };
        ScopeState::link(&state);
        if let (Some(at), Some(runtime)) = (deadline, runtime) {
            let target = Arc::new(ScopeTimer(Arc::downgrade(&state)));
            *state.timer.lock() = Some(runtime.arm_timer(at, target));
        }
        me.conc.push_scope(state);
        handle
    })
}

/// Closes the scope `scope` that [`cheby_rt_scope_open`] opened on this
/// fiber, consuming the handle, also when it raises (D-693, architecture
/// §7.5):
///
/// 1. waits, without interruption, until every member has left;
/// 2. pops the scope from the fiber's open scopes, unlinks it from its
///    parent (step 1 marked it ended) and cancels its timer, if it is
///    timed: on every path out, a return, a panic or the pass-through of an
///    outer cancellation (D-671);
/// 3. gives the fiber the outermost of its proper ancestors that has failed
///    or is being cancelled as its target (`cancel::check_chain`), then
///    reads and clears the fiber's cancellation target under its fiber
///    lock (D-706, `cancel::at_close`), so that a cancellation of an outer
///    scope running at the same time is ordered with the decision and
///    never lost:
///    - a proper ancestor of this scope is the target: the cancellation
///      passes through (D-667). The close takes the body's record if it
///      unwound (a panic after the cancellation was requested is not
///      reported, D-178) and raises the cancellation again, after
///      releasing the handle, so the scope never returns (I-7);
///    - this scope is the target: the target and the CANCEL reason are
///      cleared, and the scope returns per its first cause, as below; a
///      later outer cancellation finds no target, sets its own and is
///      delivered at the next entry check;
///    - otherwise it decides from the scope's first cause (D-666),
///      recorded when the first panic of a member or of the body started.
///      When the body unwound (`unwound`), the close takes the fiber's
///      record, which ends the unwinding; the body's panic becomes the
///      cause only if none was recorded. Without a cause the scope
///      succeeded and the result is true; with one it is stored for
///      `take_failure` and the result is false, so that `scope_run`
///      returns the static `Err(Nil)`. Nothing is printed (D-701).
///
/// A body that unwound for this scope's own cancellation without a first
/// cause is an expired limit: only a timed scope's timer cancels a scope
/// without recording a cause (D-790; the test hook
/// `fiber_ops::testing::cancel_running_fiber_at_next_entry` acts as one).
/// The scope then fails with kind 1 and empty texts, which `fiber::timeout`
/// reads as `TimedOut` (D-671). Each outcome of spec §10.8 follows: a body
/// that returns, even after the timer fired, gives `Ok`; a panic that
/// starts before the firing is the first cause (`Panicked`), and one that
/// starts after it is the body's own record (`Panicked` too: the
/// cancellation had not reached `f`); a body cancelled while it waits in a
/// nested scope's close unwinds through it (D-667), and a panic of a fiber
/// during that unwinding is not recorded (D-178), so the result is kind 1.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `scope` the handle its
/// `scope_open` returned, whose reference the call consumes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_scope_close(
    ctx: *mut ExecCtx,
    scope: Word,
    unwound: bool,
) -> bool {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller: the call consumes one
        // reference to the live handle.
        let handle = unsafe { Owned::new(scope) };
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: a live `Scope` handle, kept by `handle`.
        let state = unsafe { state_of(handle.word()) };
        // 1. Wait for the members.
        let own_cause = loop {
            let mut inner = state.inner.lock();
            if inner.members.is_empty() {
                inner.ended = true;
                break inner.first_cause.take();
            }
            if me.sched.runtime().is_none() {
                drop(inner);
                needs_scheduler("fiber::scope");
            }
            inner.waiter = Some(FiberRef::from_ctx(me));
            // The wait site (D-856, D-858), copied from the frame
            // `scope_run`'s body published before this call, before the
            // park (`park`'s SeqCst store orders it), and cleared once the
            // fiber runs again.
            let op = if state.of_timeout {
                WaitOp::Timeout
            } else {
                WaitOp::Scope
            };
            me.sched.note_wait(op, me.wait_frame());
            park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
            me.sched.clear_wait();
        };
        // 2. Pop, unlink, and cancel the timer. A firing that already took
        // the timer finds the scope ended, and its walk skips it.
        me.conc.pop_scope(&state);
        state.unlink();
        let timer = state.timer.lock().take();
        if let Some(timer) = timer {
            timer.cancel();
        }
        // 3. Decide, after the target (D-769): a
        // cancellation of an enclosing scope reached this fiber when that
        // scope was marked, even if the walk has not set its target yet
        // (`cancel::check_chain`, review finding CONC-W2-1): it becomes the
        // target here, so the close passes it through at every thread
        // count. The body's own panic was recorded
        // in this scope when it started, unless an earlier one was; the
        // record is taken either way.
        if let Some(outer) = cancel::outermost_failing(state.parent().cloned()) {
            cancel::set_target(me, &outer);
        }
        let target = cancel::at_close(me, &state);
        let record = if unwound {
            match take_record(me) {
                Some(record) => Some(record),
                None => fatal(format_args!("a scope's body unwound without a record")),
            }
        } else {
            None
        };
        if target == AtClose::Outer {
            // The cancellation passes through (D-667): nothing of the body's
            // unwinding is reported, and no lock is held.
            drop(record);
            drop(state);
            drop(handle);
            raise_cancel(Some(ctx));
        }
        let unwound_cause = match record {
            Some(UnwindRecord::Panic(report)) => {
                Some(Failure::panic(report.message, report.location))
            }
            Some(UnwindRecord::Cancel) if target == AtClose::This => Some(Failure {
                kind: 1,
                message: String::new(),
                location: String::new(),
            }),
            Some(UnwindRecord::Cancel) => fatal(format_args!(
                "a cancellation reached a scope that is not its target"
            )),
            None => None,
        };
        drop(handle);
        match own_cause.or(unwound_cause) {
            Some(failure) => {
                me.conc.set_last_failure(failure);
                false
            }
            None => true,
        }
    })
}

/// `fiber::take_failure`, private: why the scope that `scope_run` just
/// closed on this fiber failed, as `(kind, message, location)`: kind 0 for
/// a panic and 1 for an expired limit, with empty texts (architecture
/// §5.3). `std::fiber` builds its `Panic` record from it, on this fiber, so
/// no `Panic` crosses fibers (D-674).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_take_failure(ctx: *mut ExecCtx) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        let Some(failure) = me.conc.take_last_failure() else {
            fatal(format_args!(
                "fiber::take_failure ran after no failed scope"
            ));
        };
        // SAFETY: fresh strings, owned here until the tuple takes them.
        let message = unsafe { Owned::new(values::string(&failure.message)) };
        // SAFETY: as above.
        let location = unsafe { Owned::new(values::string(&failure.location)) };
        let tuple = values::tuple(&[
            values::int_word(failure.kind),
            message.word(),
            location.word(),
        ]);
        // The tuple owns them now.
        let _ = (message.into_word(), location.into_word());
        tuple
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::ScopeState;
    use crate::abi::PanicKind;
    use crate::ctx::{ExecCtx, FiberInit, reason};
    use crate::fiber_ops::Failure;
    use crate::fiber_ops::cancel::hooks;
    use crate::panic::PanicReport;
    use crate::sched::FiberRef;

    /// An owner for scopes that no fiber runs: an isolated block.
    fn owner() -> Box<ExecCtx> {
        Box::new(ExecCtx::new(FiberInit::placeholder()))
    }

    /// D-666, D-667: a scope records its first cause once and cancels
    /// itself, which targets its owner; a later panic leaves the cause as
    /// it is, and the scope's parent records nothing. A scope cancelled
    /// from outside records no cause.
    #[test]
    fn cancel_record_panic_records_once_and_cancels_the_scope() {
        let me = owner();
        let parent = Arc::new(ScopeState::new(None, FiberRef::from_ctx(&me)));
        let scope = Arc::new(ScopeState::new(
            Some(Arc::clone(&parent)),
            FiberRef::from_ctx(&me),
        ));
        ScopeState::link(&scope);
        let first = PanicReport::new(PanicKind::Explicit, Some("first"), "a.cheby:1:2");
        let second = PanicReport::new(PanicKind::Todo, None, "b.cheby:3:4");
        assert!(scope.record_panic(&first));
        assert!(!scope.record_panic(&second));
        assert!(!scope.record_panic(&first));
        assert_eq!(
            scope.inner.lock().first_cause,
            Some(Failure::panic("first".to_owned(), "a.cheby:1:2".to_owned()))
        );
        assert!(scope.inner.lock().cancelled);
        assert!(hooks::target(&me).is_some_and(|target| Arc::ptr_eq(&target, &scope)));
        assert!(crate::ctx::pending(&me, reason::CANCEL));
        assert_eq!(parent.inner.lock().first_cause, None);
        assert!(!parent.inner.lock().cancelled);
        // The parent is cancelled now: its owner's target becomes the outer
        // scope (the outermost wins), and the child, cancelled from outside
        // already, records nothing new.
        crate::fiber_ops::cancel::cancel(&parent);
        assert!(hooks::target(&me).is_some_and(|target| Arc::ptr_eq(&target, &parent)));
        assert!(!parent.record_panic(&second));
        assert_eq!(parent.inner.lock().first_cause, None);
        scope.unlink();
        hooks::clear_target(&me);
    }

    /// Survey-u5 §2.3, hazard 3: a scope linked under a cancelled parent is
    /// cancelled itself, so it records no cause and a spawn into it starts
    /// cancelled.
    #[test]
    fn cancel_link_inherits_the_parents_cancellation() {
        let me = owner();
        let parent = Arc::new(ScopeState::new(None, FiberRef::from_ctx(&me)));
        crate::fiber_ops::cancel::cancel(&parent);
        let child = Arc::new(ScopeState::new(
            Some(Arc::clone(&parent)),
            FiberRef::from_ctx(&me),
        ));
        ScopeState::link(&child);
        assert!(child.inner.lock().cancelled);
        let report = PanicReport::new(PanicKind::Explicit, Some("late"), "a.cheby:1:2");
        assert!(!child.record_panic(&report));
        assert_eq!(child.depth(), 1);
        child.unlink();
        hooks::clear_target(&me);
    }

    /// Opens `count` sibling scopes under one parent, all open at once,
    /// then closes them oldest first; returns the time the closes took.
    fn close_siblings(count: usize) -> Duration {
        let me = owner();
        let parent = Arc::new(ScopeState::new(None, FiberRef::from_ctx(&me)));
        let children: Vec<Arc<ScopeState>> = (0..count)
            .map(|_| {
                let child = Arc::new(ScopeState::new(
                    Some(Arc::clone(&parent)),
                    FiberRef::from_ctx(&me),
                ));
                ScopeState::link(&child);
                child
            })
            .collect();
        assert_eq!(parent.nested_count(), count);
        let started = Instant::now();
        for child in &children {
            child.unlink();
        }
        let took = started.elapsed();
        assert_eq!(parent.nested_count(), 0);
        // The freed slots are used again.
        let again = Arc::new(ScopeState::new(
            Some(Arc::clone(&parent)),
            FiberRef::from_ctx(&me),
        ));
        ScopeState::link(&again);
        assert_eq!(parent.inner.lock().nested.slots.len(), count);
        again.unlink();
        took
    }

    /// Review finding R3-ROB-1: closing a scope unlinks it from its parent
    /// in O(1), however many siblings are open. Scanning the siblings made
    /// 200,000 of them take minutes in a debug build; a slab takes
    /// milliseconds.
    #[test]
    fn closing_many_open_sibling_scopes_is_linear() {
        let took = close_siblings(200_000);
        if !crate::sched::testing::emulated() {
            assert!(took < Duration::from_secs(2), "{took:?}");
        }
    }
}
