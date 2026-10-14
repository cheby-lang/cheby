//! The lifecycle of a runtime's fibers (D-073, D-669, D-708 to D-710,
//! D-723, D-782 to D-786): detached fibers and their groups, teardown, the
//! halt at a safepoint, and termination as an outcome (D-709).
//!
//! # Groups (D-799)
//!
//! A [`DetachedGroup`] holds the detached fibers of one root: `main`'s
//! (`run_main`), one test's (`cheby test`, D-782), later one REPL
//! session's. A root runs in the group its host gives
//! ([`Runtime::run_root_in`]; [`Runtime::run_root`] uses the runtime's own
//! default group), and every fiber inherits its spawner's group, as it
//! inherits its domain (D-691), so a detached fiber spawned by a detached
//! fiber, or by a scoped fiber of a test, lands in the same group. A
//! root's end does not end its group: a host tears the group down when it
//! chooses, and the runtime tears every group down when it shuts down.
//!
//! Each member is a detached fiber and its **base scope**
//! (D-798, `fiber_ops::detached`): a parentless scope,
//! owned by the fiber, its spawn scope, the root of every scope it opens.
//!
//! # Teardown (D-708, D-710, D-723)
//!
//! [`Runtime::teardown`] marks the group closing, cancels each member
//! through its base scope (`cancel::cancel`), which reaches the fiber and
//! everything it started, wherever it waits, and waits until the group is
//! empty. No user code runs (ADR-0029): a cancelled fiber unwinds through
//! its cleanup pads, and every scope it has open passes the cancellation
//! through (D-667), so none returns `Err` to user code. A detached spawn
//! into a closing group starts cancelled and never calls its function
//! (D-800). A fiber blocked inline in a system
//! call (standard input, file IO, until I1's blocking pool) cannot be
//! stopped: the teardown skips it and counts it (D-786), and with it the
//! fibers of the same member's subtree that wait for it, such as a
//! detached fiber waiting in a scope's close for a scoped child blocked in
//! `io::read_line`. It looks for that state by surveying each member's
//! subtree, scope by scope, as cancellation walks it. A teardown that
//! meets a started halt stops waiting, because a frozen fiber never leaves
//! its group, and reports `halted`; its host completes the halt.
//!
//! # Locks
//!
//! The group's lock is a leaf, taken with no scope, channel or fiber lock
//! held, and nothing is taken under it: the teardown cancels and surveys
//! the members' subtrees after releasing it, holding only `Arc`s of their
//! base scopes, and a member's control block is only read (an atomic)
//! under it. A member is alive while it is in the group: it leaves under
//! the group's lock as its last touch of shared state, before it retires.

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use parking_lot::{Condvar, Mutex, MutexGuard};

use super::{FiberRef, NO_STATUS, Runtime, Shared};
use crate::entry::{EntryConfig, EntryFn, EntryOutcome, RuntimeError};
use crate::fiber_ops::cancel;
use crate::fiber_ops::scope::ScopeState;
use crate::panic::fatal;

/// How long a teardown waits for the group to become empty before it looks
/// again for a started halt or members blocked in a system call, which do
/// not signal it.
const TEARDOWN_POLL: Duration = Duration::from_millis(5);

/// The detached fibers of one root: `main`, one test, later one REPL
/// session (D-799).
pub struct DetachedGroup {
    /// The members and the closing flag: a leaf lock, taken with no scope,
    /// channel or fiber lock held.
    inner: Mutex<GroupInner>,
    /// Signalled when the group becomes empty.
    emptied: Condvar,
}

/// The part of a group under its lock.
#[derive(Default)]
struct GroupInner {
    /// The member slots.
    slots: Vec<GroupSlot>,
    /// The free slots.
    free: Vec<usize>,
    /// The slots that are not free.
    count: usize,
    /// How many members have left, ever: a teardown counts the members it
    /// waited for from it.
    left: usize,
    /// Set by a teardown until the group is empty: a detached spawn into
    /// the group starts cancelled (D-800).
    closing: bool,
}

