//! Tests of the fiber and channel runtime (D-666, D-670, D-672, D-674,
//! D-677, D-692 to D-694, D-704, D-705, D-707): Rust-level fibers on a
//! `Runtime`, no JIT. Poisoning is on (the crate's tests default), and
//! every test that does not panic finds its domain's live count at zero
//! once its runtime has stopped.
//!
//! Spawned functions are test closures whose first capture is an
//! operation code that the test trampoline [`t0`] interprets, in place of
//! the generated `cb$tramp$0`.

use std::ptr;
use std::sync::Arc;

use super::base::{FiberHandle, Outcome};
use super::channel::{SenderEnd, cheby_channel_new, cheby_channel_receive, cheby_channel_send};
use super::join::cheby_fiber_sys_join;
use super::scope::{
    ScopeHandle, cheby_fiber_take_failure, cheby_rt_scope_close, cheby_rt_scope_open,
};
use super::spawn::cheby_fiber_sys_spawn;
use crate::abi::{PanicKind, UnwindBackend, Word};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{Callbacks, EntryConfig, EntryMode, EntryOutcome, run_entry};
use crate::generic::tests::{captures, closure};
use crate::handles::{self, HandleKind};
use crate::object::{load_rc, word_to_ptr};
use crate::panic::cheby_rt_panic;
use crate::rc;
use crate::sched::testing::{self, HANG};
use crate::sched::{AfterSwitch, park};
use crate::values;

/// The operation codes of test closures: their first capture.
mod op {
    /// `[op, tx, n]`: sends 0 to n - 1 on `tx`; returns `Nil`.
    pub(super) const SEND_RANGE: i64 = 0;
    /// `[op, value]`: returns `value`.
    pub(super) const RETURN: i64 = 1;
    /// `[op, tx, value, yields]`: yields `yields` times, then sends `value`
    /// on `tx`; returns `Nil`.
    pub(super) const YIELD_SEND: i64 = 2;
    /// `[op, fiber]`: joins `fiber` and returns its value.
    pub(super) const JOIN: i64 = 3;
    /// `[op, scope, tx, value]`: yields, then spawns a `YIELD_SEND` of
    /// `value` on `tx` into `scope`; returns `Nil`.
    pub(super) const SPAWN_SEND: i64 = 4;
    /// `[op, tx, value]`: sends `value` on `tx` and returns the result.
    pub(super) const SEND: i64 = 5;
    /// `[op, rx]`: receives on `rx` and returns the result.
    pub(super) const RECEIVE: i64 = 6;
    /// `[op, yields]`: yields `yields` times, then returns a new `String`.
    pub(super) const MAKE: i64 = 7;
    /// `[op]`: panics, which under the flag backend returns with the flag
    /// set, as generated code's trampoline would.
    pub(super) const PANIC: i64 = 8;
    /// `[op]`: makes a new `String`, then lets the fiber allocate nothing
    /// more (D-536), so that marking the result shared raises; returns the
    /// string.
    pub(super) const MAKE_EXHAUSTED: i64 = 9;
}

/// Lets the fiber `me` allocate at most `extra` more bytes, scratch
/// included (D-536).
fn limit_to(me: &ExecCtx, extra: u64) {
    let live = u64::try_from(me.domain().live_bytes()).unwrap_or(u64::MAX);
    let scratch = me.scratch().0.load(std::sync::atomic::Ordering::Relaxed);
    me.set_budget(live.saturating_add(scratch).saturating_add(extra));
}

fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    ptr::from_ref(me).cast_mut()
}

fn int(word: Word) -> i64 {
    // SAFETY: tagged integers only.
    unsafe { values::word_to_i64(word) }
}

/// Yields `times` times, so other fibers run.
fn yield_now(me: &ExecCtx, times: i64) {
    for _ in 0..times {
        park(me, AfterSwitch::Requeue);
    }
}

