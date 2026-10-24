//! Turns: the fiber-level FIFO locks that serialize `std::net`'s
//! operations on one socket (D-1001, D-984).
//!
//! The socket seam (`sched/io.rs`) parks one fiber per direction of a
//! socket, so the kernels serialize the rest themselves: each socket has
//! one turn per direction ([`Turn`]), and a kernel takes its direction's
//! turn ([`Turn::take`]) before it tries its system call and holds it, also
//! while it waits on the seam, until it returns ([`TurnGuard`]). Reads are
//! thereby served one at a time, and so are writes, each in the order its
//! fibers reached the turn; a read and a write hold different turns and
//! proceed at once; a `write_bytes` that needs several system calls and
//! waits between them keeps its turn throughout, so no other write
//! interleaves (D-684, D-1001); and several fibers that `accept` on one
//! listener each wait for the read turn, so each connection goes to the
//! one fiber that holds it when the connection arrives (`net.md`
//! §`accept`).
//!
//! A turn is a FIFO queue of waiters' headers ([`WaitHeader`], on the heap,
//! never on a fiber's stack: D-708) under the turn's leaf lock. The holder's
//! release hands the turn to the first queued waiter whose header it
//! settles `Waiting -> Claimed`, and wakes that fiber after releasing the
//! lock; a header the cancellation walk won first is skipped, as a channel
//! waker skips a cancelled node (D-706). The waiting fiber publishes its
//! header to the walk before it parks, so a cancellation wakes it at once;
//! it then takes its header out of the queue if the release has not
//! popped it, and returns [`TurnError::Cancelled`] without the turn: a
//! cancelled `accept` or read takes nothing (D-670, D-1000).
//!
//! [`Turn::close`] (D-1063) marks the turn closed and settles every queued
//! header `Waiting -> Closed`, waking each fiber, which returns
//! [`TurnError::Closed`]; a later take returns it at once. A holder keeps
//! its turn until it returns: its seam wait wakes it with the closed
//! handle's error.
//!
//! A fiber parked on a turn records no wait site (D-858), as one parked on
//! the seam: a survey reads it as busy, so it never appears in a deadlock
//! report. Its turn's holder runs or waits on the seam, where its armed
//! wait counts (D-984).

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

use super::fiber::{AfterSwitch, Unlock, begin_park, cancel_park, park};
use super::{WaitHeader, WaitState, wake};
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};

/// Why [`Turn::take`] returned without the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnError {
    /// The socket was closed before or while the fiber waited (D-1063).
    Closed,
    /// A cancellation reached the fiber before or while it waited (D-670).
    Cancelled,
}

impl From<Cancelled> for TurnError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// What a turn's lock guards.
#[derive(Default)]
struct TurnState {
    /// Some fiber holds the turn.
    held: bool,
    /// Set once by [`Turn::close`].
    closed: bool,
    /// The waiting fibers' headers, first come first.
    queue: VecDeque<Arc<WaitHeader>>,
}

/// One direction's FIFO lock over fibers (module documentation).
#[derive(Default)]
pub(crate) struct Turn {
    /// A leaf lock: never held while a fiber is woken or runs Cheby code.
    state: Mutex<TurnState>,
}

impl fmt::Debug for Turn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        f.debug_struct("Turn")
            .field("held", &state.held)
            .field("closed", &state.closed)
            .field("queued", &state.queue.len())
            .finish()
    }
}

/// The turn, held until dropped, which hands it on.
#[derive(Debug)]
pub(crate) struct TurnGuard<'a>(&'a Turn);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl Turn {
    /// A free, open turn.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Takes the turn for the running fiber `me`, parking behind its holder
    /// and the fibers queued before it (module documentation).
    ///
    /// # Errors
    ///
    /// [`TurnError::Cancelled`] when a cancellation reached `me` before or
    /// while it waited, and [`TurnError::Closed`] when the turn was closed
    /// before or while it waited; either way `me` holds no turn.
    pub(crate) fn take(&self, me: &ExecCtx) -> Result<TurnGuard<'_>, TurnError> {
        cancel::check(me)?;
        let mut state = self.state.lock();
        if state.closed {
            return Err(TurnError::Closed);
        }
        if !state.held {
            state.held = true;
            return Ok(TurnGuard(self));
        }
        if me.sched.runtime().is_none() {
            // Only a fiber of a runtime parks; an entry without one has no
            // other fiber that could hold the turn.
            drop(state);
            crate::fiber_ops::needs_scheduler("a socket operation");
        }
        let header = Arc::new(WaitHeader::new(me));
        begin_park(me);
        state.queue.push_back(Arc::clone(&header));
        // SAFETY: `header` is held by this frame until after `unpublish`
        // below.
        if unsafe { cancel::publish(me, &header) }.is_err() {
            // Cancelled before it waits: no release has seen the header,
            // which this lock still hides.
            Self::unqueue(&mut state, &header);
            drop(state);
            cancel_park(me);
            return Err(TurnError::Cancelled);
        }
        park(me, AfterSwitch::Park(Some(Unlock::new(state))));
        cancel::unpublish(me);
        match header.state() {
            // The release handed the turn over.
            WaitState::Claimed => Ok(TurnGuard(self)),
            WaitState::Closed => Err(TurnError::Closed),
            // The walk won the header; the release may not have popped it
            // yet. A fiber is woken only by the one that settled its
            // header, so `Waiting` is never seen here.
            WaitState::Cancelled | WaitState::Waiting => {
                Self::unqueue(&mut self.state.lock(), &header);
                Err(TurnError::Cancelled)
            }
        }
    }

    /// Takes `header` out of the queue if it is still there.
    fn unqueue(state: &mut TurnState, header: &Arc<WaitHeader>) {
        state.queue.retain(|queued| !Arc::ptr_eq(queued, header));
    }

    /// Hands the turn to the first queued fiber whose header this settles,
    /// waking it after the lock, or frees it when none is left.
    fn release(&self) {
        let next = {
            let mut state = self.state.lock();
            loop {
                match state.queue.pop_front() {
                    None => {
                        state.held = false;
                        break None;
                    }
                    Some(header) if header.try_settle_arm(WaitState::Claimed, 0) => {
                        break Some(header);
                    }
                    // The walk won it: that fiber is cancelled, not served.
                    Some(_) => {}
                }
            }
        };
        if let Some(header) = next {
            // SAFETY: the swap won the header, so its fiber is parked or
            // parking with the turn's lock (released before it can run)
            // and nothing else wakes it: it cannot retire before this
            // wake.
            unsafe { wake(header.fiber(), false) };
        }
    }

    /// Closes the turn (D-1063): every queued fiber wakes without it, with
    /// [`TurnError::Closed`], and every later take fails so. Never waits.
    pub(crate) fn close(&self) {
        let woken: Vec<Arc<WaitHeader>> = {
            let mut state = self.state.lock();
            state.closed = true;
            state
                .queue
                .drain(..)
                .filter(|header| header.try_settle_arm(WaitState::Closed, 0))
                .collect()
        };
        for header in woken {
            // SAFETY: as in `release`: the swap won the header.
            unsafe { wake(header.fiber(), false) };
        }
    }

    /// Whether some fiber holds the turn, and how many wait (for tests).
    #[cfg(test)]
    pub(crate) fn probe(&self) -> (bool, usize) {
        let state = self.state.lock();
        (state.held, state.queue.len())
    }
}