/// A member slot of a group.
#[derive(Clone)]
enum GroupSlot {
    /// Free.
    Free,
    /// Reserved by a detached spawn that has not created its fiber yet:
    /// the group waits for it from now on.
    Reserved,
    /// A detached fiber and its base scope.
    Member(FiberRef, Arc<ScopeState>),
}

impl fmt::Debug for DetachedGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DetachedGroup")
            .field("live", &self.live())
            .finish_non_exhaustive()
    }
}

impl DetachedGroup {
    /// An empty group.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GroupInner::default()),
            emptied: Condvar::new(),
        })
    }

    /// Detached fibers that have not left the group.
    #[must_use]
    pub fn live(&self) -> usize {
        self.inner.lock().count
    }

    /// The members parked now: suspended in a wait (a channel, a join, a
    /// timer, a select, a scope's close), neither runnable, running nor
    /// frozen. A diagnostic for tests, never part of a program's behavior.
    #[doc(hidden)]
    #[must_use]
    pub fn parked(&self) -> usize {
        let inner = self.inner.lock();
        inner
            .slots
            .iter()
            .filter(|slot| match slot {
                GroupSlot::Member(fiber, _) => {
                    // SAFETY: a member that has not left the group is not
                    // retired (module documentation); only its state word
                    // is read.
                    let state = unsafe { fiber.ctx() }.sched.state().load(Ordering::SeqCst);
                    state == super::fiber::state::PARKED
                }
                GroupSlot::Free | GroupSlot::Reserved => false,
            })
            .count()
    }

    /// Reserves a member slot for a detached fiber about to be spawned:
    /// from now on the group waits for it.
    pub(crate) fn reserve(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.count = inner.count.saturating_add(1);
        if let Some(index) = inner.free.pop()
            && let Some(slot) = inner.slots.get_mut(index)
        {
            *slot = GroupSlot::Reserved;
            return index;
        }
        inner.slots.push(GroupSlot::Reserved);
        inner.slots.len().saturating_sub(1)
    }

    /// Records the fiber and base scope of the reserved slot `index`, and
    /// returns whether the group is closing, read under the same lock: a
    /// teardown that started since the reservation did not see the fiber,
    /// so the spawner gives it its base as its target before it is first
    /// queued, and its function is never called
    /// (D-800). A teardown after this finds it.
    pub(crate) fn fill(&self, index: usize, fiber: FiberRef, base: Arc<ScopeState>) -> bool {
        let mut inner = self.inner.lock();
        let Some(slot @ GroupSlot::Reserved) = inner.slots.get_mut(index) else {
            drop(inner);
            fatal(format_args!(
                "a detached fiber filled a group slot it did not reserve"
            ))
        };
        *slot = GroupSlot::Member(fiber, base);
        inner.closing
    }

    /// Leaves the slot `index`: the detached fiber has ended (its last
    /// touch of shared state, D-694), or its spawn failed before the fiber
    /// existed. The last member to leave wakes a waiting teardown.
    pub(crate) fn leave(&self, index: usize) {
        let released = {
            let mut inner = self.inner.lock();
            let Some(slot @ (GroupSlot::Reserved | GroupSlot::Member(..))) =
                inner.slots.get_mut(index)
            else {
                drop(inner);
                fatal(format_args!(
                    "a detached fiber left a group slot it did not hold"
                ))
            };
            let released = std::mem::replace(slot, GroupSlot::Free);
            inner.free.push(index);
            inner.count = inner.count.saturating_sub(1);
            inner.left = inner.left.saturating_add(1);
            if inner.count == 0 {
                self.emptied.notify_all();
            }
            released
        };
        // The slot's share of the base scope goes outside the lock.
        drop(released);
    }

    /// Marks the group closing and returns the base scopes of its members.
    fn close(&self) -> (Vec<Arc<ScopeState>>, usize) {
        let mut inner = self.inner.lock();
        inner.closing = true;
        let bases = inner
            .slots
            .iter()
            .filter_map(|slot| match slot {
                GroupSlot::Member(_, base) => Some(Arc::clone(base)),
                GroupSlot::Free | GroupSlot::Reserved => None,
            })
            .collect();
        (bases, inner.left)
    }

    /// Whether a teardown has marked the group closing.
    #[cfg(test)]
    pub(crate) fn closing(&self) -> bool {
        self.inner.lock().closing
    }

    /// How many fibers of the members' subtrees are blocked inline in a
    /// system call (D-786).
    #[cfg(test)]
    pub(crate) fn blocked(&self) -> usize {
        let bases = self.inner.lock().bases().unwrap_or_default();
        bases
            .iter()
            .map(|base| {
                let mut found = Survey::default();
                survey(base, &mut found);
                found.blocked
            })
            .sum()
    }

    /// Wakes a teardown that waits on the group, so it sees a started
    /// halt.
    pub(crate) fn wake_teardown(&self) {
        let inner = self.inner.lock();
        self.emptied.notify_all();
        drop(inner);
    }
}

