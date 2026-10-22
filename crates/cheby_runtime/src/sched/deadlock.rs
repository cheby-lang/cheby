//! The deadlock detector (D-724, D-856 to D-860): the per-domain pending
//! counts, the root registry's entries, the monitor's two-pass survey, its
//! report and its action.
//!
//! This documentation states the protocol exactly; `sched/deadlock_models.rs`
//! (wave 2) models it.
//!
//! # What is judged (D-857)
//!
//! A domain D is judged, never the runtime: `main`'s domain under `cheby
//! run`, each test's under `cheby test` (D-833), the session's in the REPL.
//! Its **roots** are the registered roots ([`RootEntry`], `Shared.roots`)
//! whose entry charges D; its **fibers** are its roots, every fiber in the
//! subtree of a root's base scope (D-859, `scope::open_root_base`), and
//! every fiber in the subtree of a member's base scope of every group
//! ([`DetachedGroup`]) that records D ([`DetachedGroup::has_domain`]: a group
//! records the domain of every root that starts in it), except the groups
//! marked stuck (below). A fiber reached there whose own domain is not D is
//! not D's and is ignored (a group shared by roots of several domains). D is
//! **deadlocked** when it has a registered, started, unreported root, every
//! fiber of D is `PARKED` in one of D-858's waits (its wait site is set:
//! `FiberSched::note_wait`) without the blocking mark, and every pending
//! count of D reads 0. A fiber parked anywhere else (the runtime's own test
//! mailbox, a socket wait, D-984) is never judged blocked: the survey reads
//! it as busy.
//!
//! # The pending counts ([`DomainCounts`])
//!
//! Three counters per domain and runtime, in a sidecar of the runtime keyed by
//! the domain (`Shared.domains`, [`DomainTable`]; the `Domain` itself is
//! untouched). A fiber caches its domain's counters on first use
//! (`FiberSched::domain_counts`).
//!
//! - **Timers** (`sched/timer.rs`). `arm` raises D's count, under the heap's
//!   lock, only when it really arms (a stopping runtime arms nothing, and a
//!   span longer than D-891's cap, `Deadline::Never`, never reaches `arm`);
//!   D is the arming fiber's domain, read on the arming fiber (a timer
//!   armed off every fiber counts in no domain). The entry keeps the
//!   counters. The count falls exactly once per armed entry, by whoever
//!   moves the entry's state out of `ARMED`: a winning `cancel` (or the
//!   stop's `clear`) lowers it right after its CAS; `fire_due` lowers it
//!   only after `target.fire()` has **returned**, so after the wake the
//!   firing stands for (`SelectTimer::fire` wakes the selecting fiber;
//!   `ScopeTimer::fire` runs the cancellation walk, which wakes every fiber
//!   it won before returning). The order, per firing:
//!   `CAS ARMED -> FIRED; live -= 1; target.fire() /* wake */; D.timers -= 1`.
//!   Before H1 the count fell before the wake (`settle` at `timer.rs:398`,
//!   fire at `:399`): a survey reading 0 between the two missed the wake.
//! - **Pool jobs** (`sched/blocking.rs`). `on_pool` takes a [`PoolPending`]
//!   token, which raises D's count (`counted = true`), registers it with
//!   the fiber's group ([`DetachedGroup::track_job`]), then stores
//!   `PARKING`, sets the blocking mark and submits. The completion stores
//!   the result, clears the mark, wakes the fiber (or skips the wake during
//!   a halt or after a stop gave up on the pool), and only then releases
//!   the token. A refused submit runs the jobs inline on the fiber, whose
//!   completion releases its token the same way. The order, per job:
//!   `D.pool += 1; mark = 1; submit ... mark = 0; wake; release`.
//! - **Release is exactly once**: [`PoolPending::release`] is
//!   `if counted.swap(false) { D.pool -= 1 }` (`SeqCst`). Its two callers are
//!   the completion and a stuck mark (below), so a job leaves the count
//!   once, whichever comes first.
//! - **Armed IO waits** (`sched/io.rs`, D-984). A socket wait raises D's
//!   count when it arms its direction, under the socket entry's lock, and
//!   takes an [`IoPending`] token, released exactly once by the same swap:
//!   by the dispatcher right after its wake, or by the waiting fiber once
//!   it runs again (woken by a cancellation, or not parked after all). The
//!   order, per dispatched wait: `D.io += 1; ... settle Claimed; wake;
//!   release`. A registered socket nobody waits on is not counted (D-984),
//!   and a fiber parked on a socket records no wait site, so it is never
//!   listed (D-858).
//!
//! # Stuck groups (D-857's stuck-group sentence; plan §7.1, UQ-12 (b))
//!
//! A teardown that skips members blocked in a system call (D-786, D-804)
//! leaves its group closing; the REPL keeps such a group until the session
//! ends (D-827). [`crate::sched::Runtime::teardown`] then marks the group
//! stuck ([`DetachedGroup::mark_stuck`]): under the group's job lock (a
//! leaf) it sets `stuck` and takes the group's list; after the lock it
//! releases each token still alive (a gone token's completion released it
//! already). A token registered later ([`DetachedGroup::track_job`],
//! also under that lock) finds `stuck` set and is released at once, so a
//! stuck group's job is never counted afterwards. The race with a
//! completion is the token's swap: the completion's `release` and the mark's
//! `release` both swap `counted`, so exactly one lowers the count, and a
//! completion after the mark changes nothing (the wave-2 model 8). A second
//! teardown of the group (the session's end) marks it again and finds an
//! empty list: no count changes. From the mark on, the survey never walks
//! the group (its fibers are neither surveyed nor listed), and the monitor
//! does not count a fiber of it blocked inline on a worker as a running
//! fiber of its domain. The cost (D-857): a stuck fiber whose call returns
//! still unwinds, and may wake a later entry's fiber through a value a
//! session value shares, after that entry was reported.
//!
//! # The resume count and the wait site
//!
//! `FiberSched::resumes` is stored `+ 1` (`Relaxed`, a load and a store, its
//! one writer) by the worker that resumes the fiber, before the switch
//! (`Worker::run_fiber`). A park ends with the worker's `PARKING -> PARKED`
//! CAS (`SeqCst`) on the same thread, after the fiber's own stores, so a
//! survey that reads `PARKED` (`SeqCst`) and then the count (`Relaxed`) sees
//! the count of the resume before that park. The wait site (two atomics,
//! `wait_op` then `wait_frame`, critique RISK-1) is stored by the fiber
//! before `begin_park`, so it is visible to the same read; a fiber read
//! `PARKED` may be resumed at once and clear its site, so the site read in
//! a pass is never acted on.
//!
//! # The survey (D-860)
//!
//! The monitor runs [`detect`] after each look at the workers: on every
//! tick, and at once when a worker that parks finds it was the last
//! unparked one (`Idle::transition_worker_to_parked`, the check-now flag).
//! Nothing is surveyed while the runtime stops or halts, for a domain with
//! a reported root, or for a domain one of whose fibers runs on a worker
//! (read through the workers' gates; a fiber of a stuck group blocked
//! inline does not count). For each candidate domain:
//!
//! 1. **The O(1) look** (critique RISK-10): the counts are read, and each
//!    root's state and resume count under its base scope's lock while the
//!    base has not ended. A count above 0 or a root that is not `PARKED`
//!    skips the survey. A change of the roots' resume counts since the last
//!    look resets the domain's backoff.
//! 2. **Backoff**: after a failed survey the domain waits 10, 20, 40, 80,
//!    then 160 ms before the next one, except on a check-now.
//! 3. **Pass 1 and pass 2**: each walks the roots' base scopes and the
//!    domain's groups' member bases, scope by scope, one scope lock at a time
//!    (`ScopeState::survey_step`; a group's member list is copied under the
//!    group's leaf lock first, and a reserved slot reads as busy). Per fiber
//!    of D reached first there: `state` (`SeqCst`), stopping at once unless
//!    `PARKED`; then the blocking mark (`SeqCst`), stopping if set; then
//!    `resumes` (`Relaxed`); then the wait site (`Relaxed`), stopping if
//!    none. A pass collects `(id, resumes)` only. A base scope that has ended
//!    reads as busy (its fiber is ending). After the walk the counts are
//!    read; a count above 0 fails the pass.
//! 4. **Confirmation**: D is deadlocked when both passes completed and
//!    collected the same sorted `(id, resumes)` list. Soundness: let t be the
//!    end of pass 1. Every fiber seen in both passes was `PARKED` at both
//!    reads with no resume between (the counts are equal), so it was parked
//!    at t; a fiber created after its scope was walked in pass 1 changes
//!    pass 2's list; a timer or pool wake in the window is either still
//!    counted when the counts are read after a pass (the count falls after
//!    the wake), or it made its fiber runnable before, which pass 2 sees as
//!    a state or a count; so at t nothing in D runs, can run or is pending,
//!    and no host wakes a domain's fibers except a teardown (D-857). The
//!    one wake from outside is D-857's stuck-group wake (above), which the
//!    collect step's re-check handles.
//! 5. **Collect** (critique RISK-2): only now, a third walk takes each scope's
//!    lock in turn; under it, each fiber's state and resume count are read
//!    again and the whole report is given up (a failed survey) if either
//!    changed, or if a fiber is missing or new; then the line is built
//!    under that lock: the operation from `wait_site`, the location from
//!    `locate::locate` over the frame the site recorded, within the fiber's
//!    stack bounds. A frame of 0 gives `<unknown location>`. After the walk
//!    the state and resume count are read once more, and the report is
//!    given up if either changed (D-860). Within D nothing can wake the
//!    fiber, but D-857's stuck-group wake comes from outside the survey: a
//!    stuck fiber whose call returns can wake it at any moment, even
//!    during the walk. The walk's reads are relaxed atomic loads within
//!    the fiber's mapped stack (`locate::LiveStack`), so such a race reads
//!    stale or new words, and the re-check then drops the line. The
//!    re-check is ordered after the walk by an `Acquire` fence (board item
//!    DLm-1): a word the walk read from the running fiber was written after
//!    `Worker::run_fiber` stored `RUNNING` and the count, then stored
//!    `running` and loaded `life` (all `SeqCst`; the load orders the
//!    fiber's later stores after it on arm64, and x86-64 keeps stores in
//!    order), so the re-check then reads `RUNNING` or a newer count. This
//!    is a hardware argument, since the fiber's stack stores are plain
//!    stores of JIT code: C11 and `loom` need a `Release` fence on the
//!    resume path as well (the wave-2 model 6 checks that pair), which
//!    D-860's one relaxed store per resume leaves out. So no
//!    line is built from a stack that ran, and no control block is read
//!    after the lock that keeps its fiber from retiring is released, woken
//!    or not; a root's own block is
//!    read only under its base's lock while the base has not ended.
//! 6. **The report** (D-858): `fatal error: all fibers are blocked`, then
//!    `  blocked in <operation> at <location>` per fiber, sorted by path
//!    (bytes), line, column, then operation, `<unknown location>` lines
//!    last (by operation); every line ends in '\n'.
//! 7. **The action** (D-859), with no worker gate, scope lock or runtime
//!    lock held, unless the runtime now stops or halts: each root of D is
//!    reported once (`reported`, a CAS). A `cheby run` root
//!    ([`EntryMode::Run`]) gets `EntryOutcome::Deadlocked(report)` through
//!    its registry sender, as a termination sends `Exited`; `run_main`
//!    writes it and halts with nothing unwinding. Any other root gets
//!    `cancel::cancel_deadlocked_root`, which stores the report under the
//!    base's lock while the base has not ended and cancels the base; the
//!    root unwinds to its catch point, where `unwind::record::finish_entry`
//!    turns the cancellation into `Deadlocked(report)`.
//!
//! The monitor never runs Cheby code and never writes a stream: `cheby
//! run`'s report is written by `run_main` on the host thread.
//!
//! # A root's base frame (critique C5, RISK-3)
//!
//! Every root (`main`, a test, a REPL entry) opens its base scope before its
//! body and records it in its registry entry; after the body returns or
//! unwinds, the deadlock's cancellation included, it ends the base under its
//! lock (`scope::close_root_base`), then reports its outcome, unregisters
//! and retires. A survey or an action that finds the base ended reads the
//! root as finished and touches none of its block.

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::time::{Duration, Instant};

