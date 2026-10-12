//! Tests of the fiber and channel runtime (D-666, D-670, D-672, D-674,
//! D-677, D-692 to D-694, D-699, D-701, D-704, D-705, D-707): Rust-level
//! fibers on a `Runtime`, no JIT, with the return flag (D-733). Poisoning
//! is on (the crate's tests default), and every test that does not panic
//! finds its domain's live count at zero once its runtime has stopped.
//!
//! Spawned functions are test closures whose first capture is an
//! operation code that the test trampoline [`t0`] interprets, in place of
//! the generated `cb$tramp$0`. A test body that raises acts as generated
//! code does: it returns, or clears the flag where `scope_run`'s pad
//! would ([`close_unwound`]).

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::base::{FiberHandle, JoinCell, Outcome};
use super::channel::{SenderEnd, cheby_channel_new, cheby_channel_receive, cheby_channel_send};
use super::join::cheby_fiber_sys_join;
use super::scope::{
    ScopeHandle, cheby_fiber_take_failure, cheby_rt_scope_close, cheby_rt_scope_open,
};
use super::spawn::cheby_fiber_sys_spawn;
use crate::abi::{PanicKind, Word};
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
    /// `[op, message, location]`: panics with the `String`s `message` at
    /// `location`, as generated code's `panic as` does.
    pub(super) const PANIC_WITH: i64 = 10;
    /// `[op, rx, message, location]`: receives on `rx`, then panics as
    /// `PANIC_WITH` does.
    pub(super) const RECEIVE_THEN_PANIC: i64 = 11;
    /// `[op]`: overflows the stack, as a large function's entry check
    /// would, and stores the bytes of stack the raise path used in
    /// [`super::OVERFLOW_STACK_USED`].
    pub(super) const OVERFLOW: i64 = 12;
    /// `[op, scope]`: spawns a `RETURN` of 1 into `scope` and returns the
    /// `Fiber`.
    pub(super) const SPAWN_INTO: i64 = 13;
    /// `[op, message, location]`: opens a scope of its own, spawns a
    /// `PANIC_WITH` into it, closes it, and returns the message of the
    /// scope's failure, or `"no failure"`.
    pub(super) const NESTED_PANIC: i64 = 14;
}

/// The stack the raise path of the last `OVERFLOW` used, in bytes.
static OVERFLOW_STACK_USED: AtomicUsize = AtomicUsize::new(0);

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
            op::PANIC_WITH => {
                cheby_rt_panic(ctx, PanicKind::Explicit.code(), word(1), word(2));
                0
            }
            op::RECEIVE_THEN_PANIC => {
                rc::drop(cheby_channel_receive(ctx, word(1)));
                cheby_rt_panic(ctx, PanicKind::Explicit.code(), word(2), word(3));
                0
            }
            op::OVERFLOW => {
                let used = stack_used(|| {
                    // SAFETY: the running fiber's block; no location. The
                    // frame size makes the check fail whatever the depth.
                    crate::panic::cheby_rt_stack_slow_frame(ctx, 0, u64::MAX);
                });
                OVERFLOW_STACK_USED.store(used, Ordering::SeqCst);
                0
            }
            op::SPAWN_INTO => {
                let inner = closure(&[values::int_word(op::RETURN), values::int_word(1)]);
                let fiber = cheby_fiber_sys_spawn(ctx, word(1), inner);
                rc::drop(inner);
                fiber
            }
            op::NESTED_PANIC => {
                let scope = open(me);
                let fiber = spawn(
                    me,
                    scope,
                    &[values::int_word(op::PANIC_WITH), dup(word(1)), dup(word(2))],
                );
                drop_word(fiber);
                if close(me, scope) {
                    values::string("no failure")
                } else {
                    let (_, message, _) = failure_of(me);
                    values::string(&message)
                }
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
    outcome_with(threads, EntryMode::Test, body)
}

