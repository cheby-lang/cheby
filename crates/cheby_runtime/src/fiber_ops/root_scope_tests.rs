//! A root's base scope (D-859, after D-798's form) and the cancellation a
//! deadlock gives it in `cheby test` and the REPL (D-724, D-667): the
//! `root_scope_*` tests.
//!
//! A root's own scopes nest under its parentless base scope, which never
//! has members and records no first cause, so a program that does not
//! deadlock sees nothing of it. The detector's action for a test or a REPL
//! entry is `cancel::cancel_deadlocked_root`, from a host thread with no
//! runtime lock held: here a host thread stands in for the monitor, reads
//! the root's wait site under the base's lock (`ScopeState::survey_step`,
//! as the survey does), and reports the root once the root waits. The root
//! and every fiber of its scopes unwind, every close passes the
//! cancellation through (D-667), `fiber::scope` and `fiber::timeout`
//! included, and the root's catch point finds a `Cancel` record whose
//! target is the base, and the report, once.
//!
//! The root's base frame (`sched`) opens and closes the base for every
//! root; a root started where that wiring is missing opens and closes it
//! here through the seam API ([`RootBase`]). The root takes the
//! cancellation itself, as `finish_entry` would (`catch`), and returns, so
//! these tests do not depend on how the host turns it into an outcome.
//! Fibers never assert: they write [`Notes`], which the test thread
//! compares once the runtime has stopped, and every test ends with its
//! domain's live count at zero.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::base::Outcome;
use super::cancel::{cancel_deadlocked_root, hooks, take_deadlock_report};
use super::cancel_tests::{
    CANCEL, NONE, Notes, Step, THREADS, close_unwound, flag_of, panic_failure, panic_with,
    receive_step, run_jobs, shown, spawn_job, start_job, state_of, step_shape,
};
use super::join::cheby_fiber_sys_join;
use super::scope::{ScopeState, cheby_rt_scope_open, close_root_base, open_root_base};
use super::select::{
    cheby_selector_sys_add_receive, cheby_selector_sys_new, cheby_selector_sys_wait,
    cheby_selector_take_selected,
};
use super::tests::{
    cell_of, channel, close, ctx_of, drop_word, dup, failure_of, open, wait_for_waiters, yield_now,
};
use crate::abi::Word;
use crate::ctx::{ExecCtx, FiberInit};
use crate::sched::{FiberRef, WaitOp, testing};
use crate::unwind::record::take_unwound;
use crate::values;

/// The report the stand-in detector stores.
const REPORT: &str =
    "fatal error: all fibers are blocked\n  blocked in channel::receive at <unknown location>\n";

/// The root's base scope, and whether this test opened it because the
/// root's base frame did not.
struct RootBase {
    base: Arc<ScopeState>,
    opened_here: bool,
}

impl RootBase {
    /// The running root's base scope: its spawn scope, which the root's
    /// base frame opened, else one opened here through the seam API.
    fn of(me: &ExecCtx) -> Self {
        match me.conc.spawn_scope() {
            Some(base) => Self {
                base,
                opened_here: false,
            },
            None => Self {
                base: open_root_base(me),
                opened_here: true,
            },
        }
    }

    /// Ends the base if this test opened it, as the root's base frame
    /// would; returns whether the base is a root's base and, once ended
    /// here, has ended.
    fn finish(self, me: &ExecCtx) -> bool {
        let root = self.base.is_root_base();
        if self.opened_here {
            close_root_base(me);
            root && self.base.ended()
        } else {
            root
        }
    }
}

/// Starts the stand-in detector: a host thread that reads the root's wait
/// site under the base's lock until the root waits in `op` (at most
/// `testing::hang()`), then reports the root deadlocked with no lock held,
/// as the monitor does. Its result is the wait site it read last.
fn detect(
    me: &ExecCtx,
    base: &Arc<ScopeState>,
    op: WaitOp,
) -> thread::JoinHandle<Option<(WaitOp, usize)>> {
    let root = FiberRef::from_ctx(me);
    let base = Arc::clone(base);
    thread::spawn(move || {
        let started = Instant::now();
        let seen = loop {
            let mut seen = None;
            let mut nested = Vec::new();
            // The owner of a root's base is read only under the base's
            // lock while the base has not ended (the survey's rule).
            base.survey_step(
                &mut |ctx, _| {
                    if ptr::eq(ctx, root.as_ptr()) {
                        seen = ctx.sched.wait_site();
                    }
                },
                &mut nested,
            );
            drop(nested);
            if seen.is_some_and(|(seen, _)| seen == op) || started.elapsed() > testing::hang() {
                break seen;
            }
            thread::sleep(Duration::from_millis(1));
        };
        cancel_deadlocked_root(&root, &base, REPORT.to_owned());
        seen
    })
}