impl GroupInner {
    /// The base scopes of the members, or `None` while a slot is reserved
    /// for a fiber not created yet.
    fn bases(&self) -> Option<Vec<Arc<ScopeState>>> {
        let mut bases = Vec::with_capacity(self.count);
        for slot in &self.slots {
            match slot {
                GroupSlot::Member(_, base) => bases.push(Arc::clone(base)),
                GroupSlot::Reserved => return None,
                GroupSlot::Free => {}
            }
        }
        Some(bases)
    }
}

/// What a survey of members' subtrees found (D-786).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Survey {
    /// The fibers blocked inline in a system call, each counted once.
    blocked: usize,
    /// Whether some fiber was neither parked nor blocked inline, or a
    /// member was ending: the subtree may still make progress.
    busy: bool,
}

impl Survey {
    /// Whether the surveyed members can make no progress until a system
    /// call returns: at least one fiber is blocked inline, and every other
    /// fiber is parked.
    const fn stuck(self) -> bool {
        self.blocked > 0 && !self.busy
    }
}

/// Surveys the subtree of the base scope `base` into `found` (D-786): the
/// detached fiber that owns it and every fiber it started, which are the
/// owners and members of the scopes nested in it, each scope's lock taken
/// in turn and released before the next, with no other lock held
/// (`ScopeState::survey_step`).
///
/// After a teardown's cancellation has reached a subtree, a fiber of it
/// that stays parked waits in a scope's close for a member, which is in
/// the subtree too: every other wait is cancelled (D-667, D-670). So a
/// subtree whose fibers are all parked but at least one blocked inline
/// waits for that system call, and only for it.
fn survey(base: &Arc<ScopeState>, found: &mut Survey) {
    let mut scopes = vec![Arc::clone(base)];
    while let Some(scope) = scopes.pop() {
        let alive = scope.survey_step(
            &mut |fiber, first| {
                if fiber.sched.blocking() {
                    if first {
                        found.blocked = found.blocked.saturating_add(1);
                    }
                } else if fiber.sched.state().load(Ordering::SeqCst) != super::fiber::state::PARKED
                {
                    found.busy = true;
                }
            },
            &mut scopes,
        );
        // A base scope ends as its fiber's last step: the member is ending.
        if !alive && Arc::ptr_eq(&scope, base) {
            found.busy = true;
        }
    }
}

/// What a teardown waited for. `skipped`: fibers blocked inline in a system
/// call that it did not wait for (D-786). `halted`: a halt (a termination)
/// started during the teardown, which then stopped waiting; the host
/// completes the halt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeardownReport {
    /// The detached fibers that left the group while the teardown ran.
    pub waited: usize,
    /// The fibers blocked inline in a system call, left in the group
    /// (D-786).
    pub skipped: usize,
    /// Whether a halt started during the teardown.
    pub halted: bool,
}

