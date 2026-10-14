//! `loom` models of a worker's parker and of the poll holder's deadline
//! (D-680, ADR-0059; survey-t1 H3): a wake is never lost, whether the
//! worker blocks in the poller or on its condition variable, and a timer
//! armed earlier than the holder's deadline always reaches it.
//!
//! Each model is the protocol, not the runtime's code: `loom` atomics, a
//! mutex and a condition variable stand for the parker's state word and
//! lock, the poller's lock (taken with `try_lock`), and the poller itself,
//! which waits until woken (a wake made before the poll counts, as
//! `mio::Waker`'s does). A poll's timeout is left out: a model's poll
//! waits until woken, and one that would still wait once the waking thread
//! is done panics with "lost wakeup". Model (4)'s handshake has a
//! sequentially consistent fence between each side's store and load, as
//! the runtime has (loom checks `SeqCst` fences, not `SeqCst` accesses).
//! loom 0.7 needs no `--cfg loom`, so the models build with plain
//! `cargo test`:
//!
//! ```text
//! cargo test -p cheby_runtime sched::park_models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step                       | Mirrors (path under `crates/cheby_runtime/src/`, function)        |
//! | -------------------------------- | ----------------------------------------------------------------- |
//! | `Parker::park`                   | `sched/park.rs` `Parker::park`                                    |
//! | `Parker::park_driver`            | `sched/park.rs` `Parker::park_driver`                             |
//! | `Parker::park_condvar`           | `sched/park.rs` `Parker::park_condvar`                            |
//! | `Parker::unpark`                 | `sched/park.rs` `Parker::unpark`, from `sched/worker.rs` `unpark` |
//! | `Poll::poll`, `Poll::wake`       | `sched/poll.rs` `Poller::poll`, `Driver::wake` (`mio`)            |
//! | the worker's loop                | `sched/worker.rs` `Worker::park` (re-checks after each park)      |
//! | model 4: the holder              | `sched/poll.rs` `Driver::poll` (store the deadline, read again)   |
//! | model 4: the arming thread       | `sched/timer.rs` `Shared::arm_timer` → `Driver::timer_armed`      |
//!
//! Model (1) has a `#[should_panic]` twin whose parker stores its parked
//! state without the compare-and-swap that finds `NOTIFIED`, and model (4)
//! one whose holder reads the earliest deadline only before it publishes
//! its own: both lose a wake, which shows that each model can fail.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

/// The parker's states, as `sched/park.rs` numbers them.
mod state {
    pub(super) const EMPTY: u8 = 0;
    pub(super) const PARKED_CONDVAR: u8 = 1;
    pub(super) const PARKED_DRIVER: u8 = 2;
    pub(super) const NOTIFIED: u8 = 3;
}

/// How the parker leaves `EMPTY` before it blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// A compare-and-swap that fails on `NOTIFIED` (the runtime's).
    Notified,
    /// A plain store: the twin, which overwrites a notification.
    Skipped,
}

/// The poller: waits until woken; a wake before the poll counts. Once the
/// thread that wakes it is done, a poll with no wake pending would block
/// forever: the model reports that as a lost wakeup instead of blocking,
/// so that a failing model panics cleanly rather than deadlocking.
///
/// Its two flags change only by read-modify-writes, which loom orders as
/// the execution runs: a wake is a system call (an `eventfd` write, a
/// `kevent`), never a plain store that a later poll could miss. (loom
/// treats a `SeqCst` store as a release store, which a poll's swap could
/// order before its own write, a false alarm.)
#[derive(Default)]
struct Poll {
    pending: AtomicBool,
    done: AtomicBool,
}

impl Poll {
    fn poll(&self) {
        loop {
            if self.pending.swap(false, Ordering::SeqCst) {
                return;
            }
            if self.done.fetch_or(false, Ordering::SeqCst) {
                assert!(self.pending.swap(false, Ordering::SeqCst), "lost wakeup");
                return;
            }
            thread::yield_now();
        }
    }

    fn wake(&self) {
        self.pending.swap(true, Ordering::SeqCst);
    }

    /// The waking thread has made every wake it will make.
    fn finish(&self) {
        self.done.swap(true, Ordering::SeqCst);
    }
}

