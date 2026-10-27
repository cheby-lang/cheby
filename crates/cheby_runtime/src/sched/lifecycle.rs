//! The lifecycle of a runtime's fibers (D-073, D-669, D-708 to D-710,
//! D-723, D-782 to D-786): detached fibers and their groups, teardown, the
//! halt at a safepoint, and termination as an outcome (D-709).
//!
//! # Groups (D-799)
//!
//! A [`DetachedGroup`] holds the detached fibers of one root: `main`'s
//! (`run_main`), one test's (`cheby test`, D-782), or a REPL session's,
//! whose entries all run in its current group until a teardown at
//! `:reload` opens a fresh one (D-712, D-827). A root
//! runs in the group its host gives
//! ([`Runtime::run_root_in`]; [`Runtime::run_root`] uses the runtime's own
//! default group), and every fiber inherits its spawner's group, as it
//! inherits its domain (D-691), so a detached fiber spawned by a detached
//! fiber, or by a scoped fiber of a test, lands in the same group. A
//! root's end does not end its group: a host tears the group down when it
//! chooses, and the runtime tears every group down when it shuts down.
//! A test's group carries the test's capture ([`DetachedGroup::with_capture`],
//! D-723), which its fibers' standard-stream writes reach through the group
//! they inherit (`crate::capture`).
//!
//! # Roots a host does not block on
//!
//! [`Runtime::start_root_in`] starts a root and returns a [`RootHandle`]
//! the host waits on when it chooses; [`Runtime::start_root_notify`]
//! reports the root's end as `(tag, outcome)` on a [`RootDone`] the host
//! shares among several roots (`cheby test`'s runner). Both register the
//! group as [`Runtime::run_root_in`] does, and a root started after a
//! termination never runs and reports `Exited` at once (D-802).
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
//! (D-800). A fiber blocked in a system call (file IO, `@blocking`, or
//! standard input read inline, D-817), waiting on the blocking pool with D-786's
//! mark set or running the call inline, cannot be stopped: the teardown
//! skips it and counts it (D-786, D-814), and with it the
//! fibers of the same member's subtree that wait for it, such as a
//! detached fiber waiting in a scope's close for a scoped child blocked in
//! an `@blocking` call. A fiber waiting on a refill of standard input's
//! reader is not blocked in this sense: the cancellation abandons the
//! refill, and the fiber unwinds (D-1065). It looks for the blocked state
//! by surveying each member's subtree, scope by scope, as cancellation
//! walks it. A teardown that meets a started halt stops waiting, because a frozen fiber never leaves
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
//! The group's list of roots reporting on a [`RootDone`] and each such
//! root's own report are leaf locks too: a report is sent after both are
//! released, and a termination reads the list after the roots' lock.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use super::deadlock::{PoolPending, RootEntry};
use super::{FiberRef, NO_STATUS, Runtime, Shared};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::capture::Capture;
use crate::entry::{EntryConfig, EntryFn, EntryOutcome, RuntimeError};
use crate::fiber_ops::cancel;
use crate::fiber_ops::scope::ScopeState;
use crate::panic::fatal;

/// How long a teardown waits for the group to become empty before it looks
/// again for a started halt or members blocked in a system call, which do
/// not signal it.
const TEARDOWN_POLL: Duration = Duration::from_millis(5);

/// The detached fibers of one root: `main`, one test, or a REPL session
/// between two teardowns (D-799, D-712).
pub struct DetachedGroup {
    /// The members and the closing flag: a leaf lock, taken with no scope,
    /// channel or fiber lock held.
    inner: Mutex<GroupInner>,
    /// Signalled when the group becomes empty.
    emptied: Condvar,
    /// Where its fibers' standard-stream writes go: a test's capture
    /// (D-723), never `main`'s or a REPL session's.
    capture: Option<Arc<Capture>>,
    /// The roots started in the group by [`Runtime::start_root_notify`]
    /// that have not reported yet: a termination reports to them
    /// (D-709). A leaf lock, taken with no other lock held.
    notified: Mutex<Vec<Weak<RootNotice>>>,
    /// The domains of the roots that started in the group (D-857): the
    /// deadlock detector surveys the group for each. A leaf lock.
    domains: Mutex<Vec<Weak<Domain>>>,
    /// Set by a teardown that skipped members blocked in a system call
    /// (D-786, D-804): the group is stuck, kept by the REPL until the
    /// session ends (D-827), and left out of deadlock detection (D-857;
    /// `sched/deadlock.rs`). Written under `jobs`' lock.
    stuck: AtomicBool,
    /// The blocking-pool jobs of the group's fibers that are counted in
    /// their domain (`deadlock::PoolPending`): a stuck mark releases them
    /// once. A leaf lock.
    jobs: Mutex<TrackedJobs>,
    /// Set while [`Runtime::teardown`] tears the group's fibers down: the
    /// values they release meanwhile call no user drop function (D-949,
    /// D-710; `ffi::finalize_external`).
    tearing_down: AtomicBool,
}

