//! Timers (D-682, D-652, architecture §2.3): one heap per runtime.
//!
//! - **Spans** cross from std as `(seconds, nanoseconds)` ([`deadline_after`],
//!   D-652): a span of zero or less is [`Deadline::Elapsed`], and a span
//!   whose end `Instant` cannot represent is [`Deadline::Never`], which arms
//!   nothing.
//! - **Entries** are reference counted ([`TimerHandle`]): an entry outlives
//!   the frame that armed it (a `select` woken by a channel returns while
//!   its timer is still in the heap; D-708's frozen fibers leave timers
//!   behind), so it never points into a fiber's stack. Its own state word
//!   moves once, from armed to cancelled or fired, by a compare-and-swap:
//!   only the winner acts, takes the target out of the entry and decrements
//!   the live count. Cancellation is lazy: a cancelled entry stays in the
//!   heap until it is popped, or until a rebuild drops it when more than
//!   half the heap is dead. So the heap's length is never "timers pending";
//!   [`Shared::timers_pending`] reads the live count (D-724's seam, H1).
//! - **Order**: `(deadline, seq)`, so timers with equal deadlines fire in
//!   the order they were armed (D-682).
//! - **The due check** is lock-free: an atomic holds the earliest deadline
//!   in the heap, in nanoseconds since the runtime's epoch.
//! - **Firing** ([`Shared::fire_due`]) pops the due entries under the heap's
//!   lock, releases it, and only then calls each target, with no runtime
//!   lock held: a scope's target runs the cancellation walk, which takes
//!   scope and fiber locks. A timer never fires before its deadline
//!   (D-671): the instant it fires at is never later than `Instant::now()`.
//!   Firing runs no Cheby code (ADR-0029). The poll holder, a worker that
//!   is about to search, and the monitor on every tick fire timers
//!   (GAP(monitor-fires-timers)).
//! - **Locks**: the heap's lock is a leaf. It may be taken under a channel
//!   lock (a `select` arms its timer under its channels' locks), and
//!   nothing is taken while it is held. An entry's own lock, which guards
//!   its target, is a leaf too and is never taken under the heap's lock.
//! - **Stop** (C7): [`Shared::clear_timers`] drops every entry unfired once
//!   every thread is joined, and a stopping runtime arms nothing: its
//!   handles never fire.
//!
//! Per-worker heaps wait for measurements (D-731).

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::Shared;

/// What a timer does when it fires: runs on a worker loop, the poll holder
/// or the monitor, with no runtime lock held; never runs Cheby code and
/// never blocks.
///
/// The target is dropped where its timer ends: after it fired, in the
/// winning [`TimerHandle::cancel`] (on the canceller's thread, under any
/// lock the canceller holds), or by [`Shared::clear_timers`]. So its `Drop`
/// takes no runtime lock either.
pub(crate) trait TimerTarget: Send + Sync {
    /// Fires the timer: wakes a fiber, cancels a scope.
    fn fire(&self);
}

/// A deadline from std's `(seconds, nanoseconds)` (D-652, D-682).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deadline {
    /// The span was zero or less: the deadline has passed.
    Elapsed,
    /// The span ends past `Instant`'s range: the deadline never comes, and
    /// nothing is armed for it.
    Never,
    /// The span ends at this instant.
    At(Instant),
}

/// The deadline `(seconds, nanoseconds)` after now. `seconds < 0 ||
/// (seconds == 0 && nanoseconds <= 0)` is `Elapsed`; a span past
/// `Instant`'s range is `Never`.
pub(crate) fn deadline_after(seconds: i64, nanoseconds: i64) -> Deadline {
    deadline_from(Instant::now(), seconds, nanoseconds)
}

/// The nanoseconds in a second.
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// [`deadline_after`] from `now`. Std passes both parts with the same sign
/// (it truncates toward zero, D-652); a mixed pair is read as the sum of
/// its parts, and a sum of zero or less is elapsed too.
fn deadline_from(now: Instant, seconds: i64, nanoseconds: i64) -> Deadline {
    if seconds < 0 || (seconds == 0 && nanoseconds <= 0) {
        return Deadline::Elapsed;
    }
    let total = i128::from(seconds)
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(i128::from(nanoseconds));
    if total <= 0 {
        return Deadline::Elapsed;
    }
    let (Some(whole), Some(part)) = (
        total.checked_div(NANOS_PER_SECOND),
        total.checked_rem(NANOS_PER_SECOND),
    ) else {
        return Deadline::Never;
    };
    let (Ok(whole), Ok(part)) = (u64::try_from(whole), u32::try_from(part)) else {
        return Deadline::Never;
    };
    match now.checked_add(Duration::new(whole, part)) {
        Some(at) => Deadline::At(at),
        None => Deadline::Never,
    }
}

