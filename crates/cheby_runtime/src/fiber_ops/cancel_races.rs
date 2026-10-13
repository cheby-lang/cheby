//! Races of cancellation's protocol (D-666, D-667, D-670, D-700, D-706):
//! the three hazards of survey-u5 §2.3, each with its test, members that
//! end and retire while a walk runs, and the walk on a stack overflow's
//! raise path.
//!
//! Each race is run many times at several thread counts, with D-687's
//! migration stress mode on at 2 and 4 workers. A race that the protocol
//! loses shows as a hang (a fiber parked that nothing wakes, so a scope's
//! close waits for ever: `HANG` fails the test), as a scope whose `Err`
//! is not the first panic's, or as a leak. Every outcome below is the same
//! whichever interleaving happens; only the interleaving varies, and no
//! result depends on timing.

use std::sync::atomic::Ordering;

use super::base::Outcome;
use super::channel::cheby_channel_receive;
use super::tests::{
    OVERFLOW_WHEN_STACK_USED, RAISE_BOUND, cell_of, channel, close, close_unwound, ctx_of,
    drop_word, dup, failure_of, op, open, outcome_stressed, spawn, string, yield_now,
};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;
use crate::values;

/// The settings of a race: a worker count and the migration stress mode.
const SETTINGS: [(usize, bool); 4] = [(1, false), (2, false), (2, true), (4, true)];

/// The message of the panic that cancels a race's scope.
const CAUSE: &str = "the first panic";

/// A member that yields `yields` times, then panics with [`CAUSE`].
fn panicker(me: &ExecCtx, scope: Word, yields: usize) -> Word {
    spawn(
        me,
        scope,
        &[
            values::int_word(op::PANIC_AFTER),
            values::int_word(i64::try_from(yields).unwrap()),
            string(CAUSE),
            string("src/race.cheby:1:1"),
        ],
    )
}

/// `channel::receive` from the root on a channel nobody sends to: it must
/// be cancelled, before it waits or while it waits. Returns whether it
/// was, with the flag set and the result the sentinel 0.
fn receive_is_cancelled(me: &ExecCtx, rx: Word) -> bool {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let result = unsafe { cheby_channel_receive(ctx_of(me), rx) };
    result == 0 && me.unwind.load(Ordering::Acquire) != 0
}

/// Closes the race's scope after the root's body was cancelled, and checks
/// that the scope's `Err` is the first panic.
fn close_cancelled(me: &ExecCtx, scope: Word) {
    assert!(!close_unwound(me, scope));
    let (kind, message, _) = failure_of(me);
    assert_eq!((kind, message.as_str()), (0, CAUSE));
}

/// Runs `round` at every setting, `rounds` times, requiring that the root
/// returns and nothing leaks.
fn race(rounds: usize, settings: &[(usize, bool)], round: fn(&ExecCtx, usize)) {
    for &(threads, stress) in settings {
        for index in 0..rounds {
            let (outcome, domain) = outcome_stressed(threads, stress, move |me| {
                round(me, index);
                0
            });
            assert_eq!(
                outcome,
                EntryOutcome::Returned(0),
                "{threads} workers, stress {stress}, round {index}"
            );
            assert_eq!(
                domain.live_objects(),
                0,
                "{threads} workers, stress {stress}, round {index}: objects leaked"
            );
        }
    }
}

/// Survey-u5 §2.3, hazard 1: fibers parking in `receive` race the walk of
/// their scope, which a sibling's panic starts after a varying number of
/// yields; the scope's owner parks too. Every waiter makes `PARKING`
/// visible before it publishes its node (`begin_park`), and publishes it
/// under its fiber lock, which refuses once a target is set: a waiter that
/// published before the walk is woken by it, one that publishes after
/// finds its target, so none waits for ever.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_races_park() {
    race(50, &SETTINGS, |me, round| {
        let (tx, rx) = channel(me, 0);
        let scope = open(me);
        let mut fibers: Vec<Word> = (0..4)
            .map(|_| spawn(me, scope, &[values::int_word(op::RECEIVE), dup(rx)]))
            .collect();
        fibers.push(panicker(me, scope, round % 8));
        assert!(receive_is_cancelled(me, rx));
        close_cancelled(me, scope);
        for fiber in fibers {
            assert_eq!(cell_of(fiber).outcome(), Outcome::Failed);
            drop_word(fiber);
        }
        drop_word(tx);
        drop_word(rx);
    });
}

/// Survey-u5 §2.3, hazard 2: spawns race the walk of their scope. A walk
/// between a spawn's reservation and its fill finds the slot without its
/// fiber; `fill` then reports the cancellation, read under the scope's
/// lock, and the spawner gives the fiber its target before it is queued.
/// A fiber that started uncancelled would wait in `receive` for ever. Every
/// fiber spawned after the walk never calls its function, releases it
/// (D-670) and joins as failed (D-666).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_races_spawn() {
    race(50, &SETTINGS, |me, round| {
        let (tx, rx) = channel(me, 0);
        let scope = open(me);
        let mut fibers = vec![panicker(me, scope, round % 8)];
        for index in 0..32 {
            fibers.push(spawn(me, scope, &[values::int_word(op::RECEIVE), dup(rx)]));
            if index % 4 == 0 {
                yield_now(me, 1);
            }
        }
        assert!(receive_is_cancelled(me, rx));
        close_cancelled(me, scope);
        for fiber in fibers {
            assert_eq!(cell_of(fiber).outcome(), Outcome::Failed);
            drop_word(fiber);
        }
        drop_word(tx);
        drop_word(rx);
    });
}

