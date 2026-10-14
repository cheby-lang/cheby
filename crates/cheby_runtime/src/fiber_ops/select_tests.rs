//! Selectors' runtime tests (D-041, D-180, D-205, D-206, D-670, D-673,
//! D-692, D-695, D-704, D-769; architecture §7.2, §7.3 and §7.5): the
//! kernels of `std::selector` driven from Rust fibers on a real runtime,
//! with poisoning on, through `fiber_ops::cancel_tests`' jobs, notes and
//! gates.
//!
//! The apply closure of an arm is a stand-in here: a tagged integer that
//! names the arm (`sys_wait` returns it dup'd, as it returns a closure), or
//! a test closure where a test watches what it holds. Every test orders
//! its events with channels, gates and the runtime's own state (a waiter
//! linked into a channel, a published wait node), never with sleeps; a
//! test about time asserts only that a timer never fires early (D-232,
//! D-722). Every test ends through a channel, a timer or a cancellation,
//! and finds no node left in any channel's queues and its domain's live
//! count at zero.
//!
//! The first eight tests mirror the spec tests of `docs/stdlib/selector.md`
//! in order; the spec's "a selector without arms waits until it is
//! cancelled" uses `fiber::timeout`, which this file stands in for with the
//! cancellation walk (`select_without_arms_waits_until_it_is_cancelled`).

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::cancel::{self, hooks};
use super::cancel_tests::{
    CANCEL, Gate, NONE, Notes, Step, THREADS, close_unwound, drop_shape, flag_of, outcome_on,
    panic_failure, panic_with, receive_step, run_jobs, send_step, shown, spawn_job, start_job,
    state_of, step_shape,
};
use super::channel::{Chan, ReceiverEnd};
use super::select::{
    CLOSED_WAKES_CANCELLED, cheby_selector_sys_add_after, cheby_selector_sys_add_receive,
    cheby_selector_sys_new, cheby_selector_sys_wait, cheby_selector_take_selected,
};
use super::tests::{
    cell_of, channel, close, config, ctx_of, drop_word, dup, failure_of, int, open, result, string,
    yield_now,
};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryOutcome, run_entry};
use crate::generic::tests::closure;
use crate::handles::{self, HandleKind};
use crate::object::load_rc;
use crate::object::word_to_ptr;
use crate::sched::testing;
use crate::values;

// ---------------------------------------------------------------------------
// Selectors from Rust.

/// `selector::new()`, owned.
fn empty(me: &ExecCtx) -> Word {
    // SAFETY: the running fiber's context.
    unsafe { cheby_selector_sys_new(ctx_of(me)) }
}

/// `selector::receive`'s kernel: `selector` (consumed) with an arm on the
/// borrowed `rx` applied through the borrowed `apply`; the new selector,
/// owned.
fn on_receive(me: &ExecCtx, selector: Word, rx: Word, apply: Word) -> Word {
    // SAFETY: the running fiber's context and live values, borrowed.
    let next = unsafe { cheby_selector_sys_add_receive(ctx_of(me), selector, rx, apply) };
    drop_word(selector);
    next
}

/// `selector::after`'s kernel with a span of `millis` milliseconds, as
/// std's `Duration` passes it; `selector` consumed.
fn on_after(me: &ExecCtx, selector: Word, millis: i64, apply: Word) -> Word {
    let (seconds, nanoseconds) = (millis / 1000, (millis % 1000).wrapping_mul(1_000_000));
    // SAFETY: the running fiber's context and live values, borrowed.
    let next =
        unsafe { cheby_selector_sys_add_after(ctx_of(me), selector, seconds, nanoseconds, apply) };
    drop_word(selector);
    next
}

/// An arm's stand-in apply closure: the integer `n`.
fn arm(n: i64) -> Word {
    values::int_word(n)
}

/// What a `select` chose: the apply word, owned, and the value its receive
/// arm takes (`take_selected`), owned, if it is a receive arm.
type Chosen = (Word, Option<Result<Word, Word>>);

/// `select` as std writes it, without calling the closure: `sys_wait`,
/// then the receive arm's `take_selected` when the select chose a value.
/// The flag when it raised.
fn select_step(me: &ExecCtx, selector: Word) -> Step<Chosen> {
    // SAFETY: the running fiber's context and a live selector, borrowed.
    let apply = unsafe { cheby_selector_sys_wait(ctx_of(me), selector) };
    match flag_of(me) {
        NONE => {}
        raised => return Err(raised),
    }
    let taken = me.conc.select.holds_value().then(|| {
        // SAFETY: the running fiber's context; the receiver only fixes the
        // type, and `Nil` stands for it.
        result(unsafe { cheby_selector_take_selected(ctx_of(me), values::nil()) })
    });
    Ok((apply, taken))
}

/// A short description of a borrowed value.
fn describe(word: Word) -> String {
    if word == values::nil() {
        "Nil".to_owned()
    } else if crate::abi::is_tagged_int(word) {
        int(word).to_string()
    } else {
        "a value".to_owned()
    }
}

/// What a select step gave, for notes: `arm N` with what its arm took
/// (`arm 1 Ok(7)`, `arm 2 Err(Nil)`), or `raised N`. Releases every word.
fn chosen_text(step: Step<Chosen>) -> String {
    match step {
        Err(raised) => format!("raised {raised}"),
        Ok((apply, taken)) => {
            let arm = describe(apply);
            drop_word(apply);
            match taken {
                None => format!("arm {arm}"),
                Some(Ok(word)) => {
                    let text = format!("arm {arm} Ok({})", describe(word));
                    drop_word(word);
                    text
                }
                Some(Err(word)) => {
                    let text = format!("arm {arm} Err({})", describe(word));
                    drop_word(word);
                    text
                }
            }
        }
    }
}

