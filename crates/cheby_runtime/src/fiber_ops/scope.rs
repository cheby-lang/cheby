//! Scopes (D-666, D-693, architecture §5.3 and §7.5): the open and close
//! that the compiler-generated body of the `scope_run` intrinsic calls,
//! with the body called in between and no Cheby call around it, and the
//! kernel `take_failure` that tells `std::fiber::scope` why a scope failed.
//!
//! A scope's state ([`ScopeState`]) is shared by its `Scope` handle (kind
//! 43), the fibers spawned into it and the scopes nested in it. Its lock
//! guards the members, the nested scopes, the first cause and the `ended`
//! flag. Lock order (architecture §7.5): a parent's lock before a child's;
//! a scope lock is never held together with a channel lock, and never
//! across a switch (I-5): the owner waiting in close hands it to `park`.
//!
//! A scope's first cause is the first panic, by start, of a fiber
//! responsible to it: a member, or the body of the fiber that opened it
//! (D-666). The panic is recorded when it starts
//! ([`ScopeState::record_panic`], called by `unwind::record::start_panic`),
//! before the panicking fiber unwinds, so a panic that starts later never
//! replaces it, whichever fiber ends first. Cancelling the other members at
//! that moment belongs to cancellation (package U5).

use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::{Failure, Owned, needs_scheduler, running};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind, HandlePayload};
use crate::panic::{PanicReport, fatal};
use crate::sched::{AfterSwitch, FiberRef, Unlock, park, wake};
use crate::unwind::guard::guarded;
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
    /// The scope's slot among its parent's nested scopes, set by the open
    /// once it is linked; only the fiber that opened the scope reads it,
    /// at the close.
    nested_slot: Mutex<Option<usize>>,
}

/// The part of a scope under its lock.
#[derive(Debug, Default)]
struct ScopeInner {
    /// The fibers spawned into the scope that have not left it yet.
    members: Members,
    /// The scopes opened inside its region that have not closed yet, for
    /// cancellation's walk of a subtree (U5).
    nested: Nested,
    /// The first panic of the scope (D-666).
    first_cause: Option<Failure>,
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
    /// Held by a member fiber, which cancellation (U5) reaches through it.
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
    /// A member slot; `cancelled` when the scope has failed, so the new
    /// fiber is cancelled before it starts (D-666).
    Member {
        /// The slot.
        index: usize,
        /// Whether the scope has failed.
        cancelled: bool,
    },
}

impl ScopeState {
    fn new(parent: Option<Arc<Self>>) -> Self {
        Self {
            inner: Mutex::new(ScopeInner::default()),
            parent,
            nested_slot: Mutex::new(None),
        }
    }

    /// Links the new scope `state` into its parent's nested scopes.
    fn link(state: &Arc<Self>) {
        if let Some(parent) = &state.parent {
            let index = parent.inner.lock().nested.insert(Arc::downgrade(state));
            *state.nested_slot.lock() = Some(index);
        }
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
        let cancelled = inner.first_cause.is_some();
        Membership::Member {
            index: inner.members.reserve(),
            cancelled,
        }
    }

    /// Records `report` as the scope's first cause if it has none (D-666),
    /// under the scope's lock. Returns whether it recorded. Called by
    /// `record::start_panic` after the record's lock is released, for the
    /// top of `open_scopes` (a root's body inside `scope` included), else
    /// the spawn scope; a fiber with neither records nothing. Entered with
    /// no runtime lock held, and holds only the scope's lock; from U5 on it
    /// releases the scope's lock before it calls `cancel` (plan §0
    /// 'Locks'). It allocates only the copy of the report's two texts, and
    /// only when it records.
    pub(crate) fn record_panic(&self, report: &PanicReport) -> bool {
        let mut inner = self.inner.lock();
        if inner.first_cause.is_some() {
            return false;
        }
        inner.first_cause = Some(Failure::panic(
            report.message.clone(),
            report.location.clone(),
        ));
        true
    }

    /// Records the fiber of the reserved slot `index`.
    pub(crate) fn fill(&self, index: usize, fiber: FiberRef) {
        self.inner.lock().members.fill(index, fiber);
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

/// Opens a scope for the running fiber and returns its handle, owned
/// (D-693, architecture §7.5): its parent is the fiber's innermost open
/// scope, else the scope the fiber was spawned into; it is linked into the
/// parent's nested scopes and pushed on the fiber's open scopes. It may
/// raise only out of memory, when the handle cannot be allocated, which
/// happens before anything is open.
///
/// `timed` gives the scope a deadline `seconds` and `nanoseconds` from
/// now, which only `fiber::timeout` asks for. Timers arrive with package
/// T1, so until then a timed scope ends the program with `fatal runtime
/// error: fiber::timeout is not supported yet` (D-754); a limit of zero or
/// less never gets here (D-671).
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
        let _ = (seconds, nanoseconds);
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        if timed {
            fatal(format_args!("fiber::timeout is not supported yet"));
        }
        let parent = me.conc.responsible_scope();
        let state = Arc::new(ScopeState::new(parent));
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
        me.conc.push_scope(state);
        handle
    })
}

/// Closes the scope `scope` that [`cheby_rt_scope_open`] opened on this
/// fiber, consuming the handle, also when it raises (D-693, architecture
/// §7.5):
///
/// 1. waits, without interruption, until every member has left;
/// 2. pops the scope from the fiber's open scopes, marks it ended and
///    unlinks it from its parent;
/// 3. decides from the scope's first cause (D-666), recorded when the
///    first panic of a member or of the body started. When the body
///    unwound (`unwound`), the close takes the fiber's record, which ends
///    the unwinding; the body's panic becomes the cause only if none was
///    recorded. Without a cause the scope succeeded and the result is
///    true; with one it is stored for `take_failure` and the result is
///    false, so that `scope_run` returns the static `Err(Nil)`. Nothing is
///    printed (D-701).
///
/// A cancellation passing through re-raises from here once package U5
/// brings it; until then a cancellation record here is a broken invariant.
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
            park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
        };
        // 2. Pop, end, unlink.
        me.conc.pop_scope(&state);
        state.unlink();
        // 3. Decide. The body's own panic was recorded in this scope when
        // it started, unless an earlier one was; the record is taken
        // either way.
        let unwound_cause = if unwound {
            match take_record(me) {
                Some(UnwindRecord::Panic(report)) => {
                    Some(Failure::panic(report.message, report.location))
                }
                Some(UnwindRecord::Cancel) => fatal(format_args!(
                    "a cancellation reached a scope (cancellation arrives with package U5)"
                )),
                None => fatal(format_args!("a scope's body unwound without a record")),
            }
        } else {
            None
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
    use crate::fiber_ops::Failure;
    use crate::panic::PanicReport;

    /// D-666: a scope records its first cause once; a later panic leaves it
    /// as it is, and the scope's parent and nested scopes record nothing.
    #[test]
    fn record_panic_records_once() {
        let parent = Arc::new(ScopeState::new(None));
        let scope = Arc::new(ScopeState::new(Some(Arc::clone(&parent))));
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
        assert_eq!(parent.inner.lock().first_cause, None);
        scope.unlink();
    }

    /// Opens `count` sibling scopes under one parent, all open at once,
    /// then closes them oldest first; returns the time the closes took.
    fn close_siblings(count: usize) -> Duration {
        let parent = Arc::new(ScopeState::new(None));
        let children: Vec<Arc<ScopeState>> = (0..count)
            .map(|_| {
                let child = Arc::new(ScopeState::new(Some(Arc::clone(&parent))));
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
        let again = Arc::new(ScopeState::new(Some(Arc::clone(&parent))));
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
