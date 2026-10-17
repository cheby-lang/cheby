//! Idle workers: Tokio's protocol (D-689).
//!
//! One word packs the number of workers that are searching for work (the
//! low 16 bits) and the number that are not parked (the rest). A worker
//! becomes searching only while fewer than half the workers search, and
//! parks only after its own queues, the global queue and every other
//! worker's deque came up empty.
//!
//! - **Notify.** After work is pushed, [`Idle::worker_to_notify`] picks one
//!   parked worker to wake, but only when nobody searches: a searcher will
//!   find the work. The woken worker starts out searching.
//! - **The last searcher.** A worker that stops searching or parks while it
//!   was the last searcher re-checks every queue and notifies if it finds
//!   work ([`Idle::transition_worker_to_parked`] and
//!   [`Idle::transition_worker_from_searching`] say whether it was the last
//!   one). That closes the race between a push that saw a searcher and the
//!   searcher giving up.
//! - **Targeted wakeups.** Work that only one worker may run (a pinned
//!   fiber, D-687) wakes that worker with [`Idle::unpark_specific`]; a
//!   worker that finds such work after it registered as parked takes itself
//!   back with [`Idle::unpark_self`].
//!
//! - **Live workers** (D-689, D-795). The number of
//!   workers follows `set_thread_count`: a started worker joins as unparked
//!   ([`Idle::add_worker`]), and a retiring one leaves once it no longer
//!   searches ([`Idle::remove_worker`]), so the half-the-workers rule and
//!   the notify check count only the workers that take fibers.
//!
//! Every operation on the word is sequentially consistent, and the list of
//! parked workers and the worker count are only changed under its lock
//! together with the word, so the count of unparked workers is exact under
//! that lock.

use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

/// The bits of the searching count.
const SEARCH_MASK: usize = (1 << UNPARK_SHIFT) - 1;
/// Where the unparked count starts.
const UNPARK_SHIFT: u32 = 16;
/// One unparked worker.
const ONE_UNPARKED: usize = 1 << UNPARK_SHIFT;

/// What [`Idle::transition_worker_to_parked`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parked {
    /// The worker was the last searcher: it re-checks every queue.
    pub(crate) last_searcher: bool,
    /// The worker was the last unparked one (D-860's trigger).
    pub(crate) last_unparked: bool,
}

/// The idle state of a runtime's workers.
#[derive(Debug)]
pub(crate) struct Idle {
    /// Searching and unparked counts.
    state: AtomicUsize,
    /// The parked workers, by index.
    sleepers: Mutex<Vec<usize>>,
    /// The number of live workers, at most 1024; changed under the
    /// sleepers' lock.
    workers: AtomicUsize,
}

/// The searching count of a state word.
const fn searching(state: usize) -> usize {
    state & SEARCH_MASK
}

/// The unparked count of a state word.
const fn unparked(state: usize) -> usize {
    state >> UNPARK_SHIFT
}

impl Idle {
    /// The state of `workers` workers, all unparked and none searching.
    pub(crate) fn new(workers: usize) -> Self {
        Self {
            state: AtomicUsize::new(workers << UNPARK_SHIFT),
            sleepers: Mutex::new(Vec::with_capacity(workers)),
            workers: AtomicUsize::new(workers),
        }
    }

    /// The number of live workers.
    pub(crate) fn workers(&self) -> usize {
        self.workers.load(Ordering::SeqCst)
    }

    /// Counts a started worker (or one that rejoins): live, unparked and
    /// not searching.
    pub(crate) fn add_worker(&self) {
        let sleepers = self.sleepers.lock();
        self.workers.fetch_add(1, Ordering::SeqCst);
        self.state.fetch_add(ONE_UNPARKED, Ordering::SeqCst);
        drop(sleepers);
    }

