//! Channels (D-054, D-670, D-672, D-704, architecture §7.2): Go's `hchan`,
//! and the kernels `new`, `send` and `receive` of `std::channel`.
//!
//! A channel ([`Chan`]) is a lock over a FIFO buffer of up to `cap` values
//! and two FIFO queues of waiting fibers: receivers waiting for a value,
//! and senders waiting with theirs. Its `Sender` (kind 40) and `Receiver`
//! (kind 41) handles each hold it; every copy of a handle is the same
//! object, so the handle's finalizer runs when its last copy goes, which
//! closes that side of the channel (D-054, D-672).
//!
//! - **Waiters are intrusive wait nodes on the waiting fiber's own stack**
//!   (`sched::WaitNode`, whose payload is the value cell): Cheby stacks
//!   never move (D-124), and a node is unlinked before its fiber leaves the
//!   node's frame. A waker claims a node by a compare-and-swap of its state
//!   `Waiting -> Claimed` (or `Closed`), under the channel lock, before it
//!   touches the value cell; it hands the value over and only then, after
//!   unlocking, wakes the fiber. The cancellation walk races it with its
//!   own swap `Waiting -> Cancelled`, under the fiber's lock (D-706): a
//!   waker skips a node the walk won (`WaitQueue::pop_settled`), and the
//!   cancelled fiber unlinks its node itself if it is still linked.
//! - **A waiter checks for cancellation before it parks** (D-102): after
//!   `sched::begin_park` it links its node and publishes it under its fiber
//!   lock (`cancel::publish`), which refuses if a cancellation reached the
//!   fiber; it then unlinks the node and raises instead of parking.
//! - **What a cancelled wait leaves** (D-670): a cancelled `send` has not
//!   enqueued its value, which it releases as it unwinds; a cancelled
//!   `receive` takes nothing, so the value goes to another receiver.
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
//! - **Lock order**: the channel lock, then a waiter's fiber lock (its
//!   publication); no other lock is taken while it is held, no Cheby value
//!   is dropped and nothing is allocated under it, and nothing raises
//!   while it is held. Selectors (later) lock several channels in address
//!   order.
//! - **Cost**: O(1) per operation that does not wait (D-300), plus the
//!   marking of a sent value; a cancelled waiter unlinks in O(1).

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::pin::pin;
use std::ptr::NonNull;
use std::sync::Arc;

use parking_lot::Mutex;

use super::cancel;
use super::{Owned, needs_scheduler, running};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind, HandlePayload};
use crate::object::{STATIC_OK_NIL, cheby_rt_static_err_nil};
use crate::panic::fatal;
use crate::rc;
use crate::sched::{
    AfterSwitch, FiberRef, Unlock, WaitNode, WaitQueue, WaitState, begin_park, cancel_park, park,
    wake,
};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::values;

/// A fiber waiting on a channel: a wait node whose payload is the value
/// cell, holding a sender's value, which the claimer takes, or the value
/// handed to a receiver; one reference, shared. Only the node's waker (the
/// winner of its swap) or its own fiber touches the cell.
type Waiter = WaitNode<UnsafeCell<Word>>;

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
    recvq: WaitQueue<UnsafeCell<Word>>,
    /// Senders waiting with a value.
    sendq: WaitQueue<UnsafeCell<Word>>,
}

// SAFETY: the only fields that are not `Send` are the queues' pointers to
// waiting fibers' nodes. They are used only under the channel lock, while
// the node's fiber is parked on it or holds the lock itself, on whichever
// thread holds the lock; the node itself is designed to be reached from
// other threads (an atomic state, values handed over by the swap's winner
// only).
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
        // The `Receiver` finalizer empties the buffer, both handles hold the
        // channel, and a waiter unlinks its node before it leaves, so
        // nothing is buffered or waiting when the last of them goes.
        if !self.buf.is_empty() || !self.recvq.is_empty() || !self.sendq.is_empty() {
            fatal(format_args!("a channel was freed with values or waiters"));
        }
    }
}