use crossbeam_utils::CachePadded;
use parking_lot::Mutex;

use super::fiber::{FiberRef, WaitOp, state};
use super::{DetachedGroup, Shared};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;
use crate::fiber_ops::cancel;
use crate::fiber_ops::scope::ScopeState;
use crate::locate::{self, Location};
use crate::panic::fatal;

/// The first line of a deadlock report (D-724, D-858).
pub(crate) const HEADER: &str = "fatal error: all fibers are blocked\n";

/// The text of a line whose location was not found (D-856, D-858).
const UNKNOWN: &str = "<unknown location>";

/// The waits after a failed survey, by how many surveys in a row failed
/// (D-860).
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
];

/// The pending timers, blocking-pool jobs and armed IO waits of one domain
/// in one runtime (D-857, D-984, module documentation): a count falls only
/// after the wake it stands for.
#[derive(Debug, Default)]
pub(crate) struct DomainCounts {
    /// Timers armed by a fiber of the domain whose firing has not returned
    /// and which no cancel won.
    timers: CachePadded<AtomicUsize>,
    /// Pool jobs of fibers of the domain whose completion has not finished
    /// its wake, and that no stuck mark released.
    pool: CachePadded<AtomicUsize>,
    /// Socket waits a fiber of the domain armed (`sched/io.rs`, D-984),
    /// not registered sockets: from the arming until the wake that ends
    /// the wait has happened ([`IoPending::release`]).
    io: CachePadded<AtomicUsize>,
}