/// The trampoline of test closures, with `cb$tramp$0`'s ABI: the closure
/// is borrowed, the result owned.
unsafe extern "C" fn t0(ctx: *mut ExecCtx, function: u64) -> u64 {
    // SAFETY: the running fiber's control block.
    let me = unsafe { &*ctx };
    // SAFETY: a live test closure, borrowed.
    let words = unsafe { captures(function) };
    let word = |index: usize| words.get(index).copied().unwrap_or(0);
    // SAFETY: each operation reads the captures its code says it has.
    unsafe {
        match int(word(0)) {
            op::SEND_RANGE => {
                for n in 0..int(word(2)) {
                    let result = cheby_channel_send(ctx, word(1), values::int_word(n));
                    rc::drop(result);
                }
                values::nil()
            }
            op::RETURN => {
                rc::dup(word(1));
                word(1)
            }
            op::YIELD_SEND => {
                yield_now(me, int(word(3)));
                rc::drop(cheby_channel_send(ctx, word(1), word(2)));
                values::nil()
            }
            op::JOIN => cheby_fiber_sys_join(ctx, word(1)),
            op::SPAWN_SEND => {
                yield_now(me, 5);
                rc::dup(word(2));
                let inner = closure(&[
                    values::int_word(op::YIELD_SEND),
                    word(2),
                    word(3),
                    values::int_word(20),
                ]);
                let fiber = cheby_fiber_sys_spawn(ctx, word(1), inner);
                rc::drop(fiber);
                rc::drop(inner);
                values::nil()
            }
            op::SEND => cheby_channel_send(ctx, word(1), word(2)),
            op::RECEIVE => cheby_channel_receive(ctx, word(1)),
            op::MAKE => {
                yield_now(me, int(word(1)));
                values::string("made on the spawned fiber")
            }
            op::PANIC => {
                cheby_rt_panic(ctx, PanicKind::Explicit.code(), 0, 0);
                0
            }
            op::MAKE_EXHAUSTED => {
                let made = values::string("made on the spawned fiber");
                limit_to(me, 0);
                made
            }
            _ => values::nil(),
        }
    }
}

