//! Selectors (D-041, D-180, D-206, D-673, D-695, architecture §7.3): the
//! private kernels of `std::selector`.
//!
//! A `Selector` is a persistent list of kind-44 nodes
//! (`handles::new_selector_node`, whose module documentation gives the
//! layout), built in O(1) by `sys_new`, `sys_add_receive` and
//! `sys_add_after`. `select` is Cheby: it calls `sys_wait`, which waits
//! until an arm is ready, moves the chosen value into the fiber
//! ([`SelectState`]) and returns the chosen arm's apply closure; Cheby
//! releases the selector, then calls the closure, whose receive arm takes
//! the value with `take_selected` before it calls the mapping function
//! (D-205, D-670).
//!
//! GAP(selector-kernel-names): the kernels are `sys_new`,
//! `sys_add_receive`, `sys_add_after`, `sys_wait` and `take_selected`, all
//! `@std_context`, bound as `cheby_selector_<name>` (D-755's pattern).
//!
//! # `sys_wait`
//!
//! 1. Gathers the arms from the list and starts every timer arm's clock
//!    now, so each `select` waits anew (D-673).
//! 2. Draws a uniformly random order of the arms from the fiber's xorshift
//!    state ([`SelectState::next_random`]), seeded per fiber and never
//!    shared between fibers.
//! 3. Locks the receive arms' channels in address order, each once
//!    (duplicate receivers share a channel), and scans the arms in the
//!    random order for a ready one: a buffered value, a waiting sender, a
//!    closed and empty channel (ready with `Err(Nil)`, D-180), a timer arm
//!    of zero or less (D-652). The first ready arm in that order is chosen,
//!    so the choice among ready arms is uniform (D-673). A closed arm found
//!    here reads no scope chain (D-769: an operation that completes
//!    without suspending reads the target alone, and nothing is delivered
//!    there).
//! 4. Otherwise, without a runtime, the program ends
//!    (`needs_scheduler("selector::select")`, D-692). With one, the fiber
//!    links one node per receive arm (`ChanInner::link_receiver`), all
//!    sharing one header on the heap (`sched::WaitNode::sharing`); it
//!    publishes that header under its fiber lock (`cancel::publish`), also
//!    when there is no receive arm or no arm at all, so the cancellation
//!    walk and a teardown can wake every select (GAP(select-always-publishes));
//!    it arms one timer for the earliest timer arm; and it parks with
//!    every channel lock (`AfterSwitch::ParkAll`).
//! 5. Woken, it unpublishes, unlinks each node under its channel's lock and
//!    cancels its timer before anything else, then reads the header, which
//!    exactly one waker settled: a sender's hand-off (`Claimed`, the value
//!    in that arm's node, D-206), the timer (`Claimed` for the timer arm),
//!    a channel's close (`Closed`), or the walk (`Cancelled`: the select
//!    unwinds and takes nothing, D-670). A `Closed` wake reads the scope
//!    chain first (D-769): a fiber being cancelled unwinds and takes
//!    nothing; otherwise the arm is chosen with `Err(Nil)`.
//! 6. Moves the chosen receive arm's `Result` into the fiber and returns
//!    the arm's apply closure, dup'd.
//!
//! Timer arms are ordered by their spans, all measured from one `select`'s
//! start, so arms of equal spans have equal deadlines. Several timer arms
//! with the same, shortest span arm one timer for the first of them in the
//! random order, which is a uniform choice among them
//! (GAP(select-equal-timers)).
//!
//! **Locks** (`fiber_ops` module documentation): the channel locks in
//! address order, each once, then the fiber lock (to publish) and the
//! timer heap's lock (a leaf), never with a scope lock. No Cheby value is
//! allocated or dropped under the channel locks, and nothing raises under
//! them. Senders a ready scan released are woken after every lock is
//! released.
//!
//! **Cost** (D-675, informative): O(k) in the number of arms, for the
//! gathering, the order, the locks and the nodes, plus the wait.

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::{Mutex, MutexGuard};