/// The channel of a borrowed `Receiver`.
fn chan_of(rx: Word) -> Arc<Chan> {
    // SAFETY: a live `Receiver`, borrowed.
    Arc::clone(&unsafe { handles::payload::<ReceiverEnd>(rx, HandleKind::Receiver) }.0)
}

/// Yields until the channel `chan` has `receivers` waiting receivers (a
/// select's linked nodes count): an event, never a time.
fn wait_for_receivers(me: &ExecCtx, chan: &Chan, receivers: usize) -> bool {
    for _ in 0..1_000_000 {
        if chan.counts().2 == receivers {
            return true;
        }
        yield_now(me, 1);
    }
    false
}

/// What a channel holds: buffered values, waiting senders, waiting
/// receivers.
fn counts(chan: &Chan) -> (usize, usize, usize) {
    chan.counts()
}

/// Where a job stores its control block's address, so that another fiber
/// can see when it has published its wait.
#[derive(Debug, Clone, Default)]
struct Waiter(Arc<AtomicUsize>);

impl Waiter {
    /// Called by the waiting fiber before it waits.
    fn mark(&self, me: &ExecCtx) {
        self.0
            .store(ptr::from_ref(me).expose_provenance(), Ordering::SeqCst);
    }

    /// Yields until the marked fiber has published a wait node: it is
    /// parked, or parking, in a cancellable wait.
    fn wait_published(&self, me: &ExecCtx) -> bool {
        for _ in 0..1_000_000 {
            let at = self.0.load(Ordering::SeqCst);
            if at != 0 {
                let fiber = ptr::with_exposed_provenance::<ExecCtx>(at);
                // SAFETY: the marked fiber waits (or is about to) and cannot
                // end before the caller cancels it, which it does only
                // after this returns.
                if hooks::published(unsafe { &*fiber }) {
                    return true;
                }
            }
            yield_now(me, 1);
        }
        false
    }
}

/// Yields until the channel `chan` has `senders` waiting senders.
fn wait_for_senders(me: &ExecCtx, chan: &Chan, senders: usize) -> bool {
    for _ in 0..1_000_000 {
        if chan.counts().1 == senders {
            return true;
        }
        yield_now(me, 1);
    }
    false
}

// ---------------------------------------------------------------------------
// selector.md's spec tests.

/// selector.md, "selectors print as a placeholder": an empty selector is a
/// real kind-44 object, printed as `<Selector>`, and so is a selector with
/// arms.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_selectors_print_as_a_placeholder() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(1, move |me| {
        let waiting = empty(me);
        // SAFETY: a live selector.
        seen.set("new", unsafe { crate::generic::debug_string(waiting) });
        let (tx, rx) = channel(me, 0);
        let waiting = on_after(me, on_receive(me, waiting, rx, arm(1)), 5, arm(2));
        // SAFETY: as above.
        seen.set("with arms", unsafe {
            crate::generic::debug_string(waiting)
        });
        for word in [waiting, tx, rx] {
            drop_word(word);
        }
        7
    });
    notes.require(
        1,
        &[
            ("new", shown("<Selector>")),
            ("with arms", shown("<Selector>")),
        ],
    );
}

/// selector.md, "select takes a value from a ready arm" (D-180): a buffered
/// value is taken in the ready scan, and the channel is empty after.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_takes_a_value_from_a_ready_arm() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 1);
            seen.set("send", step_shape(send_step(me, tx, values::int_word(7))));
            let chan = chan_of(rx);
            let waiting = on_receive(me, empty(me), rx, arm(1));
            seen.set("select", chosen_text(select_step(me, waiting)));
            seen.set("left", counts(&chan));
            for word in [waiting, tx, rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("send", "Ok(Nil)".to_owned()),
                ("select", shown("arm 1 Ok(7)")),
                ("left", shown((0, 0, 0))),
            ],
        );
    }
}

/// selector.md, "a closed channel's arm is ready with Err(Nil)" (D-180):
/// found in the ready scan, without waiting.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_closed_channels_arm_is_ready_with_err_nil() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(1, move |me| {
        let (tx, rx) = channel(me, 0);
        drop_word(tx);
        let waiting = on_receive(me, empty(me), rx, arm(1));
        seen.set("select", chosen_text(select_step(me, waiting)));
        drop_word(waiting);
        drop_word(rx);
        7
    });
    notes.require(1, &[("select", shown("arm 1 Err(Nil)"))]);
}

/// selector.md, "a timer arm is ready once its time has passed" (D-103,
/// D-673): a 10 ms timer arm beats a channel nobody sends on, never
/// earlier than its span, and releasing the selector releases the only
/// `Receiver`, so a send then fails (D-205, D-670).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_timer_arm_is_ready_once_its_time_has_passed() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (keep, never) = channel(me, 0);
            let chan = chan_of(never);
            let waiting = on_after(me, on_receive(me, empty(me), never, arm(1)), 10, arm(2));
            drop_word(never);
            let started = Instant::now();
            seen.set("select", chosen_text(select_step(me, waiting)));
            seen.set("not early", started.elapsed() >= Duration::from_millis(10));
            seen.set("left", counts(&chan));
            drop(chan);
            drop_word(waiting);
            seen.set("send", step_shape(send_step(me, keep, values::int_word(1))));
            drop_word(keep);
            7
        });
        notes.require(
            threads,
            &[
                ("select", shown("arm 2")),
                ("not early", shown(true)),
                ("left", shown((0, 0, 0))),
                ("send", "Err(1)".to_owned()),
            ],
        );
    }
}

