//! Channels (D-054, D-672, D-704, architecture §7.2): Go's `hchan`, and the
//! kernels `new`, `send` and `receive` of `std::channel`.
//!
//! A channel ([`Chan`]) is a lock over a FIFO buffer of up to `cap` values
//! and two FIFO queues of waiting fibers: receivers waiting for a value,
//! and senders waiting with theirs. Its `Sender` (kind 40) and `Receiver`
//! (kind 41) handles each hold it; every copy of a handle is the same
//! object, so the handle's finalizer runs when its last copy goes, which
//! closes that side of the channel (D-054, D-672).
//!
//! - **Waiters are intrusive nodes on the waiting fiber's own stack**
//!   ([`Waiter`]): Cheby stacks never move (D-124), and a node is unlinked
//!   by the fiber that wakes it before the waiting fiber can run again, so
//!   it never outlives its frame. A waker claims a node by storing its
//!   state (`CLAIMED` or `CLOSED`) under the channel lock, hands the value
//!   over, unlinks it and only then, after unlocking, wakes its fiber. The
//!   state is atomic so that cancellation (U5) can race a waker with a
//!   compare-and-swap.
//! - **The fiber parks with the channel lock** (`AfterSwitch::Park`): its
//!   worker releases the lock after the switch (I-5), so a waker that holds
//!   the lock always finds it parked or parking.
//! - **Values are marked shared outside the lock** (D-705): `send` marks
//!   the value before it locks, whether it is then enqueued, handed over,
//!   or given back because the channel is closed.
//! - **Order** (D-672): the buffer and both queues are FIFO, and a receive
//!   from a full buffer refills it from the first waiting sender.
//! - **Closing**: the `Sender` finalizer marks the channel closed for its
//!   receivers and wakes every waiting receiver, which gets `Err(Nil)`
//!   (the buffer is empty while receivers wait). The `Receiver` finalizer
//!   marks it closed for its senders, wakes every waiting sender with its
//!   own value back as `Err`, and releases the buffered values at once, on
//!   the dropping fiber (D-670). Finalizers never block, suspend, raise or
//!   run Cheby code (D-171, D-672).
//! - **Lock order**: a channel lock is a leaf here; no other lock is taken
//!   while it is held, no Cheby value is dropped and nothing is allocated
//!   under it. Selectors (later) lock several channels in address order.
//! - **Cost**: O(1) per operation that does not wait (D-300), plus the
//!   marking of a sent value.

use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::marker::PhantomPinned;
use std::pin::pin;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;

use super::{Owned, needs_scheduler, running};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind, HandlePayload};
use crate::object::{STATIC_OK_NIL, cheby_rt_static_err_nil};
use crate::panic::fatal;
use crate::rc;
use crate::sched::{AfterSwitch, FiberRef, Unlock, park, wake};
use crate::unwind::guard::guarded;
use crate::values;

/// A channel (architecture §7.2).
#[derive(Debug)]
pub(crate) struct Chan {
    inner: Mutex<ChanInner>,
}

/// The part of a channel under its lock.
#[derive(Debug)]
struct ChanInner {
    /// The buffered values, oldest first: each one reference, shared.
    buf: VecDeque<Word>,
    /// How many values the buffer holds at most; 0 is a synchronous
    /// handoff.
    cap: usize,
    /// Whether the `Sender` handle is alive.
    senders: bool,
    /// Whether the `Receiver` handle is alive.
    receivers: bool,
    /// Receivers waiting for a value.
    recvq: WaitQueue,
    /// Senders waiting with a value.
    sendq: WaitQueue,
}

// SAFETY: the only fields that are not `Send` are the queues' pointers to
// waiting fibers' nodes. They are used only under the channel lock, while
// the node's fiber is parked on it, on whichever thread holds the lock;
// the node itself is designed to be reached from other threads (atomic
// state, values handed over before the state is published).
unsafe impl Send for ChanInner {}

impl Chan {
    /// An open channel of capacity `cap`.
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(ChanInner {
                buf: VecDeque::new(),
                cap,
                senders: true,
                receivers: true,
                recvq: WaitQueue::default(),
                sendq: WaitQueue::default(),
            }),
        }
    }

    /// How many values are buffered and how many senders and receivers
    /// wait.
    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        let inner = self.inner.lock();
        (inner.buf.len(), inner.sendq.len(), inner.recvq.len())
    }
}