use super::channel::{Chan, ChanInner, Ready, ReceiverEnd, Waiter, wake_released};
use super::{Owned, cancel, needs_scheduler, running};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::handles::{self, HandleKind, SELECTOR_AFTER, SELECTOR_EMPTY, SELECTOR_RECEIVE};
use crate::object::cheby_rt_static_err_nil;
use crate::panic::fatal;
use crate::rc;
use crate::sched::{
    AfterSwitch, Deadline, TimerHandle, TimerTarget, Unlocks, WaitHeader, WaitNode, WaitState,
    begin_park, cancel_park, deadline_after, park, wake,
};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::values;

/// A fiber's selector state, part of its [`super::FiberConc`]: the value
/// the last `select` chose, until its apply closure takes it, and the
/// random state that orders a select's arms. Only the fiber itself touches
/// it, so the lock is never contended.
#[derive(Debug, Default)]
pub(crate) struct SelectState {
    /// The chosen receive arm's `Result`, owned; 0 when there is none.
    selected: Mutex<Word>,
    /// The fiber's xorshift state; 0 until the fiber's first `select`
    /// seeds it.
    random: AtomicU64,
}

impl SelectState {
    /// Stores the chosen value, owned. A value still there is a broken
    /// invariant: every `sys_wait` that chooses a receive arm is followed
    /// by its closure's take, or by a raise that drops the value.
    fn put(&self, value: Owned) {
        let mut selected = self.selected.lock();
        if *selected != 0 {
            drop(selected);
            fatal(format_args!("a select chose a value while one was waiting"));
        }
        *selected = value.into_word();
    }

    /// Takes the chosen value, owned, if there is one.
    fn take(&self) -> Option<Owned> {
        let word = std::mem::take(&mut *self.selected.lock());
        // SAFETY: a stored word is one owned reference, given up here.
        (word != 0).then(|| unsafe { Owned::new(word) })
    }

    /// Whether a chosen value waits to be taken.
    #[cfg(test)]
    pub(crate) fn holds_value(&self) -> bool {
        *self.selected.lock() != 0
    }

    /// The next number of the fiber's xorshift generator, seeded on first
    /// use from the process seed and a per-process count of seeded fibers,
    /// so no two fibers share a sequence.
    pub(crate) fn next_random(&self) -> u64 {
        let mut state = self.random.load(Ordering::Relaxed);
        if state == 0 {
            static SEEDED: AtomicU64 = AtomicU64::new(0);
            let count = SEEDED.fetch_add(1, Ordering::Relaxed);
            state = splitmix64(crate::generic::process_seed() ^ splitmix64(count)) | 1;
        }
        state ^= state.wrapping_shl(13);
        state ^= state.wrapping_shr(7);
        state ^= state.wrapping_shl(17);
        self.random.store(state, Ordering::Relaxed);
        state
    }

    /// A uniformly random index below `bound` (at least 1).
    fn below(&self, bound: usize) -> usize {
        let bound = u64::try_from(bound).unwrap_or(u64::MAX).max(1);
        usize::try_from(self.next_random().checked_rem(bound).unwrap_or(0)).unwrap_or(0)
    }
}

/// The output function of `SplitMix64`, to spread a seed's bits.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ z.wrapping_shr(31)
}

/// Releases the value the last `select` chose if it is still in the fiber
/// (architecture §7.3: any raise clears it), for example when a
/// cancellation reaches the apply closure's entry check before it takes
/// the value. `unwind::record::start_panic` and `start_cancel` call it
/// first; they are entered with no runtime lock held, so the drop may run
/// handle finalizers.
pub(crate) fn drop_selected(me: &ExecCtx) {
    drop(me.conc.select.take());
}

/// A word the kernel gives away: `word` with one more reference.
///
/// # Safety
///
/// `word` is a live value or a tagged integer, borrowed.
unsafe fn dup(word: Word) -> Word {
    // SAFETY: forwarded from the caller.
    unsafe { rc::dup(word) };
    word
}

/// `selector::new`'s kernel: an empty selector, owned (D-673, D-695).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_selector_sys_new(ctx: *mut ExecCtx) -> Word {
    guarded(Some(ctx), || {
        let zero = values::int_word(0);
        handles::new_selector_node([zero, values::int_word(SELECTOR_EMPTY), zero, zero, zero])
    })
}