/// selector.md, "a timer of zero is ready at once" (D-652): it is found in
/// the ready scan, and so is a negative span.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_timer_of_zero_is_ready_at_once() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(1, move |me| {
        let zero = on_after(me, empty(me), 0, arm(1));
        seen.set("zero", chosen_text(select_step(me, zero)));
        let negative = on_after(me, empty(me), -5, arm(2));
        seen.set("negative", chosen_text(select_step(me, negative)));
        drop_word(zero);
        drop_word(negative);
        7
    });
    notes.require(1, &[("zero", shown("arm 1")), ("negative", shown("arm 2"))]);
}

/// selector.md, "a waiting selector takes a handoff on a capacity-0
/// channel" (D-206): a sender spawned before the select hands its value to
/// the select, which waits as a receiver; the sender's `send` is `Ok`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_waiting_selector_takes_a_handoff_on_capacity_zero() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 0);
            let chan = chan_of(rx);
            let owner = open(me);
            let sender = seen.clone();
            start_job(me, owner, &[tx], move |me, words| {
                sender.set(
                    "send",
                    step_shape(send_step(me, words[0], values::int_word(3))),
                );
                0
            });
            let waiting = on_receive(me, empty(me), rx, arm(1));
            seen.set("select", chosen_text(select_step(me, waiting)));
            seen.set("close", close(me, owner));
            seen.set("left", counts(&chan));
            drop_word(waiting);
            drop_word(rx);
            7
        });
        notes.require(
            threads,
            &[
                ("select", shown("arm 1 Ok(3)")),
                ("send", "Ok(Nil)".to_owned()),
                ("close", shown(true)),
                ("left", shown((0, 0, 0))),
            ],
        );
    }
}

/// selector.md, "select chooses among ready arms without favoring one"
/// (D-041, D-673): two arms on closed channels are both always ready; over
/// 1,000 selects of one selector each is chosen between 350 and 650 times
/// (a binomial of p = 1/2 leaves that range with probability below 10⁻²⁰).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_chooses_among_ready_arms_without_favoring_one() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(2, move |me| {
        let (left_tx, left) = channel(me, 0);
        let (right_tx, right) = channel(me, 0);
        drop_word(left_tx);
        drop_word(right_tx);
        let choose = on_receive(me, on_receive(me, empty(me), left, arm(1)), right, arm(2));
        let mut lefts = 0_u32;
        for _ in 0..1000 {
            if chosen_text(select_step(me, choose)) == "arm 1 Err(Nil)" {
                lefts += 1;
            }
        }
        seen.set("fair", (350..=650).contains(&lefts));
        seen.set("lefts", lefts);
        for word in [choose, left, right] {
            drop_word(word);
        }
        7
    });
    notes.require(2, &[("fair", shown(true))]);
}

/// selector.md, "a selector without arms waits until it is cancelled"
/// (D-673), and C-1 (GAP(select-always-publishes)): a select without arms
/// publishes its wait, so the cancellation walk of its scope wins it and
/// wakes it; it unwinds and returns nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_without_arms_waits_until_it_is_cancelled() {
    cancelled_while_waiting(empty);
}

/// C-1 (GAP(select-always-publishes)): a select with only a long timer arm
/// is woken by the cancellation walk, unwinds, and its timer is cancelled.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_timer_only_select_is_woken_by_the_walk() {
    cancelled_while_waiting(|me| on_after(me, empty(me), 60_000, arm(1)));
}

/// A select of the selector `make` gives, in a member `f` of the scope
/// `s`, waits until the root cancels `s` from outside, after `f` has
/// published its wait: `f` unwinds with a cancellation, and the root's
/// close of `s`, which has no cause, succeeds. At every thread count.
fn cancelled_while_waiting(make: fn(&ExecCtx) -> Word) {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let s = open(me);
            let waiter = Waiter::default();
            let (f_notes, f_waiter) = (seen.clone(), waiter.clone());
            let (f, _) = spawn_job(me, s, &[], move |me, _| {
                let waiting = make(me);
                f_waiter.mark(me);
                f_notes.set("f select", chosen_text(select_step(me, waiting)));
                f_notes.set("f holds a value", me.conc.select.holds_value());
                drop_word(waiting);
                0
            });
            seen.set("published", waiter.wait_published(me));
            cancel::cancel(&state_of(s));
            seen.set("close", close(me, s));
            seen.set("f", cell_of(f).outcome());
            drop_word(f);
            7
        });
        notes.require(
            threads,
            &[
                ("published", shown(true)),
                ("f select", raised()),
                ("close", shown(true)),
            ],
        );
    }
}

/// How [`chosen_text`]'s note of a select that unwound for a cancellation
/// reads.
fn raised() -> String {
    shown(format!("raised {CANCEL}"))
}

// ---------------------------------------------------------------------------
// D-206: the hand-off.