impl DomainCounts {
    /// A timer was armed.
    pub(super) fn timer_armed(&self) {
        self.timers.fetch_add(1, Ordering::SeqCst);
    }

    /// An armed timer was cancelled, cleared, or its firing returned.
    pub(super) fn timer_done(&self) {
        if self.timers.fetch_sub(1, Ordering::SeqCst) == 0 {
            fatal(format_args!("a domain's pending timer count fell below 0"));
        }
    }

    /// The pending timers.
    pub(crate) fn timers(&self) -> usize {
        self.timers.load(Ordering::SeqCst)
    }

    /// The pending pool jobs.
    pub(crate) fn pool(&self) -> usize {
        self.pool.load(Ordering::SeqCst)
    }

    /// The armed IO waits (D-984).
    pub(crate) fn io(&self) -> usize {
        self.io.load(Ordering::SeqCst)
    }

    /// Whether every count reads 0 (timers first, then the pool, then the
    /// armed IO waits).
    fn quiet(&self) -> bool {
        self.timers() == 0 && self.pool() == 0 && self.io() == 0
    }

    /// Counts one more armed IO wait and returns its token, which lowers
    /// the count once ([`IoPending::release`]).
    pub(super) fn io_armed(self: &Arc<Self>) -> Arc<IoPending> {
        self.io.fetch_add(1, Ordering::SeqCst);
        Arc::new(IoPending {
            counts: Arc::clone(self),
            counted: AtomicBool::new(true),
        })
    }

