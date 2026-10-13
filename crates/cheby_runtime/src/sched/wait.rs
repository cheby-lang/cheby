//! Wait nodes (D-670, D-706, architecture §3.4, §7.2 and §7.6): how a
//! fiber parked in a cancellable wait (a channel `send` or `receive`, a
//! `join`) meets its wakers and the cancellation walk.
//!
//! A wait node ([`WaitNode`]) lives on the waiting fiber's own stack (Cheby
//! stacks never move, D-124) and is linked into a FIFO [`WaitQueue`] under
//! the lock of the channel or join cell it waits on. Its header
//! ([`WaitHeader`]) starts at [`WaitState::Waiting`] and is settled exactly
//! once, by a compare-and-swap: a waker, under the queue's lock, moves it
//! to [`WaitState::Claimed`] or [`WaitState::Closed`]
//! ([`WaitHeader::try_settle`]) before it touches the node's payload; the
//! cancellation walk, under the waiting fiber's lock, moves it to
//! [`WaitState::Cancelled`] ([`WaitHeader::try_cancel`]). Whoever wins the
//! swap is the node's only waker and wakes its fiber once, after releasing
//! its locks; the loser leaves the node alone.
//!
//! A waker pops every node it looks at, won or not ([`WaitQueue::pop_settled`]),
//! so a node the walk won may already be unlinked when its fiber runs
//! again; the fiber then takes the queue's lock and unlinks its node if it
//! is still linked ([`WaitQueue::remove`]), in O(1): the queue is doubly
//! linked. A node is never left linked once its fiber leaves the node's
//! frame, so a queue never points to a dead frame.
//!
//! The links and the `linked` mark are used only under the queue's lock;
//! the header's state is atomic, since the walk reaches it under the fiber
//! lock instead (through the fiber's published wait pointer,
//! `fiber_ops::cancel::publish`).

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomPinned;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};

use super::fiber::FiberRef;
use crate::ctx::ExecCtx;

/// The states of a wait node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WaitState {
    /// Published and waiting.
    Waiting = 0,
    /// A waker handed the fiber what it waited for (a channel's value, a
    /// taken send, a join cell's end).
    Claimed = 1,
    /// The other side of a channel is gone.
    Closed = 2,
    /// The cancellation walk won the node (D-706): the fiber unwinds.
    Cancelled = 3,
}

impl WaitState {
    /// The byte of the state.
    const fn bits(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Claimed => 1,
            Self::Closed => 2,
            Self::Cancelled => 3,
        }
    }

    /// The state of the byte `value`.
    fn of(value: u8) -> Self {
        match value {
            0 => Self::Waiting,
            1 => Self::Claimed,
            2 => Self::Closed,
            _ => Self::Cancelled,
        }
    }
}

/// The part of a wait node that wakers and the cancellation walk share
/// (module documentation).
pub(crate) struct WaitHeader {
    /// One of [`WaitState`]'s values.
    state: AtomicU8,
    /// The waiting fiber.
    fiber: FiberRef,
}

impl fmt::Debug for WaitHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitHeader")
            .field("state", &self.state())
            .field("fiber", &self.fiber)
            .finish()
    }
}

impl WaitHeader {
    /// A header of the waiting fiber `fiber`, in [`WaitState::Waiting`].
    pub(crate) fn new(fiber: &ExecCtx) -> Self {
        Self {
            state: AtomicU8::new(WaitState::Waiting.bits()),
            fiber: FiberRef::from_ctx(fiber),
        }
    }

    /// The waiting fiber.
    pub(crate) fn fiber(&self) -> FiberRef {
        self.fiber
    }

    /// The state now, read with Acquire. The fiber reads it once it has
    /// been woken; what its waker wrote into the node after winning the
    /// swap is ordered before that by the wake itself (the scheduler's
    /// state word and run queues, `sched::wake`).
    pub(crate) fn state(&self) -> WaitState {
        WaitState::of(self.state.load(Ordering::Acquire))
    }