/// `selector::receive`'s kernel: `selector` with one more arm that
/// receives from `receiver` and is applied through `apply`; all three are
/// borrowed, the new selector owned (D-187, D-695).
///
/// # Safety
///
/// `ctx` is the running fiber's context, `selector` a live selector,
/// `receiver` a live `Receiver` and `apply` a live closure, all borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_selector_sys_add_receive(
    ctx: *mut ExecCtx,
    selector: Word,
    receiver: Word,
    apply: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller: live values, borrowed; the
        // guards own the new references until the node takes them.
        let owned = unsafe {
            [
                Owned::new(dup(selector)),
                Owned::new(dup(receiver)),
                Owned::new(dup(apply)),
            ]
        };
        let node = handles::new_selector_node([
            owned[0].word(),
            values::int_word(SELECTOR_RECEIVE),
            owned[1].word(),
            values::int_word(0),
            owned[2].word(),
        ]);
        // The node owns them now.
        for word in owned {
            let _ = word.into_word();
        }
        node
    })
}

/// `selector::after`'s kernel: `selector` with one more arm, a timer of
/// `seconds` and `nanoseconds` (std's `Duration`, D-652) whose clock starts
/// in each `select` (D-673), applied through `apply`; arguments borrowed,
/// the new selector owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context, `selector` a live selector and
/// `apply` a live closure, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_selector_sys_add_after(
    ctx: *mut ExecCtx,
    selector: Word,
    seconds: i64,
    nanoseconds: i64,
    apply: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller: live values, borrowed; the
        // guards own the new references until the node takes them. An
        // integer outside 63 bits is boxed, which may raise: the guards
        // made so far release theirs.
        let owned = unsafe {
            [
                Owned::new(dup(selector)),
                Owned::new(dup(apply)),
                Owned::new(values::int_word(seconds)),
                Owned::new(values::int_word(nanoseconds)),
            ]
        };
        let node = handles::new_selector_node([
            owned[0].word(),
            values::int_word(SELECTOR_AFTER),
            owned[2].word(),
            owned[3].word(),
            owned[1].word(),
        ]);
        // The node owns them now.
        for word in owned {
            let _ = word.into_word();
        }
        node
    })
}

/// What an arm waits for, as `sys_wait` gathers it.
#[derive(Debug)]
enum Kind {
    /// A receive arm: its channel, the index of that channel's lock among
    /// the select's distinct channels, and the index of the arm's node.
    Receive {
        chan: Arc<Chan>,
        lock: usize,
        node: usize,
    },
    /// A timer arm: its deadline, from this `select`'s start (D-673), and
    /// its span in nanoseconds, which orders the timer arms: arms of equal
    /// spans have equal deadlines (GAP(select-equal-timers)).
    After(Deadline, i128),
}

/// The nanoseconds in a second.
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// How many selects of this process woke for a channel's close and then
/// unwound because their scope chain was being cancelled (D-769): the
/// path `select_closed_arm_reads_the_chain` reports it took.
#[cfg(test)]
pub(super) static CLOSED_WAKES_CANCELLED: AtomicU64 = AtomicU64::new(0);

/// One arm of a select.
#[derive(Debug)]
struct Arm {
    /// What it waits for.
    kind: Kind,
    /// Its apply closure, borrowed from the selector.
    apply: Word,
}