/// Takes the deadlock's cancellation at the root, as its catch point
/// would: the record (which clears the flag), whether the target is the
/// base, the report taken twice, and the wait site left; then drops the
/// target, so the root returns normally.
fn catch(me: &ExecCtx, base: &Arc<ScopeState>) -> String {
    let record = take_unwound(me);
    let targeted = hooks::target(me).is_some_and(|target| Arc::ptr_eq(&target, base));
    hooks::clear_target(me);
    let first = take_deadlock_report(me);
    let second = take_deadlock_report(me);
    format!(
        "{record:?}, targeted {targeted}, {first:?}, {second:?}, {:?}",
        me.sched.wait_site()
    )
}

/// What [`catch`] finds after a deadlock's cancellation, as noted.
fn caught() -> String {
    shown(format!(
        "Some(Cancel), targeted true, {:?}, None, None",
        Some(REPORT)
    ))
}

/// `fiber::join` of the borrowed `fiber`: the value, owned, or the flag.
fn join_step(me: &ExecCtx, fiber: Word) -> Step<Word> {
    // SAFETY: the running fiber's context and a live `Fiber` handle.
    let word = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
    match flag_of(me) {
        NONE => Ok(word),
        raised => Err(raised),
    }
}

/// `select` over one receive arm on the borrowed `rx`: the value its arm
/// took (released) as `Ok(n)`, or the flag when it raised.
fn select_step(me: &ExecCtx, rx: Word) -> Step<i64> {
    let ctx = ctx_of(me);
    // SAFETY: the running fiber's context and live values, borrowed; each
    // owned result is released below.
    unsafe {
        let empty = cheby_selector_sys_new(ctx);
        let selector = cheby_selector_sys_add_receive(ctx, empty, rx, values::int_word(1));
        drop_word(empty);
        let apply = cheby_selector_sys_wait(ctx, selector);
        let step = match flag_of(me) {
            NONE => {
                drop_word(apply);
                let taken = cheby_selector_take_selected(ctx, values::nil());
                let parts = values::read_result(taken);
                let value = match parts {
                    Ok(word) if crate::abi::is_tagged_int(word) => values::word_to_i64(word),
                    _ => -1,
                };
                drop_word(taken);
                Ok(value)
            }
            raised => Err(raised),
        };
        drop_word(selector);
        step
    }
}

/// D-859: a root's scopes nest under its base, which is the root's spawn
/// scope, has depth 0, holds the root's open scopes as nested scopes,
/// never has members, and is ended by the root's base frame. A member's
/// panic in the root's scope is that scope's first cause as before, and
/// the base records nothing and is not cancelled.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_nests_the_roots_scopes_under_its_base() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            let base = Arc::clone(&root.base);
            seen.set("root base", base.is_root_base());
            seen.set("depth", base.depth());
            seen.set("no parent", base.parent().is_none());
            let scope = open(me);
            let state = state_of(scope);
            seen.set(
                "parent is the base",
                state
                    .parent()
                    .is_some_and(|parent| Arc::ptr_eq(parent, &base)),
            );
            seen.set("scope depth", state.depth());
            seen.set("nested", base.nested_count());
            start_job(me, scope, &[], |me, _| {
                panic_with(me, "a member's panic", "src/m.cheby:2:3");
                0
            });
            seen.set("close", close(me, scope));
            seen.set("failure", failure_of(me));
            seen.set("nested after", base.nested_count());
            seen.set("base failing", base.failing());
            let mut looks = 0_usize;
            let mut nested = Vec::new();
            let live = base.survey_step(&mut |_, _| looks += 1, &mut nested);
            seen.set("survey", (live, looks, nested.len()));
            drop((nested, state, base));
            seen.set("finished", root.finish(me));
            7
        });
        notes.require(
            threads,
            &[
                ("root base", shown(true)),
                ("depth", shown(0_u32)),
                ("no parent", shown(true)),
                ("parent is the base", shown(true)),
                ("scope depth", shown(1_u32)),
                ("nested", shown(1_usize)),
                ("close", shown(false)),
                (
                    "failure",
                    panic_failure("a member's panic", "src/m.cheby:2:3"),
                ),
                ("nested after", shown(0_usize)),
                ("base failing", shown(false)),
                // The owner alone: the base never has members.
                ("survey", shown((true, 1_usize, 0_usize))),
                ("finished", shown(true)),
            ],
        );
    }
}