    /// Counts one more pool job and returns its token, which lowers the
    /// count once ([`PoolPending::release`]).
    pub(super) fn pool_submitted(self: &Arc<Self>) -> Arc<PoolPending> {
        self.pool.fetch_add(1, Ordering::SeqCst);
        Arc::new(PoolPending {
            counts: Arc::clone(self),
            counted: AtomicBool::new(true),
        })
    }
}

/// One blocking-pool job counted in its domain's `pool` count (module
/// documentation): released once, by its completion after the wake or by
/// its group's stuck mark, whichever comes first.
#[derive(Debug)]
pub(crate) struct PoolPending {
    /// Its domain's counts.
    counts: Arc<DomainCounts>,
    /// Whether it is still counted.
    counted: AtomicBool,
}

impl PoolPending {
    /// Takes the job out of its domain's count, if it is still counted;
    /// returns whether this call did.
    pub(crate) fn release(&self) -> bool {
        if !self.counted.swap(false, Ordering::SeqCst) {
            return false;
        }
        if self.counts.pool.fetch_sub(1, Ordering::SeqCst) == 0 {
            fatal(format_args!("a domain's pending pool count fell below 0"));
        }
        true
    }
}

/// One armed IO wait counted in its domain's `io` count (D-984,
/// `sched/io.rs`): released once, by the dispatcher after its wake or by
/// the waiting fiber once it runs again (a cancellation's wake, a re-check
/// that found the socket ready, a refused publication), whichever comes
/// first.
#[derive(Debug)]
pub(crate) struct IoPending {
    /// Its domain's counts.
    counts: Arc<DomainCounts>,
    /// Whether it is still counted.
    counted: AtomicBool,
}

