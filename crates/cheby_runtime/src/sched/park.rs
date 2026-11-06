//! A worker's parker (D-680, D-689, ADR-0059), after Tokio's `park.rs`: an
//! idle worker either blocks in the runtime's poller, if it can take it,
//! or on its own condition variable.
//!
//! The state word moves between four values, only by sequentially
//! consistent operations:
//!
//! ```text
//! EMPTY --park, poller taken--> PARKED_DRIVER --poll returns--> EMPTY
//! EMPTY --park, poller busy---> PARKED_CONDVAR --notified------> EMPTY
//! any   --unpark--------------> NOTIFIED --park---------------> EMPTY (returns at once)
//! ```
//!
//! [`Parker::unpark`] swaps in `NOTIFIED` and acts on what it replaced:
//! `PARKED_CONDVAR` is signalled under the condition variable's lock,
//! `PARKED_DRIVER` is woken through the poller's waker, and `EMPTY` needs
//! nothing, since the worker reads `NOTIFIED` before it blocks (a
//! compare-and-swap from `EMPTY` fails on it). So a wake is never lost
//! between a worker deciding to park and blocking (survey-t1 H3); the
//! `loom` model in `park_models` checks this protocol and its failing twin
//! without the `NOTIFIED` check.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

use super::Shared;
use super::poll::{Driver, Poller};
use crate::panic::fatal;

/// The values of a parker's state word.
pub(super) mod state {
    /// Neither parked nor notified.
    pub(in crate::sched) const EMPTY: u8 = 0;
    /// Blocked on its condition variable.
    pub(in crate::sched) const PARKED_CONDVAR: u8 = 1;
    /// Blocked in the poller.
    pub(in crate::sched) const PARKED_DRIVER: u8 = 2;
    /// Notified: the next park returns at once.
    pub(in crate::sched) const NOTIFIED: u8 = 3;
}

/// One worker's parker.
#[derive(Default)]
pub(super) struct Parker {
    /// One of [`state`]'s values.
    state: AtomicU8,
    /// The condition variable's lock: a leaf, held only to block or signal.
    lock: Mutex<()>,
    condvar: Condvar,
}

impl fmt::Debug for Parker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parker")
            .field("state", &self.state.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Parker {
    /// Blocks the calling worker until [`Parker::unpark`], or less: in the
    /// poller, if no other worker holds it, until a wake or the earliest
    /// timer's deadline, firing the due timers once it has released the
    /// poller; otherwise on the condition variable. Returns at once if
    /// notified since the last park. The caller re-checks why it parked.
    pub(super) fn park(&self, shared: &Shared) {
        if self.take_notified() {
            return;
        }
        if let Some(mut poller) = shared.driver.try_hold() {
            self.park_driver(&mut **poller, &shared.driver, shared);
            drop(poller);
            shared.fire_due(Instant::now());
            // D-1073: signals the poll drained cancel their scopes now,
            // with the poller released, as due timers fire.
            super::signal::run_pending(shared);
        } else {
            self.park_condvar();
        }
    }

    /// Blocks the calling worker until [`Parker::unpark`] on the condition
    /// variable only, never in the poller: a retiring worker that waits for
    /// its pinned fibers (D-687, D-689) leaves the poller to the live
    /// workers. Returns at once if notified since the last park.
    pub(super) fn park_aside(&self) {
        if !self.take_notified() {
            self.park_condvar();
        }
    }

    /// Consumes a notification made since the last park, if any.
    fn take_notified(&self) -> bool {
        self.state
            .compare_exchange(
                state::NOTIFIED,
                state::EMPTY,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Blocks in the poller the caller holds.
    fn park_driver(&self, poller: &mut dyn Poller, driver: &Driver, shared: &Shared) {
        match self.state.compare_exchange(
            state::EMPTY,
            state::PARKED_DRIVER,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(state::NOTIFIED) => {
                self.state.swap(state::EMPTY, Ordering::SeqCst);
                return;
            }
            Err(other) => fatal(format_args!("a parking worker was in the state {other}")),
        }
        driver.poll(poller, &shared.timers);
        match self.state.swap(state::EMPTY, Ordering::SeqCst) {
            state::NOTIFIED | state::PARKED_DRIVER => {}
            other => fatal(format_args!("a polling worker was in the state {other}")),
        }
    }

    /// Blocks on the condition variable until notified.
    fn park_condvar(&self) {
        let mut guard = self.lock.lock();
        match self.state.compare_exchange(
            state::EMPTY,
            state::PARKED_CONDVAR,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(state::NOTIFIED) => {
                self.state.swap(state::EMPTY, Ordering::SeqCst);
                return;
            }
            Err(other) => fatal(format_args!("a parking worker was in the state {other}")),
        }
        loop {
            self.condvar.wait(&mut guard);
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
            // A spurious wakeup: still parked.
        }
    }

    /// Wakes the worker: from the poller through `driver`'s waker, from its
    /// condition variable, or, if it is not blocked, by making its next
    /// park return at once.
    pub(super) fn unpark(&self, driver: &Driver) {
        match self.state.swap(state::NOTIFIED, Ordering::SeqCst) {
            state::EMPTY | state::NOTIFIED => {}
            state::PARKED_CONDVAR => {
                // Taking the lock orders this signal after the worker's
                // wait began: it holds the lock from its state change
                // until it waits.
                drop(self.lock.lock());
                self.condvar.notify_one();
            }
            state::PARKED_DRIVER => driver.wake(),
            other => fatal(format_args!("a worker's parker was in the state {other}")),
        }
    }

    /// The state word (for tests).
    #[cfg(test)]
    pub(super) fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }
}