/// D-859, D-701: a root's panic outside every scope is not recorded in its
/// base, which stays uncancelled; the catch point reports it as before.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_a_roots_panic_records_nothing_in_its_base() {
    let notes = Notes::default();
    let seen = notes.clone();
    let (outcome, domain) = super::tests::outcome(2, move |me| {
        let root = RootBase::of(me);
        seen.set(
            "flag",
            panic_with(me, "the root's panic", "src/r.cheby:1:1"),
        );
        seen.set("base failing", root.base.failing());
        seen.set("finished", root.finish(me));
        0
    });
    let crate::entry::EntryOutcome::Panicked(report) = outcome else {
        panic!("the root did not panic: {outcome:?}");
    };
    assert_eq!(report.location, "src/r.cheby:1:1");
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    notes.require(
        2,
        &[
            ("flag", shown(super::cancel_tests::PANIC)),
            ("base failing", shown(false)),
            ("finished", shown(true)),
        ],
    );
}

/// D-859: the report is stored on the root under its live base and
/// taken once; the base's cancellation targets its owner. No fiber runs:
/// the root is a placeholder block.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_report_is_stored_and_taken_once() {
    let root = Box::new(ExecCtx::new(FiberInit::placeholder()));
    let base = open_root_base(&root);
    assert!(
        root.conc
            .spawn_scope()
            .is_some_and(|scope| Arc::ptr_eq(&scope, &base))
    );
    assert!(
        root.conc
            .responsible_scope()
            .is_some_and(|scope| scope.is_root_base())
    );
    assert_eq!(take_deadlock_report(&root), None);
    assert!(!base.failing());
    cancel_deadlocked_root(&FiberRef::from_ctx(&root), &base, REPORT.to_owned());
    assert!(base.failing(), "the base is cancelled");
    assert!(hooks::target(&root).is_some_and(|target| Arc::ptr_eq(&target, &base)));
    assert!(crate::ctx::pending(&root, crate::ctx::reason::CANCEL));
    assert_eq!(take_deadlock_report(&root).as_deref(), Some(REPORT));
    assert_eq!(take_deadlock_report(&root), None);
    close_root_base(&root);
    assert!(base.ended());
    assert!(hooks::target(&root).is_none(), "the end drops the target");
    assert!(root.conc.spawn_scope().is_none());
}

/// D-859, critique C5 and RISK-3: a survey reads a root's block only under
/// its base's lock while the base has not ended, and the base is ended
/// under that lock before the root retires; so a survey racing the end
/// reads the owner while the base lives and skips it, reading nothing,
/// once it has ended. A late report is then neither stored nor acted on.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_a_base_ended_under_its_lock_is_skipped_by_a_concurrent_survey() {
    let root = Box::new(ExecCtx::new(FiberInit::placeholder()));
    let owner = FiberRef::from_ctx(&root);
    let base = open_root_base(&root);
    let live_reads = Arc::new(AtomicU64::new(0));
    let wrong = Arc::new(AtomicBool::new(false));
    let surveyor = {
        let base = Arc::clone(&base);
        let live_reads = Arc::clone(&live_reads);
        let wrong = Arc::clone(&wrong);
        thread::spawn(move || {
            let mut skipped = 0_u32;
            let started = Instant::now();
            while skipped < 100 && started.elapsed() < testing::hang() {
                let mut looks = 0_u32;
                let mut nested = Vec::new();
                let live = base.survey_step(
                    &mut |ctx, first| {
                        looks += 1;
                        if !ptr::eq(ctx, owner.as_ptr()) || !first {
                            wrong.store(true, Ordering::SeqCst);
                        }
                        // What the detector reads of the root.
                        let _ = (ctx.sched.wait_site(), ctx.sched.resumes());
                    },
                    &mut nested,
                );
                if !nested.is_empty() || looks != u32::from(live) {
                    wrong.store(true, Ordering::SeqCst);
                }
                if live {
                    live_reads.fetch_add(1, Ordering::SeqCst);
                } else {
                    skipped += 1;
                }
            }
            skipped
        })
    };
    let started = Instant::now();
    while live_reads.load(Ordering::SeqCst) < 100 && started.elapsed() < testing::hang() {
        thread::yield_now();
    }
    close_root_base(&root);
    let skipped = surveyor.join().expect("the surveyor panicked");
    assert!(live_reads.load(Ordering::SeqCst) >= 100);
    assert_eq!(skipped, 100);
    assert!(
        !wrong.load(Ordering::SeqCst),
        "a survey read the wrong fiber"
    );
    assert!(base.ended());
    cancel_deadlocked_root(&owner, &base, REPORT.to_owned());
    assert_eq!(
        take_deadlock_report(&root),
        None,
        "a late report is dropped"
    );
    assert!(hooks::target(&root).is_none());
    assert!(!base.failing());
}