impl Runtime {
    /// `run_root` in `group`: detached fibers that the root or its fibers
    /// spawn join `group`. `run_root` itself runs in the runtime's own
    /// default group.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::run_root`].
    ///
    /// # Safety
    ///
    /// As for [`Runtime::run_root`].
    pub unsafe fn run_root_in(
        &self,
        entry: EntryFn,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
    ) -> Result<EntryOutcome, RuntimeError> {
        self.shared.register_group(group);
        // SAFETY: forwarded from the caller.
        unsafe { self.run_root_with(entry, config, Arc::clone(group)) }
    }

    /// The state every thread of the runtime shares, for `run_main`, which
    /// fixes the exit status there and reads whether a termination
    /// happened after the runtime is gone.
    pub(crate) fn shared_handle(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// Cancels every member of `group` through its base scope and waits
    /// until the group is empty (D-708, D-710, D-723), unless a halt starts
    /// meanwhile (`halted`). Once every remaining member's subtree is
    /// parked except fibers blocked inline in a system call, on two polls
    /// in a row, those members are skipped and the blocked fibers counted
    /// (D-786, D-804: D-786 names the marked fiber, and D-804 skips with it
    /// the fibers that wait in a scope's close for it,
    /// counted once by the marked fibers). From the host only, with no runtime
    /// lock held. Runs no user code (ADR-0029).
    pub fn teardown(&self, group: &DetachedGroup) -> TeardownReport {
        let (bases, left_before) = group.close();
        for base in &bases {
            cancel::cancel(base);
        }
        drop(bases);
        let mut inner = group.inner.lock();
        let mut report = TeardownReport::default();
        // The `left` count of the last poll that found every member stuck
        // behind a fiber blocked inline (D-786): the teardown skips them
        // when two polls in a row find so with no member gone between, so
        // that a survey that raced with a wake-up cannot end it early.
        let mut stuck_at: Option<usize> = None;
        loop {
            if inner.count == 0 {
                inner.closing = false;
                break;
            }
            if self.shared.halting() {
                report.halted = true;
                break;
            }
            let left = inner.left;
            let found = inner.bases().and_then(|bases| {
                // The survey takes scope locks: never under the group's.
                MutexGuard::unlocked(&mut inner, || {
                    let mut found = Survey::default();
                    for base in &bases {
                        survey(base, &mut found);
                    }
                    drop(bases);
                    found.stuck().then_some(found.blocked)
                })
            });
            match found {
                Some(blocked) if inner.left == left && stuck_at == Some(left) => {
                    report.skipped = blocked;
                    break;
                }
                Some(_) if inner.left == left => stuck_at = Some(left),
                _ => stuck_at = None,
            }
            if inner.count == 0 {
                continue;
            }
            group.emptied.wait_for(&mut inner, TEARDOWN_POLL);
        }
        report.waited = inner.left.saturating_sub(left_before);
        report
    }

    /// Stops the world without unwinding and joins every thread, then
    /// releases the code tokens (D-073, D-708). At a normal exit only
    /// `cheby run` without the leak check halts, right before the process
    /// exits; after a termination (D-709) every host completes the halt
    /// (D-797). `Drop` and `shutdown` of a runtime whose
    /// halt has started complete the halt and never tear down.
    pub fn halt(mut self) {
        self.shared.begin_halt();
        self.finish();
    }
}

impl Shared {
    /// Remembers the host waiting for root `root`'s outcome, unless a halt
    /// has started: then returns false, and the root must not run. Under
    /// the roots' lock, which a termination takes after it set the halt
    /// flag, so a root is either registered before the termination sends
    /// `Exited` to every waiting host, or sees the flag.
    pub(super) fn register_root(&self, root: u64, sender: mpsc::Sender<EntryOutcome>) -> bool {
        let mut roots = self.roots.lock();
        if self.halting() {
            return false;
        }
        roots.push((root, sender));
        true
    }