/// D-206, D-704: a select waits on two capacity-0 channels; a sender on
/// each races to hand it a value. The first sender to win the select's
/// shared header completes its hand-off: its `send` returns `Ok` only
/// because the select chose its arm. The other sender's swap fails, it
/// skips the select's node and waits with its value, which stays in its
/// channel until the root receives it. No node of the select is left
/// linked.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_handoff_completes_only_for_the_chosen_arm() {
    for threads in THREADS {
        for round in 0..10 {
            let notes = Notes::default();
            let seen = notes.clone();
            run_jobs(threads, move |me| {
                let (tx1, rx1) = channel(me, 0);
                let (tx2, rx2) = channel(me, 0);
                let (done_tx, done_rx) = channel(me, 1);
                let (chan1, chan2) = (chan_of(rx1), chan_of(rx2));
                let s = open(me);
                let waiting = on_receive(me, on_receive(me, empty(me), rx1, arm(1)), rx2, arm(2));
                let f_notes = seen.clone();
                start_job(me, s, &[dup(waiting), done_tx], move |me, words| {
                    let chosen = chosen_text(select_step(me, words[0]));
                    let arm = i64::from(chosen.starts_with("arm 2"));
                    f_notes.set("select", chosen);
                    drop_shape(send_step(me, words[1], values::int_word(arm + 1)));
                    0
                });
                // The select waits on both channels.
                seen.set("waiting 1", wait_for_receivers(me, &chan1, 1));
                seen.set("waiting 2", wait_for_receivers(me, &chan2, 1));
                let sent = [Gate::default(), Gate::default()];
                for (index, tx) in [tx1, tx2].into_iter().enumerate() {
                    let (s_notes, gate) = (seen.clone(), sent[index].clone());
                    let value = values::int_word(10 * (i64::try_from(index).unwrap() + 1));
                    start_job(me, s, &[tx], move |me, words| {
                        if round % 2 == 1 {
                            yield_now(me, i64::try_from(index).unwrap());
                        }
                        let shape = step_shape(send_step(me, words[0], value));
                        s_notes.set(&format!("send {}", index + 1), shape);
                        gate.open();
                        0
                    });
                }
                let chosen = receive_step(me, done_rx).map(|r| r.map(int));
                let (won, other_rx, other_chan, other_value) = match chosen {
                    Ok(Ok(1)) => (0, rx2, &chan2, 20),
                    _ => (1, rx1, &chan1, 10),
                };
                // The winner's send returns; the other's waits with its
                // value, which the root then receives from its channel.
                sent[won].wait(me);
                seen.set("other waits", wait_for_senders(me, other_chan, 1));
                seen.set(
                    "other returned early",
                    seen.get(&format!("send {}", 2 - won)).is_some(),
                );
                seen.set(
                    "other value",
                    shown(step_shape(receive_step(me, other_rx))) == format!("Ok({other_value})"),
                );
                sent[1 - won].wait(me);
                seen.set("close", close(me, s));
                seen.set("left 1", counts(&chan1));
                seen.set("left 2", counts(&chan2));
                for word in [waiting, rx1, rx2, done_rx] {
                    drop_word(word);
                }
                7
            });
            let chosen = notes.get("select");
            let chose_one =
                chosen == Some(shown("arm 1 Ok(10)")) || chosen == Some(shown("arm 2 Ok(20)"));
            assert!(chose_one, "at {threads} threads: {chosen:?}");
            notes.require(
                threads,
                &[
                    ("waiting 1", shown(true)),
                    ("waiting 2", shown(true)),
                    ("other waits", shown(true)),
                    ("other returned early", shown(false)),
                    ("other value", shown(true)),
                    ("send 1", "Ok(Nil)".to_owned()),
                    ("send 2", "Ok(Nil)".to_owned()),
                    ("close", shown(true)),
                    ("left 1", shown((0, 0, 0))),
                    ("left 2", shown((0, 0, 0))),
                ],
            );
        }
    }
}

/// D-206 with two arms on one `Receiver`: the select locks the channel
/// once and links two nodes into it; a sender's hand-off wins the shared
/// header for one of them and skips the other, which the select unlinks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_two_arms_on_one_receiver_lock_it_once() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 0);
            let chan = chan_of(rx);
            let s = open(me);
            let waiting = on_receive(me, on_receive(me, empty(me), rx, arm(1)), rx, arm(2));
            let f_notes = seen.clone();
            start_job(me, s, &[dup(waiting)], move |me, words| {
                let chosen = chosen_text(select_step(me, words[0]));
                f_notes.set("select", chosen == "arm 1 Ok(4)" || chosen == "arm 2 Ok(4)");
                0
            });
            seen.set("waiting", wait_for_receivers(me, &chan, 2));
            seen.set("send", step_shape(send_step(me, tx, values::int_word(4))));
            seen.set("close", close(me, s));
            seen.set("left", counts(&chan));
            for word in [waiting, tx, rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("waiting", shown(true)),
                ("send", "Ok(Nil)".to_owned()),
                ("select", shown(true)),
                ("close", shown(true)),
                ("left", shown((0, 0, 0))),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// Closed arms (D-180, D-769).

/// D-180: a channel whose last `Sender` goes while a select waits on it
/// wakes the select with `Err(Nil)` for that arm; no scope on the select's
/// chain is failing, so the chain says nothing (D-769).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_closed_arm_wakes_a_waiting_select() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 0);
            let (never_tx, never_rx) = channel(me, 0);
            let (chan, never) = (chan_of(rx), chan_of(never_rx));
            let s = open(me);
            let waiting = on_receive(me, on_receive(me, empty(me), never_rx, arm(1)), rx, arm(2));
            let f_notes = seen.clone();
            start_job(me, s, &[dup(waiting)], move |me, words| {
                f_notes.set("select", chosen_text(select_step(me, words[0])));
                0
            });
            seen.set("waiting", wait_for_receivers(me, &chan, 1));
            drop_word(tx);
            seen.set("close", close(me, s));
            seen.set("left", (counts(&chan), counts(&never)));
            for word in [waiting, rx, never_tx, never_rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("waiting", shown(true)),
                ("select", shown("arm 2 Err(Nil)")),
                ("close", shown(true)),
                ("left", shown(((0, 0, 0), (0, 0, 0)))),
            ],
        );
    }
}