impl IoPending {
    /// Takes the wait out of its domain's count, if it is still counted;
    /// returns whether this call did.
    pub(crate) fn release(&self) -> bool {
        if !self.counted.swap(false, Ordering::SeqCst) {
            return false;
        }
        if self.counts.io.fetch_sub(1, Ordering::SeqCst) == 0 {
            fatal(format_args!("a domain's armed IO wait count fell below 0"));
        }
        true
    }
}

/// The runtime's [`DomainCounts`] by domain (module documentation): a
/// sidecar keyed by the domain, whose lock is a leaf.
#[derive(Debug, Default)]
pub(super) struct DomainTable(Mutex<Vec<(Weak<Domain>, Arc<DomainCounts>)>>);

impl DomainTable {
    /// The counts of `domain`, made on first use; forgets the domains that
    /// no longer exist (a pending timer or job keeps its own counts).
    pub(super) fn of(&self, domain: &Arc<Domain>) -> Arc<DomainCounts> {
        let mut table = self.0.lock();
        table.retain(|(known, _)| known.strong_count() > 0);
        if let Some((_, counts)) = table
            .iter()
            .find(|(known, _)| ptr::eq(known.as_ptr(), Arc::as_ptr(domain)))
        {
            return Arc::clone(counts);
        }
        let counts = Arc::new(DomainCounts::default());
        table.push((Arc::downgrade(domain), Arc::clone(&counts)));
        counts
    }
}

/// The counts of the domain of `me`, a fiber running on this thread, in its
/// runtime; `None` for an isolated entry. Only `me` itself calls it.
pub(super) fn counts_of(me: &ExecCtx) -> Option<Arc<DomainCounts>> {
    let shared = me.sched.runtime()?;
    Some(Arc::clone(
        me.sched
            .domain_counts()
            .get_or_init(|| shared.domains.of(me.domain())),
    ))
}

/// A registered root (`Shared.roots`): the host waiting for its outcome and
/// what the detector needs of it.
pub(crate) struct RootEntry {
    /// The root's number.
    pub(super) id: u64,
    /// Where its outcome goes: a termination's `Exited` (D-709), and a
    /// `cheby run` root's `Deadlocked` (D-859).
    pub(super) sender: mpsc::Sender<EntryOutcome>,
    /// The domain its entry charges.
    domain: Arc<Domain>,
    /// That domain's pending counts.
    counts: Arc<DomainCounts>,
    /// Whether it is a `cheby run` root ([`crate::entry::EntryMode::Run`]):
    /// a deadlock is sent to its host and nothing unwinds (D-859).
    run: bool,
    /// The root and its base scope, set by its base frame when it starts,
    /// before its body.
    started: OnceLock<(FiberRef, Arc<ScopeState>)>,
    /// Set once by the detector's action: a root is reported once.
    reported: AtomicBool,
}

impl RootEntry {
    /// The entry of root `id` of `shared`, whose host waits on `sender`.
    pub(super) fn new(
        shared: &Shared,
        id: u64,
        sender: mpsc::Sender<EntryOutcome>,
        domain: &Arc<Domain>,
        run: bool,
    ) -> Self {
        Self {
            id,
            sender,
            domain: Arc::clone(domain),
            counts: shared.domains.of(domain),
            run,
            started: OnceLock::new(),
            reported: AtomicBool::new(false),
        }
    }

    /// Records the root and its base scope, from the root's base frame
    /// before its body runs.
    pub(super) fn start(&self, root: FiberRef, base: Arc<ScopeState>) {
        if self.started.set((root, base)).is_err() {
            fatal(format_args!("a root started twice"));
        }
    }
}

/// A domain the detector judges: its roots, started and unreported.
struct Candidate {
    domain: Arc<Domain>,
    counts: Arc<DomainCounts>,
    roots: Vec<Arc<RootEntry>>,
}

impl Candidate {
    /// The domain's address, its key.
    fn key(&self) -> usize {
        Arc::as_ptr(&self.domain).addr()
    }
}

/// The detector's state between ticks: one backoff per domain.
#[derive(Debug, Default)]
pub(super) struct Detector {
    backoff: HashMap<usize, Backoff>,
}