    /// Waker side: `Waiting -> to` (`Claimed` or `Closed`). False if a
    /// canceller won the node first, which the waker then skips.
    pub(crate) fn try_settle(&self, to: WaitState) -> bool {
        self.state
            .compare_exchange(
                WaitState::Waiting.bits(),
                to.bits(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Canceller side: `Waiting -> Cancelled`. False if a waker won the
    /// node first.
    pub(crate) fn try_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                WaitState::Waiting.bits(),
                WaitState::Cancelled.bits(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// A fiber waiting in a [`WaitQueue`]: the shared header, what the wait
/// carries (`T`), and the queue's links. Pinned on the waiting fiber's
/// stack while it is linked.
pub(crate) struct WaitNode<T> {
    /// What wakers and the walk share.
    header: WaitHeader,
    /// What the wait carries: a channel waiter's value cell; nothing for a
    /// joiner.
    payload: T,
    /// The previous node of the queue; under the queue's lock.
    prev: Cell<Option<NonNull<WaitNode<T>>>>,
    /// The next node of the queue; under the queue's lock.
    next: Cell<Option<NonNull<WaitNode<T>>>>,
    /// Whether the node is in a queue; under the queue's lock.
    linked: Cell<bool>,
    /// The node's address is in a queue while it is linked.
    _pinned: PhantomPinned,
}

impl<T> fmt::Debug for WaitNode<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitNode")
            .field("header", &self.header)
            .field("linked", &self.linked.get())
            .finish_non_exhaustive()
    }
}

impl<T> WaitNode<T> {
    /// An unlinked node of the waiting fiber `fiber`, carrying `payload`.
    pub(crate) fn new(fiber: &ExecCtx, payload: T) -> Self {
        Self {
            header: WaitHeader::new(fiber),
            payload,
            prev: Cell::new(None),
            next: Cell::new(None),
            linked: Cell::new(false),
            _pinned: PhantomPinned,
        }
    }

    /// The header wakers and the walk share.
    pub(crate) fn header(&self) -> &WaitHeader {
        &self.header
    }

    /// What the wait carries.
    pub(crate) fn payload(&self) -> &T {
        &self.payload
    }
}

/// A FIFO queue of waiting fibers' nodes, intrusive and doubly linked
/// (module documentation). Used only under the lock of the channel or join
/// cell that holds it.
pub(crate) struct WaitQueue<T> {
    head: Option<NonNull<WaitNode<T>>>,
    tail: Option<NonNull<WaitNode<T>>>,
}

impl<T> Default for WaitQueue<T> {
    fn default() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }
}

impl<T> fmt::Debug for WaitQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitQueue")
            .field("empty", &self.is_empty())
            .finish()
    }
}