/// [`outcome`] for an entry of `mode`.
fn outcome_with(
    threads: usize,
    mode: EntryMode,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> (EntryOutcome, Arc<Domain>) {
    let runtime = testing::runtime(threads);
    let domain = Domain::new();
    let config = EntryConfig {
        mode,
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

/// Closes a scope whose body unwound, as `scope_run`'s pad does: clears
/// the flag, then closes with `unwound`, consuming the handle.
fn close_unwound(me: &ExecCtx, scope: Word) -> bool {
    assert_ne!(
        me.unwind.load(Ordering::Acquire),
        0,
        "the body is not unwinding"
    );
    crate::unwind::flag::clear(me);
    // SAFETY: the running fiber's context and the handle its open gave.
    unsafe { cheby_rt_scope_close(ctx_of(me), scope, true) }
}

/// `take_failure` after a scope failed: its kind, message and location.
fn failure_of(me: &ExecCtx) -> (i64, String, String) {
    // SAFETY: the running fiber's context, right after a failed close.
    let failure = unsafe { cheby_fiber_take_failure(ctx_of(me)) };
    // SAFETY: a live tuple.
    let parts = unsafe { values::read_tuple(failure) };
    let taken = (int(parts[0]), text(parts[1]), text(parts[2]));
    drop_word(failure);
    taken
}

/// The join cell of a borrowed `Fiber` handle.
fn cell_of(fiber: Word) -> Arc<JoinCell> {
    // SAFETY: a live `Fiber` handle, borrowed.
    Arc::clone(&unsafe { handles::payload::<FiberHandle>(fiber, HandleKind::Fiber) }.0)
}

/// Waits, yielding, until the fiber of `cell` has ended with `Failed`.
fn wait_until_failed(me: &ExecCtx, cell: &JoinCell) {
    for _ in 0..1_000_000 {
        if cell.outcome() == Outcome::Failed {
            return;
        }
        yield_now(me, 1);
    }
    panic!("the fiber never failed");
}

/// A fresh `String`, owned.
fn string(text: &str) -> Word {
    values::string(text)
}

/// The byte a painted stack holds where nothing has written.
const PAINT: u8 = 0xA5;
/// How much stack below the measuring frame is painted.
const PAINTED: usize = 128 << 10;
/// The most stack the raise path may use (D-700), as `unwind::tests`
/// bounds it.
const RAISE_BOUND: usize = 32 << 10;

/// Paints `PAINTED` bytes of the stack below the caller's frame and
/// returns their addresses (as `unwind::tests` measures the raise path).
#[inline(never)]
fn paint() -> std::ops::Range<usize> {
    #[expect(
        clippy::large_stack_arrays,
        reason = "the test paints the stack on purpose"
    )]
    let mut area = [PAINT; PAINTED];
    std::hint::black_box(&mut area);
    let start = area.as_mut_ptr().expose_provenance();
    start..start.saturating_add(PAINTED)
}

/// The bytes of stack that `raise` used: from the measuring frame down to
/// the lowest painted byte it overwrote.
#[inline(never)]
fn stack_used(raise: impl FnOnce()) -> usize {
    let top = crate::panic::stack_position();
    let area = paint();
    raise();
    let lowest = area
        .clone()
        .find(|&at| {
            let byte = std::ptr::with_exposed_provenance::<u8>(at);
            // SAFETY: the painted bytes lie in this thread's stack, below
            // the current frame, mapped and no longer used.
            let value = unsafe { byte.read_volatile() };
            value != PAINT
        })
        .unwrap_or(area.end);
    top.saturating_sub(lowest)
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

/// The handle of a fiber that failed, owned, with its cell failed by hand.
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

/// Architecture §5.3, D-666: a kernel that raises in the body returns with
/// the flag set, and the panic is recorded in the scope when it starts;
/// after `scope_run`'s pad clears the flag, `scope_close(_, true)` takes
/// the record, returns false and leaves the cause for `take_failure`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scope_takes_its_bodys_panic() {
    let (outcome, domain) = outcome(1, |me| {
        let scope = open(me);
        let fiber = failed_fiber(me);
        // SAFETY: the running fiber's context and a live `Fiber`.
        let joined = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
        assert_eq!(joined, 0);
        drop_word(fiber);
        assert!(!close_unwound(me, scope));
        assert_eq!(me.conc.open_scope_count(), 0);
        assert!(!crate::unwind::record::unwinding(me));
        assert_eq!(
            failure_of(me),
            (
                0,
                "joined fiber failed".to_owned(),
                "<unknown location>".to_owned()
            )
        );
        7
    });
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0);
}