/// The runtime's poller and its lock.
#[derive(Default)]
struct Driver {
    held: Mutex<()>,
    poll: Poll,
}

struct Parker {
    state: AtomicU8,
    lock: Mutex<()>,
    condvar: Condvar,
    check: Check,
}

impl Parker {
    fn new(check: Check) -> Self {
        Self {
            state: AtomicU8::new(state::EMPTY),
            lock: Mutex::new(()),
            condvar: Condvar::new(),
            check,
        }
    }

    /// Leaves `EMPTY` for `parked`; false if notified meanwhile.
    fn enter(&self, parked: u8) -> bool {
        match self.check {
            Check::Notified => match self.state.compare_exchange(
                state::EMPTY,
                parked,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => true,
                Err(state::NOTIFIED) => {
                    self.state.swap(state::EMPTY, Ordering::SeqCst);
                    false
                }
                Err(other) => panic!("a parking worker was in the state {other}"),
            },
            Check::Skipped => {
                self.state.store(parked, Ordering::SeqCst);
                true
            }
        }
    }

    fn park(&self, driver: &Driver) {
        if self
            .state
            .compare_exchange(
                state::NOTIFIED,
                state::EMPTY,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return;
        }
        if let Ok(guard) = driver.held.try_lock() {
            self.park_driver(driver);
            drop(guard);
        } else {
            self.park_condvar();
        }
    }

    fn park_driver(&self, driver: &Driver) {
        if !self.enter(state::PARKED_DRIVER) {
            return;
        }
        driver.poll.poll();
        let left = self.state.swap(state::EMPTY, Ordering::SeqCst);
        assert!(
            left == state::NOTIFIED || left == state::PARKED_DRIVER,
            "a polling worker was in the state {left}"
        );
    }

