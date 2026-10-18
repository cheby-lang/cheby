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
//! ([`WaitHeader::try_settle_arm`]) before it touches the node's payload; the
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
//!
//! **Shared headers** (D-704, D-206, architecture §7.2 and §7.3). A
//! `select` links one node per receive arm, each into its own channel's
//! queue, and all of them share one header ([`WaitNode::sharing`]) that
//! lives on the heap in an `Arc`, never on the fiber's stack: a timer entry
//! and the fiber lock's published pointer reach it too, and a timer entry
//! can outlive the select's frame (survey-t1 H2). Each node carries its
//! arm's index, and whoever settles the shared header records that index
//! in the same compare-and-swap ([`WaitHeader::try_settle_arm`]), so the
//! first waker of any of the nodes (a sender, a channel's close, the
//! select's timer) or the walk is the select's only waker, and the others'
//! swaps fail: a waker skips such a node as it skips one the walk won. A
//! node with its own header is arm 0.

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomPinned;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// One of [`WaitState`]'s values in the low byte, and above it the
    /// index of the arm whose waker settled it (module documentation,
    /// "Shared headers"); 0 above while `Waiting` and after a cancel.
    state: AtomicU64,
    /// The waiting fiber.
    fiber: FiberRef,
}

/// The bits of [`WaitHeader`]'s word below the arm's index.
const ARM_SHIFT: u32 = 8;

impl fmt::Debug for WaitHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitHeader")
            .field("state", &self.state())
            .field("arm", &self.arm())
            .field("fiber", &self.fiber)
            .finish()
    }
}

impl WaitHeader {
    /// A header of the waiting fiber `fiber`, in [`WaitState::Waiting`].
    pub(crate) fn new(fiber: &ExecCtx) -> Self {
        Self {
            state: AtomicU64::new(u64::from(WaitState::Waiting.bits())),
            fiber: FiberRef::from_ctx(fiber),
        }
    }

    /// The state's low byte.
    fn low(word: u64) -> u8 {
        u8::try_from(word & 0xFF).unwrap_or(u8::MAX)
    }

    /// The index of the arm whose waker settled the header, read with
    /// Acquire; 0 for a header that one node owns, and while it waits.
    pub(crate) fn arm(&self) -> usize {
        usize::try_from(self.state.load(Ordering::Acquire).wrapping_shr(ARM_SHIFT))
            .unwrap_or(usize::MAX)
    }

    /// Waker side: `Waiting -> to` (`Claimed` or `Closed`) for the arm
    /// `arm`, recorded in the same swap. False if the walk or another arm's
    /// waker won the header first, and the waker then skips the node.
    pub(crate) fn try_settle_arm(&self, to: WaitState, arm: usize) -> bool {
        let arm = u64::try_from(arm)
            .unwrap_or(u64::MAX)
            .min(u64::MAX >> ARM_SHIFT);
        self.state
            .compare_exchange(
                u64::from(WaitState::Waiting.bits()),
                arm.wrapping_shl(ARM_SHIFT) | u64::from(to.bits()),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
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
        WaitState::of(Self::low(self.state.load(Ordering::Acquire)))
    }

    /// Canceller side: `Waiting -> Cancelled`. False if a waker won the
    /// node first.
    pub(crate) fn try_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                u64::from(WaitState::Waiting.bits()),
                u64::from(WaitState::Cancelled.bits()),
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
    /// What wakers and the walk share, unless `shared` names another.
    header: WaitHeader,
    /// The header this node shares with the other nodes of one `select`
    /// (module documentation), which outlives the node.
    shared: Option<NonNull<WaitHeader>>,
    /// The index of the node's arm in its `select`; 0 for a node that owns
    /// its header.
    arm: usize,
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
            .field("header", self.header())
            .field("arm", &self.arm)
            .field("linked", &self.linked.get())
            .finish_non_exhaustive()
    }
}

impl<T> WaitNode<T> {
    /// An unlinked node of the waiting fiber `fiber`, carrying `payload`.
    pub(crate) fn new(fiber: &ExecCtx, payload: T) -> Self {
        Self {
            header: WaitHeader::new(fiber),
            shared: None,
            arm: 0,
            payload,
            prev: Cell::new(None),
            next: Cell::new(None),
            linked: Cell::new(false),
            _pinned: PhantomPinned,
        }
    }