/// A domain's backoff after failed surveys (D-860).
#[derive(Debug, Clone, Copy)]
struct Backoff {
    /// Failed surveys in a row, capped at the last step.
    failed: usize,
    /// No survey before this, except on a check-now.
    next: Instant,
    /// The roots' resume counts at the last look: a change resets.
    signature: u64,
}

/// One line of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    location: Option<Location>,
    op: WaitOp,
}

/// Runs the detector once (module documentation): on the monitor thread,
/// after its look at the workers, holding no lock. `busy` holds the
/// addresses of the domains whose fibers run on a worker now; `check_now`
/// says the last unparked worker parked, which skips the backoff.
pub(super) fn detect(
    shared: &Shared,
    detector: &mut Detector,
    busy: &[usize],
    check_now: bool,
    now: Instant,
) {
    if !shared.detect_deadlocks || shared.stopping() {
        return;
    }
    let candidates = candidates(shared);
    detector
        .backoff
        .retain(|key, _| candidates.iter().any(|candidate| candidate.key() == *key));
    let mut groups: Option<Vec<Arc<DetachedGroup>>> = None;
    for candidate in &candidates {
        if shared.stopping() {
            return;
        }
        let key = candidate.key();
        if busy.contains(&key) {
            continue;
        }
        let Some(signature) = look(candidate) else {
            // Not quiet: a count above 0, or a root not parked.
            detector.backoff.remove(&key);
            continue;
        };
        let backoff = detector.backoff.entry(key).or_insert(Backoff {
            failed: 0,
            next: now,
            signature,
        });
        if backoff.signature != signature {
            *backoff = Backoff {
                failed: 0,
                next: now,
                signature,
            };
        }
        if !check_now && !eager(shared) && now < backoff.next {
            continue;
        }
        let all = groups.get_or_insert_with(|| shared.live_groups());
        let domain_groups: Vec<Arc<DetachedGroup>> = all
            .iter()
            .filter(|group| !group.stuck() && group.has_domain(&candidate.domain))
            .cloned()
            .collect();
        if let Some(report) = survey(shared, candidate, &domain_groups) {
            detector.backoff.remove(&key);
            act(shared, candidate, &report);
        } else {
            let delay = if eager(shared) {
                Duration::ZERO
            } else {
                BACKOFF
                    .get(backoff.failed)
                    .or(BACKOFF.last())
                    .copied()
                    .unwrap_or(Duration::ZERO)
            };
            backoff.failed = backoff.failed.saturating_add(1).min(BACKOFF.len());
            backoff.next = now.checked_add(delay).unwrap_or(now);
        }
    }
}