/// The values of a timer entry's state word.
mod state {
    /// In the heap, waiting for its deadline.
    pub(super) const ARMED: u8 = 0;
    /// Cancelled by its handle, cleared at stop, or never armed.
    pub(super) const CANCELLED: u8 = 1;
    /// Fired.
    pub(super) const FIRED: u8 = 2;
}

/// One armed timer, shared by the heap and the arming code's handles.
struct TimerEntry {
    /// One of [`state`]'s values; leaves `ARMED` once.
    state: AtomicU8,
    /// The target, taken by whoever moves the state out of `ARMED`.
    target: Mutex<Option<Arc<dyn TimerTarget>>>,
    /// The runtime's count of armed entries.
    live: Arc<AtomicUsize>,
}

impl TimerEntry {
    /// Moves an armed entry to `to`. The one winner decrements the live
    /// count and gets the target; everyone else gets `None`.
    fn settle(&self, to: u8) -> Option<Arc<dyn TimerTarget>> {
        self.state
            .compare_exchange(state::ARMED, to, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.target.lock().take()
    }

    fn is_armed(&self) -> bool {
        self.state.load(Ordering::SeqCst) == state::ARMED
    }
}

/// An armed timer. Dropping the handle does not cancel it.
#[derive(Clone)]
pub(crate) struct TimerHandle(Arc<TimerEntry>);

impl fmt::Debug for TimerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TimerHandle")
            .field(&self.0.state.load(Ordering::Relaxed))
            .finish()
    }
}

impl TimerHandle {
    /// Armed -> Cancelled; false if it already fired or was cancelled. The
    /// winning call drops the entry's reference to its target here.
    pub(crate) fn cancel(&self) -> bool {
        self.0.settle(state::CANCELLED).is_some()
    }

    /// Whether the timer fired (for tests).
    #[cfg(test)]
    pub(crate) fn fired(&self) -> bool {
        self.0.state.load(Ordering::SeqCst) == state::FIRED
    }
}

/// A heap slot: the entry with its deadline and arming number.
struct Slot {
    deadline: Instant,
    seq: u64,
    entry: Arc<TimerEntry>,
}

impl Slot {
    /// The heap is a max-heap: the earliest `(deadline, seq)` is greatest.
    fn key(&self) -> (Instant, u64) {
        (self.deadline, self.seq)
    }
}

impl PartialEq for Slot {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Slot {}

impl PartialOrd for Slot {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for Slot {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other.key().cmp(&self.key())
    }
}

/// The heap itself, pure logic under the runtime's heap lock (the
/// `miri_timer_*` tests run it alone).
#[derive(Default)]
struct TimerHeap {
    slots: BinaryHeap<Slot>,
    /// The next arming number.
    next_seq: u64,
}

impl TimerHeap {
    /// Adds an armed entry.
    fn push(&mut self, deadline: Instant, entry: Arc<TimerEntry>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.slots.push(Slot {
            deadline,
            seq,
            entry,
        });
    }

    /// The deadline at the top, armed or dead.
    fn earliest(&self) -> Option<Instant> {
        self.slots.peek().map(|slot| slot.deadline)
    }

    /// Pops every slot due at `now` in `(deadline, seq)` order, appending
    /// the armed ones to `due` and dropping the dead ones (whose targets
    /// their winners already took).
    fn pop_due(&mut self, now: Instant, due: &mut Vec<Arc<TimerEntry>>) {
        while self.slots.peek().is_some_and(|slot| slot.deadline <= now) {
            if let Some(slot) = self.slots.pop()
                && slot.entry.is_armed()
            {
                due.push(slot.entry);
            }
        }
    }

    /// Drops the dead slots when they are more than half the heap. Returns
    /// whether it rebuilt.
    fn rebuild_if_mostly_dead(&mut self, live: usize) -> bool {
        if self.slots.len() <= live.saturating_mul(2) {
            return false;
        }
        self.slots.retain(|slot| slot.entry.is_armed());
        true
    }