    /// Stops counting worker `worker`, which retires: it no longer
    /// searches (the caller ended its search first), and it is taken out
    /// of the parked list if it is still there.
    pub(crate) fn remove_worker(&self, worker: usize) {
        let mut sleepers = self.sleepers.lock();
        if let Some(at) = sleepers.iter().position(|&w| w == worker) {
            sleepers.swap_remove(at);
        } else {
            self.state.fetch_sub(ONE_UNPARKED, Ordering::SeqCst);
        }
        self.workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// A parked worker, if any, left in the list: the one to wake so that
    /// it takes the poller over from a worker that retires.
    pub(crate) fn any_parked(&self) -> Option<usize> {
        self.sleepers.lock().last().copied()
    }

    /// Whether a notification should wake a worker: nobody searches and
    /// some worker is parked.
    fn notify_should_wake(&self) -> bool {
        let state = self.state.load(Ordering::SeqCst);
        searching(state) == 0 && unparked(state) < self.workers()
    }

    /// The parked worker to wake after work was pushed, now counted as
    /// unparked and searching, or `None` when a searcher will find the
    /// work or every worker is awake.
    pub(crate) fn worker_to_notify(&self) -> Option<usize> {
        if !self.notify_should_wake() {
            return None;
        }
        let mut sleepers = self.sleepers.lock();
        if !self.notify_should_wake() {
            return None;
        }
        let worker = sleepers.pop()?;
        self.state.fetch_add(ONE_UNPARKED | 1, Ordering::SeqCst);
        Some(worker)
    }

    /// Registers `worker` as parked. Returns whether it was the last
    /// searcher, which must then re-check every queue, and whether it was
    /// the last unparked worker, exact under the sleepers' lock: the
    /// deadlock detector's trigger (D-860), never a proof (a worker blocked
    /// inline in a system call, or one that is leaving, is not parked).
    pub(crate) fn transition_worker_to_parked(&self, worker: usize, is_searching: bool) -> Parked {
        let mut sleepers = self.sleepers.lock();
        let decrement = ONE_UNPARKED | usize::from(is_searching);
        let previous = self.state.fetch_sub(decrement, Ordering::SeqCst);
        sleepers.push(worker);
        Parked {
            last_searcher: is_searching && searching(previous) == 1,
            last_unparked: unparked(previous) == 1,
        }
    }

    /// Makes a worker searching, unless half the workers already search.
    pub(crate) fn transition_worker_to_searching(&self) -> bool {
        let state = self.state.load(Ordering::SeqCst);
        if searching(state).saturating_mul(2) >= self.workers() {
            return false;
        }
        self.state.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Ends a worker's search. Returns whether it was the last searcher,
    /// which must then notify another worker if work is left.
    pub(crate) fn transition_worker_from_searching(&self) -> bool {
        let previous = self.state.fetch_sub(1, Ordering::SeqCst);
        searching(previous) == 1
    }

    /// Takes `worker` out of the parked list on its own behalf, counting it
    /// unparked but not searching. False if someone else already woke it,
    /// which counted it searching.
    pub(crate) fn unpark_self(&self, worker: usize) -> bool {
        self.remove(worker, ONE_UNPARKED)
    }

    /// Takes `worker` out of the parked list for work only it may run,
    /// counting it unparked and searching. False if it was not parked.
    pub(crate) fn unpark_specific(&self, worker: usize) -> bool {
        self.remove(worker, ONE_UNPARKED | 1)
    }

    /// Whether `worker` is still registered as parked.
    pub(crate) fn is_parked(&self, worker: usize) -> bool {
        self.sleepers.lock().contains(&worker)
    }

    fn remove(&self, worker: usize, increment: usize) -> bool {
        let mut sleepers = self.sleepers.lock();
        let Some(at) = sleepers.iter().position(|&w| w == worker) else {
            return false;
        };
        sleepers.swap_remove(at);
        self.state.fetch_add(increment, Ordering::SeqCst);
        true
    }

    /// The number of searching workers.
    pub(crate) fn num_searching(&self) -> usize {
        searching(self.state.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_wakes_a_parked_worker_only_when_nobody_searches() {
        let idle = Idle::new(2);
        assert_eq!(idle.worker_to_notify(), None);
        assert_eq!(
            idle.transition_worker_to_parked(0, false),
            Parked {
                last_searcher: false,
                last_unparked: false,
            }
        );
        assert!(idle.transition_worker_to_searching());
        // Worker 1 searches: it will find the work.
        assert_eq!(idle.worker_to_notify(), None);
        // It was the last searcher, and the last unparked worker (D-860's
        // trigger), when it parks.
        assert_eq!(
            idle.transition_worker_to_parked(1, true),
            Parked {
                last_searcher: true,
                last_unparked: true,
            }
        );
        assert!(idle.is_parked(1));
        let woken = idle.worker_to_notify().unwrap();
        assert!(!idle.is_parked(woken));
        assert_eq!(idle.num_searching(), 1);
        // A second notify finds a searcher and wakes nobody.
        assert_eq!(idle.worker_to_notify(), None);
        assert!(idle.transition_worker_from_searching());
        let other = usize::from(woken == 0);
        assert!(idle.unpark_specific(other));
        assert!(!idle.unpark_self(other));
        assert_eq!(idle.num_searching(), 1);
    }

    #[test]
    fn at_most_half_the_workers_search() {
        let idle = Idle::new(4);
        assert!(idle.transition_worker_to_searching());
        assert!(idle.transition_worker_to_searching());
        assert!(!idle.transition_worker_to_searching());
        assert!(!idle.transition_worker_from_searching());
        assert!(idle.transition_worker_from_searching());
        // One worker may always search.
        let one = Idle::new(1);
        assert!(one.transition_worker_to_searching());
    }

    /// D-795: started and retired workers change the
    /// live count, so the notify check and the half rule follow them.
    #[test]
    fn thread_count_idle_follows_started_and_retired_workers() {
        let idle = Idle::new(0);
        idle.add_worker();
        assert_eq!(idle.workers(), 1);
        // Every worker is awake: nobody to notify.
        assert_eq!(idle.worker_to_notify(), None);
        idle.add_worker();
        assert!(!idle.transition_worker_to_parked(1, false).last_searcher);
        assert_eq!(idle.any_parked(), Some(1));
        // Worker 1 retires while parked; worker 0 while awake.
        idle.remove_worker(1);
        assert_eq!(idle.any_parked(), None);
        assert_eq!(idle.workers(), 1);
        assert_eq!(idle.worker_to_notify(), None);
        idle.remove_worker(0);
        assert_eq!(idle.workers(), 0);
        assert_eq!(unparked(idle.state.load(Ordering::SeqCst)), 0);
        // A restarted worker counts again; with four, two may search.
        for _ in 0..4 {
            idle.add_worker();
        }
        assert!(idle.transition_worker_to_searching());
        assert!(idle.transition_worker_to_searching());
        assert!(!idle.transition_worker_to_searching());
    }
}