impl Drop for ChanInner {
    fn drop(&mut self) {
        // The `Receiver` finalizer empties the buffer, and both handles
        // hold the channel, so nothing is buffered or waiting when the
        // last of them goes.
        if !self.buf.is_empty() || !self.recvq.is_empty() || !self.sendq.is_empty() {
            fatal(format_args!("a channel was freed with values or waiters"));
        }
    }
}

/// The states of a [`Waiter`].
mod state {
    /// Linked in a queue, waiting.
    pub(super) const WAITING: u8 = 0;
    /// A waker took the sender's value or gave the receiver one.
    pub(super) const CLAIMED: u8 = 1;
    /// The other side of the channel is gone.
    pub(super) const CLOSED: u8 = 2;
}

/// A fiber waiting on a channel: a node on its own stack, linked into one
/// of the channel's queues while it is parked (architecture §7.2).
#[derive(Debug)]
struct Waiter {
    /// The waiting fiber.
    fiber: FiberRef,
    /// A sender's value, which the claimer takes, or the value handed to a
    /// receiver; one reference, shared.
    value: UnsafeCell<Word>,
    /// One of [`state`]'s values, stored with Release by the waker before
    /// it wakes the fiber, and read with Acquire by the fiber.
    state: AtomicU8,
    /// The next node of the queue.
    next: Cell<Option<NonNull<Waiter>>>,
    /// The node's address is in a queue while it is linked.
    _pinned: PhantomPinned,
}

impl Waiter {
    fn new(fiber: &ExecCtx, value: Word) -> Self {
        Self {
            fiber: FiberRef::from_ctx(fiber),
            value: UnsafeCell::new(value),
            state: AtomicU8::new(state::WAITING),
            next: Cell::new(None),
            _pinned: PhantomPinned,
        }
    }
}

/// A FIFO queue of waiting fibers' nodes, intrusive through
/// [`Waiter::next`].
#[derive(Debug, Default)]
struct WaitQueue {
    head: Option<NonNull<Waiter>>,
    tail: Option<NonNull<Waiter>>,
}

impl WaitQueue {
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        let mut count = 0_usize;
        let mut node = self.head;
        while let Some(current) = node {
            count = count.saturating_add(1);
            // SAFETY: a linked node is live (the module documentation).
            node = unsafe { current.as_ref() }.next.get();
        }
        count
    }

    /// Links `node` at the end.
    ///
    /// # Safety
    ///
    /// `node` is a pinned node of a fiber about to park with the lock of
    /// the channel this queue belongs to, which the caller holds; it stays
    /// live until a waker unlinks it.
    unsafe fn push_back(&mut self, node: NonNull<Waiter>) {
        // SAFETY: forwarded from the caller.
        unsafe { node.as_ref() }.next.set(None);
        match self.tail {
            // SAFETY: a linked node is live.
            Some(tail) => unsafe { tail.as_ref() }.next.set(Some(node)),
            None => self.head = Some(node),
        }
        self.tail = Some(node);
    }

    /// Unlinks the first node. The caller holds the channel lock, and
    /// becomes the node's only waker.
    fn pop_front(&mut self) -> Option<NonNull<Waiter>> {
        let node = self.head?;
        // SAFETY: a linked node is live: its fiber is parked on the lock
        // the caller holds, and only a waker unlinks it.
        let next = unsafe { node.as_ref() }.next.take();
        self.head = next;
        if next.is_none() {
            self.tail = None;
        }
        Some(node)
    }
}

/// Settles an unlinked node: stores `value` in it if given (for a
/// receiver), publishes `outcome`, and returns its fiber and the value it
/// held before (a sender's). The caller wakes the fiber afterwards, once,
/// and never touches the node again: the fiber may return and leave the
/// node's frame as soon as it runs.
///
/// # Safety
///
/// `node` was just unlinked by the caller, under the channel lock.
unsafe fn settle(node: NonNull<Waiter>, value: Option<Word>, outcome: u8) -> (FiberRef, Word) {
    // SAFETY: forwarded from the caller: the node is live until its fiber
    // runs again, which needs the wake that follows.
    let waiter = unsafe { node.as_ref() };
    // SAFETY: as above: only the caller touches the cell now.
    let held = unsafe { waiter.value.get().read() };
    if let Some(value) = value {
        // SAFETY: as above.
        unsafe { waiter.value.get().write(value) };
    }
    let fiber = waiter.fiber;
    waiter.state.store(outcome, Ordering::Release);
    (fiber, held)
}