/// The waiter of a node the caller won with `WaitQueue::pop_settled`.
///
/// # Safety
///
/// The caller holds the channel lock and won `node`'s swap: the node is
/// live until its fiber runs again, which needs the wake that follows, and
/// only the caller touches its value cell meanwhile.
unsafe fn won<'a>(node: NonNull<Waiter>) -> &'a Waiter {
    // SAFETY: forwarded from the caller.
    unsafe { node.as_ref() }
}

/// Wakes the fiber of a node the caller won and settled, once, after
/// releasing the channel lock; `from_running` when the caller is a running
/// fiber that hands the value over directly. The caller touches the node
/// no more: the fiber may leave the node's frame as soon as it runs.
fn wake_settled(fiber: FiberRef, from_running: bool) {
    // SAFETY: the fiber's node was linked under the channel lock and the
    // caller won its swap, so the fiber is parked or parking on it and
    // nothing else wakes it: it is not retired.
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
    /// receivers, and every waiting receiver wakes to `Err(Nil)`; one that
    /// is being cancelled is skipped and unwinds.
    fn finalize(self: Box<Self>, _release: &mut dyn FnMut(Word)) {
        let mut woken = Vec::new();
        {
            let mut inner = self.0.inner.lock();
            inner.senders = false;
            while let Some(node) = inner.recvq.pop_settled(WaitState::Closed) {
                // SAFETY: won under the lock.
                woken.push(unsafe { won(node) }.header().fiber());
            }
        }
        for fiber in woken {
            wake_settled(fiber, false);
        }
    }
}