/// The tracked pool jobs of a group ([`DetachedGroup::track_job`]): weak
/// tokens, of which the finished ones are dropped in amortized O(1) per
/// tracked job (gate-1 review ROB1-1; D-860: tracking costs O(1) at a
/// submission).
#[derive(Default)]
struct TrackedJobs {
    /// The tokens, finished ones included until the next compaction.
    list: Vec<Weak<PoolPending>>,
    /// The length at which the next tracking compacts `list`: twice the
    /// live tokens found by the last compaction, at least
    /// [`TrackedJobs::MIN_COMPACT`].
    compact_at: usize,
    /// The tokens the compactions examined, for the tests.
    #[cfg(test)]
    examined: usize,
}

impl TrackedJobs {
    /// The smallest length that compacts.
    const MIN_COMPACT: usize = 16;

    /// Adds `job`, after dropping the finished tokens once the list has
    /// grown to twice the live ones the last compaction kept: a
    /// compaction that keeps L tokens is followed by at least
    /// `max(L, MIN_COMPACT - L)` additions before the next one, so the
    /// work is O(1) per addition, amortized.
    fn push(&mut self, job: &Arc<PoolPending>) {
        if self.list.len() >= self.compact_at.max(Self::MIN_COMPACT) {
            #[cfg(test)]
            {
                self.examined = self.examined.saturating_add(self.list.len());
            }
            self.list.retain(|known| known.strong_count() > 0);
            self.compact_at = self.list.len().saturating_mul(2);
        }
        self.list.push(Arc::downgrade(job));
    }
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
        Self::make(None)
    }

    /// A group whose fibers' standard-stream writes go to `capture` (a
    /// test's group, D-723): every fiber of the group, scoped, detached or
    /// nested, reaches it through the group it inherits (D-799).
    #[must_use]
    pub fn with_capture(capture: Arc<Capture>) -> Arc<Self> {
        Self::make(Some(capture))
    }

    /// An empty group with `capture`.
    fn make(capture: Option<Arc<Capture>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GroupInner::default()),
            emptied: Condvar::new(),
            capture,
            notified: Mutex::new(Vec::new()),
            domains: Mutex::new(Vec::new()),
            stuck: AtomicBool::new(false),
            jobs: Mutex::new(TrackedJobs::default()),
            tearing_down: AtomicBool::new(false),
        })
    }

    /// Whether a teardown is tearing the group's fibers down (D-710): a
    /// user drop function is then not called (D-949).
    pub(crate) fn tearing_down(&self) -> bool {
        self.tearing_down.load(Ordering::SeqCst)
    }

    /// Sets the teardown mark by hand, for the tests of drop functions.
    #[cfg(test)]
    pub(crate) fn set_tearing_down(&self, on: bool) {
        self.tearing_down.store(on, Ordering::SeqCst);
    }

    /// Records `domain` as the domain of a root that starts in the group,
    /// once (D-857): the detector finds every group of a domain so.
    pub(super) fn add_domain(&self, domain: &Arc<Domain>) {
        let mut domains = self.domains.lock();
        domains.retain(|known| known.strong_count() > 0);
        if !domains
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), Arc::as_ptr(domain)))
        {
            domains.push(Arc::downgrade(domain));
        }
    }

    /// Whether a root of `domain` started in the group.
    pub(super) fn has_domain(&self, domain: &Arc<Domain>) -> bool {
        self.domains
            .lock()
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), Arc::as_ptr(domain)))
    }

    /// Whether a teardown marked the group stuck (D-857, D-827).
    pub(crate) fn stuck(&self) -> bool {
        self.stuck.load(Ordering::SeqCst)
    }

    /// Marks the group stuck and takes its fibers' pending pool jobs out of
    /// their domains' counts, each once (D-857 under UQ-12 (b);
    /// `sched/deadlock.rs`): under the jobs' leaf lock, so a job registered
    /// later finds the mark ([`DetachedGroup::track_job`]). Marking again
    /// changes no count.
    pub(super) fn mark_stuck(&self) {
        let jobs = {
            let mut jobs = self.jobs.lock();
            self.stuck.store(true, Ordering::SeqCst);
            std::mem::take(&mut jobs.list)
        };
        for job in jobs.iter().filter_map(Weak::upgrade) {
            job.release();
        }
    }

    /// Registers a pending pool job of one of the group's fibers, counted
    /// in its domain: released at once if the group is stuck, so a stuck
    /// group's jobs never hold detection off; otherwise kept until the
    /// group is marked stuck or the job is gone. Forgets the jobs that are
    /// gone, in amortized O(1) per call ([`TrackedJobs::push`]).
    pub(super) fn track_job(&self, job: &Arc<PoolPending>) {
        let mut jobs = self.jobs.lock();
        if self.stuck.load(Ordering::SeqCst) {
            drop(jobs);
            job.release();
            return;
        }
        jobs.push(job);
    }

    /// The tracked tokens now and the tokens the compactions examined so
    /// far, for the tests.
    #[cfg(test)]
    pub(super) fn tracked_jobs(&self) -> (usize, usize) {
        let jobs = self.jobs.lock();
        (jobs.list.len(), jobs.examined)
    }

    /// The base scopes of the members for a deadlock survey, copied under
    /// the group's lock, or `None` while a slot is reserved for a fiber
    /// not created yet (busy).
    pub(super) fn survey_bases(&self) -> Option<Vec<Arc<ScopeState>>> {
        self.inner.lock().bases()
    }

    /// The group's capture, if any.
    pub(crate) fn capture(&self) -> Option<&Arc<Capture>> {
        self.capture.as_ref()
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

    /// Remembers a root started in the group that reports on a
    /// [`RootDone`], so a termination reaches it; forgets the roots that
    /// have reported.
    fn remember_notice(&self, notice: &Arc<RootNotice>) {
        let mut notified = self.notified.lock();
        notified.retain(|known| known.strong_count() > 0);
        notified.push(Arc::downgrade(notice));
    }

    /// The roots started in the group that report on a [`RootDone`] and
    /// have not reported yet.
    fn pending_notices(&self) -> Vec<Arc<RootNotice>> {
        self.notified
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

/// A root started by [`Runtime::start_root_in`]: the host waits for it
/// when it chooses, so one host thread can start several roots and wait
/// for them in any order.
pub struct RootHandle {
    /// Where the root, or a termination (D-709, D-802), sends its outcome.
    receiver: mpsc::Receiver<EntryOutcome>,
    /// The domain the root charges, which counts the entry's time.
    domain: Arc<Domain>,
    /// When the root was started.
    started: Instant,
    /// How long the root ran, set on its fiber when its thunk returns, so
    /// a host that waits later counts the entry's own time.
    took: Arc<OnceLock<Duration>>,
}

impl fmt::Debug for RootHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootHandle").finish_non_exhaustive()
    }
}