    fn park_condvar(&self) {
        let mut guard = self.lock.lock().unwrap();
        if !self.enter(state::PARKED_CONDVAR) {
            return;
        }
        loop {
            guard = self.condvar.wait(guard).unwrap();
            if self
                .state
                .compare_exchange(
                    state::NOTIFIED,
                    state::EMPTY,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    fn unpark(&self, driver: &Driver) {
        match self.state.swap(state::NOTIFIED, Ordering::SeqCst) {
            state::EMPTY | state::NOTIFIED => {}
            state::PARKED_CONDVAR => {
                drop(self.lock.lock().unwrap());
                self.condvar.notify_one();
            }
            state::PARKED_DRIVER => driver.poll.wake(),
            other => panic!("a worker's parker was in the state {other}"),
        }
    }
}

/// The worker's loop: parks until it sees its work.
fn worker(parker: &Parker, driver: &Driver, work: &AtomicBool) {
    while !work.load(Ordering::SeqCst) {
        parker.park(driver);
    }
}

/// At most three preemptions per execution, the loom documentation's
/// advice; the models with one worker are exhaustive within it.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(f);
}

/// Model (1): one worker, the poller free: it blocks in the poller, and a
/// notifier that publishes work and unparks it always wakes it.
fn one_worker_in_the_poller(check: Check) {
    explore(move || {
        let driver = Arc::new(Driver::default());
        let parker = Arc::new(Parker::new(check));
        let work = Arc::new(AtomicBool::new(false));
        let notifier = {
            let (driver, parker, work) =
                (Arc::clone(&driver), Arc::clone(&parker), Arc::clone(&work));
            thread::spawn(move || {
                work.store(true, Ordering::SeqCst);
                parker.unpark(&driver);
                driver.poll.finish();
            })
        };
        worker(&parker, &driver, &work);
        notifier.join().unwrap();
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn park_model_1_a_wake_reaches_the_poll_holder() {
    one_worker_in_the_poller(Check::Notified);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "lost wakeup")]
fn park_model_1_twin_without_the_notified_check_loses_a_wake() {
    one_worker_in_the_poller(Check::Skipped);
}

/// Model (2): another thread holds the poller, so the worker blocks on its
/// condition variable; the notifier's wake reaches it.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn park_model_2_a_wake_reaches_a_condvar_parker() {
    explore(|| {
        let driver = Arc::new(Driver::default());
        let parker = Arc::new(Parker::new(Check::Notified));
        let work = Arc::new(AtomicBool::new(false));
        let held = driver.held.lock().unwrap();
        let notifier = {
            let (driver, parker, work) =
                (Arc::clone(&driver), Arc::clone(&parker), Arc::clone(&work));
            thread::spawn(move || {
                work.store(true, Ordering::SeqCst);
                parker.unpark(&driver);
                driver.poll.finish();
            })
        };
        worker(&parker, &driver, &work);
        notifier.join().unwrap();
        drop(held);
    });
}

/// Model (3): two workers share one poller: one blocks in it, the other on
/// its condition variable (which one depends on the interleaving); a
/// notifier wakes both, and both wake.
#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn park_model_3_two_workers_share_one_poller() {
    explore(|| {
        let driver = Arc::new(Driver::default());
        let parkers = Arc::new([Parker::new(Check::Notified), Parker::new(Check::Notified)]);
        let work = Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
        let other = {
            let (driver, parkers, work) =
                (Arc::clone(&driver), Arc::clone(&parkers), Arc::clone(&work));
            thread::spawn(move || worker(&parkers[1], &driver, &work[1]))
        };
        let notifier = {
            let (driver, parkers, work) =
                (Arc::clone(&driver), Arc::clone(&parkers), Arc::clone(&work));
            thread::spawn(move || {
                for (parker, work) in parkers.iter().zip(work.iter()) {
                    work.store(true, Ordering::SeqCst);
                    parker.unpark(&driver);
                }
                driver.poll.finish();
            })
        };
        worker(&parkers[0], &driver, &work[0]);
        other.join().unwrap();
        notifier.join().unwrap();
    });
}

/// How the model's holder publishes its deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Publish {
    /// Store the deadline, then read the earliest deadline again (the
    /// runtime's `Driver::poll`).
    ThenReread,
    /// Read the earliest deadline once, then store: the twin.
    ReadOnce,
}

/// Nobody polls.
const NOT_POLLING: u64 = 0;
/// The timer in the heap when the holder starts.
const LATE: u64 = 100;
/// The timer armed meanwhile.
const EARLY: u64 = 50;

/// Model (4): the holder polls for the earliest deadline while another
/// thread arms an earlier timer. Either the holder sees the earlier one
/// (and its poll "times out" in time, so it does not block), or the arming
/// thread sees the holder's later deadline and wakes it.
fn holder_and_an_earlier_timer(publish: Publish) {
    explore(move || {
        let earliest = Arc::new(AtomicU64::new(LATE));
        let deadline = Arc::new(AtomicU64::new(NOT_POLLING));
        let poll = Arc::new(Poll::default());
        let armer = {
            let (earliest, deadline, poll) = (
                Arc::clone(&earliest),
                Arc::clone(&deadline),
                Arc::clone(&poll),
            );
            thread::spawn(move || {
                // Under the heap's lock in the runtime.
                earliest.store(EARLY, Ordering::SeqCst);
                fence(Ordering::SeqCst);
                if EARLY + 1 < deadline.load(Ordering::SeqCst) {
                    poll.wake();
                }
                poll.finish();
            })
        };
        let mut seen = earliest.load(Ordering::SeqCst);
        match publish {
            Publish::ThenReread => loop {
                deadline.store(seen.saturating_add(1), Ordering::SeqCst);
                fence(Ordering::SeqCst);
                let again = earliest.load(Ordering::SeqCst);
                if again >= seen {
                    break;
                }
                seen = again;
            },
            Publish::ReadOnce => deadline.store(seen.saturating_add(1), Ordering::SeqCst),
        }
        // A limit at the early deadline ends in time without a wake; a later
        // one waits for the wake (the model leaves timeouts out).
        if seen > EARLY {
            poll.poll();
        }
        deadline.store(NOT_POLLING, Ordering::SeqCst);
        armer.join().unwrap();
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn park_model_4_an_earlier_timer_reaches_the_holder() {
    holder_and_an_earlier_timer(Publish::ThenReread);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "lost wakeup")]
fn park_model_4_twin_reading_the_deadline_once_loses_the_timer() {
    holder_and_an_earlier_timer(Publish::ReadOnce);
}