    /// An unlinked node of the arm `arm` of a `select` of the waiting fiber
    /// `fiber`, carrying `payload` and sharing `header` with the select's
    /// other nodes (module documentation).
    ///
    /// # Safety
    ///
    /// `header` is `fiber`'s and stays live (it is in an `Arc` the fiber
    /// holds) until the node is unlinked for good and dropped.
    pub(crate) unsafe fn sharing(
        fiber: &ExecCtx,
        header: &WaitHeader,
        arm: usize,
        payload: T,
    ) -> Self {
        Self {
            shared: Some(NonNull::from(header)),
            arm,
            ..Self::new(fiber, payload)
        }
    }

    /// The header wakers and the walk share: the node's own, or the one it
    /// shares.
    pub(crate) fn header(&self) -> &WaitHeader {
        match self.shared {
            // SAFETY: a shared header outlives the node (the contract of
            // `sharing`).
            Some(shared) => unsafe { shared.as_ref() },
            None => &self.header,
        }
    }

    /// The index of the node's arm.
    #[cfg(test)]
    pub(crate) fn arm(&self) -> usize {
        self.arm
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
    /// Closed), and returns that one; a node the walk cancelled, or whose
    /// shared header another arm's waker won, is unlinked and skipped (its
    /// fiber finds it unlinked). The caller is the returned node's only
    /// waker: it may use the node's payload, then wakes its fiber once,
    /// after releasing the lock, and touches the node no more.
    pub(crate) fn pop_settled(&mut self, to: WaitState) -> Option<NonNull<WaitNode<T>>> {
        while let Some(node) = self.pop_front() {
            // SAFETY: as in `pop_front`: the node's fiber needs the lock the
            // caller holds before it can leave the node's frame.
            let waiter = unsafe { node.as_ref() };
            if waiter.header().try_settle_arm(to, waiter.arm) {
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

    use super::{WaitHeader, WaitNode, WaitQueue, WaitState};
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

    /// Nodes of one `select` in two queues share a header: the first
    /// waker wins it for its node's arm, recorded in the same swap; the
    /// other queue's waker skips (and unlinks) the select's node there, and
    /// the walk can no longer cancel it.
    #[test]
    fn select_nodes_share_one_header_with_one_winner() {
        let ctx = ExecCtx::new(FiberInit::placeholder());
        let header = std::sync::Arc::new(WaitHeader::new(&ctx));
        // SAFETY: the header outlives the nodes, which are dropped first.
        let left = pin!(unsafe { WaitNode::sharing(&ctx, &header, 3, 30_u8) });
        // SAFETY: as above.
        let right = pin!(unsafe { WaitNode::sharing(&ctx, &header, 5, 50_u8) });
        let other = pin!(WaitNode::new(&ctx, 7_u8));
        let (mut first, mut second) = (WaitQueue::default(), WaitQueue::default());
        let [left, right, other] = [&*left, &*right, &*other].map(NonNull::from);
        // SAFETY: pinned nodes on this frame, unlinked before it returns.
        unsafe {
            first.push_back(left);
            second.push_back(right);
            second.push_back(other);
        }
        assert_eq!(first.pop_settled(WaitState::Claimed), Some(left));
        assert_eq!((header.state(), header.arm()), (WaitState::Claimed, 3));
        // The other queue's waker skips the select's node and takes the
        // next one.
        assert_eq!(second.pop_settled(WaitState::Closed), Some(other));
        assert!(second.is_empty());
        assert!(!header.try_cancel());
        assert_eq!((header.state(), header.arm()), (WaitState::Claimed, 3));
        // SAFETY: as above.
        let (left_ref, right_ref) = unsafe { (left.as_ref(), right.as_ref()) };
        assert_eq!((left_ref.arm(), *left_ref.payload()), (3, 30));
        assert_eq!((right_ref.arm(), *right_ref.payload()), (5, 50));
        // SAFETY: as above.
        unsafe { second.remove(right) };
    }

    /// A header the walk won records no arm, and no waker can settle it.
    #[test]
    fn select_header_cancelled_first_takes_no_arm() {
        let ctx = ExecCtx::new(FiberInit::placeholder());
        let header = WaitHeader::new(&ctx);
        assert_eq!((header.state(), header.arm()), (WaitState::Waiting, 0));
        assert!(header.try_cancel());
        assert!(!header.try_settle_arm(WaitState::Claimed, 2));
        assert!(!header.try_settle_arm(WaitState::Closed, 0));
        assert_eq!((header.state(), header.arm()), (WaitState::Cancelled, 0));
    }
}