impl RootHandle {
    /// Blocks until the root ends, and reports how, as
    /// [`Runtime::run_root_in`] does.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::EntryLost`] if the root fiber ends without an
    /// outcome.
    pub fn wait(self) -> Result<EntryOutcome, RuntimeError> {
        let outcome = self.receiver.recv().map_err(|_| RuntimeError::EntryLost)?;
        self.count();
        Ok(outcome)
    }

    /// Counts the entry and its time in its domain, once it has ended.
    fn count(&self) {
        let time = self
            .took
            .get()
            .copied()
            .unwrap_or_else(|| self.started.elapsed());
        self.domain.stats().add_entry(time);
    }

    /// Waits at most `timeout` for the root to end: `Ok` with what
    /// [`RootHandle::wait`] would return once it has ended, `Err(self)`
    /// while it still runs.
    ///
    /// # Errors
    ///
    /// The handle itself, while the root still runs.
    pub fn wait_timeout(
        self,
        timeout: Duration,
    ) -> Result<Result<EntryOutcome, RuntimeError>, Self> {
        match self.receiver.recv_timeout(timeout) {
            Ok(outcome) => {
                self.count();
                Ok(Ok(outcome))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Err(RuntimeError::EntryLost)),
        }
    }
}

/// Where several roots report their end, each once, as `(tag, outcome)`:
/// one host thread blocks on the receiver for any of k roots (`cheby
/// test`'s runner), with no polling ([`Runtime::start_root_notify`]).
pub type RootDone = mpsc::Sender<(usize, Result<EntryOutcome, RuntimeError>)>;