/// D-699, D-701 at the root runner: a root whose code unwound out of its
/// thunk reports `Panicked` from the record, whatever word it returned. A
/// root outside every scope records its panic in no scope.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_root_that_unwound_panicked() {
    let (outcome, domain) = outcome(2, |me| {
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

/// D-666 (rewritten from the interim's "fatal for now" test): `fiber_main`
/// takes the record of a spawned function that unwound and drops it; the
/// panic was recorded in the fiber's scope when it started, so the scope
/// fails with it, the fiber's join cell is `Failed`, and the process goes
/// on.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_spawned_fibers_panic_is_its_scopes_cause() {
    let (outcome, domain) = outcome(2, |me| {
        let scope = open(me);
        let fiber = spawn(me, scope, &[values::int_word(op::PANIC)]);
        let cell = cell_of(fiber);
        drop_word(fiber);
        assert!(!close(me, scope));
        assert_eq!(cell.outcome(), Outcome::Failed);
        let (kind, message, location) = failure_of(me);
        assert_eq!(kind, 0);
        assert_eq!(message, PanicKind::Explicit.default_message());
        assert_eq!(location, "<unknown location>");
        3
    });
    assert_eq!(outcome, EntryOutcome::Returned(3));
    assert_eq!(domain.live_objects(), 0);
}

/// Review finding MEM-1 and D-666 (rewritten from the interim's test):
/// when marking a fiber's result shared raises out of memory (here the
/// memory limit, which takes the same path), `fiber_main` catches it, so
/// the raise does not unwind into the fiber's `extern "C"` start function;
/// the fiber has no scope open by then, so the panic is recorded in the
/// scope it was spawned into, which fails with it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_raise_marking_a_fibers_result_fails_its_scope() {
    let (outcome, domain) = outcome(1, |me| {
        let scope = open(me);
        let fiber = spawn(me, scope, &[values::int_word(op::MAKE_EXHAUSTED)]);
        let cell = cell_of(fiber);
        drop_word(fiber);
        assert!(!close(me, scope));
        assert_eq!(cell.outcome(), Outcome::Failed);
        let (_, message, _) = failure_of(me);
        assert_eq!(message, crate::consteval::Limit::Memory.message());
        4
    });
    assert_eq!(outcome, EntryOutcome::Returned(4));
    assert_eq!(domain.live_objects(), 0);
}

/// Review finding MEM-1: `spawn` marks its function shared before it
/// reserves a member slot, so when the marking raises out of memory the
/// scope does not wait for a fiber that never existed: it closes, and the
/// program goes on. The raise is the body's panic, recorded in the scope
/// when it started (D-666), so the scope fails with it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_spawn_whose_marking_raises_leaves_no_member() {
    let (outcome, domain) = outcome(1, |me| {
        let scope = open(me);
        let function = closure(&[values::int_word(op::RETURN), values::int_word(1)]);
        // Room for the `Fiber` handle, not for the walk's worklist.
        limit_to(me, 64);
        // SAFETY: the running fiber's context, a live scope and closure.
        let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
        me.set_budget(u64::MAX);
        assert_eq!(fiber, 0);
        drop_word(function);
        assert!(!close_unwound(me, scope));
        let (_, message, _) = failure_of(me);
        assert_eq!(message, crate::consteval::Limit::Memory.message());
        1
    });
    assert_eq!(outcome, EntryOutcome::Returned(1));
    assert_eq!(domain.live_objects(), 0);
}

/// The child side of [`a_members_panic_is_the_scopes_err_and_is_not_printed`]
/// in an entry of mode `Run`, which prints the panics that reach its catch
/// point: does nothing unless `CHEBY_RT_TEST_MEMBER_PANIC` is set. Prints
/// what the scope reported to standard output.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn member_panic_child() {
    if std::env::var_os("CHEBY_RT_TEST_MEMBER_PANIC").is_none() {
        return;
    }
    let (outcome, domain) = outcome_with(2, EntryMode::Run, |me| {
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[
                values::int_word(op::PANIC_WITH),
                string("a member failed"),
                string("src/member.cheby:3:5"),
            ],
        );
        let cell = cell_of(fiber);
        drop_word(fiber);
        assert!(!close(me, scope));
        assert_eq!(cell.outcome(), Outcome::Failed);
        let (kind, message, location) = failure_of(me);
        println!("scope failure: {kind} {message} at {location}");
        5
    });
    assert_eq!(outcome, EntryOutcome::Returned(5));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    println!("live objects: 0");
}