/// An entry configuration in test mode with the test trampoline and a
/// fresh domain.
fn config(domain: &Arc<Domain>) -> EntryConfig {
    EntryConfig {
        stack_size: 1 << 20,
        callbacks: Callbacks {
            t0: Some(t0),
            ..Callbacks::default()
        },
        domain: Arc::clone(domain),
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// Runs `body` as a root fiber on a runtime of `threads` workers and
/// returns its outcome and its domain, after the runtime has stopped.
fn outcome(
    threads: usize,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> (EntryOutcome, Arc<Domain>) {
    outcome_with(threads, UnwindBackend::Off, body)
}

/// [`outcome`] for code compiled for the unwinding backend `backend`.
fn outcome_with(
    threads: usize,
    backend: UnwindBackend,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> (EntryOutcome, Arc<Domain>) {
    let runtime = testing::runtime(threads);
    let domain = Domain::new();
    let config = EntryConfig {
        unwind: backend,
        ..config(&domain)
    };
    let outcome = testing::start_root_with(&runtime, &config, body)
        .recv_timeout(HANG)
        .expect("the root fiber hung");
    runtime.shutdown();
    (outcome, domain)
}

/// Runs `body` as a root fiber, requires that it returns, and checks that
/// nothing leaked.
fn run(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) -> u64 {
    let (outcome, domain) = outcome(threads, body);
    let EntryOutcome::Returned(word) = outcome else {
        panic!("the root fiber did not return: {outcome:?}");
    };
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    word
}

/// The two ends of a new channel, owned.
fn channel(me: &ExecCtx, capacity: i64) -> (Word, Word) {
    // SAFETY: the running fiber's context; the pair is owned and its
    // elements are taken over with a reference each.
    unsafe {
        let pair = cheby_channel_new(ctx_of(me), capacity);
        let parts = values::read_tuple(pair);
        for part in &parts {
            rc::dup(*part);
        }
        rc::drop(pair);
        (parts[0], parts[1])
    }
}

/// Takes a `Result` apart: the payload, owned, and the result dropped.
fn result(word: Word) -> Result<Word, Word> {
    // SAFETY: an owned, live `Result`.
    unsafe {
        let parts = values::read_result(word);
        let payload = match parts {
            Ok(value) | Err(value) => value,
        };
        rc::dup(payload);
        rc::drop(word);
        parts.map(|_| payload).map_err(|_| payload)
    }
}

/// `channel::send` from the root, with `value` borrowed.
fn send(me: &ExecCtx, tx: Word, value: Word) -> Result<Word, Word> {
    // SAFETY: the running fiber's context, a live `Sender` and value.
    result(unsafe { cheby_channel_send(ctx_of(me), tx, value) })
}

/// `channel::receive` from the root.
fn receive(me: &ExecCtx, rx: Word) -> Result<Word, Word> {
    // SAFETY: the running fiber's context and a live `Receiver`.
    result(unsafe { cheby_channel_receive(ctx_of(me), rx) })
}

/// Opens a scope.
fn open(me: &ExecCtx) -> Word {
    // SAFETY: the running fiber's context.
    unsafe { cheby_rt_scope_open(ctx_of(me), false, 0, 0) }
}

/// Closes a scope, consuming its handle.
fn close(me: &ExecCtx, scope: Word) -> bool {
    // SAFETY: the running fiber's context and the handle its open gave.
    unsafe { cheby_rt_scope_close(ctx_of(me), scope, false) }
}

/// Spawns a test closure with `captures` (an operation code first, each
/// capture given away) into `scope`; returns the `Fiber` handle, owned.
fn spawn(me: &ExecCtx, scope: Word, captures: &[Word]) -> Word {
    let function = closure(captures);
    // SAFETY: the running fiber's context, a live scope and closure.
    let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
    // SAFETY: the test's reference to the closure.
    unsafe { rc::drop(function) };
    fiber
}

fn dup(word: Word) -> Word {
    // SAFETY: a live value.
    unsafe { rc::dup(word) };
    word
}

fn drop_word(word: Word) {
    // SAFETY: an owned reference to a live value.
    unsafe { rc::drop(word) };
}

/// The text of a borrowed `String`.
fn text(word: Word) -> String {
    // SAFETY: a live `String`.
    unsafe { values::str_of(word) }.to_owned()
}

/// Waits, yielding, until the channel of `tx` has `senders` waiting
/// senders and `receivers` waiting receivers.
fn wait_for_waiters(me: &ExecCtx, tx: Word, senders: usize, receivers: usize) {
    // SAFETY: a live `Sender`, borrowed.
    let chan = Arc::clone(&unsafe { handles::payload::<SenderEnd>(tx, HandleKind::Sender) }.0);
    for _ in 0..1_000_000 {
        let (_, waiting_senders, waiting_receivers) = chan.counts();
        if (waiting_senders, waiting_receivers) == (senders, receivers) {
            return;
        }
        yield_now(me, 1);
    }
    panic!("the waiters never arrived");
}

/// 100,000 messages over a capacity-0 channel between two fibers.
fn messages_over_capacity_zero(threads: usize) {
    const COUNT: i64 = 100_000;
    let total = run(threads, |me| {
        let (tx, rx) = channel(me, 0);
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[
                values::int_word(op::SEND_RANGE),
                tx,
                values::int_word(COUNT),
            ],
        );
        drop_word(fiber);
        let mut expected = 0_i64;
        while let Ok(value) = receive(me, rx) {
            assert_eq!(int(value), expected, "out of order");
            expected = expected.wrapping_add(1);
        }
        drop_word(rx);
        assert!(close(me, scope));
        u64::try_from(expected).unwrap()
    });
    assert_eq!(total, 100_000);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_capacity_zero_channel_carries_100000_messages_on_one_worker() {
    messages_over_capacity_zero(1);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_capacity_zero_channel_carries_100000_messages_on_four_workers() {
    messages_over_capacity_zero(4);
}

/// D-672: values arrive in the order they were enqueued, through the
/// buffer and through a full buffer's waiting senders.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_buffered_channel_keeps_order() {
    for threads in [1, 2] {
        run(threads, |me| {
            let (tx, rx) = channel(me, 3);
            for n in 0..3 {
                assert!(send(me, tx, values::int_word(n)).is_ok());
            }
            let scope = open(me);
            // A fourth value waits in a sender until the buffer has room.
            let fiber = spawn(
                me,
                scope,
                &[
                    values::int_word(op::SEND_RANGE),
                    dup(tx),
                    values::int_word(10),
                ],
            );
            drop_word(fiber);
            drop_word(tx);
            let mut seen = Vec::new();
            while let Ok(value) = receive(me, rx) {
                seen.push(int(value));
            }
            assert_eq!(seen, [0, 1, 2, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
            drop_word(rx);
            assert!(close(me, scope));
            0
        });
    }
}

/// D-054, D-672: dropping the only `Receiver` wakes a sender blocked on a
/// capacity-0 channel, which gets its own value back.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn dropping_the_receiver_wakes_a_blocked_sender_with_its_value() {
    run(2, |me| {
        let (tx, rx) = channel(me, 0);
        let value = values::string("given back");
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[values::int_word(op::SEND), dup(tx), dup(value)],
        );
        wait_for_waiters(me, tx, 1, 0);
        drop_word(rx);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let sent = result(unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) });
        assert_eq!(sent, Err(value));
        drop_word(value);
        drop_word(value);
        drop_word(fiber);
        drop_word(tx);
        assert!(close(me, scope));
        0
    });
}

/// D-054: once the last `Sender` is gone, receivers drain the buffer and
/// then get `Err(Nil)`; a receiver that was waiting wakes to `Err(Nil)`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn dropping_the_last_sender_closes_the_channel_after_the_buffer() {
    run(2, |me| {
        let (tx, rx) = channel(me, 2);
        assert!(send(me, tx, values::int_word(1)).is_ok());
        assert!(send(me, tx, values::int_word(2)).is_ok());
        drop_word(tx);
        assert_eq!(receive(me, rx).map(int), Ok(1));
        assert_eq!(receive(me, rx).map(int), Ok(2));
        assert_eq!(receive(me, rx), Err(values::nil()));
        assert_eq!(receive(me, rx), Err(values::nil()));
        drop_word(rx);
        // A receiver waiting on an empty channel.
        let (tx, rx) = channel(me, 0);
        let scope = open(me);
        let fiber = spawn(me, scope, &[values::int_word(op::RECEIVE), dup(rx)]);
        wait_for_waiters(me, tx, 0, 1);
        drop_word(tx);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let received = result(unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) });
        assert_eq!(received, Err(values::nil()));
        drop_word(fiber);
        drop_word(rx);
        assert!(close(me, scope));
        0
    });
}

