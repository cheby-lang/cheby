//! The wait sites of the five waiting kernels (D-856, D-858): the
//! `wait_site_*` tests.
//!
//! Before it parks, each kernel records in the fiber's scheduler block what
//! it waits in and the frame the generated caller published
//! (`ExecCtx::wait_frame`, which a test sets by hand: no JIT runs here),
//! and clears both once the fiber runs again, also when it does not park
//! after all because it is being cancelled. A watcher fiber of the root's
//! scope reads the root's site while the root waits, then ends the wait
//! (a receive, a send or its own return); the root reads its own site
//! afterwards. Fibers never assert: they write `Notes`, compared by the
//! test thread once the runtime has stopped, and every test ends with its
//! domain's live count at zero.

use std::ptr;

use super::cancel;
use super::cancel_tests::{
    CANCEL, NONE, Notes, Step, THREADS, close_unwound, flag_of, receive_step, run_jobs, send_step,
    shown, spawn_job, start_job, state_of, step_shape,
};
use super::join::cheby_fiber_sys_join;
use super::scope::cheby_rt_scope_open;
use super::select::{
    cheby_selector_sys_add_receive, cheby_selector_sys_new, cheby_selector_sys_wait,
    cheby_selector_take_selected,
};
use super::tests::{channel, close, ctx_of, drop_word, failure_of, open, yield_now};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::sched::WaitOp;
use crate::values;

/// The frame the root publishes before it waits, as generated code would.
const FRAME: usize = 0x7000_4560;

/// A wait site, for notes.
type Site = Option<(WaitOp, usize)>;

/// The running root's control block as an exposed address, for a watcher.
fn address(me: &ExecCtx) -> usize {
    ptr::from_ref(me).expose_provenance()
}

/// Yields until the root at `root` records a wait site (at most a million
/// times) and returns it.
fn watch(me: &ExecCtx, root: usize) -> Site {
    for _ in 0..1_000_000 {
        // SAFETY: the root waits for this fiber (it closes the scope this
        // fiber is a member of, or waits for what this fiber does next),
        // so its control block lives; only its atomics are read.
        let site = unsafe { &*ptr::with_exposed_provenance::<ExecCtx>(root) }
            .sched
            .wait_site();
        if site.is_some() {
            return site;
        }
        yield_now(me, 1);
    }
    None
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

/// `select` over one receive arm on the borrowed `rx`: what its arm took,
/// as an integer, or the flag when it raised.
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
                let value = match values::read_result(taken) {
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

/// D-858: a `send` that waits records `channel::send` and the published
/// frame, and clears both once a receiver took its value.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_send_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let watcher = seen.clone();
            start_job(me, scope, &[rx], move |me, words| {
                watcher.set("seen", watch(me, root));
                watcher.set("receive", step_shape(receive_step(me, words[0])));
                0
            });
            seen.set("before", me.sched.wait_site());
            seen.set("send", step_shape(send_step(me, tx, values::int_word(5))));
            seen.set("after", me.sched.wait_site());
            seen.set("close", close(me, scope));
            drop_word(tx);
            7
        });
        notes.require(
            threads,
            &[
                ("before", shown(None::<(WaitOp, usize)>)),
                ("seen", shown(Some((WaitOp::Send, FRAME)))),
                ("receive", "Ok(5)".to_owned()),
                ("send", "Ok(Nil)".to_owned()),
                ("after", shown(None::<(WaitOp, usize)>)),
                ("close", shown(true)),
            ],
        );
    }
}

/// D-858: a `receive` that waits records `channel::receive` and the
/// published frame, and clears both once a sender handed it a value.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_receive_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let watcher = seen.clone();
            start_job(me, scope, &[tx], move |me, words| {
                watcher.set("seen", watch(me, root));
                watcher.set(
                    "send",
                    step_shape(send_step(me, words[0], values::int_word(5))),
                );
                0
            });
            seen.set("receive", step_shape(receive_step(me, rx)));
            seen.set("after", me.sched.wait_site());
            seen.set("close", close(me, scope));
            drop_word(rx);
            7
        });
        notes.require(
            threads,
            &[
                ("seen", shown(Some((WaitOp::Receive, FRAME)))),
                ("send", "Ok(Nil)".to_owned()),
                ("receive", "Ok(5)".to_owned()),
                ("after", shown(None::<(WaitOp, usize)>)),
                ("close", shown(true)),
            ],
        );
    }
}

/// D-858: a `join` that waits records `fiber::join` and the published
/// frame, and clears both once the joined fiber returned.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_join_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            let scope = open(me);
            let watcher = seen.clone();
            let (fiber, _) = spawn_job(me, scope, &[], move |me, _| {
                watcher.set("seen", watch(me, root));
                values::int_word(9)
            });
            seen.set(
                "join",
                // SAFETY: a tagged integer, which needs no release.
                join_step(me, fiber).map(|word| unsafe { values::word_to_i64(word) }),
            );
            seen.set("after", me.sched.wait_site());
            seen.set("close", close(me, scope));
            drop_word(fiber);
            7
        });
        notes.require(
            threads,
            &[
                ("seen", shown(Some((WaitOp::Join, FRAME)))),
                ("join", shown(Ok::<i64, u32>(9))),
                ("after", shown(None::<(WaitOp, usize)>)),
                ("close", shown(true)),
            ],
        );
    }
}