/// D-769, both halves, made deterministic with the walk's first step
/// (`hooks::cancel_first_step`): the root owns `s`, and `o`, a member of
/// `s`, opens `n` with the member `f`. The first step of `s`'s walk marks
/// `s` and leaves `n` unvisited, so `f` has no target while a scope on its
/// chain is cancelled. A select of `f` that finds a closed arm in its ready
/// scan, without suspending, reads the target alone and returns
/// `Err(Nil)`; a second select of `f`, woken by the close of the channel it
/// waits on, reads the chain and unwinds, taking nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_closed_arm_reads_the_chain_only_after_a_wait() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (closed_tx, closed_rx) = channel(me, 0);
            drop_word(closed_tx);
            let (tx, rx) = channel(me, 0);
            let (never_tx, never_rx) = channel(me, 0);
            let chan = chan_of(rx);
            let s = open(me);
            let s_state = state_of(s);
            let (go, done) = (Gate::default(), Gate::default());
            let (o_notes, o_go, o_done) = (seen.clone(), go.clone(), done.clone());
            start_job(me, s, &[closed_rx, rx, dup(never_rx)], move |me, words| {
                let n = open(me);
                let (f_notes, f_go, f_done) = (o_notes.clone(), o_go, o_done);
                start_job(me, n, &[dup(words[0]), dup(words[1])], move |me, words| {
                    let ready = on_receive(me, empty(me), words[0], arm(1));
                    let waiting = on_receive(me, empty(me), words[1], arm(2));
                    // Yielding is no entry check: nothing is delivered.
                    f_go.wait(me);
                    f_notes.set("targeted", cancel::check(me).is_err());
                    f_notes.set("ready select", chosen_text(select_step(me, ready)));
                    f_notes.set("waiting select", chosen_text(select_step(me, waiting)));
                    f_done.open();
                    drop_word(ready);
                    drop_word(waiting);
                    0
                });
                o_notes.set("o receive", step_shape(receive_step(me, words[2])));
                close_unwound(me, n);
                o_notes.set("o n close flag", flag_of(me));
                0
            });
            seen.set("o parked", wait_for_receivers(me, &chan_of(never_rx), 1));
            hooks::cancel_first_step(&s_state);
            go.open();
            seen.set("f waiting", wait_for_receivers(me, &chan, 1));
            drop_word(tx);
            done.wait(me);
            cancel::cancel(&s_state);
            seen.set("s close", close(me, s));
            seen.set("left", counts(&chan));
            drop_word(never_tx);
            drop_word(never_rx);
            7
        });
        notes.require(
            threads,
            &[
                ("o parked", shown(true)),
                ("targeted", shown(false)),
                ("ready select", shown("arm 1 Err(Nil)")),
                ("f waiting", shown(true)),
                ("waiting select", raised()),
                ("o receive", format!("raised {CANCEL}")),
                ("o n close flag", shown(CANCEL)),
                ("s close", shown(true)),
                ("left", shown((0, 0, 0))),
            ],
        );
    }
}

/// One round of [`select_closed_arm_reads_the_chain`]: the root owns `s`,
/// whose members are `parked` fibers waiting on a channel nobody sends to
/// (which makes the walk's wake loop long), `q`, which holds the only
/// `Sender` of `c` and waits too, `o`, which opens `n` with the member
/// `f` selecting on `c`, and `p`, which panics once `f` waits. The walk of
/// `s` wakes `q` before it reaches `n`; at two or more threads `q` can
/// unwind and drop `c`'s `Sender` before the walk reaches `f`, which then
/// wakes for the close. Either way `f` is being cancelled (D-667, D-769):
/// it unwinds and takes nothing, and never returns `Err(Nil)`.
fn closed_chain_round(
    runtime: &crate::sched::Runtime,
    parked: usize,
) -> (EntryOutcome, i64, Notes) {
    let notes = Notes::default();
    let seen = notes.clone();
    let (outcome, domain) = outcome_on(runtime, move |me| {
        let (c_tx, c_rx) = channel(me, 0);
        let (never_tx, never_rx) = channel(me, 0);
        let (go_tx, go_rx) = channel(me, 0);
        let (c, never) = (chan_of(c_rx), chan_of(never_rx));
        let s = open(me);
        // `q` holds the only `Sender` of `c` until it unwinds. It is the
        // first member, so the walk wakes it first, then the parked ones.
        start_job(me, s, &[c_tx, dup(never_rx)], |me, words| {
            drop_shape(receive_step(me, words[1]));
            0
        });
        for _ in 0..parked {
            start_job(me, s, &[dup(never_rx)], |me, words| {
                drop_shape(receive_step(me, words[0]));
                0
            });
        }
        let o_notes = seen.clone();
        start_job(me, s, &[c_rx, dup(never_rx)], move |me, words| {
            let n = open(me);
            let f_notes = o_notes.clone();
            start_job(me, n, &[dup(words[0])], move |me, words| {
                let waiting = on_receive(me, empty(me), words[0], arm(1));
                f_notes.set("f select", chosen_text(select_step(me, waiting)));
                drop_word(waiting);
                0
            });
            drop_shape(receive_step(me, words[1]));
            close_unwound(me, n);
            0
        });
        start_job(me, s, &[go_rx], |me, words| {
            if receive_step(me, words[0]).is_ok() {
                panic_with(me, "first", "src/p.cheby:1:1");
            }
            0
        });
        seen.set("f waiting", wait_for_receivers(me, &c, 1));
        seen.set(
            "all parked",
            wait_for_receivers(me, &never, parked.saturating_add(2)),
        );
        seen.set("go", step_shape(send_step(me, go_tx, values::nil())));
        seen.set("body receive", step_shape(receive_step(me, never_rx)));
        seen.set("close", close_unwound(me, s));
        seen.set("failure", failure_of(me));
        seen.set("left", counts(&c));
        for word in [never_tx, never_rx, go_tx] {
            drop_word(word);
        }
        7
    });
    (outcome, domain.live_objects(), notes)
}