/// D-666, D-693: a scope's close waits until every member has ended,
/// including a member spawned by another member while the scope waits.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scope_waits_for_its_members() {
    for threads in [1, 4] {
        run(threads, |me| {
            let (tx, rx) = channel(me, 16);
            let scope = open(me);
            assert_eq!(me.conc.open_scope_count(), 1);
            for n in 0..4 {
                let fiber = spawn(
                    me,
                    scope,
                    &[
                        values::int_word(op::YIELD_SEND),
                        dup(tx),
                        values::int_word(n),
                        values::int_word(50),
                    ],
                );
                drop_word(fiber);
            }
            let fiber = spawn(
                me,
                scope,
                &[
                    values::int_word(op::SPAWN_SEND),
                    dup(scope),
                    dup(tx),
                    values::int_word(4),
                ],
            );
            drop_word(fiber);
            drop_word(tx);
            // SAFETY: a live `Scope` handle, borrowed.
            let state =
                Arc::clone(&unsafe { handles::payload::<ScopeHandle>(scope, HandleKind::Scope) }.0);
            assert!(close(me, scope));
            assert!(state.ended());
            assert_eq!(me.conc.open_scope_count(), 0);
            // Every member had sent before the close returned.
            let mut seen = Vec::new();
            while let Ok(value) = receive(me, rx) {
                seen.push(int(value));
            }
            seen.sort_unstable();
            assert_eq!(seen, [0, 1, 2, 3, 4]);
            drop_word(rx);
            0
        });
    }
}

/// D-668, D-707: `join` returns the same value every time, to any fiber.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn join_returns_the_same_value_twice() {
    run(2, |me| {
        let value = values::string("the answer");
        let scope = open(me);
        let fiber = spawn(me, scope, &[values::int_word(op::RETURN), dup(value)]);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let (first, second) = unsafe {
            (
                cheby_fiber_sys_join(ctx_of(me), fiber),
                cheby_fiber_sys_join(ctx_of(me), fiber),
            )
        };
        assert_eq!(first, value);
        assert_eq!(second, value);
        for word in [first, second, value, fiber] {
            drop_word(word);
        }
        assert!(close(me, scope));
        0
    });
}