impl HandlePayload for ReceiverEnd {
    /// The last `Receiver` is gone (D-054, D-670): the channel is closed
    /// for its senders, every waiting sender wakes to get its value back
    /// (one that is being cancelled is skipped and releases its value as it
    /// unwinds), and the buffered values are released now, on this fiber.
    fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word)) {
        let mut woken = Vec::new();
        let buffered = {
            let mut inner = self.0.inner.lock();
            inner.receivers = false;
            while let Some(node) = inner.sendq.pop_settled(WaitState::Closed) {
                // SAFETY: won under the lock; the sender keeps its value,
                // which it gets back.
                woken.push(unsafe { won(node) }.header().fiber());
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
/// while it waited. The value is marked shared first (D-705). A fiber that
/// is being cancelled unwinds instead of waiting, or when the cancellation
/// reaches it while it waits, and its value is not enqueued (D-670); so
/// does one woken by the receivers' end while a scope on its chain has
/// failed or is being cancelled (`cancel::check_chain`).
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
    if let Some(node) = inner.recvq.pop_settled(WaitState::Claimed) {
        // SAFETY: won under the lock.
        let waiter = unsafe { won(node) };
        // SAFETY: as above: only this caller touches the cell.
        unsafe { waiter.payload().get().write(value.into_word()) };
        let fiber = waiter.header().fiber();
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
    begin_park(me);
    let node = pin!(Waiter::new(me, UnsafeCell::new(value.into_word())));
    let link = NonNull::from(&*node);
    // SAFETY: the node is pinned on this fiber's stack, which never moves,
    // and stays there until it is unlinked under the channel lock: by a
    // waker, or below by this fiber; the fiber parks with the lock held
    // here.
    unsafe { inner.sendq.push_back(link) };
    // SAFETY: the node lives until after `unpublish` below.
    if unsafe { cancel::publish(me, node.header()) }.is_err() {
        // Cancelled before it waits: no waker has seen the node.
        // SAFETY: this fiber's own node, linked into `sendq` under the lock
        // it holds.
        unsafe { inner.sendq.remove(link) };
        drop(inner);
        cancel_park(me);
        // SAFETY: nobody else saw the node: the value is still the
        // sender's, released here as the cancellation unwinds (D-670).
        drop(unsafe { Owned::new(node.payload().get().read()) });
        raise_cancel(Some(ctx));
    }
    park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
    cancel::unpublish(me);
    match node.header().state() {
        WaitState::Claimed => STATIC_OK_NIL.word(),
        WaitState::Closed => {
            // SAFETY: the waker settled the node and woke this fiber; the
            // value in it is still the sender's.
            let value = unsafe { Owned::new(node.payload().get().read()) };
            // GAP(chain-cancellation). The close may come from the
            // unwinding of a fiber that a cancellation of this fiber's
            // enclosing scope reached first (`cancel::check_chain`, review
            // finding CONC-W2-1): then this fiber is being cancelled too,
            // and the value is released as it unwinds (D-670).
            if cancel::check_chain(me, me.conc.responsible_scope()).is_err() {
                drop(value);
                raise_cancel(Some(ctx));
            }
            give_back(value.into_word())
        }
        WaitState::Cancelled => {
            // SAFETY: this fiber's own node, which a waker may have
            // unlinked already; the lock is released at the end of the
            // statement.
            unsafe { chan.inner.lock().sendq.remove(link) };
            // SAFETY: the walk won the node, so no waker took the value:
            // it is still the sender's, not enqueued, and released here as
            // the cancellation unwinds (D-670).
            drop(unsafe { Owned::new(node.payload().get().read()) });
            raise_cancel(Some(ctx))
        }
        WaitState::Waiting => fatal(format_args!("a sender woke while still waiting")),
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
/// `Sender` is gone. A fiber that is being cancelled unwinds instead of
/// waiting, or when the cancellation reaches it while it waits, and takes
/// nothing (D-670); so does one woken by the senders' end while a scope on
/// its chain has failed or is being cancelled (`cancel::check_chain`).
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
        let woken = inner.sendq.pop_settled(WaitState::Claimed).map(|node| {
            // SAFETY: won under the lock.
            let waiter = unsafe { won(node) };
            // SAFETY: as above: only this caller touches the cell.
            inner
                .buf
                .push_back(unsafe { waiter.payload().get().read() });
            waiter.header().fiber()
        });
        drop(inner);
        if let Some(fiber) = woken {
            wake_settled(fiber, true);
        }
        return ok(value);
    }
    if let Some(node) = inner.sendq.pop_settled(WaitState::Claimed) {
        // SAFETY: won under the lock.
        let waiter = unsafe { won(node) };
        // SAFETY: as above.
        let held = unsafe { waiter.payload().get().read() };
        let fiber = waiter.header().fiber();
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
    begin_park(me);
    let node = pin!(Waiter::new(me, UnsafeCell::new(0)));
    let link = NonNull::from(&*node);
    // SAFETY: as in `send`.
    unsafe { inner.recvq.push_back(link) };
    // SAFETY: the node lives until after `unpublish` below.
    if unsafe { cancel::publish(me, node.header()) }.is_err() {
        // SAFETY: as in `send`.
        unsafe { inner.recvq.remove(link) };
        drop(inner);
        cancel_park(me);
        raise_cancel(Some(ctx));
    }
    park(me, AfterSwitch::Park(Some(Unlock::new(inner))));
    cancel::unpublish(me);
    match node.header().state() {
        // SAFETY: the waker stored the value after winning the node and
        // before waking this fiber, which orders the store before this
        // read (`WaitHeader::state`).
        WaitState::Claimed => ok(unsafe { node.payload().get().read() }),
        // GAP(chain-cancellation), as in `send`: a waiter woken by a close
        // while it is being cancelled unwinds and takes nothing (D-667,
        // D-670).
        WaitState::Closed => {
            if cancel::check_chain(me, me.conc.responsible_scope()).is_err() {
                raise_cancel(Some(ctx));
            }
            cheby_rt_static_err_nil.word()
        }
        WaitState::Cancelled => {
            // SAFETY: as in `send`. The walk won the node, so no value was
            // handed to it: the receive takes nothing (D-670).
            unsafe { chan.inner.lock().recvq.remove(link) };
            raise_cancel(Some(ctx))
        }
        WaitState::Waiting => fatal(format_args!("a receiver woke while still waiting")),
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