/// The report of one root started by [`Runtime::start_root_notify`]: sent
/// once, by whichever comes first of the root's end (on its fiber), a
/// termination (D-709, through the root's group), the refusal of a root
/// started after a termination (D-802), or the loss of a root that never
/// ran (when its start closure is dropped unrun).
pub(super) struct RootNotice {
    /// The host's tag for the root.
    tag: usize,
    /// The host's sender, taken by the first report.
    done: Mutex<Option<RootDone>>,
    /// The domain the root charges, which counts the entry's time.
    domain: Arc<Domain>,
    /// When the root was started.
    started: Instant,
}

impl RootNotice {
    /// Sends `outcome` with the tag, unless the root has reported already
    /// or its start failed. Takes only the notice's own lock, a leaf, and
    /// sends after releasing it.
    pub(super) fn report(&self, outcome: Result<EntryOutcome, RuntimeError>) {
        let Some(done) = self.done.lock().take() else {
            return;
        };
        if outcome.is_ok() {
            self.domain.stats().add_entry(self.started.elapsed());
        }
        done.send((self.tag, outcome)).ok();
    }

    /// Never reports: the root did not start, and its host got the error.
    fn disarm(&self) {
        let done = self.done.lock().take();
        drop(done);
    }
}

impl Drop for RootNotice {
    fn drop(&mut self) {
        // The start closure that held the last reference was dropped
        // without running the root, as `run_root_in`'s host then sees
        // `EntryLost`.
        self.report(Err(RuntimeError::EntryLost));
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

/// What a teardown waited for. `skipped`: fibers blocked in a system call
/// (on the blocking pool or inline) that it did not wait for (D-786,
/// D-814). `halted`: a halt (a termination)
/// started during the teardown, which then stopped waiting; the host
/// completes the halt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeardownReport {
    /// The detached fibers that left the group while the teardown ran.
    pub waited: usize,
    /// The fibers blocked in a system call (on the blocking pool or
    /// inline), left in the group (D-786).
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

    /// [`Runtime::run_root_in`] without waiting: registers `group` (D-799)
    /// and starts the root at once; the host waits for it through the
    /// returned handle when it chooses. A root started after a termination
    /// never runs, and its handle reports `Exited` at once (D-802).
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] if the stack is too small or cannot be taken.
    ///
    /// # Safety
    ///
    /// As for [`Runtime::run_root`], for as long as the root and its
    /// fibers may run, which can outlast this call.
    pub unsafe fn start_root_in(
        &self,
        entry: EntryFn,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
    ) -> Result<RootHandle, RuntimeError> {
        self.shared.register_group(group);
        let started = Instant::now();
        let took = Arc::new(OnceLock::new());
        let on_fiber = Arc::clone(&took);
        let receiver = self.spawn_root_in(
            config,
            Arc::clone(group),
            Box::new(move |ctx| {
                // SAFETY: the caller guarantees that `entry` is an entry
                // thunk whose code stays valid while it runs; `ctx` is the
                // root fiber's control block.
                let word = unsafe { entry(ctx) };
                on_fiber.set(started.elapsed()).ok();
                word
            }),
            None,
        )?;
        Ok(RootHandle {
            receiver,
            domain: Arc::clone(&config.domain),
            started,
            took,
        })
    }