/// D-670, D-707: a `Fiber` handle dropped before its fiber returns
/// releases the result as soon as the fiber returns; one dropped after
/// releases it then.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_dropped_fiber_handle_releases_the_result() {
    run(2, |me| {
        let domain = Arc::clone(me.domain());
        let before = domain.live_objects();
        // Dropped first: the fiber yields, so the handle goes before it
        // returns. The scope's close waits until the fiber has left it,
        // which it does last (D-694).
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[values::int_word(op::MAKE), values::int_word(20)],
        );
        // SAFETY: a live `Fiber` handle.
        let cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(fiber, HandleKind::Fiber) }.0);
        drop_word(fiber);
        assert!(close(me, scope));
        assert_eq!(cell.outcome(), Outcome::Released);
        assert_eq!(domain.live_objects(), before);
        // Dropped after: the result is kept until then.
        let scope = open(me);
        let kept = spawn(
            me,
            scope,
            &[values::int_word(op::MAKE), values::int_word(0)],
        );
        // SAFETY: a live `Fiber` handle.
        let kept_cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(kept, HandleKind::Fiber) }.0);
        assert!(close(me, scope));
        assert!(matches!(kept_cell.outcome(), Outcome::Done(_)));
        // The kept handle and its result.
        assert_eq!(domain.live_objects(), before + 2);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let value = unsafe { cheby_fiber_sys_join(ctx_of(me), kept) };
        assert_eq!(text(value), "made on the spawned fiber");
        drop_word(value);
        drop_word(kept);
        assert_eq!(kept_cell.outcome(), Outcome::Released);
        assert_eq!(domain.live_objects(), before);
        0
    });
}

/// D-674, D-705, D-707: a fiber's result is marked shared when it is
/// stored in its join cell, and another fiber reads it through `join`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_fibers_result_is_shared_and_read_from_another_fiber() {
    run(4, |me| {
        let scope = open(me);
        let maker = spawn(
            me,
            scope,
            &[values::int_word(op::MAKE), values::int_word(3)],
        );
        let joiner = spawn(me, scope, &[values::int_word(op::JOIN), dup(maker)]);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let value = unsafe { cheby_fiber_sys_join(ctx_of(me), joiner) };
        assert_eq!(text(value), "made on the spawned fiber");
        // SAFETY: a live object.
        assert!(unsafe { load_rc(word_to_ptr(value)) } < 0, "not shared");
        for word in [value, joiner, maker] {
            drop_word(word);
        }
        assert!(close(me, scope));
        0
    });
}

/// D-705: a sent value, and what it reaches, is marked shared.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_sent_value_is_shared() {
    run(1, |me| {
        let (tx, rx) = channel(me, 1);
        let inner = values::string("inner");
        let value = values::some(inner);
        // SAFETY: live objects.
        assert!(unsafe { load_rc(word_to_ptr(inner)) } > 0);
        assert!(send(me, tx, value).is_ok());
        // SAFETY: live objects.
        unsafe {
            assert!(load_rc(word_to_ptr(value)) < 0);
            assert!(load_rc(word_to_ptr(inner)) < 0);
        }
        assert_eq!(receive(me, rx), Ok(value));
        for word in [value, value, tx, rx] {
            drop_word(word);
        }
        0
    });
}

/// D-670: dropping the last `Receiver` releases the buffered values at
/// once, on the dropping fiber, although the `Sender` is alive; a later
/// send gets its value back.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_receivers_finalizer_releases_buffered_values_at_once() {
    run(1, |me| {
        let domain = Arc::clone(me.domain());
        let (tx, rx) = channel(me, 4);
        let before = domain.live_objects();
        for n in 0..3 {
            let value = values::string(&format!("buffered {n}"));
            assert!(send(me, tx, value).is_ok());
            drop_word(value);
        }
        assert_eq!(domain.live_objects(), before + 3);
        drop_word(rx);
        // The `Receiver` handle and the three values.
        assert_eq!(domain.live_objects(), before - 1);
        let value = values::string("refused");
        assert_eq!(send(me, tx, value), Err(value));
        drop_word(value);
        drop_word(value);
        drop_word(tx);
        // A buffered `Sender` of another channel: releasing it closes that
        // channel, from the free worklist (channel.md's "values still
        // buffered are released with the last Receiver").
        let (inner_tx, inner_rx) = channel(me, 0);
        let (tx, rx) = channel(me, 1);
        assert!(send(me, tx, inner_tx).is_ok());
        drop_word(inner_tx);
        drop_word(rx);
        assert_eq!(receive(me, inner_rx), Err(values::nil()));
        drop_word(inner_rx);
        drop_word(tx);
        0
    });
}