/// D-859, D-670: a deadlocked root blocked in a `receive` unwinds from it,
/// taking nothing, and finds the report at its catch point.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_cancel_unwinds_a_root_blocked_in_a_receive() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let (detector_tx, detector_rx) = mpsc::channel();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            me.set_wait_frame(0x7000_1230);
            let (tx, rx) = channel(me, 0);
            let _ = detector_tx.send(detect(me, &root.base, WaitOp::Receive));
            seen.set("receive", step_shape(receive_step(me, rx)));
            seen.set("caught", catch(me, &root.base));
            drop_word(tx);
            drop_word(rx);
            seen.set("finished", root.finish(me));
            7
        });
        let detector = detector_rx.recv().expect("no detector");
        assert_eq!(
            detector.join().expect("the detector panicked"),
            Some((WaitOp::Receive, 0x7000_1230))
        );
        notes.require(
            threads,
            &[
                ("receive", format!("raised {CANCEL}")),
                ("caught", caught()),
                ("finished", shown(true)),
            ],
        );
    }
}

/// D-859, D-667, D-668: a deadlocked root blocked in a `join` of a fiber
/// of its scope, which is blocked in a `receive`: both unwind, the joined
/// fiber fails, and the scope's close passes the cancellation through.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_cancel_unwinds_a_root_blocked_in_a_join_and_its_scoped_fiber() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let (detector_tx, detector_rx) = mpsc::channel();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let member_notes = seen.clone();
            let (member, _) = spawn_job(me, scope, &[rx], move |me, words| {
                member_notes.set("member", step_shape(receive_step(me, words[0])));
                0
            });
            // The member waits before the root does: one that the walk
            // reached before it started would never call its function.
            wait_for_waiters(me, tx, 0, 1);
            let _ = detector_tx.send(detect(me, &root.base, WaitOp::Join));
            seen.set("join", join_step(me, member).map(drop_word));
            seen.set("close", close_unwound(me, scope));
            seen.set("close flag", flag_of(me));
            seen.set("no failure", me.conc.take_last_failure());
            seen.set("member outcome", cell_of(member).outcome());
            seen.set("caught", catch(me, &root.base));
            drop_word(member);
            drop_word(tx);
            seen.set("finished", root.finish(me));
            7
        });
        let detector = detector_rx.recv().expect("no detector");
        assert_eq!(
            detector
                .join()
                .expect("the detector panicked")
                .map(|(op, _)| op),
            Some(WaitOp::Join)
        );
        notes.require(
            threads,
            &[
                ("member", format!("raised {CANCEL}")),
                ("join", shown(Err::<(), _>(CANCEL))),
                ("close", shown(false)),
                ("close flag", shown(CANCEL)),
                ("no failure", shown(None::<super::Failure>)),
                ("member outcome", shown(Outcome::Failed)),
                ("caught", caught()),
                ("finished", shown(true)),
            ],
        );
    }
}

/// D-859, D-670: a deadlocked root blocked in a `select` unwinds from it
/// and takes nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_cancel_unwinds_a_root_blocked_in_a_select() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let (detector_tx, detector_rx) = mpsc::channel();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            let (tx, rx) = channel(me, 0);
            let _ = detector_tx.send(detect(me, &root.base, WaitOp::Select));
            seen.set("select", select_step(me, rx));
            seen.set("holds", me.conc.select.holds_value());
            seen.set("caught", catch(me, &root.base));
            drop_word(tx);
            drop_word(rx);
            seen.set("finished", root.finish(me));
            7
        });
        let detector = detector_rx.recv().expect("no detector");
        assert_eq!(
            detector
                .join()
                .expect("the detector panicked")
                .map(|(op, _)| op),
            Some(WaitOp::Select)
        );
        notes.require(
            threads,
            &[
                ("select", shown(Err::<i64, _>(CANCEL))),
                ("holds", shown(false)),
                ("caught", caught()),
                ("finished", shown(true)),
            ],
        );
    }
}