    /// [`Runtime::start_root_in`] that reports on `done` with `tag` instead
    /// of returning a handle: exactly one `(tag, outcome)` when the root
    /// ends, `Exited` when a termination ends it (D-709) or came before it
    /// (D-802), `Err(EntryLost)` if it ends without an outcome. Nothing is
    /// sent when this returns an error. A root that never ends sends
    /// nothing: one still blocked in a system call when the host's stop
    /// gives up on it (D-786, D-815), or frozen by
    /// a halt that is not a termination.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] if the stack is too small or cannot be taken.
    ///
    /// # Safety
    ///
    /// As for [`Runtime::start_root_in`].
    pub unsafe fn start_root_notify(
        &self,
        entry: EntryFn,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
        tag: usize,
        done: RootDone,
    ) -> Result<(), RuntimeError> {
        self.shared.register_group(group);
        let notice = Arc::new(RootNotice {
            tag,
            done: Mutex::new(Some(done)),
            domain: Arc::clone(&config.domain),
            started: Instant::now(),
        });
        // Before the root is registered: a termination that the
        // registration does not see reaches the notice through the group.
        group.remember_notice(&notice);
        // The root's base frame reports its outcome on the notice
        // (`Runtime::spawn_root_in`), after its catch point.
        let started = self.spawn_root_in(
            config,
            Arc::clone(group),
            Box::new(move |ctx| {
                // SAFETY: as in `start_root_in`.
                unsafe { entry(ctx) }
            }),
            Some(Arc::clone(&notice)),
        );
        match started {
            Ok(receiver) => {
                // A root refused because a termination halted the runtime
                // got `Exited` at once (D-802), and never runs; any other
                // message here was reported already.
                if let Ok(outcome) = receiver.try_recv() {
                    notice.report(Ok(outcome));
                }
                Ok(())
            }
            Err(error) => {
                notice.disarm();
                Err(error)
            }
        }
    }

    /// The program's exit status once a termination happened (D-709), else
    /// `None`: a host that keeps running entries (the REPL) reads it before
    /// each one. Public form of the runtime's fixed status.
    #[must_use]
    pub fn exit_status(&self) -> Option<i32> {
        if self.shared.terminated() {
            self.shared.exit_status()
        } else {
            None
        }
    }

    /// Whether the channel of `receiver`, a live `Receiver` handle, has a
    /// wait node linked in either of its queues. A diagnostic for tests,
    /// never part of a program's behavior: after a REPL teardown, no node
    /// of a cancelled fiber stays linked in a channel that a session value
    /// still reaches (D-712, D-831). The reader is
    /// `fiber_ops::channel`'s; this is its public path.
    ///
    /// # Safety
    ///
    /// `receiver` is a live `Receiver` handle; the caller holds a reference
    /// to it for the call.
    #[doc(hidden)]
    #[must_use]
    pub unsafe fn channel_has_waiters(receiver: Word) -> bool {
        // SAFETY: forwarded from the caller.
        unsafe { crate::fiber_ops::channel::channel_has_waiters(receiver) }
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
        // D-949: teardown calls no user drop function (D-710); set before
        // the first cancellation, cleared once the teardown ends.
        group.tearing_down.store(true, Ordering::SeqCst);
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
                    // The group stays closing with members blocked in a
                    // system call: stuck, left out of deadlock detection,
                    // its pending pool jobs out of their domains' counts
                    // (D-857 under UQ-12 (b)). The mark takes only the
                    // group's leaf job lock, never under the group's.
                    MutexGuard::unlocked(&mut inner, || group.mark_stuck());
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
        group.tearing_down.store(false, Ordering::SeqCst);
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
    pub(super) fn register_root(&self, root: Arc<RootEntry>) -> bool {
        let mut roots = self.roots.lock();
        if self.halting() {
            return false;
        }
        roots.push(root);
        true
    }

    /// Forgets root `root`'s host: the root has reported its outcome. The
    /// registry keeps the roots in registration order (the detector lists
    /// a domain's roots so).
    pub(super) fn unregister_root(&self, root: u64) {
        let removed = {
            let mut roots = self.roots.lock();
            roots
                .iter()
                .position(|known| known.id == root)
                .map(|at| roots.remove(at))
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
        for root in roots.iter() {
            root.sender.send(EntryOutcome::Exited(status)).ok();
        }
        drop(roots);
        // The roots that report on a `RootDone`: each was remembered in its
        // group before its registration, so it is either reached here or
        // refused at its registration (D-802).
        for group in self.live_groups() {
            for notice in group.pending_notices() {
                notice.report(Ok(EntryOutcome::Exited(status)));
            }
        }
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