/// D-298, D-672: a negative capacity counts as 0, so a send waits for a
/// receiver.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_negative_capacity_counts_as_zero() {
    run(2, |me| {
        let (tx, rx) = channel(me, -5);
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[values::int_word(op::SEND), dup(tx), values::int_word(7)],
        );
        wait_for_waiters(me, tx, 1, 0);
        assert_eq!(receive(me, rx).map(int), Ok(7));
        // SAFETY: the running fiber's context and a live `Fiber`.
        let sent = result(unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) });
        assert_eq!(sent, Ok(values::nil()));
        for word in [fiber, tx, rx] {
            drop_word(word);
        }
        assert!(close(me, scope));
        0
    });
}

/// D-666, D-732: spawning into a scope that has ended panics, at an
/// unknown location.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn spawning_into_an_ended_scope_panics() {
    let (outcome, _) = outcome(1, |me| {
        let scope = open(me);
        let kept = dup(scope);
        assert!(close(me, scope));
        let fiber = spawn(
            me,
            kept,
            &[values::int_word(op::RETURN), values::int_word(1)],
        );
        u64::from(fiber == 0)
    });
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("no panic: {outcome:?}");
    };
    assert_eq!(report.message, "spawn into a scope that has ended");
    assert_eq!(report.location, "<unknown location>");
}

/// D-677: a stack region that cannot be mapped makes `spawn` panic with
/// `out of memory`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_stack_that_cannot_be_mapped_makes_spawn_panic() {
    let (outcome, _) = outcome(1, |me| {
        let scope = open(me);
        if let Some(runtime) = me.sched.runtime() {
            runtime
                .fail_stack
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let fiber = spawn(
            me,
            scope,
            &[values::int_word(op::RETURN), values::int_word(1)],
        );
        u64::from(fiber == 0)
    });
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("no panic: {outcome:?}");
    };
    assert_eq!(report.message, "out of memory");
}

/// The handle of a fiber that failed, owned. In the slice no fiber fails
/// without ending the process, so the cell is failed by hand.
fn failed_fiber(me: &ExecCtx) -> Word {
    let cell = Arc::new(super::base::JoinCell::default());
    assert_eq!(cell.finish(Outcome::Failed), None);
    // SAFETY: the running fiber's context.
    unsafe { handles::new_handle(ctx_of(me), HandleKind::Fiber, Box::new(FiberHandle(cell))) }
}

/// D-668: joining a fiber that failed panics with `joined fiber failed`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn joining_a_failed_fiber_panics() {
    let (outcome, _) = outcome(1, |me| {
        let fiber = failed_fiber(me);
        // SAFETY: the running fiber's context and a live `Fiber`.
        unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) }
    });
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("no panic: {outcome:?}");
    };
    assert_eq!(report.message, "joined fiber failed");
    assert_eq!(report.location, "<unknown location>");
}

/// Architecture §5.3 under the flag backend: a kernel that raises in the
/// body returns with the flag set; after `scope_run`'s pad clears it,
/// `scope_close(_, true)` takes U3's record as the scope's first cause,
/// returns false and leaves the cause for `take_failure`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn under_the_flag_backend_a_scope_takes_its_bodys_panic() {
    let (outcome, domain) = outcome_with(1, UnwindBackend::Flag, |me| {
        let scope = open(me);
        let fiber = failed_fiber(me);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let joined = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
        assert_eq!(joined, 0);
        assert_ne!(me.unwind.load(std::sync::atomic::Ordering::Acquire), 0);
        drop_word(fiber);
        // The pad's `clear_unwind`.
        crate::unwind::flag::clear(me);
        // SAFETY: the running fiber's context and its scope's handle.
        assert!(!unsafe { cheby_rt_scope_close(ctx_of(me), scope, true) });
        assert_eq!(me.conc.open_scope_count(), 0);
        // SAFETY: the running fiber's context.
        let failure = unsafe { cheby_fiber_take_failure(ctx_of(me)) };
        // SAFETY: a live tuple.
        let parts = unsafe { values::read_tuple(failure) };
        assert_eq!(int(parts[0]), 0);
        assert_eq!(text(parts[1]), "joined fiber failed");
        assert_eq!(text(parts[2]), "<unknown location>");
        drop_word(failure);
        7
    });
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0);
}