/// D-769 for `select` (the checkpoint-2 session's Open item): a select in
/// a scope whose sibling panics while it waits, another sibling's
/// unwinding dropping the select's only `Sender`, always unwinds and never
/// returns `Err(Nil)`, whichever of the walk and the close reaches it
/// first. 200 rounds at 1, 2, 4 and 8 threads, and under the migration
/// stress mode (D-687, what `CHEBY_RT_STRESS_MIGRATE=1` sets) at 2, 4 and
/// 8. Prints how many selects woke for the close and read the chain.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_closed_arm_reads_the_chain() {
    let stressed = std::env::var_os("CHEBY_RT_STRESS_MIGRATE").is_some_and(|value| value == "1");
    let settings = [
        (1, stressed),
        (2, stressed),
        (4, stressed),
        (8, stressed),
        (2, true),
        (4, true),
        (8, true),
    ];
    let before = CLOSED_WAKES_CANCELLED.load(Ordering::Relaxed);
    for (threads, stress) in settings {
        let runtime = testing::tuned(threads, stress, true);
        for round in 0..200 {
            let (outcome, live, notes) = closed_chain_round(&runtime, 200);
            let context = format!("round {round} at {threads} threads, stress {stress}");
            assert_eq!(outcome, EntryOutcome::Returned(7), "{context}");
            assert_eq!(live, 0, "objects leaked in {context}");
            notes.require(
                threads,
                &[
                    ("f waiting", shown(true)),
                    ("all parked", shown(true)),
                    ("go", "Ok(Nil)".to_owned()),
                    ("f select", raised()),
                    ("body receive", format!("raised {CANCEL}")),
                    ("close", shown(false)),
                    ("failure", panic_failure("first", "src/p.cheby:1:1")),
                    ("left", shown((0, 0, 0))),
                ],
            );
        }
        runtime.shutdown();
    }
    let woken = CLOSED_WAKES_CANCELLED.load(Ordering::Relaxed) - before;
    println!("select_closed_arm_reads_the_chain: {woken} selects woke for the close and unwound");
}

// ---------------------------------------------------------------------------
// Cancellation (D-670) and timers racing (D-673, D-682).

/// D-670: a select cancelled while it waits takes nothing: a value sent
/// after the cancellation stays in the channel, and the select's node is
/// gone from it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_cancelled_select_takes_nothing() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 1);
            let chan = chan_of(rx);
            let s = open(me);
            let waiting = on_receive(me, empty(me), rx, arm(1));
            let f_notes = seen.clone();
            let (f, _) = spawn_job(me, s, &[dup(waiting)], move |me, words| {
                f_notes.set("select", chosen_text(select_step(me, words[0])));
                0
            });
            seen.set("waiting", wait_for_receivers(me, &chan, 1));
            cancel::cancel(&state_of(s));
            seen.set("close", close(me, s));
            seen.set("send", step_shape(send_step(me, tx, values::int_word(5))));
            seen.set("left", counts(&chan));
            seen.set("receive", step_shape(receive_step(me, rx)));
            drop_word(f);
            for word in [waiting, tx, rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("waiting", shown(true)),
                ("select", raised()),
                ("close", shown(true)),
                ("send", "Ok(Nil)".to_owned()),
                ("left", shown((1, 0, 0))),
                ("receive", "Ok(5)".to_owned()),
            ],
        );
    }
}

/// D-206, D-673, D-682: a timer arm of 1 ms races a sender's hand-off on a
/// capacity-0 channel. Exactly one arm is chosen; when the timer wins, the
/// sender's value stays in its channel for the next receiver. The value is
/// seen exactly once either way. 30 rounds at each thread count.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_timer_arm_races_a_channel_arm() {
    for threads in THREADS {
        for round in 0..30_i64 {
            let notes = Notes::default();
            let seen = notes.clone();
            run_jobs(threads, move |me| {
                let (tx, rx) = channel(me, 0);
                let (done_tx, done_rx) = channel(me, 1);
                let chan = chan_of(rx);
                let s = open(me);
                let waiting = on_after(me, on_receive(me, empty(me), rx, arm(1)), 1, arm(2));
                let f_notes = seen.clone();
                start_job(me, s, &[dup(waiting), done_tx], move |me, words| {
                    let chosen = chosen_text(select_step(me, words[0]));
                    let timer = i64::from(chosen == "arm 2");
                    f_notes.set("select", chosen);
                    drop_shape(send_step(me, words[1], values::int_word(timer)));
                    0
                });
                let s_notes = seen.clone();
                start_job(me, s, &[tx], move |me, words| {
                    yield_now(me, round % 4);
                    s_notes.set(
                        "send",
                        step_shape(send_step(me, words[0], values::int_word(9))),
                    );
                    0
                });
                let timer = receive_step(me, done_rx).map(|r| r.map(int));
                if timer == Ok(Ok(1)) {
                    // The sender's value stayed in its channel.
                    seen.set("received", step_shape(receive_step(me, rx)));
                } else {
                    seen.set("received", step_shape(Ok(Ok(values::int_word(9)))));
                }
                seen.set("close", close(me, s));
                seen.set("left", counts(&chan));
                for word in [waiting, rx, done_rx] {
                    drop_word(word);
                }
                7
            });
            let chosen = notes.get("select");
            assert!(
                chosen == Some(shown("arm 1 Ok(9)")) || chosen == Some(shown("arm 2")),
                "{chosen:?}"
            );
            notes.require(
                threads,
                &[
                    ("send", "Ok(Nil)".to_owned()),
                    ("received", "Ok(9)".to_owned()),
                    ("close", shown(true)),
                    ("left", shown((0, 0, 0))),
                ],
            );
        }
    }
}

