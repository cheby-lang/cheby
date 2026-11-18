//! Tests of the operations that never wait (D-1099), `channel::sys_send_now`,
//! `channel::sys_receive_now` and `selector::sys_select_now`, and of
//! `fiber::sys_yield_now` (D-1102): the kernels driven from Rust fibers on
//! a runtime, with poisoning on, and in an entry that runs no scheduler
//! (D-692). Every test finds its domain's live count at zero at its end.

use super::channel::{
    CLOSED, FULL, SENT, cheby_channel_sys_receive_now, cheby_channel_sys_send_now,
};
use super::join::cheby_fiber_sys_join;
use super::select::{
    cheby_selector_sys_add_after, cheby_selector_sys_add_receive, cheby_selector_sys_new,
    cheby_selector_sys_select_now, cheby_selector_take_selected,
};
use super::tests::{
    channel, close, config, ctx_of, drop_word, dup, int, op, open, result, run, send, spawn,
    string, text, wait_for_waiters,
};
use super::yielding::cheby_fiber_sys_yield_now;
use crate::abi::Word;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryOutcome, run_entry};
use crate::values;

/// `sys_send_now` from the running fiber, with `value` borrowed.
fn send_now(me: &ExecCtx, tx: Word, value: Word) -> i64 {
    // SAFETY: the running fiber's context, a live `Sender` and value.
    unsafe { cheby_channel_sys_send_now(ctx_of(me), tx, value) }
}

/// `sys_receive_now` from the running fiber, its `Option` and `Result`
/// taken apart: the payload, owned.
fn receive_now(me: &ExecCtx, rx: Word) -> Option<Result<Word, Word>> {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let word = unsafe { cheby_channel_sys_receive_now(ctx_of(me), rx) };
    // SAFETY: an owned, live `Option`.
    let inner = unsafe { values::read_option(word) }.map(dup);
    drop_word(word);
    inner.map(result)
}

/// What `select_now` gave: `None`, or the chosen apply word, owned, and
/// the value its receive arm took (`take_selected`), owned, if it is a
/// receive arm.
type Chosen = Option<(Word, Option<Result<Word, Word>>)>;

/// `select_now` as std writes it, without calling the closure.
fn select_now(me: &ExecCtx, selector: Word) -> Chosen {
    // SAFETY: the running fiber's context and a live selector, borrowed.
    let word = unsafe { cheby_selector_sys_select_now(ctx_of(me), selector) };
    // SAFETY: an owned, live `Option`.
    let apply = unsafe { values::read_option(word) }.map(dup);
    drop_word(word);
    let apply = apply?;
    let taken = me.conc.select.holds_value().then(|| {
        // SAFETY: the running fiber's context; the receiver only fixes the
        // type, and `Nil` stands for it.
        result(unsafe { cheby_selector_take_selected(ctx_of(me), values::nil()) })
    });
    Some((apply, taken))
}

/// An arm's stand-in apply closure: the integer `n`.
fn arm(n: i64) -> Word {
    values::int_word(n)
}

/// `selector` (consumed) with a receive arm on the borrowed `rx` whose
/// apply closure stands in as `arm(n)`.
fn with_receive(me: &ExecCtx, selector: Word, rx: Word, n: i64) -> Word {
    // SAFETY: the running fiber's context and live values, borrowed.
    let next = unsafe { cheby_selector_sys_add_receive(ctx_of(me), selector, rx, arm(n)) };
    drop_word(selector);
    next
}

/// `selector` (consumed) with a timer arm of `seconds` whose apply closure
/// stands in as `arm(n)`.
fn with_after(me: &ExecCtx, selector: Word, seconds: i64, n: i64) -> Word {
    // SAFETY: the running fiber's context and live values, borrowed.
    let next = unsafe { cheby_selector_sys_add_after(ctx_of(me), selector, seconds, 0, arm(n)) };
    drop_word(selector);
    next
}

/// `selector::new()`, owned.
fn empty(me: &ExecCtx) -> Word {
    // SAFETY: the running fiber's context.
    unsafe { cheby_selector_sys_new(ctx_of(me)) }
}

/// D-1099: on a buffer, `send_now` enqueues while there is room and is
/// `FULL` after; `receive_now` takes the oldest value, is `None` on an
/// empty channel whose `Sender` lives, and `Err(Nil)` once it is gone.
/// Values given back are the caller's, and nothing leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn send_now_and_receive_now_on_a_buffer() {
    run(1, |me| {
        let (tx, rx) = channel(me, 1);
        let first = string("first");
        let second = string("second");
        assert_eq!(send_now(me, tx, first), SENT);
        assert_eq!(send_now(me, tx, second), FULL);
        // The value taken is `first` with a reference of its own.
        assert_eq!(receive_now(me, rx), Some(Ok(first)));
        drop_word(first);
        drop_word(first);
        assert_eq!(receive_now(me, rx), None);
        assert_eq!(send_now(me, tx, second), SENT);
        drop_word(tx);
        assert_eq!(
            receive_now(me, rx).map(|r| r.map(text)),
            Some(Ok("second".to_owned()))
        );
        drop_word(second);
        drop_word(second);
        assert_eq!(receive_now(me, rx), Some(Err(values::nil())));
        drop_word(rx);
        0
    });
}

/// D-1099: `send_now` with every `Receiver` gone is `CLOSED`, checked
/// before the room in the buffer, and keeps no reference to the value.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn send_now_without_receivers_is_closed() {
    run(1, |me| {
        let (tx, rx) = channel(me, 4);
        drop_word(rx);
        let value = string("kept");
        assert_eq!(send_now(me, tx, value), CLOSED);
        assert_eq!(text(value), "kept");
        drop_word(value);
        drop_word(tx);
        0
    });
}