/// The arms of `selector`, newest first, each timer's deadline from now.
///
/// # Safety
///
/// `selector` is a live selector, borrowed for as long as the arms are
/// used: its nodes hold the receivers and the closures.
unsafe fn gather(selector: Word) -> Vec<Arm> {
    let mut arms = Vec::new();
    let mut receives = 0_usize;
    let mut node = selector;
    loop {
        // SAFETY: a node of the live selector: the caller's, or one an
        // earlier node holds.
        let words = unsafe { handles::selector_words(node) };
        let [previous, tag, payload, nanoseconds, apply] = words;
        // SAFETY: the tag and a timer's span are `Int` words
        // (`handles::new_selector_node`'s layout).
        let kind = match unsafe { values::word_to_i64(tag) } {
            SELECTOR_EMPTY => return arms,
            SELECTOR_RECEIVE => {
                // SAFETY: a receive arm's live `Receiver`, held by the node.
                let end = unsafe { handles::payload::<ReceiverEnd>(payload, HandleKind::Receiver) };
                let kind = Kind::Receive {
                    chan: Arc::clone(&end.0),
                    lock: 0,
                    node: receives,
                };
                receives = receives.saturating_add(1);
                kind
            }
            SELECTOR_AFTER => {
                // SAFETY: as above.
                let (seconds, nanoseconds) = unsafe {
                    (
                        values::word_to_i64(payload),
                        values::word_to_i64(nanoseconds),
                    )
                };
                let span = i128::from(seconds)
                    .saturating_mul(NANOS_PER_SECOND)
                    .saturating_add(i128::from(nanoseconds));
                Kind::After(deadline_after(seconds, nanoseconds), span)
            }
            other => fatal(format_args!("a selector node has the tag {other}")),
        };
        arms.push(Arm { kind, apply });
        node = previous;
    }
}

/// The distinct channels of `arms`' receive arms in address order, the
/// order a select locks them in, with each receive arm's `lock` set to its
/// channel's index there.
fn channels_in_lock_order(arms: &mut [Arm]) -> Vec<Arc<Chan>> {
    let mut chans: Vec<Arc<Chan>> = arms
        .iter()
        .filter_map(|arm| match &arm.kind {
            Kind::Receive { chan, .. } => Some(Arc::clone(chan)),
            Kind::After(..) => None,
        })
        .collect();
    chans.sort_by_key(|chan| Arc::as_ptr(chan).addr());
    chans.dedup_by(|a, b| Arc::ptr_eq(a, b));
    for arm in arms {
        if let Kind::Receive { chan, lock, .. } = &mut arm.kind {
            let at = Arc::as_ptr(chan).addr();
            *lock = chans
                .binary_search_by_key(&at, |chan| Arc::as_ptr(chan).addr())
                .unwrap_or_else(|_| fatal(format_args!("a select lost one of its channels")));
        }
    }
    chans
}

/// The guard of the lock `lock` among a select's held channel locks.
fn held<'g>(guards: &'g mut [MutexGuard<'_, ChanInner>], lock: usize) -> &'g mut ChanInner {
    match guards.get_mut(lock) {
        Some(guard) => guard,
        None => fatal(format_args!("a select arm names a lock it does not hold")),
    }
}

/// The node of a receive arm.
fn node_of(nodes: &[Waiter], node: usize) -> &Waiter {
    nodes
        .get(node)
        .unwrap_or_else(|| fatal(format_args!("a select arm names a node it does not have")))
}

/// The arm `index` of a select.
fn arm_of(arms: &[Arm], index: usize) -> &Arm {
    arms.get(index)
        .unwrap_or_else(|| fatal(format_args!("a select chose an arm it does not have")))
}

/// The timer of a waiting select (D-673): when it fires first, it settles
/// the select's shared header for its arm and wakes the fiber.
#[derive(Debug)]
struct SelectTimer {
    /// The select's shared header.
    header: Arc<WaitHeader>,
    /// The index of the earliest timer arm.
    arm: usize,
}

impl TimerTarget for SelectTimer {
    fn fire(&self) {
        if self.header.try_settle_arm(WaitState::Claimed, self.arm) {
            // SAFETY: the timer won the select's shared header, which the
            // fiber published before it parks (`begin_park` first), so the
            // fiber is parking or parked on it and nothing else wakes it:
            // it is not retired. The header lives in the `Arc` this target
            // holds, so a timer that fires late touches no dead memory, and
            // its swap fails then.
            unsafe { wake(self.header.fiber(), false) };
        }
    }
}

/// `select`'s waiting kernel (D-673, D-695, module documentation): waits
/// until an arm of `selector` is ready, takes exactly one value from
/// exactly one ready arm into the fiber, and returns that arm's apply
/// closure, owned. The selector is borrowed (D-187); Cheby releases it
/// after the return, before it calls the closure (D-205, D-670).
///
/// # Safety
///
/// `ctx` is the running fiber's context and `selector` a live selector,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_selector_sys_wait(ctx: *mut ExecCtx, selector: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { wait(ctx, selector) })
}