    /// Takes every slot out.
    fn drain(&mut self) -> Vec<Arc<TimerEntry>> {
        self.slots.drain().map(|slot| slot.entry).collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len()
    }
}

/// No timer in the heap, in [`Timers::earliest`].
pub(super) const NO_TIMER: u64 = u64::MAX;

/// A runtime's timers.
pub(super) struct Timers {
    /// The heap; its lock is a leaf (module documentation).
    heap: Mutex<TimerHeap>,
    /// Armed entries: neither cancelled nor fired.
    live: Arc<AtomicUsize>,
    /// The deadline at the top of the heap in nanoseconds since `epoch`,
    /// or [`NO_TIMER`]; written under the heap's lock.
    earliest: AtomicU64,
    /// The runtime's epoch for the nanosecond words.
    epoch: Instant,
}

impl fmt::Debug for Timers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timers")
            .field("live", &self.live.load(Ordering::Relaxed))
            .field("earliest", &self.earliest.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Timers {
    /// No timers; nanosecond words count from `epoch`.
    pub(super) fn new(epoch: Instant) -> Self {
        Self {
            heap: Mutex::new(TimerHeap::default()),
            live: Arc::new(AtomicUsize::new(0)),
            earliest: AtomicU64::new(NO_TIMER),
            epoch,
        }
    }

    /// `at` in nanoseconds since the epoch: 0 before it, and below
    /// [`NO_TIMER`] however late.
    pub(super) fn nanos(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
            .unwrap_or(NO_TIMER)
            .min(NO_TIMER.saturating_sub(1))
    }

    /// The instant `nanos` after the epoch, or `None` past `Instant`'s
    /// range.
    pub(super) fn instant(&self, nanos: u64) -> Option<Instant> {
        self.epoch.checked_add(Duration::from_nanos(nanos))
    }

    /// The earliest deadline in the heap, armed or dead, in nanoseconds
    /// since the epoch, or [`NO_TIMER`]. Lock-free.
    pub(super) fn earliest(&self) -> u64 {
        self.earliest.load(Ordering::SeqCst)
    }

    /// Whether some deadline in the heap has passed at `now`. Lock-free.
    pub(super) fn is_due(&self, now: Instant) -> bool {
        let earliest = self.earliest();
        earliest != NO_TIMER && self.nanos(now) >= earliest
    }

    /// Stores the heap's earliest deadline; called under its lock.
    fn publish(&self, heap: &TimerHeap) {
        let earliest = heap.earliest().map_or(NO_TIMER, |at| self.nanos(at));
        self.earliest.store(earliest, Ordering::SeqCst);
    }

    /// A handle that never fires: the timer is born cancelled.
    fn never(&self) -> TimerHandle {
        TimerHandle(Arc::new(TimerEntry {
            state: AtomicU8::new(state::CANCELLED),
            target: Mutex::new(None),
            live: Arc::clone(&self.live),
        }))
    }

    /// Arms a timer at `at` unless `stopping` says the runtime stops (read
    /// under the heap's lock, so a timer is either armed before
    /// [`Timers::clear`] takes the heap or never). Returns the handle and,
    /// when armed, the deadline in nanoseconds since the epoch.
    pub(super) fn arm(
        &self,
        at: Instant,
        target: Arc<dyn TimerTarget>,
        stopping: impl FnOnce() -> bool,
    ) -> (TimerHandle, Option<u64>) {
        let entry = Arc::new(TimerEntry {
            state: AtomicU8::new(state::ARMED),
            target: Mutex::new(Some(target)),
            live: Arc::clone(&self.live),
        });
        let mut heap = self.heap.lock();
        if stopping() {
            drop(heap);
            // Never armed: the target goes now, with no lock held.
            drop(entry);
            return (self.never(), None);
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        heap.push(at, Arc::clone(&entry));
        heap.rebuild_if_mostly_dead(self.live.load(Ordering::SeqCst));
        self.publish(&heap);
        drop(heap);
        (TimerHandle(entry), Some(self.nanos(at)))
    }

    /// Armed timers.
    pub(super) fn pending(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Fires every timer due at `now`, or at `Instant::now()` if that is
    /// earlier, so that no timer fires before its deadline (D-671). The
    /// due entries are popped under the heap's lock and fired after it is
    /// released, with no lock held. Returns how many fired.
    pub(super) fn fire_due(&self, now: Instant) -> usize {
        let now = now.min(Instant::now());
        if !self.is_due(now) {
            return 0;
        }
        let mut due = Vec::new();
        {
            let mut heap = self.heap.lock();
            heap.pop_due(now, &mut due);
            heap.rebuild_if_mostly_dead(self.live.load(Ordering::SeqCst));
            self.publish(&heap);
        }
        let mut fired = 0_usize;
        for entry in due {
            if let Some(target) = entry.settle(state::FIRED) {
                target.fire();
                fired = fired.saturating_add(1);
            }
        }
        fired
    }

    /// Drops every entry unfired; their targets are dropped after the
    /// heap's lock is released.
    pub(super) fn clear(&self) {
        let entries = {
            let mut heap = self.heap.lock();
            let entries = heap.drain();
            self.publish(&heap);
            entries
        };
        for entry in entries {
            drop(entry.settle(state::CANCELLED));
        }
    }

    /// The heap's length, dead entries included (for tests).
    #[cfg(test)]
    pub(super) fn heap_len(&self) -> usize {
        self.heap.lock().len()
    }
}

impl Shared {
    /// Arms a timer that fires `target` at `at`; after `stopping` it
    /// returns a handle that never fires (C7). A deadline earlier than the
    /// poll holder's wakes the holder, which recomputes its timeout.
    pub(crate) fn arm_timer(&self, at: Instant, target: Arc<dyn TimerTarget>) -> TimerHandle {
        let (handle, armed) = self.timers.arm(at, target, || self.stopping());
        if let Some(nanos) = armed {
            self.driver.timer_armed(nanos);
        }
        handle
    }

    /// Live timers (armed, neither cancelled nor fired), for D-724's
    /// detector (H1). Never the heap's length, which counts dead entries.
    pub(crate) fn timers_pending(&self) -> usize {
        self.timers.pending()
    }

    /// Drops every entry unfired; `Runtime::stop` calls it after joining
    /// every thread (C7).
    pub(crate) fn clear_timers(&self) {
        self.timers.clear();
    }

    /// Fires every timer due at `now`, after releasing the heap lock
    /// (GAP(monitor-fires-timers)). The monitor calls it on every tick, the
    /// poll holder after each poll, and a worker before it searches; firing
    /// never runs Cheby code. A fiber a target wakes goes to the global
    /// queue (none of these threads runs a fiber), which makes
    /// `fibers_waiting()` true, so the monitor's next tick preempts a lone
    /// CPU-bound fiber (D-737) and `timeout` cancels CPU-bound work at every
    /// thread count (§10.8). Returns how many fired.
    pub(crate) fn fire_due(&self, now: Instant) -> usize {
        // C-4: once a halt has started no timer fires, at any of the three
        // sites; the halt joins every thread that fires, and the entries of
        // frozen fibers stay in the heap, never dereferenced (D-708).
        if self.halting() {
            return 0;
        }
        self.timers.fire_due(now)
    }

    /// [`Shared::fire_due`] now, for a worker's search: with no timer in
    /// the heap it reads one atomic and not the clock. Returns whether any
    /// fired.
    pub(super) fn fire_if_due(&self) -> bool {
        self.timers.earliest() != NO_TIMER && self.fire_due(Instant::now()) > 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A target that counts its firings.
    #[derive(Default)]
    struct Probe {
        fired: AtomicUsize,
    }

    impl TimerTarget for Probe {
        fn fire(&self) {
            self.fired.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn tagged(tag: u32, order: &Arc<Mutex<Vec<u32>>>) -> Arc<Ordered> {
        Arc::new(Ordered {
            tag,
            order: Arc::clone(order),
        })
    }

    /// A target that appends its tag to a shared list.
    struct Ordered {
        tag: u32,
        order: Arc<Mutex<Vec<u32>>>,
    }

    impl TimerTarget for Ordered {
        fn fire(&self) {
            self.order.lock().push(self.tag);
        }
    }

    fn timers() -> Timers {
        Timers::new(Instant::now())
    }

    fn arm(timers: &Timers, at: Instant, target: Arc<dyn TimerTarget>) -> TimerHandle {
        timers.arm(at, target, || false).0
    }

    fn armed_entry(live: &Arc<AtomicUsize>) -> Arc<TimerEntry> {
        live.fetch_add(1, Ordering::SeqCst);
        Arc::new(TimerEntry {
            state: AtomicU8::new(state::ARMED),
            target: Mutex::new(Some(Arc::new(Probe::default()))),
            live: Arc::clone(live),
        })
    }

    #[test]
    fn timer_spans_of_zero_or_less_have_elapsed() {
        let now = Instant::now();
        for (seconds, nanoseconds) in [(0, 0), (0, -1), (-1, 0), (-1, -999_999_999), (i64::MIN, 0)]
        {
            assert_eq!(
                deadline_from(now, seconds, nanoseconds),
                Deadline::Elapsed,
                "({seconds}, {nanoseconds})"
            );
        }
        // A mixed pair is the sum of its parts.
        assert_eq!(deadline_from(now, 1, -2_000_000_000), Deadline::Elapsed);
        assert_eq!(
            deadline_from(now, 1, -500_000_000),
            Deadline::At(now + Duration::from_millis(500))
        );
    }

    #[test]
    fn timer_positive_spans_end_after_now() {
        let now = Instant::now();
        assert_eq!(
            deadline_from(now, 0, 1),
            Deadline::At(now + Duration::from_nanos(1))
        );
        assert_eq!(
            deadline_from(now, 2, 500_000_000),
            Deadline::At(now + Duration::from_millis(2500))
        );
        // Nanoseconds past a second carry into the seconds.
        assert_eq!(
            deadline_from(now, 0, 1_500_000_000),
            Deadline::At(now + Duration::from_millis(1500))
        );
        assert!(matches!(deadline_after(0, 10_000_000), Deadline::At(at) if at > now));
    }

    #[test]
    fn timer_spans_beyond_instant_never_end_and_arm_nothing() {
        let now = Instant::now();
        assert_eq!(deadline_from(now, i64::MAX, 999_999_999), Deadline::Never);
        assert_eq!(deadline_from(now, i64::MAX, i64::MAX), Deadline::Never);
        assert_eq!(deadline_after(i64::MAX, 0), Deadline::Never);
        // Nothing to arm: the caller arms only `At`.
        let timers = timers();
        assert_eq!(timers.pending(), 0);
        assert_eq!(timers.earliest(), NO_TIMER);
    }

    #[test]
    fn timer_equal_deadlines_fire_in_arming_order() {
        let timers = timers();
        let order = Arc::new(Mutex::new(Vec::new()));
        let at = Instant::now();
        let later = at + Duration::from_millis(1);
        arm(&timers, later, tagged(9, &order));
        for tag in 0..5 {
            arm(&timers, at, tagged(tag, &order));
        }
        while Instant::now() < later {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(timers.fire_due(later), 6);
        assert_eq!(*order.lock(), vec![0, 1, 2, 3, 4, 9]);
        assert_eq!(timers.pending(), 0);
        assert_eq!(timers.earliest(), NO_TIMER);
    }

    #[test]
    fn timer_never_fires_before_its_deadline() {
        let timers = timers();
        let probe = Arc::new(Probe::default());
        let at = Instant::now() + Duration::from_secs(3600);
        let handle = arm(&timers, at, probe.clone());
        // A `now` from the future is capped at the real clock.
        assert_eq!(timers.fire_due(at + Duration::from_secs(1)), 0);
        assert_eq!(probe.fired.load(Ordering::SeqCst), 0);
        assert!(!handle.fired());
        assert_eq!(timers.pending(), 1);
        assert!(!timers.is_due(Instant::now()));
    }

    #[test]
    fn timer_cancel_and_fire_race_with_one_winner() {
        let timers = timers();
        let probe = Arc::new(Probe::default());
        let at = Instant::now();
        let handle = arm(&timers, at, probe.clone());
        assert_eq!(Arc::strong_count(&probe), 2);
        assert!(handle.cancel());
        // The winning cancel dropped the target and the live count.
        assert_eq!(Arc::strong_count(&probe), 1);
        assert_eq!(timers.pending(), 0);
        assert!(!handle.cancel());
        assert_eq!(timers.fire_due(at), 0);
        assert_eq!(probe.fired.load(Ordering::SeqCst), 0);
        // A fired timer cannot be cancelled.
        let fired = Arc::new(Probe::default());
        let handle = arm(&timers, at, fired.clone());
        assert_eq!(timers.fire_due(at), 1);
        assert!(handle.fired());
        assert!(!handle.cancel());
        assert_eq!(Arc::strong_count(&fired), 1);
    }

    #[test]
    fn timer_cancel_races_firing_from_threads() {
        for _ in 0..200 {
            let timers = Arc::new(timers());
            let probe = Arc::new(Probe::default());
            let at = Instant::now();
            let handle = arm(&timers, at, probe.clone());
            let firing = {
                let timers = Arc::clone(&timers);
                std::thread::spawn(move || timers.fire_due(at))
            };
            let cancelled = handle.cancel();
            let fired = firing.join().unwrap();
            assert_eq!(usize::from(cancelled) + fired, 1);
            assert_eq!(probe.fired.load(Ordering::SeqCst), fired);
            assert_eq!(timers.pending(), 0);
            assert_eq!(Arc::strong_count(&probe), 1);
        }
    }

    #[test]
    fn timer_live_count_is_not_the_heap_length() {
        let timers = timers();
        let at = Instant::now() + Duration::from_secs(3600);
        let handles: Vec<_> = (0..4)
            .map(|_| arm(&timers, at, Arc::new(Probe::default())))
            .collect();
        assert!(handles[0].cancel());
        // The cancelled entry stays in the heap until popped or rebuilt.
        assert_eq!(timers.pending(), 3);
        assert_eq!(timers.heap_len(), 4);
    }

    #[test]
    fn timer_heap_is_rebuilt_when_over_half_is_dead() {
        let timers = timers();
        let at = Instant::now() + Duration::from_secs(3600);
        let handles: Vec<_> = (0..10)
            .map(|_| arm(&timers, at, Arc::new(Probe::default())))
            .collect();
        for handle in &handles[..6] {
            assert!(handle.cancel());
        }
        // 6 of 10 dead: the next arm rebuilds, leaving 4 live and the new one.
        arm(&timers, at, Arc::new(Probe::default()));
        assert_eq!(timers.heap_len(), 5);
        assert_eq!(timers.pending(), 5);
        // Exactly half dead is not over half.
        for handle in &handles[6..8] {
            assert!(handle.cancel());
        }
        arm(&timers, at, Arc::new(Probe::default()));
        assert_eq!(timers.heap_len(), 6);
        assert_eq!(timers.pending(), 4);
    }

    #[test]
    fn timer_earliest_deadline_is_published_lock_free() {
        let timers = timers();
        let now = Instant::now();
        assert_eq!(timers.earliest(), NO_TIMER);
        assert!(!timers.is_due(now));
        let late = now + Duration::from_secs(10);
        let soon = now + Duration::from_millis(5);
        arm(&timers, late, Arc::new(Probe::default()));
        assert_eq!(timers.earliest(), timers.nanos(late));
        let handle = arm(&timers, soon, Arc::new(Probe::default()));
        assert_eq!(timers.earliest(), timers.nanos(soon));
        assert!(timers.is_due(soon));
        assert!(!timers.is_due(now));
        // A cancelled top stays until popped: the check is conservative.
        assert!(handle.cancel());
        assert_eq!(timers.earliest(), timers.nanos(soon));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(timers.fire_due(Instant::now()), 0);
        assert_eq!(timers.earliest(), timers.nanos(late));
    }

    #[test]
    fn timer_targets_fire_after_the_heap_lock_is_released() {
        /// Arms another timer from inside `fire`, which deadlocks if the
        /// heap's lock is held while targets fire.
        struct Rearm {
            timers: Arc<Timers>,
            probe: Arc<Probe>,
        }
        impl TimerTarget for Rearm {
            fn fire(&self) {
                assert!(!self.timers.heap.is_locked());
                arm(&self.timers, Instant::now(), self.probe.clone());
            }
        }
        let timers = Arc::new(timers());
        let probe = Arc::new(Probe::default());
        let at = Instant::now();
        arm(
            &timers,
            at,
            Arc::new(Rearm {
                timers: Arc::clone(&timers),
                probe: probe.clone(),
            }),
        );
        assert_eq!(timers.fire_due(at), 1);
        assert_eq!(timers.fire_due(Instant::now()), 1);
        assert_eq!(probe.fired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timer_clear_drops_every_entry_unfired() {
        let timers = timers();
        let probe = Arc::new(Probe::default());
        let at = Instant::now();
        let handles: Vec<_> = (0..3).map(|_| arm(&timers, at, probe.clone())).collect();
        timers.clear();
        assert_eq!(timers.pending(), 0);
        assert_eq!(timers.heap_len(), 0);
        assert_eq!(timers.earliest(), NO_TIMER);
        assert_eq!(Arc::strong_count(&probe), 1);
        assert_eq!(timers.fire_due(at), 0);
        assert!(
            handles
                .iter()
                .all(|handle| !handle.fired() && !handle.cancel())
        );
    }

    #[test]
    fn timer_a_stopping_runtime_arms_nothing() {
        let timers = timers();
        let probe = Arc::new(Probe::default());
        let at = Instant::now();
        let (handle, armed) = timers.arm(at, probe.clone(), || true);
        assert_eq!(armed, None);
        assert_eq!(Arc::strong_count(&probe), 1);
        assert_eq!(timers.pending(), 0);
        assert_eq!(timers.fire_due(at), 0);
        assert!(!handle.fired());
        assert!(!handle.cancel());
    }

    // The heap's logic alone: no threads, no clock but `Instant`'s
    // arithmetic, so Miri runs them (architecture §10.4).

    #[test]
    fn miri_timer_heap_pops_due_slots_in_deadline_then_seq_order() {
        let live = Arc::new(AtomicUsize::new(0));
        let mut heap = TimerHeap::default();
        let base = Instant::now();
        let entries: Vec<_> = (0..6).map(|_| armed_entry(&live)).collect();
        let deadlines = [3, 1, 2, 1, 5, 2];
        for (entry, ms) in entries.iter().zip(deadlines) {
            heap.push(base + Duration::from_millis(ms), Arc::clone(entry));
        }
        let mut due = Vec::new();
        heap.pop_due(base + Duration::from_millis(2), &mut due);
        let popped: Vec<usize> = due
            .iter()
            .map(|entry| entries.iter().position(|e| Arc::ptr_eq(e, entry)).unwrap())
            .collect();
        assert_eq!(popped, vec![1, 3, 2, 5]);
        assert_eq!(heap.earliest(), Some(base + Duration::from_millis(3)));
        assert_eq!(heap.len(), 2);
    }

    #[test]
    fn miri_timer_heap_drops_dead_slots_when_popped_or_rebuilt() {
        let live = Arc::new(AtomicUsize::new(0));
        let mut heap = TimerHeap::default();
        let base = Instant::now();
        let entries: Vec<_> = (0..4).map(|_| armed_entry(&live)).collect();
        for entry in &entries {
            heap.push(base, Arc::clone(entry));
        }
        assert!(entries[0].settle(state::CANCELLED).is_some());
        assert!(entries[0].settle(state::FIRED).is_none());
        assert_eq!(live.load(Ordering::SeqCst), 3);
        assert!(!heap.rebuild_if_mostly_dead(3));
        assert!(entries[1].settle(state::CANCELLED).is_some());
        assert!(entries[2].settle(state::CANCELLED).is_some());
        assert!(heap.rebuild_if_mostly_dead(1));
        assert_eq!(heap.len(), 1);
        let mut due = Vec::new();
        heap.pop_due(base, &mut due);
        assert_eq!(due.len(), 1);
        assert!(Arc::ptr_eq(&due[0], &entries[3]));
        assert_eq!(heap.earliest(), None);
    }

    #[test]
    fn miri_timer_heap_seq_breaks_ties_after_a_rebuild() {
        let live = Arc::new(AtomicUsize::new(0));
        let mut heap = TimerHeap::default();
        let at = Instant::now();
        let entries: Vec<_> = (0..5).map(|_| armed_entry(&live)).collect();
        for entry in &entries {
            heap.push(at, Arc::clone(entry));
        }
        for entry in &entries[..3] {
            assert!(entry.settle(state::CANCELLED).is_some());
        }
        assert!(heap.rebuild_if_mostly_dead(2));
        let mut due = Vec::new();
        heap.pop_due(at, &mut due);
        assert!(Arc::ptr_eq(&due[0], &entries[3]));
        assert!(Arc::ptr_eq(&due[1], &entries[4]));
    }
}