/// D-666, D-699, D-701: a member's panic is its scope's `Err`, with its
/// message and location; nothing is printed, even in an entry that prints
/// the panics reaching its catch point; the member's join cell is `Failed`;
/// nothing leaks. Runs [`member_panic_child`] in a child process to see
/// its standard error.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn a_members_panic_is_the_scopes_err_and_is_not_printed() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fiber_ops::tests::member_panic_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CHEBY_RT_TEST_MEMBER_PANIC", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("scope failure: 0 a member failed at src/member.cheby:3:5\n"),
        "{stdout}"
    );
    assert!(stdout.contains("live objects: 0\n"), "{stdout}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("a member failed"), "{stderr}");
    assert!(!stderr.contains("fatal runtime error"), "{stderr}");
}

/// D-666: the first panic by start is the cause, not the first to end. A
/// member panics and ends while the body runs, then the body panics: the
/// member's panic is the scope's `Err`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_first_panic_by_start_wins() {
    for threads in [1, 2] {
        let (outcome, domain) = outcome(threads, |me| {
            let scope = open(me);
            let fiber = spawn(
                me,
                scope,
                &[
                    values::int_word(op::PANIC_WITH),
                    string("the member's"),
                    string("src/member.cheby:1:1"),
                ],
            );
            let cell = cell_of(fiber);
            drop_word(fiber);
            wait_until_failed(me, &cell);
            let (message, location) = (string("the body's"), string("src/body.cheby:2:2"));
            // SAFETY: the running fiber's context and live `String`s.
            unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), message, location) };
            drop_word(message);
            drop_word(location);
            assert!(!close_unwound(me, scope));
            let (_, message, location) = failure_of(me);
            assert_eq!(
                (message.as_str(), location.as_str()),
                ("the member's", "src/member.cheby:1:1")
            );
            0
        });
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(domain.live_objects(), 0);
    }
}

/// D-666, D-699, spec §10.3 item 3: a root's body inside `scope` records
/// its panic in that scope when it starts, like a member. The body panics
/// while a member still runs; its unwinding releases the channel's only
/// `Sender`, which ends the member's wait, and the member panics after the
/// body's panic started: the scope's `Err` carries the body's message and
/// location, although the body's close ends last.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn body_panic_first_wins_over_a_later_member_panic() {
    for threads in [1, 2] {
        let (outcome, domain) = outcome(threads, |me| {
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let fiber = spawn(
                me,
                scope,
                &[
                    values::int_word(op::RECEIVE_THEN_PANIC),
                    rx,
                    string("the member's"),
                    string("src/member.cheby:1:1"),
                ],
            );
            let cell = cell_of(fiber);
            drop_word(fiber);
            let (message, location) = (string("the body's"), string("src/body.cheby:2:2"));
            // SAFETY: the running fiber's context and live `String`s.
            unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), message, location) };
            drop_word(message);
            drop_word(location);
            // The body's cleanup pads: the `Sender` goes, which wakes the
            // member with `Err(Nil)`.
            drop_word(tx);
            assert!(!close_unwound(me, scope));
            assert_eq!(cell.outcome(), Outcome::Failed);
            let (_, message, location) = failure_of(me);
            assert_eq!(
                (message.as_str(), location.as_str()),
                ("the body's", "src/body.cheby:2:2")
            );
            0
        });
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(domain.live_objects(), 0);
    }
}