/// D-699, D-701 at the root runner: a root whose code unwound out of its
/// thunk reports `Panicked` from the record, whatever word it returned.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn under_the_flag_backend_a_root_that_unwound_panicked() {
    let (outcome, domain) = outcome_with(2, UnwindBackend::Flag, |me| {
        let fiber = failed_fiber(me);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let joined = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
        drop_word(fiber);
        joined
    });
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("no panic: {outcome:?}");
    };
    assert_eq!(report.message, "joined fiber failed");
    assert_eq!(domain.live_objects(), 0);
}

/// GAP(interim-panics) under the flag backend: `fiber_main` takes the
/// record of a spawned function that unwound, and a panic ends the process
/// with the interim's fatal error until U4. Runs itself in a child process.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn under_the_flag_backend_a_spawned_fibers_panic_is_fatal_for_now() {
    const CHILD: &str = "CHEBY_RT_TEST_FLAG_CHILD_PANIC";
    const NAME: &str =
        "fiber_ops::tests::under_the_flag_backend_a_spawned_fibers_panic_is_fatal_for_now";
    if std::env::var_os(CHILD).is_some() {
        let (outcome, _) = outcome_with(2, UnwindBackend::Flag, |me| {
            let scope = open(me);
            let fiber = spawn(me, scope, &[values::int_word(op::PANIC)]);
            drop_word(fiber);
            u64::from(close(me, scope))
        });
        panic!("the root ended: {outcome:?}");
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "fatal runtime error: a fiber panicked (unwinding arrives with package U4)\n"
        ),
        "{stderr}"
    );
}

/// Review finding MEM-1 under the flag backend: when marking a fiber's
/// result shared raises out of memory (here the memory limit, which takes
/// the same path), `fiber_main` catches it, so the raise does not unwind
/// into the fiber's `extern "C"` start function, which aborts; the fiber
/// ends with the panic, the interim's fatal error until U4. Runs itself in
/// a child process.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn under_the_flag_backend_a_raise_marking_a_fibers_result_is_its_panic() {
    const CHILD: &str = "CHEBY_RT_TEST_FLAG_CHILD_RESULT";
    const NAME: &str =
        "fiber_ops::tests::under_the_flag_backend_a_raise_marking_a_fibers_result_is_its_panic";
    if std::env::var_os(CHILD).is_some() {
        let (outcome, _) = outcome_with(1, UnwindBackend::Flag, |me| {
            let scope = open(me);
            let fiber = spawn(me, scope, &[values::int_word(op::MAKE_EXHAUSTED)]);
            drop_word(fiber);
            u64::from(close(me, scope))
        });
        panic!("the root ended: {outcome:?}");
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "fatal runtime error: a fiber panicked (unwinding arrives with package U4)\n"
        ),
        "{stderr}"
    );
}

/// Review finding MEM-1 under the flag backend: `spawn` marks its function
/// shared before it reserves a member slot, so when the marking raises out
/// of memory the scope does not wait for a fiber that never existed: it
/// closes, and the program goes on.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn under_the_flag_backend_a_spawn_whose_marking_raises_leaves_no_member() {
    let (outcome, domain) = outcome_with(1, UnwindBackend::Flag, |me| {
        let scope = open(me);
        let function = closure(&[values::int_word(op::RETURN), values::int_word(1)]);
        // Room for the `Fiber` handle, not for the walk's worklist.
        limit_to(me, 64);
        // SAFETY: the running fiber's context, a live scope and closure.
        let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
        me.set_budget(u64::MAX);
        assert_eq!(fiber, 0);
        assert_ne!(me.unwind.load(std::sync::atomic::Ordering::Acquire), 0);
        let report = match crate::unwind::record::take_unwound(me) {
            Some(crate::unwind::record::UnwindRecord::Panic(report)) => report,
            other => panic!("no panic: {other:?}"),
        };
        assert_eq!(report.message, crate::consteval::Limit::Memory.message());
        drop_word(function);
        u64::from(close(me, scope))
    });
    assert_eq!(outcome, EntryOutcome::Returned(1));
    assert_eq!(domain.live_objects(), 0);
}