/// Survey-u5 §2.3, hazard 3: members open nested scopes while a sibling's
/// panic walks their scope, spawn a receiver into the nested scope and
/// close it. A nested scope linked after the walk visited its parent
/// inherits the parent's cancellation under the parent's lock, so the
/// receiver starts cancelled; one linked before is found by the walk. A
/// receiver that started uncancelled would wait for ever, and so would its
/// scope's close. Each member's close passes the cancellation through
/// (D-667).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_races_scope_open() {
    race(50, &SETTINGS, |me, round| {
        let (tx, rx) = channel(me, 0);
        let scope = open(me);
        let mut fibers: Vec<Word> = (0..8)
            .map(|index| {
                spawn(
                    me,
                    scope,
                    &[
                        values::int_word(op::NESTED_RECEIVE),
                        dup(rx),
                        values::int_word(index % 4),
                    ],
                )
            })
            .collect();
        fibers.push(panicker(me, scope, round % 8));
        assert!(receive_is_cancelled(me, rx));
        close_cancelled(me, scope);
        for fiber in fibers {
            assert_eq!(cell_of(fiber).outcome(), Outcome::Failed);
            drop_word(fiber);
        }
        drop_word(tx);
        drop_word(rx);
    });
}

/// D-706, plan §0 'Locks': members end, leave their scope and retire while
/// the walk of their scope runs. The walk takes each member's fiber lock
/// while it holds the scope's lock, where a member that has not left
/// cannot retire, and never acts on a member after releasing that lock:
/// a member that left meanwhile is not reached, and its retired control
/// block and reused slot are never touched. Running members are cancelled
/// at an entry check. 200 rounds at 2 and 4 workers with the migration
/// stress mode on (D-687, what `CHEBY_RT_STRESS_MIGRATE=1` sets), and at
/// 1 worker.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_races_member_exit() {
    race(200, &[(1, false), (2, true), (4, true)], |me, round| {
        let scope = open(me);
        let mut fibers: Vec<Word> = (0..16)
            .map(|index| {
                let yields = (index + round) % 5;
                spawn(
                    me,
                    scope,
                    &[
                        values::int_word(op::YIELD_RETURN),
                        values::int_word(i64::try_from(yields).unwrap()),
                    ],
                )
            })
            .collect();
        for _ in 0..2 {
            fibers.push(spawn(
                me,
                scope,
                &[values::int_word(op::CHECK_UNTIL_CANCELLED)],
            ));
        }
        fibers.push(panicker(me, scope, round % 6));
        // The body returns without an entry check: the close finds its
        // own scope as the target and reports the cause.
        assert!(!close(me, scope));
        let (_, message, _) = failure_of(me);
        assert_eq!(message, CAUSE);
        for fiber in fibers {
            let outcome = cell_of(fiber).outcome();
            assert!(
                matches!(outcome, Outcome::Failed | Outcome::Done(_)),
                "{outcome:?}"
            );
            drop_word(fiber);
        }
    });
}

/// D-700, D-706: a member's stack overflow records the scope's first cause
/// and runs the walk on the raise path, from the slow path's outermost
/// frame. The scope has 8 members (7 parked receivers and the one that
/// overflows) and a nested scope with 2 parked receivers and a joiner
/// parked on one of them; the owner parks too, inside the nested scope.
/// The walk reaches all of them, wins their nodes and wakes them, within
/// D-700's 32 KiB of stack, measured as the raise path's tests measure it.
/// The owner's unwinding passes through the nested scope's close and ends
/// at the outer scope, whose `Err` is `stack overflow`; nothing leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[cfg_attr(miri, ignore = "reads the stack below the current frame")]
fn cancel_overflow_walk_within_the_raise_bound() {
    for threads in [1, 2] {
        OVERFLOW_WHEN_STACK_USED.store(0, Ordering::SeqCst);
        let (outcome, domain) = outcome_stressed(threads, false, |me| {
            let (tx, rx) = channel(me, 0);
            let outer = open(me);
            let inner = open(me);
            let first = spawn(me, inner, &[values::int_word(op::RECEIVE), dup(rx)]);
            let second = spawn(me, inner, &[values::int_word(op::RECEIVE), dup(rx)]);
            let joiner = spawn(me, inner, &[values::int_word(op::JOIN), dup(first)]);
            let mut fibers: Vec<Word> = (0..7)
                .map(|_| spawn(me, outer, &[values::int_word(op::RECEIVE), dup(rx)]))
                .collect();
            fibers.push(spawn(
                me,
                outer,
                &[
                    values::int_word(op::OVERFLOW_WHEN),
                    dup(tx),
                    values::int_word(10),
                    dup(first),
                ],
            ));
            assert!(receive_is_cancelled(me, rx));
            // The nested scope is not the target: its close passes the
            // cancellation through, without returning `Err` (D-667).
            assert!(!close_unwound(me, inner));
            assert_ne!(me.unwind.load(Ordering::Acquire), 0);
            assert!(!close_unwound(me, outer));
            let (kind, message, _) = failure_of(me);
            assert_eq!((kind, message.as_str()), (0, "stack overflow"));
            // The cancelled joiner left no node in the cell (D-668).
            assert_eq!(cell_of(first).joiners(), 0);
            fibers.extend([first, second, joiner]);
            for fiber in fibers {
                assert_eq!(cell_of(fiber).outcome(), Outcome::Failed);
                drop_word(fiber);
            }
            drop_word(tx);
            drop_word(rx);
            0
        });
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(domain.live_objects(), 0);
        let used = OVERFLOW_WHEN_STACK_USED.load(Ordering::SeqCst);
        // For the report.
        eprintln!("raise path stack with the walk, {threads} workers: {used} bytes");
        assert!(used > 0, "nothing was measured");
        assert!(
            used <= RAISE_BOUND,
            "the raise path used {used} bytes of stack"
        );
    }
}