/// The body of [`cheby_selector_sys_wait`].
///
/// # Safety
///
/// As for [`cheby_selector_sys_wait`].
#[expect(
    clippy::too_many_lines,
    reason = "the select's steps read in order, as the module documentation lists them"
)]
unsafe fn wait(ctx: *mut ExecCtx, selector: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // Step 1: the arms, each timer's clock started now.
    // SAFETY: a live selector, borrowed for the whole call.
    let mut arms = unsafe { gather(selector) };
    // Step 2: a uniformly random order (Fisher-Yates).
    let mut order: Vec<usize> = (0..arms.len()).collect();
    for last in (1..order.len()).rev() {
        let pick = me.conc.select.below(last.saturating_add(1));
        order.swap(last, pick);
    }
    // Step 3: lock the channels in address order and scan.
    let chans = channels_in_lock_order(&mut arms);
    let mut guards: Vec<MutexGuard<'_, ChanInner>> = Vec::with_capacity(chans.len());
    for chan in &chans {
        guards.push(chan.lock());
    }
    for &index in &order {
        let arm = arm_of(&arms, index);
        let ready = match &arm.kind {
            Kind::Receive { lock, .. } => match held(&mut guards, *lock).take_ready() {
                Some(ready) => Some(ready),
                None => continue,
            },
            Kind::After(Deadline::Elapsed, _) => None,
            Kind::After(..) => continue,
        };
        drop(guards);
        let chosen = match ready {
            Some(Ready::Value(value, woken)) => {
                wake_released(woken);
                Some(ok(value))
            }
            // A closed arm found without suspending reads no chain (D-769).
            Some(Ready::Closed) => Some(err_nil()),
            None => None,
        };
        if let Some(chosen) = chosen {
            me.conc.select.put(chosen);
        }
        // SAFETY: the closure is held by the borrowed selector.
        return unsafe { dup(arm.apply) };
    }
    // Step 4: wait.
    let Some(shared) = me.sched.runtime() else {
        drop(guards);
        needs_scheduler("selector::select")
    };
    begin_park(me);
    let header = Arc::new(WaitHeader::new(me));
    let nodes: Box<[Waiter]> = arms
        .iter()
        .enumerate()
        .filter(|(_, arm)| matches!(arm.kind, Kind::Receive { .. }))
        // SAFETY: the header is in an `Arc` this frame holds until every
        // node is unlinked and the box dropped.
        .map(|(index, _)| unsafe { WaitNode::sharing(me, &header, index, UnsafeCell::new(0)) })
        .collect();
    for arm in &arms {
        if let Kind::Receive { lock, node, .. } = &arm.kind {
            let link = NonNull::from(node_of(&nodes, *node));
            // SAFETY: the box's nodes never move once made, and stay until
            // each is unlinked under its channel's lock below, after the
            // fiber runs again; the fiber parks with that lock.
            unsafe { held(&mut guards, *lock).link_receiver(link) };
        }
    }
    // SAFETY: the header lives until after `unpublish` below.
    if unsafe { cancel::publish(me, &header) }.is_err() {
        // Cancelled before it waits: no waker has seen the nodes.
        for arm in &arms {
            if let Kind::Receive { lock, node, .. } = &arm.kind {
                let link = NonNull::from(node_of(&nodes, *node));
                // SAFETY: this fiber's own node, linked into that channel
                // under the lock it holds.
                unsafe { held(&mut guards, *lock).unlink_receiver(link) };
            }
        }
        drop(guards);
        cancel_park(me);
        raise_cancel(Some(ctx));
    }
    let timer = earliest_timer(&arms, &order).map(|(at, arm)| {
        let target = Arc::new(SelectTimer {
            header: Arc::clone(&header),
            arm,
        });
        shared.arm_timer(at, target)
    });
    let mut unlocks = Unlocks::new();
    for guard in guards {
        unlocks.push(guard);
    }
    park(me, AfterSwitch::ParkAll(unlocks));
    // Step 5: unpublish, unlink, cancel the timer, then decide.
    cancel::unpublish(me);
    for arm in &arms {
        if let Kind::Receive { chan, node, .. } = &arm.kind {
            let link = NonNull::from(node_of(&nodes, *node));
            // SAFETY: this fiber's own node, which a waker may have
            // unlinked already; the lock is released at the end of the
            // statement.
            unsafe { chan.lock().unlink_receiver(link) };
        }
    }
    if let Some(timer) = timer {
        let _: bool = TimerHandle::cancel(&timer);
    }
    let state = header.state();
    if state == WaitState::Cancelled {
        // The walk won the header: the select takes nothing (D-670).
        raise_cancel(Some(ctx));
    }
    let chosen = header.arm();
    let arm = arm_of(&arms, chosen);
    let value = match (state, &arm.kind) {
        (WaitState::Claimed, Kind::Receive { node, .. }) => {
            // SAFETY: the waker stored the value in this arm's node after
            // winning the shared header and before waking this fiber,
            // which orders the store before this read
            // (`WaitHeader::state`); the node is unlinked.
            Some(ok(unsafe { node_of(&nodes, *node).payload().get().read() }))
        }
        (WaitState::Claimed, Kind::After(..)) => None,
        (WaitState::Closed, Kind::Receive { .. }) => {
            // (D-769) The close may come from the unwinding of a fiber that
            // a cancellation of this fiber's enclosing scope reached first:
            // then this fiber is being cancelled too, and takes nothing
            // (D-670).
            if cancel::check_chain(me, me.conc.responsible_scope()).is_err() {
                #[cfg(test)]
                CLOSED_WAKES_CANCELLED.fetch_add(1, Ordering::Relaxed);
                raise_cancel(Some(ctx));
            }
            Some(err_nil())
        }
        (state, _) => fatal(format_args!(
            "a select woke in state {state:?} for arm {chosen}"
        )),
    };
    if let Some(value) = value {
        me.conc.select.put(value);
    }
    // SAFETY: the closure is held by the borrowed selector.
    unsafe { dup(arm.apply) }
}