/// Architecture §5.3: a scope that closes after its body unwound takes the
/// panic in U3's record as its first cause, and `take_failure` reports it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scope_without_a_cause_succeeds_and_take_failure_reports_one() {
    run(1, |me| {
        let scope = open(me);
        assert!(close(me, scope));
        me.conc.set_last_failure(super::Failure {
            kind: 0,
            message: "boom".to_owned(),
            location: "src/main.cheby:2:3".to_owned(),
        });
        // SAFETY: the running fiber's context.
        let failure = unsafe { cheby_fiber_take_failure(ctx_of(me)) };
        // SAFETY: a live tuple.
        let parts = unsafe { values::read_tuple(failure) };
        assert_eq!(int(parts[0]), 0);
        assert_eq!(text(parts[1]), "boom");
        assert_eq!(text(parts[2]), "src/main.cheby:2:3");
        drop_word(failure);
        0
    });
}

/// The no-runtime rule (D-692, architecture §5.1), in an isolated entry:
/// what completes without waiting works, and what would start a fiber or
/// wait ends the program with a fatal error naming the std function. Runs
/// itself in a child process per operation.
#[test]
fn without_a_runtime_only_what_does_not_wait_works() {
    const CHILD: &str = "CHEBY_RT_TEST_NO_RUNTIME";
    const NAME: &str = "fiber_ops::tests::without_a_runtime_only_what_does_not_wait_works";
    if let Some(operation) = std::env::var_os(CHILD) {
        let operation = operation.to_string_lossy().into_owned();
        let domain = Domain::new();
        let config = config(&domain);
        let entry: crate::entry::EntryFn = match operation.as_str() {
            "spawn" => isolated_spawn,
            "receive" => isolated_receive,
            _ => isolated_send,
        };
        // SAFETY: a test thunk with the entry ABI.
        let outcome = unsafe { run_entry(entry, &config) };
        panic!("the isolated entry ended: {outcome:?}");
    }
    for (operation, path) in [
        ("spawn", "fiber::spawn"),
        ("receive", "channel::receive"),
        ("send", "channel::send"),
    ] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, operation)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert!(stderr.contains("what does not wait works\n"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "fatal runtime error: {path} needs the fiber scheduler, which this entry does not run\n"
            )),
            "{stderr}"
        );
        assert_eq!(
            stderr.contains("note: fibers on Windows arrive in M6\n"),
            cfg!(windows),
            "{stderr}"
        );
    }
}

/// The part of the isolated entries that works without a runtime: a
/// buffered send and receive, a closed channel's receive, a scope whose
/// body spawns nothing. Returns an open channel of capacity 1 holding one
/// value, and a scope, owned.
fn isolated_prelude(me: &ExecCtx) -> (Word, Word, Word) {
    let (tx, rx) = channel(me, 1);
    assert!(send(me, tx, values::int_word(5)).is_ok());
    assert_eq!(receive(me, rx).map(int), Ok(5));
    let (closed_tx, closed_rx) = channel(me, 0);
    drop_word(closed_tx);
    assert_eq!(receive(me, closed_rx), Err(values::nil()));
    drop_word(closed_rx);
    let scope = open(me);
    let inner = open(me);
    assert!(close(me, inner));
    assert!(send(me, tx, values::int_word(6)).is_ok());
    eprintln!("what does not wait works");
    (tx, rx, scope)
}

unsafe extern "C" fn isolated_spawn(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    let (_, _, scope) = isolated_prelude(me);
    spawn(
        me,
        scope,
        &[values::int_word(op::RETURN), values::int_word(1)],
    )
}

unsafe extern "C" fn isolated_receive(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    let (_, rx, _) = isolated_prelude(me);
    assert_eq!(receive(me, rx).map(int), Ok(6));
    // Empty now, with its `Sender` alive: this waits.
    let _ = receive(me, rx);
    0
}

unsafe extern "C" fn isolated_send(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    let (tx, _, _) = isolated_prelude(me);
    // Full now: this waits.
    let _ = send(me, tx, values::int_word(7));
    0
}