    /// Forgets root `root`'s host: the root has reported its outcome.
    pub(super) fn unregister_root(&self, root: u64) {
        let removed = {
            let mut roots = self.roots.lock();
            roots
                .iter()
                .position(|(known, _)| *known == root)
                .map(|at| roots.swap_remove(at))
        };
        drop(removed);
    }

    /// Fixes the program's exit status to `status` unless one is fixed
    /// already, and returns the status that stands: the first one wins
    /// (D-709, D-785). `run_main` fixes it after its flush; a termination
    /// fixes it before it halts the runtime.
    pub(crate) fn fix_status(&self, status: i32) -> i32 {
        match self.exit_status.compare_exchange(
            NO_STATUS,
            i64::from(status),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => status,
            Err(fixed) => i32::try_from(fixed).unwrap_or(crate::abi::status::ERR),
        }
    }

    /// The program's exit status, once fixed.
    pub(crate) fn exit_status(&self) -> Option<i32> {
        let status = self.exit_status.load(Ordering::SeqCst);
        (status != NO_STATUS).then(|| i32::try_from(status).unwrap_or(crate::abi::status::ERR))
    }

    /// Whether a termination has happened (D-709).
    pub(crate) fn terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }

    /// A termination on a fiber of this runtime: fixes the status (the
    /// first wins), starts the halt and, if this call fixed the status,
    /// sends `Exited` with it to every host waiting for a root.
    fn terminate(&self, status: i32) {
        let won = self
            .exit_status
            .compare_exchange(
                NO_STATUS,
                i64::from(status),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok();
        self.terminated.store(true, Ordering::SeqCst);
        self.begin_halt();
        if !won {
            return;
        }
        let roots = self.roots.lock();
        for (_, sender) in roots.iter() {
            sender.send(EntryOutcome::Exited(status)).ok();
        }
        drop(roots);
    }
}

/// Ends the program with `status` (D-709, D-306): never returns. Where
/// the program ends "at once with status 1" (a write that fails), the
/// runtime never ends the process itself; the termination is an outcome its
/// host receives:
///
/// - **On a fiber of a runtime**: the status is fixed, the first one wins
///   (a later termination, or one after `run_main` fixed the status at its
///   flush, leaves it: D-785); the halt starts (no worker takes a fiber
///   any more, every running fiber freezes at its next entry check, no
///   timer fires); every host waiting for a root receives
///   `EntryOutcome::Exited(status)`; then this fiber freezes. Each host
///   completes the halt ([`Runtime::halt`], or a `Drop`/`shutdown`, which
///   never tear down a halting runtime): it drops no Cheby value of that
///   runtime and keeps its code alive until the halt has joined every
///   thread.
/// - **On an isolated entry's thread** (constant evaluation, the REPL, the
///   thread backend): its host receives `Exited(status)`, and the thread
///   is parked for good, keeping everything it holds.
/// - **Off every entry** (a host thread): a broken invariant. A host that
///   writes on its own thread, such as the REPL's flush before reading a
///   line, gets the failure as an error instead and ends with status 1
///   itself (D-801, `io::read_input_line`).
///
/// Entered with no runtime lock held.
pub(crate) fn terminate(status: i32) -> ! {
    let Some(me) = crate::entry::current_fiber() else {
        fatal(format_args!(
            "the program ended with status {status} outside any entry"
        ))
    };
    if let Some(runtime) = me.sched.runtime() {
        runtime.terminate(status);
        super::park(me, super::AfterSwitch::Freeze);
        fatal(format_args!("a frozen fiber was resumed"))
    }
    // An isolated entry: its host learns of the termination and never
    // joins this thread.
    if let Some(sender) = me.outcome() {
        sender.send(EntryOutcome::Exited(status)).ok();
    }
    loop {
        std::thread::park();
    }
}