/// Wakes the fiber of a node settled by [`settle`]; `from_running` when
/// the caller is a running fiber that hands the value over directly.
fn wake_settled(fiber: FiberRef, from_running: bool) {
    // SAFETY: the fiber registered its node under the channel lock and
    // parks with it, so it is parking or parked; the node was settled and
    // unlinked by the caller, its only waker.
    unsafe { wake(fiber, from_running) };
}

/// The payload of a `Sender` handle (kind 40).
#[derive(Debug)]
pub(crate) struct SenderEnd(pub(crate) Arc<Chan>);

/// The payload of a `Receiver` handle (kind 41).
#[derive(Debug)]
pub(crate) struct ReceiverEnd(pub(crate) Arc<Chan>);

impl HandlePayload for SenderEnd {
    /// The last `Sender` is gone (D-054): the channel is closed for its
    /// receivers, and every waiting receiver wakes to `Err(Nil)`.
    fn finalize(self: Box<Self>, _release: &mut dyn FnMut(Word)) {
        let mut woken = Vec::new();
        {
            let mut inner = self.0.inner.lock();
            inner.senders = false;
            while let Some(node) = inner.recvq.pop_front() {
                // SAFETY: just unlinked under the lock.
                woken.push(unsafe { settle(node, None, state::CLOSED) }.0);
            }
        }
        for fiber in woken {
            wake_settled(fiber, false);
        }
    }
}

impl HandlePayload for ReceiverEnd {
    /// The last `Receiver` is gone (D-054, D-670): the channel is closed
    /// for its senders, every waiting sender wakes to get its value back,
    /// and the buffered values are released now, on this fiber.
    fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word)) {
        let mut woken = Vec::new();
        let buffered = {
            let mut inner = self.0.inner.lock();
            inner.receivers = false;
            while let Some(node) = inner.sendq.pop_front() {
                // SAFETY: just unlinked under the lock; the sender keeps
                // its value, which it gets back.
                woken.push(unsafe { settle(node, None, state::CLOSED) }.0);
            }
            std::mem::take(&mut inner.buf)
        };
        for fiber in woken {
            wake_settled(fiber, false);
        }
        for value in buffered {
            release(value);
        }
    }
}

/// `channel::new`: the two ends of a new channel that buffers up to
/// `capacity` values; a negative capacity counts as 0 (D-298, D-672). The
/// buffer grows with the values it holds, not with `capacity`.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_channel_new(ctx: *mut ExecCtx, capacity: i64) -> Word {
    guarded(Some(ctx), || {
        let cap = usize::try_from(capacity.max(0)).unwrap_or(usize::MAX);
        let chan = Arc::new(Chan::new(cap));
        // SAFETY: `ctx` is the running fiber's; fresh handles, owned here
        // until the tuple takes them.
        let (sender, receiver) = unsafe {
            let sender = Owned::new(handles::new_handle(
                ctx,
                HandleKind::Sender,
                Box::new(SenderEnd(Arc::clone(&chan))),
            ));
            let receiver = Owned::new(handles::new_handle(
                ctx,
                HandleKind::Receiver,
                Box::new(ReceiverEnd(chan)),
            ));
            (sender, receiver)
        };
        let pair = values::tuple(&[sender.word(), receiver.word()]);
        // The tuple owns them now.
        let _ = (sender.into_word(), receiver.into_word());
        pair
    })
}

/// `channel::send`: puts `value` into the channel (D-054, D-672). Returns
/// `Ok(Nil)` once it is enqueued or handed to a waiting receiver; while the
/// buffer is full, or for capacity 0 while nobody takes it, the fiber
/// waits. `Err(value)` when every `Receiver` is gone, before the call or
/// while it waited. The value is marked shared first (D-705).
///
/// # Safety
///
/// `ctx` is the running fiber's context, `sender` a live `Sender` handle
/// and `value` a live value of the channel's type, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_channel_send(ctx: *mut ExecCtx, sender: Word, value: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { send(ctx, sender, value) })
}