/// Whether the tests asked for surveys with no backoff (the probe).
fn eager(shared: &Shared) -> bool {
    #[cfg(test)]
    {
        shared.probe.eager_detection.load(Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        let _ = shared;
        false
    }
}

/// The domains with a registered, started root, none of whose roots was
/// reported, each with its roots in registration order. The roots' lock is
/// a leaf: the entries are copied under it and used after it.
fn candidates(shared: &Shared) -> Vec<Candidate> {
    let roots: Vec<Arc<RootEntry>> = shared.roots.lock().clone();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut reported: Vec<usize> = Vec::new();
    for root in roots {
        let key = Arc::as_ptr(&root.domain).addr();
        if root.reported.load(Ordering::SeqCst) {
            reported.push(key);
            continue;
        }
        if root.started.get().is_none() {
            // Not started yet: its domain cannot be judged before its root
            // has its base.
            reported.push(key);
            continue;
        }
        match candidates
            .iter_mut()
            .find(|candidate| candidate.key() == key)
        {
            Some(candidate) => candidate.roots.push(root),
            None => candidates.push(Candidate {
                domain: Arc::clone(&root.domain),
                counts: Arc::clone(&root.counts),
                roots: vec![root],
            }),
        }
    }
    candidates.retain(|candidate| !reported.contains(&candidate.key()));
    candidates
}

/// The O(1) look before a survey (critique RISK-10): `None` unless both
/// counts read 0 and every root is `PARKED`, each root read under its base
/// scope's lock while the base has not ended; otherwise the sum of the
/// roots' resume counts, which resets the backoff when it changes.
fn look(candidate: &Candidate) -> Option<u64> {
    if !candidate.counts.quiet() {
        return None;
    }
    let mut signature = 0_u64;
    for root in &candidate.roots {
        let (fiber, base) = root.started.get()?;
        let mut parked = None;
        let mut nested = Vec::new();
        let alive = base.survey_step(
            &mut |ctx, _| {
                if ptr::eq(ctx, fiber.as_ptr()) {
                    let state = ctx.sched.state().load(Ordering::SeqCst);
                    parked = (state == state::PARKED).then(|| ctx.sched.resumes());
                }
            },
            &mut nested,
        );
        drop(nested);
        if !alive {
            return None;
        }
        signature = signature.wrapping_add(parked?);
    }
    Some(signature)
}

/// Walks the fibers of `candidate`'s domain (module documentation, step 3):
/// calls `visit` on each fiber of the domain reached first, under the lock
/// of the scope that keeps it from retiring, and stops when `visit` returns
/// false. Returns whether the walk completed: false when `visit` stopped
/// it, a group had a reserved slot, or a base scope had ended.
fn walk(
    candidate: &Candidate,
    groups: &[Arc<DetachedGroup>],
    visit: &mut dyn FnMut(&ExecCtx) -> bool,
) -> bool {
    let domain = Arc::as_ptr(&candidate.domain);
    let mut bases: Vec<Arc<ScopeState>> = Vec::new();
    for root in &candidate.roots {
        let Some((_, base)) = root.started.get() else {
            return false;
        };
        bases.push(Arc::clone(base));
    }
    for group in groups {
        match group.survey_bases() {
            Some(members) => bases.extend(members),
            None => return false,
        }
    }
    let mut scopes = Vec::new();
    for base in &bases {
        scopes.push(Arc::clone(base));
        while let Some(scope) = scopes.pop() {
            let mut going = true;
            let alive = scope.survey_step(
                &mut |ctx, first| {
                    if going && first && ptr::eq(Arc::as_ptr(ctx.domain()), domain) {
                        going = visit(ctx);
                    }
                },
                &mut scopes,
            );
            if !going || (!alive && Arc::ptr_eq(&scope, base)) {
                return false;
            }
        }
    }
    true
}

/// One pass (module documentation, step 3): the sorted `(id, resumes)` of
/// every fiber of the domain, or `None` when one is not parked in a wait,
/// is marked, the walk stopped, or a count is above 0 after the walk.
fn pass(candidate: &Candidate, groups: &[Arc<DetachedGroup>]) -> Option<Vec<(u64, u64)>> {
    let mut seen = Vec::new();
    let complete = walk(candidate, groups, &mut |ctx| {
        let sched = &ctx.sched;
        if sched.state().load(Ordering::SeqCst) != state::PARKED || sched.blocking() {
            return false;
        }
        let resumes = sched.resumes();
        if sched.wait_site().is_none() {
            return false;
        }
        seen.push((sched.id(), resumes));
        true
    });
    if !complete || !candidate.counts.quiet() {
        return None;
    }
    seen.sort_unstable();
    Some(seen)
}

/// The two passes, the collect step and the report (module documentation,
/// steps 3 to 6), or `None` when the domain is not deadlocked or the report
/// was given up.
fn survey(shared: &Shared, candidate: &Candidate, groups: &[Arc<DetachedGroup>]) -> Option<String> {
    #[cfg(test)]
    shared.probe.surveyed(candidate.key());
    #[cfg(not(test))]
    let _ = shared;
    let first = pass(candidate, groups)?;
    #[cfg(test)]
    shared.probe.between_passes();
    let second = pass(candidate, groups)?;
    if first != second {
        return None;
    }
    #[cfg(test)]
    shared.probe.confirmed();
    let lines = collect(shared, candidate, groups, &second)?;
    Some(render(lines))
}

/// The collect step (module documentation, step 5): each fiber's line,
/// built under the lock of its scope with its state and resume count read
/// again; `None` (the report given up) if a fiber changed, is missing or is
/// new.
fn collect(
    shared: &Shared,
    candidate: &Candidate,
    groups: &[Arc<DetachedGroup>],
    seen: &[(u64, u64)],
) -> Option<Vec<Line>> {
    let mut lines = Vec::with_capacity(seen.len());
    let complete = walk(candidate, groups, &mut |ctx| {
        let sched = &ctx.sched;
        let Ok(at) = seen.binary_search_by_key(&sched.id(), |&(id, _)| id) else {
            return false;
        };
        let Some(&(_, resumes)) = seen.get(at) else {
            return false;
        };
        if sched.state().load(Ordering::SeqCst) != state::PARKED || sched.resumes() != resumes {
            return false;
        }
        let Some((op, frame)) = sched.wait_site() else {
            return false;
        };
        #[cfg(test)]
        shared.probe.before_locate();
        let location = if frame == 0 {
            None
        } else {
            let (low, high) = ctx.stack_bounds();
            locate::locate(frame, low, high)
        };
        // Critique RISK-2 and D-860: the line stands only if the fiber did
        // not run while its stack was walked. A confirmed deadlock wakes
        // nothing in the domain, except D-857's stuck-group wake: a stuck
        // fiber whose call returns can wake this fiber through a shared
        // value at any time. The report is then given up (a failed survey).
        // The fence orders the re-check after the walk's relaxed reads, so
        // a word read from the running fiber's stack makes the re-check see
        // its resume (module documentation, step 5; board item DLm-1).
        std::sync::atomic::fence(Ordering::Acquire);
        if sched.state().load(Ordering::SeqCst) != state::PARKED || sched.resumes() != resumes {
            walked_live(shared);
            return false;
        }
        lines.push(Line { location, op });
        true
    });
    (complete && lines.len() == seen.len()).then_some(lines)
}

/// The collect step walked a stack whose fiber ran meanwhile (critique
/// RISK-2; only D-857's stuck-group wake can cause it) and gave the report
/// up: counted for the tests.
fn walked_live(shared: &Shared) {
    #[cfg(test)]
    shared.probe.walked_live.fetch_add(1, Ordering::SeqCst);
    #[cfg(not(test))]
    let _ = shared;
}

/// D-858's text: the header, then one line per fiber, sorted by path
/// (bytes), line, column, then operation, `<unknown location>` lines last.
fn render(mut lines: Vec<Line>) -> String {
    lines.sort_by(|a, b| match (&a.location, &b.location) {
        (Some(x), Some(y)) => x
            .path
            .as_bytes()
            .cmp(y.path.as_bytes())
            .then(x.line.cmp(&y.line))
            .then(x.column.cmp(&y.column))
            .then(a.op.name().cmp(b.op.name())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.op.name().cmp(b.op.name()),
    });
    let mut report = String::from(HEADER);
    for line in lines {
        report.push_str("  blocked in ");
        report.push_str(line.op.name());
        report.push_str(" at ");
        match &line.location {
            Some(location) => report.push_str(&location.to_string()),
            None => report.push_str(UNKNOWN),
        }
        report.push('\n');
    }
    report
}

/// The action (module documentation, step 7): each root of the domain is
/// reported once, unless the runtime now stops or halts. With no lock held.
fn act(shared: &Shared, candidate: &Candidate, report: &str) {
    if shared.stopping() {
        return;
    }
    for root in &candidate.roots {
        if root
            .reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            continue;
        }
        #[cfg(test)]
        shared.probe.reported(candidate.key());
        if root.run {
            root.sender
                .send(EntryOutcome::Deadlocked(report.to_owned()))
                .ok();
        } else if let Some((fiber, base)) = root.started.get() {
            cancel::cancel_deadlocked_root(fiber, base, report.to_owned());
        }
    }
}

#[cfg(test)]
pub(super) mod tests_support {
    //! The report's rendering, for `deadlock_tests`.

    use super::{Line, Location, WaitOp, render};

    /// A line's location as `(path, line, column)`, `None` when unknown.
    pub(crate) type At<'a> = Option<(&'a str, u32, u32)>;

    /// Renders lines given as `(location, op)`.
    pub(crate) fn render_lines(lines: &[(At<'_>, WaitOp)]) -> String {
        render(
            lines
                .iter()
                .map(|(location, op)| Line {
                    location: location.map(|(path, line, column)| Location {
                        path: path.into(),
                        line,
                        column,
                    }),
                    op: *op,
                })
                .collect(),
        )
    }
}