/// D-666, D-700: a member that overflows its stack records the overflow in
/// its scope at the start of the raise, and the raise path, the scope's
/// lock and the copy of the report included, stays within the 32 KiB that
/// `unwind::tests` bounds it to. The scope's `Err` is `stack overflow`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[cfg_attr(miri, ignore = "reads the stack below the current frame")]
fn overflow_in_a_member_is_recorded_within_the_raise_bound() {
    let (outcome, domain) = outcome(2, |me| {
        let scope = open(me);
        let fiber = spawn(me, scope, &[values::int_word(op::OVERFLOW)]);
        let cell = cell_of(fiber);
        drop_word(fiber);
        assert!(!close(me, scope));
        assert_eq!(cell.outcome(), Outcome::Failed);
        let (kind, message, location) = failure_of(me);
        assert_eq!((kind, message.as_str()), (0, "stack overflow"));
        assert_eq!(location, "<unknown location>");
        0
    });
    assert_eq!(outcome, EntryOutcome::Returned(0));
    assert_eq!(domain.live_objects(), 0);
    let used = OVERFLOW_STACK_USED.load(Ordering::SeqCst);
    // For the report.
    eprintln!("raise path stack in a member: stack overflow {used} bytes");
    assert!(used > 0, "nothing was measured");
    assert!(
        used <= RAISE_BOUND,
        "the raise path used {used} bytes of stack"
    );
}

/// D-666, plan §0 'Locks': a member that spawns into an ended scope raises
/// after the ended scope's lock is released, and its panic is recorded in
/// its own scope, which has other members, without holding another lock:
/// no deadlock, and the scope fails with `spawn into a scope that has
/// ended`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn spawn_into_an_ended_scope_raises_while_the_spawners_scope_has_members() {
    for threads in [1, 2] {
        let (outcome, domain) = outcome(threads, |me| {
            let ended = open(me);
            let kept = dup(ended);
            assert!(close(me, ended));
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let waiting = spawn(me, scope, &[values::int_word(op::RECEIVE), rx]);
            let spawner = spawn(me, scope, &[values::int_word(op::SPAWN_INTO), kept]);
            let cell = cell_of(spawner);
            wait_until_failed(me, &cell);
            // The scope still waits for `waiting`; let it return.
            assert_eq!(send(me, tx, values::int_word(9)), Ok(values::nil()));
            assert!(!close(me, scope));
            let (_, message, location) = failure_of(me);
            assert_eq!(message, "spawn into a scope that has ended");
            assert_eq!(location, "<unknown location>");
            for word in [waiting, spawner, tx] {
                drop_word(word);
            }
            0
        });
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(domain.live_objects(), 0);
    }
}

/// D-666, spec §10.3: a scope nested in a member keeps the panic of its own
/// member to itself: the inner scope fails, the member that opened it
/// returns, and the outer scope succeeds.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_nested_scope_keeps_a_members_panic_to_itself() {
    for threads in [1, 2] {
        let (outcome, domain) = outcome(threads, |me| {
            let scope = open(me);
            let fiber = spawn(
                me,
                scope,
                &[
                    values::int_word(op::NESTED_PANIC),
                    string("inner"),
                    string("src/inner.cheby:4:4"),
                ],
            );
            // SAFETY: the running fiber's context and a live `Fiber`.
            let value = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
            assert_eq!(text(value), "inner");
            drop_word(value);
            drop_word(fiber);
            assert!(close(me, scope));
            0
        });
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(domain.live_objects(), 0);
    }
}

/// D-699, ADR-0029: a panic that starts while the fiber is already
/// unwinding is dropped and recorded nowhere, not even in a scope that has
/// no cause yet.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_panic_while_unwinding_is_recorded_nowhere() {
    let (outcome, domain) = outcome(1, |me| {
        let outer = open(me);
        crate::unwind::record::start_panic(
            me,
            crate::panic::PanicReport::new(PanicKind::Explicit, Some("first"), "a:1:1"),
        );
        let inner = open(me);
        crate::unwind::record::start_panic(
            me,
            crate::panic::PanicReport::new(PanicKind::Explicit, Some("second"), "b:2:2"),
        );
        // The inner scope has no cause: it succeeds.
        assert!(close(me, inner));
        match crate::unwind::record::take_unwound(me) {
            Some(crate::unwind::record::UnwindRecord::Panic(report)) => {
                assert_eq!(report.message, "first");
            }
            other => panic!("no panic: {other:?}"),
        }
        assert!(!close(me, outer));
        let (_, message, _) = failure_of(me);
        assert_eq!(message, "first");
        0
    });
    assert_eq!(outcome, EntryOutcome::Returned(0));
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