/// D-670, D-673, D-706: a select's timer arm of 1 ms races the
/// cancellation walk of its scope, which the root starts after a varying
/// number of yields. Whichever wins the select's shared header decides:
/// the timer's arm, or an unwinding; nothing hangs or leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_timer_races_the_cancellation_walk() {
    for threads in THREADS {
        for round in 0..30_i64 {
            let notes = Notes::default();
            let seen = notes.clone();
            run_jobs(threads, move |me| {
                let (tx, rx) = channel(me, 0);
                let chan = chan_of(rx);
                let s = open(me);
                let waiting = on_after(me, on_receive(me, empty(me), rx, arm(1)), 1, arm(2));
                let (f_notes, started) = (seen.clone(), Gate::default());
                let f_started = started.clone();
                start_job(me, s, &[dup(waiting)], move |me, words| {
                    f_started.open();
                    f_notes.set("select", chosen_text(select_step(me, words[0])));
                    0
                });
                // `f` has started (so it calls its function), and selects
                // before, while or after the walk reaches it.
                started.wait(me);
                yield_now(me, round % 6);
                cancel::cancel(&state_of(s));
                seen.set("close", close(me, s));
                seen.set("left", counts(&chan));
                for word in [waiting, tx, rx] {
                    drop_word(word);
                }
                7
            });
            let chosen = notes.get("select");
            assert!(
                chosen == Some(shown("arm 2")) || chosen == Some(raised()),
                "{chosen:?}"
            );
            notes.require(
                threads,
                &[("close", shown(true)), ("left", shown((0, 0, 0)))],
            );
        }
    }
}

/// GAP(select-equal-timers): two timer arms of the same span have the same
/// deadline; the select arms one timer and chooses either arm. Over 64
/// selects both are chosen (each misses with probability 2⁻⁶⁴); none fires
/// before its span.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_equal_timers_choose_uniformly() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(2, move |me| {
        let waiting = on_after(me, on_after(me, empty(me), 1, arm(1)), 1, arm(2));
        let (mut ones, mut early) = (0_u32, 0_u32);
        for _ in 0..64 {
            let started = Instant::now();
            if chosen_text(select_step(me, waiting)) == "arm 1" {
                ones += 1;
            }
            if started.elapsed() < Duration::from_millis(1) {
                early += 1;
            }
        }
        seen.set("both", ones > 0 && ones < 64);
        seen.set("early", early);
        drop_word(waiting);
        7
    });
    notes.require(2, &[("both", shown(true)), ("early", shown(0_u32))]);
}

/// D-673: one selector is selected again and again, and each select starts
/// its timer's clock anew: none returns before its span, measured from its
/// own start.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_a_reused_selector_starts_each_clock_anew() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(1, move |me| {
        let (tx, rx) = channel(me, 0);
        let waiting = on_after(me, on_receive(me, empty(me), rx, arm(1)), 10, arm(2));
        let mut early = 0_u32;
        for _ in 0..3 {
            let started = Instant::now();
            seen.set("select", chosen_text(select_step(me, waiting)));
            if started.elapsed() < Duration::from_millis(10) {
                early += 1;
            }
        }
        seen.set("early", early);
        for word in [waiting, tx, rx] {
            drop_word(word);
        }
        7
    });
    notes.require(1, &[("select", shown("arm 2")), ("early", shown(0_u32))]);
}

// ---------------------------------------------------------------------------
// Release points (D-205, D-670) and raises (architecture §7.3).