impl<T> WaitQueue<T> {
    /// Whether no node is linked.
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// The number of linked nodes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        let mut count = 0_usize;
        let mut node = self.head;
        while let Some(current) = node {
            count = count.saturating_add(1);
            // SAFETY: a linked node is live (module documentation).
            node = unsafe { current.as_ref() }.next.get();
        }
        count
    }

    /// Links `node` at the end.
    ///
    /// # Safety
    ///
    /// `node` is a pinned, unlinked node on the stack of a fiber that is
    /// about to park with the lock of this queue, which the caller holds;
    /// the fiber does not leave the node's frame before the node is
    /// unlinked again, under that lock.
    pub(crate) unsafe fn push_back(&mut self, node: NonNull<WaitNode<T>>) {
        // SAFETY: forwarded from the caller.
        let new = unsafe { node.as_ref() };
        new.prev.set(self.tail);
        new.next.set(None);
        new.linked.set(true);
        match self.tail {
            // SAFETY: a linked node is live.
            Some(tail) => unsafe { tail.as_ref() }.next.set(Some(node)),
            None => self.head = Some(node),
        }
        self.tail = Some(node);
    }

    /// Unlinks and returns the first node, whatever its state.
    pub(crate) fn pop_front(&mut self) -> Option<NonNull<WaitNode<T>>> {
        let node = self.head?;
        // SAFETY: a linked node is live: its fiber does not leave the
        // node's frame before it is unlinked, under the lock the caller
        // holds.
        unsafe { self.unlink(node) };
        Some(node)
    }

    /// Unlinks nodes from the front until one settles to `to` (Claimed or
    /// Closed), and returns that one; a node the walk cancelled is unlinked
    /// and skipped (its fiber finds it unlinked). The caller is the
    /// returned node's only waker: it may use the node's payload, then
    /// wakes its fiber once, after releasing the lock, and touches the node
    /// no more.
    pub(crate) fn pop_settled(&mut self, to: WaitState) -> Option<NonNull<WaitNode<T>>> {
        while let Some(node) = self.pop_front() {
            // SAFETY: as in `pop_front`: the node's fiber needs the lock the
            // caller holds before it can leave the node's frame.
            if unsafe { node.as_ref() }.header.try_settle(to) {
                return Some(node);
            }
        }
        None
    }

    /// Unlinks `node` if it is still linked: a fiber that the walk woke
    /// takes its node out of the queue before it leaves the node's frame.
    ///
    /// # Safety
    ///
    /// `node` is the caller's own node, live, and was linked only into
    /// this queue, whose lock the caller holds.
    pub(crate) unsafe fn remove(&mut self, node: NonNull<WaitNode<T>>) {
        // SAFETY: forwarded from the caller.
        if unsafe { node.as_ref() }.linked.get() {
            // SAFETY: as above.
            unsafe { self.unlink(node) };
        }
    }

    /// Unlinks the linked node `node`.
    ///
    /// # Safety
    ///
    /// `node` is linked into this queue, whose lock the caller holds.
    unsafe fn unlink(&mut self, node: NonNull<WaitNode<T>>) {
        // SAFETY: forwarded from the caller: a linked node is live.
        let old = unsafe { node.as_ref() };
        let (prev, next) = (old.prev.take(), old.next.take());
        match prev {
            // SAFETY: a linked node is live.
            Some(prev) => unsafe { prev.as_ref() }.next.set(next),
            None => self.head = next,
        }
        match next {
            // SAFETY: as above.
            Some(next) => unsafe { next.as_ref() }.prev.set(prev),
            None => self.tail = prev,
        }
        old.linked.set(false);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::ptr::NonNull;

    use super::{WaitNode, WaitQueue, WaitState};
    use crate::ctx::{ExecCtx, FiberInit};

    /// The queue is FIFO, a node in the middle unlinks in O(1), and a node
    /// cancelled while linked is skipped by `pop_settled`, which unlinks
    /// it, so its fiber's `remove` then finds nothing to do.
    #[test]
    fn cancel_wait_queue_skips_and_unlinks_cancelled_nodes() {
        let ctx = ExecCtx::new(FiberInit::placeholder());
        let a = pin!(WaitNode::new(&ctx, 1_u8));
        let b = pin!(WaitNode::new(&ctx, 2_u8));
        let c = pin!(WaitNode::new(&ctx, 3_u8));
        let d = pin!(WaitNode::new(&ctx, 4_u8));
        let mut queue = WaitQueue::default();
        let [a, b, c, d] = [&*a, &*b, &*c, &*d].map(NonNull::from);
        // SAFETY: pinned nodes on this frame, unlinked before it returns.
        unsafe {
            queue.push_back(a);
            queue.push_back(b);
            queue.push_back(c);
            queue.push_back(d);
        }
        assert_eq!(queue.len(), 4);
        // The middle node leaves on its own.
        // SAFETY: as above.
        unsafe { queue.remove(c) };
        assert_eq!(queue.len(), 3);
        // The walk cancels the first node; the waker skips it.
        // SAFETY: as above.
        assert!(unsafe { a.as_ref() }.header().try_cancel());
        let won = queue.pop_settled(WaitState::Claimed).unwrap();
        assert_eq!(won, b);
        // SAFETY: as above.
        let (a_ref, b_ref) = unsafe { (a.as_ref(), b.as_ref()) };
        assert_eq!(*b_ref.payload(), 2);
        assert_eq!(b_ref.header().state(), WaitState::Claimed);
        assert!(!b_ref.header().try_cancel());
        // The cancelled node is unlinked already.
        // SAFETY: as above.
        unsafe { queue.remove(a) };
        assert_eq!(a_ref.header().state(), WaitState::Cancelled);
        assert_eq!(queue.pop_front(), Some(d));
        assert!(queue.is_empty());
        assert_eq!(queue.pop_settled(WaitState::Closed), None);
    }
}