/// D-859, D-667: a deadlocked root blocked at its scope's end, waiting for
/// a member blocked in a `receive` and another blocked in a `join` of it:
/// both members unwind and fail, and the close passes the cancellation
/// through instead of returning.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_cancel_unwinds_a_root_blocked_at_a_scopes_end_and_its_members() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let (detector_tx, detector_rx) = mpsc::channel();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let receiver_notes = seen.clone();
            let (receiver, _) = spawn_job(me, scope, &[rx], move |me, words| {
                receiver_notes.set("receiver", step_shape(receive_step(me, words[0])));
                0
            });
            let joiner_notes = seen.clone();
            let (joiner, _) = spawn_job(me, scope, &[dup(receiver)], move |me, words| {
                joiner_notes.set("joiner", join_step(me, words[0]).map(drop_word));
                0
            });
            // Both members wait before the root does, as in the join test.
            wait_for_waiters(me, tx, 0, 1);
            let receiver_cell = cell_of(receiver);
            while receiver_cell.joiners() != 1 {
                yield_now(me, 1);
            }
            drop(receiver_cell);
            let _ = detector_tx.send(detect(me, &root.base, WaitOp::Scope));
            seen.set("close", close(me, scope));
            seen.set("close flag", flag_of(me));
            seen.set("no failure", me.conc.take_last_failure());
            seen.set(
                "members",
                (cell_of(receiver).outcome(), cell_of(joiner).outcome()),
            );
            seen.set("caught", catch(me, &root.base));
            for word in [receiver, joiner, tx] {
                drop_word(word);
            }
            seen.set("finished", root.finish(me));
            7
        });
        let detector = detector_rx.recv().expect("no detector");
        assert_eq!(
            detector
                .join()
                .expect("the detector panicked")
                .map(|(op, _)| op),
            Some(WaitOp::Scope)
        );
        notes.require(
            threads,
            &[
                ("receiver", format!("raised {CANCEL}")),
                ("joiner", shown(Err::<(), _>(CANCEL))),
                ("close", shown(false)),
                ("close flag", shown(CANCEL)),
                ("no failure", shown(None::<super::Failure>)),
                ("members", shown((Outcome::Failed, Outcome::Failed))),
                ("caught", caught()),
                ("finished", shown(true)),
            ],
        );
    }
}

/// D-859, D-667, D-671: neither `fiber::timeout` (a timed scope) nor
/// `fiber::scope` around it catches a deadlock's cancellation: both closes
/// pass it through, neither returns `Err`, and the timed scope's timer is
/// cancelled on the way out.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_scope_cancel_passes_through_fiber_scope_and_fiber_timeout() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let (detector_tx, detector_rx) = mpsc::channel();
        run_jobs(threads, move |me| {
            let root = RootBase::of(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            // SAFETY: the running fiber's context; a limit of 1000 s.
            let timed = unsafe { cheby_rt_scope_open(ctx_of(me), true, 1000, 0) };
            let timed_state = state_of(timed);
            seen.set("timed", timed_state.timed());
            let _ = detector_tx.send(detect(me, &root.base, WaitOp::Receive));
            seen.set("receive", step_shape(receive_step(me, rx)));
            seen.set("timeout close", close_unwound(me, timed));
            seen.set("timeout flag", flag_of(me));
            seen.set("timer cancelled", !timed_state.timed());
            seen.set("timeout failure", me.conc.take_last_failure());
            seen.set("scope close", close_unwound(me, scope));
            seen.set("scope flag", flag_of(me));
            seen.set("scope failure", me.conc.take_last_failure());
            seen.set("caught", catch(me, &root.base));
            drop(timed_state);
            drop_word(tx);
            drop_word(rx);
            seen.set("finished", root.finish(me));
            7
        });
        let detector = detector_rx.recv().expect("no detector");
        assert_eq!(
            detector
                .join()
                .expect("the detector panicked")
                .map(|(op, _)| op),
            Some(WaitOp::Receive)
        );
        notes.require(
            threads,
            &[
                ("timed", shown(true)),
                ("receive", format!("raised {CANCEL}")),
                ("timeout close", shown(false)),
                ("timeout flag", shown(CANCEL)),
                ("timer cancelled", shown(true)),
                ("timeout failure", shown(None::<super::Failure>)),
                ("scope close", shown(false)),
                ("scope flag", shown(CANCEL)),
                ("scope failure", shown(None::<super::Failure>)),
                ("caught", caught()),
                ("finished", shown(true)),
            ],
        );
    }
}