/// selector.md's Mapping rule (D-205, D-670), the kernel half: once
/// `sys_wait` has returned and the selector is released, the runtime holds
/// no reference to any arm's receiver. The arm not chosen loses its only
/// `Receiver`, whose channel closes at once; the chosen arm's apply closure
/// is the only holder of its `Receiver` left (count 1), and releasing it
/// closes that channel too. T1ui's `receivers_are_released_before_the_mapping_function`
/// checks the Cheby half.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_receivers_are_released_before_the_mapping_function() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (ready_tx, ready_rx) = channel(me, 1);
            let (other_tx, other_rx) = channel(me, 0);
            seen.set(
                "send",
                step_shape(send_step(me, ready_tx, values::int_word(6))),
            );
            let ready_chan = chan_of(ready_rx);
            // Each closure captures its arm's `Receiver`, as std's
            // `receive` builds it; the test gives its own references away.
            let ready_apply = closure(&[dup(ready_rx)]);
            let other_apply = closure(&[dup(other_rx)]);
            let waiting = on_receive(
                me,
                on_receive(me, empty(me), ready_rx, ready_apply),
                other_rx,
                other_apply,
            );
            for word in [ready_rx, other_rx, ready_apply, other_apply] {
                drop_word(word);
            }
            // SAFETY: the running fiber's context and a live selector.
            let chosen = unsafe { cheby_selector_sys_wait(ctx_of(me), waiting) };
            seen.set("chose the ready arm", chosen == ready_apply);
            // `select` releases the selector before it calls the closure.
            drop_word(waiting);
            seen.set(
                "other closed",
                step_shape(send_step(me, other_tx, values::int_word(1))),
            );
            // SAFETY: the closure is live: `chosen` holds it.
            let held = unsafe { crate::generic::tests::captures(chosen) }[0];
            // SAFETY: a live `Receiver`.
            seen.set("receiver count", unsafe { load_rc(word_to_ptr(held)) });
            seen.set("channel holders", Arc::strong_count(&ready_chan));
            // SAFETY: the running fiber's context.
            let taken = result(unsafe { cheby_selector_take_selected(ctx_of(me), held) });
            seen.set("taken", step_shape(Ok(taken)));
            drop_word(chosen);
            seen.set(
                "ready closed",
                step_shape(send_step(me, ready_tx, values::int_word(2))),
            );
            for word in [ready_tx, other_tx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("send", "Ok(Nil)".to_owned()),
                ("chose the ready arm", shown(true)),
                ("other closed", "Err(1)".to_owned()),
                ("receiver count", shown(1_i64)),
                // The test's own `Arc`, the `Sender` and the `Receiver`.
                ("channel holders", shown(3_usize)),
                ("taken", "Ok(6)".to_owned()),
                ("ready closed", "Err(2)".to_owned()),
            ],
        );
    }
}

/// Architecture §7.3: a raise releases a chosen value that its arm has not
/// taken (`drop_selected`, first in `start_panic` and `start_cancel`). A
/// select chooses a heap value; then the fiber panics inside a scope before
/// the arm takes it; nothing leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_a_raise_releases_the_chosen_value() {
    let notes = Notes::default();
    let seen = notes.clone();
    run_jobs(1, move |me| {
        let (tx, rx) = channel(me, 1);
        let value = string("a heap value");
        seen.set("send", step_shape(send_step(me, tx, value)));
        drop_word(value);
        let waiting = on_receive(me, empty(me), rx, arm(1));
        let s = open(me);
        // SAFETY: the running fiber's context and a live selector.
        let chosen = unsafe { cheby_selector_sys_wait(ctx_of(me), waiting) };
        seen.set("holds", me.conc.select.holds_value());
        seen.set(
            "panic",
            panic_with(me, "after the choice", "src/s.cheby:1:1"),
        );
        seen.set("released", !me.conc.select.holds_value());
        seen.set("close", close_unwound(me, s));
        seen.set("failure", text_of_failure(me));
        for word in [chosen, waiting, tx, rx] {
            drop_word(word);
        }
        7
    });
    notes.require(
        1,
        &[
            ("send", "Ok(Nil)".to_owned()),
            ("holds", shown(true)),
            ("panic", shown(super::cancel_tests::PANIC)),
            ("released", shown(true)),
            ("close", shown(false)),
            ("failure", shown("after the choice")),
        ],
    );
}

/// The message of the failure the last close left.
fn text_of_failure(me: &ExecCtx) -> String {
    failure_of(me).1
}

// ---------------------------------------------------------------------------
// Without a runtime (D-692).

/// D-692 in an isolated entry: building selectors and a select with a
/// ready arm (a buffered value, a closed channel, a timer of zero) work; a
/// select that would wait ends the program with a fatal error naming
/// `selector::select`. Runs itself in a child process.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn select_without_a_runtime_only_a_ready_select_works() {
    const CHILD: &str = "CHEBY_RT_TEST_SELECT_NO_RUNTIME";
    const NAME: &str =
        "fiber_ops::select_tests::select_without_a_runtime_only_a_ready_select_works";
    if std::env::var_os(CHILD).is_some() {
        let domain = Domain::new();
        let config = config(&domain);
        // SAFETY: a test thunk with the entry ABI.
        let outcome = unsafe { run_entry(isolated_select, &config) };
        panic!("the isolated entry ended: {outcome:?}");
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("ready selects work\n"), "{stderr}");
    assert!(
        stderr.contains(
            "fatal runtime error: selector::select needs the fiber scheduler, which this entry does not run\n"
        ),
        "{stderr}"
    );
}

/// The isolated entry of [`select_without_a_runtime_only_a_ready_select_works`].
unsafe extern "C" fn isolated_select(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    let (tx, rx) = channel(me, 1);
    let (closed_tx, closed_rx) = channel(me, 0);
    drop_word(closed_tx);
    assert!(send_step(me, tx, values::int_word(5)).is_ok());
    let buffered = on_receive(me, empty(me), rx, arm(1));
    assert_eq!(chosen_text(select_step(me, buffered)), "arm 1 Ok(5)");
    let closed = on_receive(me, empty(me), closed_rx, arm(2));
    assert_eq!(chosen_text(select_step(me, closed)), "arm 2 Err(Nil)");
    let zero = on_after(me, empty(me), 0, arm(3));
    assert_eq!(chosen_text(select_step(me, zero)), "arm 3");
    eprintln!("ready selects work");
    // Empty now, with its `Sender` alive, and a positive timer: this
    // would wait.
    let waiting = on_after(me, buffered, 10, arm(4));
    let _ = select_step(me, waiting);
    0
}