/// D-1099, D-206: on capacity 0, `send_now` is `FULL` while nobody waits,
/// and hands its value to a receiver that waits; `receive_now` takes the
/// value of a sender that waits, which then returns `Ok(Nil)`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_now_operations_meet_waiters_on_capacity_zero() {
    for threads in [1, 2] {
        run(threads, |me| {
            let (tx, rx) = channel(me, 0);
            let value = string("handed over");
            assert_eq!(send_now(me, tx, value), FULL);
            assert_eq!(receive_now(me, rx), None);
            let scope = open(me);
            let waiting_receiver = spawn(me, scope, &[values::int_word(op::RECEIVE), dup(rx)]);
            wait_for_waiters(me, tx, 0, 1);
            assert_eq!(send_now(me, tx, value), SENT);
            // SAFETY: the running fiber's context and a live `Fiber`.
            let received = result(unsafe { cheby_fiber_sys_join(ctx_of(me), waiting_receiver) });
            assert_eq!(received, Ok(value));
            drop_word(value);
            drop_word(waiting_receiver);
            let waiting_sender = spawn(
                me,
                scope,
                &[values::int_word(op::SEND), dup(tx), values::int_word(9)],
            );
            wait_for_waiters(me, tx, 1, 0);
            assert_eq!(receive_now(me, rx).map(|r| r.map(int)), Some(Ok(9)));
            // SAFETY: the running fiber's context and a live `Fiber`.
            let sent = result(unsafe { cheby_fiber_sys_join(ctx_of(me), waiting_sender) });
            assert_eq!(sent, Ok(values::nil()));
            drop_word(waiting_sender);
            drop_word(value);
            drop_word(tx);
            drop_word(rx);
            assert!(close(me, scope));
            0
        });
    }
}

/// D-1099, D-673: `select_now` is `None` while no arm is ready, takes the
/// value of a ready receive arm, takes a closed arm's `Err(Nil)`, and
/// counts a timer arm as ready only when its span is zero or less.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_now_takes_only_what_is_ready() {
    run(1, |me| {
        let (tx, rx) = channel(me, 1);
        let waiting = with_receive(me, empty(me), rx, 1);
        let waiting = with_after(me, waiting, 10, 2);
        assert_eq!(select_now(me, waiting), None);
        assert!(send(me, tx, values::int_word(7)).is_ok());
        let chosen =
            select_now(me, waiting).map(|(apply, taken)| (int(apply), taken.map(|r| r.map(int))));
        assert_eq!(chosen, Some((1, Some(Ok(7)))));
        assert_eq!(select_now(me, waiting), None);
        drop_word(tx);
        assert_eq!(
            select_now(me, waiting),
            Some((arm(1), Some(Err(values::nil()))))
        );
        drop_word(waiting);
        let now = with_after(me, empty(me), 0, 3);
        assert_eq!(select_now(me, now), Some((arm(3), None)));
        drop_word(now);
        let none = empty(me);
        assert_eq!(select_now(me, none), None);
        drop_word(none);
        drop_word(rx);
        0
    });
}

/// D-1102: on one worker, a loop that only polls lets another fiber run
/// when it calls `yield_now`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn yield_now_lets_another_fiber_run_on_one_worker() {
    run(1, |me| {
        let (tx, rx) = channel(me, 1);
        let scope = open(me);
        let waiting_sender = spawn(
            me,
            scope,
            &[values::int_word(op::SEND), dup(tx), values::int_word(4)],
        );
        let mut polls = 0_u32;
        let received = loop {
            if let Some(taken) = receive_now(me, rx) {
                break taken;
            }
            polls = polls.saturating_add(1);
            // SAFETY: the running fiber's context.
            unsafe { cheby_fiber_sys_yield_now(ctx_of(me)) };
        };
        assert_eq!(received.map(int), Ok(4));
        assert!(polls >= 1, "the sender ran before the first poll");
        drop_word(waiting_sender);
        drop_word(tx);
        drop_word(rx);
        assert!(close(me, scope));
        0
    });
}

/// D-692, D-1099, D-1102: the operations that never wait work in an entry
/// that runs no scheduler; `yield_now` has no fiber to let run there.
#[test]
fn the_now_operations_work_without_a_runtime() {
    let domain = Domain::new();
    let config = config(&domain);
    // SAFETY: a test thunk with the entry ABI.
    let outcome = unsafe { run_entry(isolated_now, &config) };
    assert!(
        matches!(outcome, Ok(EntryOutcome::Returned(_))),
        "{outcome:?}"
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

unsafe extern "C" fn isolated_now(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    let (tx, rx) = channel(me, 1);
    assert_eq!(send_now(me, tx, values::int_word(5)), SENT);
    assert_eq!(send_now(me, tx, values::int_word(6)), FULL);
    let waiting = with_receive(me, empty(me), rx, 1);
    let chosen =
        select_now(me, waiting).map(|(apply, taken)| (int(apply), taken.map(|r| r.map(int))));
    assert_eq!(chosen, Some((1, Some(Ok(5)))));
    drop_word(waiting);
    // SAFETY: the entry's context.
    unsafe { cheby_fiber_sys_yield_now(ctx) };
    assert_eq!(receive_now(me, rx), None);
    drop_word(tx);
    drop_word(rx);
    0
}