/// D-858: a `select` that waits records `selector::select` and the
/// published frame, and clears both once its arm took a value.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_select_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            let (tx, rx) = channel(me, 0);
            let scope = open(me);
            let watcher = seen.clone();
            start_job(me, scope, &[tx], move |me, words| {
                watcher.set("seen", watch(me, root));
                watcher.set(
                    "send",
                    step_shape(send_step(me, words[0], values::int_word(5))),
                );
                0
            });
            seen.set("select", select_step(me, rx));
            seen.set("after", me.sched.wait_site());
            seen.set("close", close(me, scope));
            drop_word(rx);
            7
        });
        notes.require(
            threads,
            &[
                ("seen", shown(Some((WaitOp::Select, FRAME)))),
                ("send", "Ok(Nil)".to_owned()),
                ("select", shown(Ok::<i64, u32>(5))),
                ("after", shown(None::<(WaitOp, usize)>)),
                ("close", shown(true)),
            ],
        );
    }
}

/// D-858: a scope's end that waits for a member records `fiber::scope`
/// and the frame `scope_run`'s body published, and clears both once the
/// member left.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_scope_end_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            let scope = open(me);
            let watcher = seen.clone();
            start_job(me, scope, &[], move |me, _| {
                watcher.set("seen", watch(me, root));
                0
            });
            seen.set("close", close(me, scope));
            seen.set("after", me.sched.wait_site());
            7
        });
        notes.require(
            threads,
            &[
                ("seen", shown(Some((WaitOp::Scope, FRAME)))),
                ("close", shown(true)),
                ("after", shown(None::<(WaitOp, usize)>)),
            ],
        );
    }
}

/// D-858, D-671: the end of a timed scope, `fiber::timeout`'s, that waits
/// records `fiber::timeout` and the published frame, and clears both once
/// the member left before the limit.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_timeout_end_records_and_clears() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let root = address(me);
            // SAFETY: the running fiber's context; a limit of 1000 s.
            let scope = unsafe { cheby_rt_scope_open(ctx_of(me), true, 1000, 0) };
            let watcher = seen.clone();
            start_job(me, scope, &[], move |me, _| {
                watcher.set("seen", watch(me, root));
                0
            });
            seen.set("close", close(me, scope));
            seen.set("after", me.sched.wait_site());
            7
        });
        notes.require(
            threads,
            &[
                ("seen", shown(Some((WaitOp::Timeout, FRAME)))),
                ("close", shown(true)),
                ("after", shown(None::<(WaitOp, usize)>)),
            ],
        );
    }
}

/// D-858, D-670: a `send`, `receive`, `select` or `join` that finds its
/// fiber cancelled when it publishes its wait does not park, and leaves no
/// wait site behind.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn wait_site_cleared_when_a_cancelled_wait_does_not_park() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            me.set_wait_frame(FRAME);
            let (tx, rx) = channel(me, 0);
            let outer = open(me);
            // A fiber to join that waits until the root lets it go.
            let (fiber, _) = spawn_job(me, outer, &[rx], |me, words| {
                if let Ok(Ok(word)) = receive_step(me, words[0]) {
                    drop_word(word);
                }
                0
            });
            let (other_tx, other_rx) = channel(me, 0);
            for name in ["send", "receive", "select", "join"] {
                let scope = open(me);
                cancel::set_target(me, &state_of(scope));
                let raised = match name {
                    "send" => send_step(me, other_tx, values::int_word(1)).err(),
                    "receive" => receive_step(me, other_rx).err(),
                    "select" => select_step(me, other_rx).err(),
                    _ => join_step(me, fiber).err(),
                };
                seen.set(&format!("{name} raised"), raised);
                seen.set(&format!("{name} site"), me.sched.wait_site());
                seen.set(&format!("{name} close"), close_unwound(me, scope));
                seen.set(&format!("{name} failure"), failure_of(me));
            }
            seen.set(
                "release",
                step_shape(send_step(me, tx, values::int_word(1))),
            );
            seen.set("outer close", close(me, outer));
            for word in [fiber, tx, other_tx, other_rx] {
                drop_word(word);
            }
            7
        });
        let mut expected = Vec::new();
        for name in ["send", "receive", "select", "join"] {
            expected.push((format!("{name} raised"), shown(Some(CANCEL))));
            expected.push((format!("{name} site"), shown(None::<(WaitOp, usize)>)));
            expected.push((format!("{name} close"), shown(false)));
            // An expired limit's shape: the scope was its own target.
            expected.push((format!("{name} failure"), shown((1_i64, "", ""))));
        }
        expected.push(("release".to_owned(), "Ok(Nil)".to_owned()));
        expected.push(("outer close".to_owned(), shown(true)));
        let expected: Vec<(&str, String)> = expected
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        notes.require(threads, &expected);
    }
}