/// The body of [`cheby_channel_send`].
///
/// # Safety
///
/// As for [`cheby_channel_send`].
unsafe fn send(ctx: *mut ExecCtx, sender: Word, value: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // SAFETY: a live `Sender` handle, borrowed. The channel is cloned so
    // that nothing borrowed from the handle is held across a park.
    let chan = Arc::clone(&unsafe { handles::payload::<SenderEnd>(sender, HandleKind::Sender) }.0);
    // SAFETY: a live value, borrowed; every object it reaches with a
    // positive count is confined to this fiber (I-1). The reference taken
    // is the channel's once the value is enqueued or handed over.
    let value = unsafe {
        rc::mark_shared(value);
        rc::dup(value);
        Owned::new(value)
    };
    let mut inner = chan.inner.lock();
    if !inner.receivers {
        drop(inner);
        return give_back(value.into_word());
    }
    if let Some(node) = inner.recvq.pop_front() {
        // SAFETY: just unlinked under the lock.
        let (fiber, _) = unsafe { settle(node, Some(value.into_word()), state::CLAIMED) };
        drop(inner);
        wake_settled(fiber, true);
        return STATIC_OK_NIL.word();
    }
    if inner.buf.len() < inner.cap {
        inner.buf.push_back(value.into_word());
        return STATIC_OK_NIL.word();
    }
    if me.sched.runtime().is_none() {
        drop(inner);
        drop(value);
        needs_scheduler("channel::send");
    }
    let node = pin!(Waiter::new(me, value.into_word()));
    // SAFETY: the node is pinned on this fiber's stack, which never moves,
    // and stays there until the fiber returns from this function, which it
    // does only after a waker unlinked and settled the node; the fiber
    // parks with the lock held here.
    unsafe { inner.sendq.push_back(NonNull::from(&*node)) };
    park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
    match node.state.load(Ordering::Acquire) {
        state::CLAIMED => STATIC_OK_NIL.word(),
        // SAFETY: the waker settled the node and woke this fiber; the
        // value in it is still the sender's.
        state::CLOSED => give_back(unsafe { node.value.get().read() }),
        other => fatal(format_args!("a sender woke with its node in state {other}")),
    }
}

/// `Err(value)` for a send whose receivers are gone, consuming the
/// reference to `value`, also when the allocation raises.
fn give_back(value: Word) -> Word {
    // SAFETY: the caller's reference, given up here.
    let value = unsafe { Owned::new(value) };
    let result = values::err(value.word());
    let _ = value.into_word();
    result
}

/// `channel::receive`: takes the next value from the channel and returns
/// it in `Ok` (D-054, D-672). While the channel is empty and its `Sender`
/// is alive, the fiber waits. `Err(Nil)` once the channel is empty and its
/// `Sender` is gone.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `receiver` a live `Receiver`
/// handle, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_channel_receive(ctx: *mut ExecCtx, receiver: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { receive(ctx, receiver) })
}

/// The body of [`cheby_channel_receive`].
///
/// # Safety
///
/// As for [`cheby_channel_receive`].
unsafe fn receive(ctx: *mut ExecCtx, receiver: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // SAFETY: a live `Receiver` handle, borrowed; cloned as in `send`.
    let chan =
        Arc::clone(&unsafe { handles::payload::<ReceiverEnd>(receiver, HandleKind::Receiver) }.0);
    let mut inner = chan.inner.lock();
    if let Some(value) = inner.buf.pop_front() {
        // A full buffer takes the first waiting sender's value.
        let woken = inner.sendq.pop_front().map(|node| {
            // SAFETY: just unlinked under the lock.
            let (fiber, held) = unsafe { settle(node, None, state::CLAIMED) };
            inner.buf.push_back(held);
            fiber
        });
        drop(inner);
        if let Some(fiber) = woken {
            wake_settled(fiber, true);
        }
        return ok(value);
    }
    if let Some(node) = inner.sendq.pop_front() {
        // SAFETY: just unlinked under the lock.
        let (fiber, held) = unsafe { settle(node, None, state::CLAIMED) };
        drop(inner);
        wake_settled(fiber, true);
        return ok(held);
    }
    if !inner.senders {
        return cheby_rt_static_err_nil.word();
    }
    if me.sched.runtime().is_none() {
        drop(inner);
        needs_scheduler("channel::receive");
    }
    let node = pin!(Waiter::new(me, 0));
    // SAFETY: as in `send`.
    unsafe { inner.recvq.push_back(NonNull::from(&*node)) };
    park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
    match node.state.load(Ordering::Acquire) {
        // SAFETY: the waker stored the value before it published the
        // state, which this load acquired.
        state::CLAIMED => ok(unsafe { node.value.get().read() }),
        state::CLOSED => cheby_rt_static_err_nil.word(),
        other => fatal(format_args!(
            "a receiver woke with its node in state {other}"
        )),
    }
}

/// `Ok(value)`, consuming the reference to `value`, also when the
/// allocation raises.
fn ok(value: Word) -> Word {
    // SAFETY: the caller's reference, given up here.
    let value = unsafe { Owned::new(value) };
    let result = values::ok(value.word());
    let _ = value.into_word();
    result
}