/// The earliest deadline among `arms`' timer arms and the index of its
/// arm: the shortest span, and among equal spans the first in the random
/// `order`, a uniform choice among them (GAP(select-equal-timers)). `None`
/// when no timer arm has a deadline to wait for.
fn earliest_timer(arms: &[Arm], order: &[usize]) -> Option<(Instant, usize)> {
    let mut earliest: Option<(Instant, i128, usize)> = None;
    for &index in order {
        if let Kind::After(Deadline::At(at), span) = arm_of(arms, index).kind
            && earliest.is_none_or(|(_, best, _)| span < best)
        {
            earliest = Some((at, span, index));
        }
    }
    earliest.map(|(at, _, index)| (at, index))
}

/// `Ok(value)` as an owned guard, consuming the reference to `value`, also
/// when the allocation raises.
fn ok(value: Word) -> Owned {
    // SAFETY: the caller's reference, given up here.
    let value = unsafe { Owned::new(value) };
    let result = values::ok(value.word());
    let _ = value.into_word();
    // SAFETY: a fresh `Result`, owned.
    unsafe { Owned::new(result) }
}

/// The static `Err(Nil)` of a closed arm (D-180).
fn err_nil() -> Owned {
    // SAFETY: a static object: its references are not counted.
    unsafe { Owned::new(cheby_rt_static_err_nil.word()) }
}

/// The receive arm's take (D-695): the value `sys_wait` chose, owned, as
/// `channel::receive` would return it (D-180). `receiver` only fixes the
/// value's type.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `receiver` a live `Receiver`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_selector_take_selected(ctx: *mut ExecCtx, receiver: Word) -> Word {
    guarded(Some(ctx), || {
        let _ = receiver;
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        match me.conc.select.take() {
            Some(value) => value.into_word(),
            None => fatal(format_args!(
                "a selector's arm took a value that no select chose"
            )),
        }
    })
}
