//! Tests of the lifecycle (D-073, D-669, D-683, D-708 to D-710, D-723,
//! D-782, D-785, D-786): teardown of a detached group wherever its fibers wait,
//! the halt, termination as an outcome and the code tokens' order. They use
//! the harness of `fiber_ops::detached_tests` (Rust actions run through a
//! test trampoline), read `CHEBY_RT_STRESS_MIGRATE`, and run at 1, 2 and 4
//! workers where they take a thread count. A test that could hang ends
//! through a channel or a timeout; a test of a halt runs in a child
//! process.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::testing;
use super::{CodeToken, DetachedGroup, Shared};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;
use crate::fiber_ops::detached_tests::{
    close_scope, config, detach, group_of, hold, new_channel, open_scope, receive_on, release,
    runtime, send_on, spawn_into, spin, until_cancelled, unwinding, yield_until,
};
use crate::fiber_ops::join::cheby_fiber_sys_join;
use crate::fiber_ops::select::{
    cheby_selector_sys_add_after, cheby_selector_sys_new, cheby_selector_sys_wait,
};
use crate::generic::tests::closure;
use crate::values;

const THREADS: [usize; 3] = [1, 2, 4];

/// How long a child process may take, at most, to exit.
const CHILD_LIMIT: Duration = Duration::from_secs(5);

fn ctx_ptr(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// Waits until `ready` holds on the host, or panics after [`testing::hang()`].
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "the host waited too long"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// On a runtime of `threads` workers, runs a root in a group of its own
/// that calls `start` (which spawns detached fibers and returns how many
/// will report themselves ready) and returns; waits until that many set
/// the flag `start` gets, and a little longer so they park; tears the
/// group down; then checks the report, that the group is empty and, after
/// shutting the runtime down, that nothing leaked.
fn teardown_case(
    threads: usize,
    start: impl Fn(&ExecCtx, Arc<AtomicUsize>) -> usize + Send + Sync + 'static,
) {
    let start = Arc::new(start);
    let runtime = runtime(threads);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let ready = Arc::new(AtomicUsize::new(0));
    let expected = Arc::new(AtomicUsize::new(0));
    let (flag, count) = (Arc::clone(&ready), Arc::clone(&expected));
    let outcome = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        count.store(start(me, flag), Ordering::SeqCst);
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    let members = expected.load(Ordering::SeqCst);
    wait_until(|| ready.load(Ordering::SeqCst) >= members);
    std::thread::sleep(Duration::from_millis(5));
    let report = runtime.teardown(&group);
    assert_eq!(report.skipped, 0);
    assert!(!report.halted);
    assert!(report.waited >= 1, "{report:?}");
    assert_eq!(group.live(), 0);
    assert!(!group.closing(), "an empty group stays closing");
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-708, D-710: a teardown cancels a spinner, which only an entry check
/// reaches.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_a_spinner() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            detach(me, move |me| {
                ready.fetch_add(1, Ordering::SeqCst);
                spin(me)
            });
            1
        });
    }
}

/// A detached fiber parked in `receive`: the teardown wins its wait node.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_receive() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            let (tx, rx) = new_channel(me, 1);
            let (tx, rx) = (hold_owned(tx), hold_owned(rx));
            detach(me, move |me| {
                ready.fetch_add(1, Ordering::SeqCst);
                let got = receive_on(me, rx.word());
                drop((tx, rx));
                assert!(unwinding(me), "the receive returned {got:?}");
                0
            });
            1
        });
    }
}

/// A detached fiber parked in a `send` to a full channel.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_send() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            let (tx, rx) = new_channel(me, 0);
            let (tx, rx) = (hold_owned(tx), hold_owned(rx));
            detach(me, move |me| {
                ready.fetch_add(1, Ordering::SeqCst);
                let value = values::string("never delivered");
                let sent = send_on(me, tx.word(), value);
                release(value);
                drop((tx, rx));
                assert!(unwinding(me), "the send returned {sent:?}");
                0
            });
            1
        });
    }
}

/// A detached fiber parked in `join` of a fiber of its own scope, which is
/// parked in `receive`: the walk reaches both, and the scope's close
/// passes the cancellation through.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_join() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            detach(me, move |me| {
                let (tx, rx) = new_channel(me, 1);
                let (tx, rx) = (hold_owned(tx), hold_owned(rx));
                let scope = open_scope(me);
                let fiber = spawn_into(me, scope, move |me| {
                    let _ = receive_on(me, rx.word());
                    drop((tx, rx));
                    0
                });
                ready.fetch_add(1, Ordering::SeqCst);
                // SAFETY: the running fiber's context and a live handle.
                let joined = unsafe { cheby_fiber_sys_join(ctx_ptr(me), fiber) };
                release(fiber);
                assert!(unwinding(me), "the join returned {joined:#x}");
                assert!(!close_scope(me, scope));
                assert!(unwinding(me), "the close did not pass the cancellation");
                0
            });
            1
        });
    }
}

/// A detached fiber parked in a `select` whose only arm is a timer (the
/// heartbeat of 019 between beats): the select's published node is won,
/// and its timer is cancelled.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_timer_select() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            detach(me, move |me| {
                let ctx = ctx_ptr(me);
                let apply = closure(&[values::int_word(0)]);
                // SAFETY: the running fiber's context, live words borrowed.
                let selector = unsafe {
                    let empty = cheby_selector_sys_new(ctx);
                    let timed = cheby_selector_sys_add_after(ctx, empty, 60, 0, apply);
                    release(empty);
                    timed
                };
                release(apply);
                ready.fetch_add(1, Ordering::SeqCst);
                // SAFETY: as above.
                let chosen = unsafe { cheby_selector_sys_wait(ctx, selector) };
                release(selector);
                assert!(unwinding(me), "the select chose {chosen:#x}");
                assert_eq!(me.sched.runtime().unwrap().timers_pending(), 0);
                0
            });
            1
        });
    }
}

/// A detached fiber parked in a `select` with no arm.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_armless_select() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            detach(me, move |me| {
                let ctx = ctx_ptr(me);
                // SAFETY: the running fiber's context.
                let selector = unsafe { cheby_selector_sys_new(ctx) };
                ready.fetch_add(1, Ordering::SeqCst);
                // SAFETY: as above, with a live selector borrowed.
                let chosen = unsafe { cheby_selector_sys_wait(ctx, selector) };
                release(selector);
                assert!(unwinding(me), "the select chose {chosen:#x}");
                0
            });
            1
        });
    }
}

/// A detached fiber waiting in a scope's close for a member parked in
/// `receive`: the member is cancelled and leaves, and the close passes the
/// cancellation through instead of returning.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_scope_close() {
    for threads in THREADS {
        teardown_case(threads, |me, ready| {
            detach(me, move |me| {
                let (tx, rx) = new_channel(me, 1);
                let (tx, rx) = (hold_owned(tx), hold_owned(rx));
                let scope = open_scope(me);
                release(spawn_into(me, scope, move |me| {
                    let _ = receive_on(me, rx.word());
                    drop((tx, rx));
                    0
                }));
                ready.fetch_add(1, Ordering::SeqCst);
                assert!(!close_scope(me, scope), "the close returned to user code");
                assert!(unwinding(me), "the close did not pass the cancellation");
                0
            });
            1
        });
    }
}

/// D-667, D-769: a detached fiber with scopes three deep, a spinning
/// member in each, waiting in the innermost close: every close passes the
/// cancellation through and none returns `Err` to user code.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_nested_scopes_three_deep() {
    for threads in THREADS {
        let returned_err = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&returned_err);
        teardown_case(threads, move |me, ready| {
            let seen = Arc::clone(&seen);
            detach(me, move |me| {
                let mut scopes = Vec::new();
                for _ in 0..3 {
                    let scope = open_scope(me);
                    release(spawn_into(me, scope, spin));
                    scopes.push(scope);
                }
                ready.fetch_add(1, Ordering::SeqCst);
                for scope in scopes.into_iter().rev() {
                    let was_unwinding = unwinding(me);
                    let ok = close_scope(me, scope);
                    if !was_unwinding && !unwinding(me) && !ok {
                        seen.store(true, Ordering::SeqCst);
                    }
                    assert!(unwinding(me), "a close did not pass the cancellation");
                }
                0
            });
            1
        });
        assert!(
            !returned_err.load(Ordering::SeqCst),
            "an inner scope returned Err to user code"
        );
    }
}

/// D-800: a detached spawn into a group that
/// is being torn down starts cancelled: its function never runs, and it
/// still leaves the group, which the teardown waits for.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_a_spawn_into_a_closing_group_never_runs() {
    for threads in THREADS {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        teardown_case(threads, move |me, ready| {
            let flag = Arc::clone(&flag);
            detach(me, move |me| {
                let group = group_of(me);
                ready.fetch_add(1, Ordering::SeqCst);
                // No entry check here: the teardown's cancellation waits
                // for one, so this fiber sees the group closing first.
                let started = Instant::now();
                while !group.closing() {
                    assert!(started.elapsed() < testing::hang());
                    std::hint::spin_loop();
                }
                detach(me, move |_| {
                    flag.store(true, Ordering::SeqCst);
                    values::nil()
                });
                until_cancelled(me)
            });
            1
        });
        assert!(!ran.load(Ordering::SeqCst), "the function ran");
    }
}

/// An owned reference to `word`, which the guard releases; takes over the
/// caller's reference.
fn hold_owned(word: Word) -> crate::fiber_ops::Owned {
    let held = hold(word);
    release(word);
    held
}

/// D-708: `shutdown` tears down every group a root ran in, and the
/// default group, before it stops the workers; nothing leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_shutdown_tears_every_group_down() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let groups = [DetachedGroup::new(), DetachedGroup::new()];
        let started = Arc::new(AtomicUsize::new(0));
        for group in &groups {
            let flag = Arc::clone(&started);
            let outcome = testing::start_root_in(&runtime, &config(&domain), group, move |me| {
                detach(me, move |me| {
                    flag.fetch_add(1, Ordering::SeqCst);
                    spin(me)
                });
                values::nil()
            })
            .recv_timeout(testing::hang())
            .unwrap();
            assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        }
        let flag = Arc::clone(&started);
        let outcome = testing::start_root_with(&runtime, &config(&domain), move |me| {
            let (tx, rx) = new_channel(me, 1);
            let (tx, rx) = (hold_owned(tx), hold_owned(rx));
            detach(me, move |me| {
                flag.fetch_add(1, Ordering::SeqCst);
                let _ = receive_on(me, rx.word());
                drop((tx, rx));
                0
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| started.load(Ordering::SeqCst) == 3);
        runtime.shutdown();
        assert!(groups.iter().all(|group| group.live() == 0));
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// The fake code of a JIT for [`lifecycle_code_tokens_outlive_every_running_fiber`]:
/// its `Drop` checks that no worker of the runtime runs a fiber and that
/// every worker thread has ended (D-708, I-6).
struct FakeCode {
    shared: Arc<Shared>,
    dropped_early: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Drop for FakeCode {
    fn drop(&mut self) {
        let running = self
            .shared
            .workers
            .iter()
            .any(|worker| !worker.running().is_null());
        // The runtime's workers hold the other references to `Shared`;
        // only this one and the runtime's own remain once they ended.
        let threads_left = Arc::strong_count(&self.shared) > 2;
        if running || threads_left {
            self.dropped_early.store(true, Ordering::SeqCst);
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// D-708, I-6: a runtime releases the code tokens it holds only after its
/// fibers have stopped: a detached spinner runs the "code" while the
/// test's own clone of the token (the JIT) is dropped first, then
/// `shutdown` tears the spinner down and stops the workers, and only then
/// drops the code.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_code_tokens_outlive_every_running_fiber() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let dropped_early = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let token = CodeToken::new(Arc::new(FakeCode {
            shared: Arc::clone(&runtime.shared),
            dropped_early: Arc::clone(&dropped_early),
            dropped: Arc::clone(&dropped),
        }));
        runtime.attach(token.clone());
        let spinning = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&spinning);
        let outcome = testing::start_root_with(&runtime, &config(&domain), move |me| {
            detach(me, move |me| {
                flag.store(true, Ordering::SeqCst);
                spin(me)
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| spinning.load(Ordering::SeqCst));
        // "Drop the JIT" while the spinner runs.
        drop(token);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "the code went while a fiber ran"
        );
        runtime.shutdown();
        assert!(
            dropped.load(Ordering::SeqCst),
            "the code was never released"
        );
        assert!(
            !dropped_early.load(Ordering::SeqCst),
            "the code was released while a worker could run it"
        );
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// Runs this test binary's test `name` alone in a child process with the
/// environment variable `child` set and standard input piped and kept
/// open; returns its status and standard error, or panics if it does not
/// exit within [`CHILD_LIMIT`].
fn child_within_limit(name: &str, child: &str) -> (std::process::ExitStatus, String) {
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(child, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = process.stdin.take();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > CHILD_LIMIT {
            process.kill().ok();
            process.wait().ok();
            panic!("the child process did not exit within {CHILD_LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drop(stdin);
    let mut stderr = String::new();
    if let Some(pipe) = process.stderr.take() {
        for line in BufReader::new(pipe).lines() {
            stderr.push_str(&line.unwrap_or_default());
            stderr.push('\n');
        }
    }
    (status, stderr)
}

/// D-786, D-814: a detached fiber waiting on the
/// blocking pool in `io::read_line` on a pipe that stays open cannot be
/// stopped: it stays marked while the job runs, so the teardown skips it
/// and counts it; `shutdown` joins every worker (the fiber holds none),
/// the job's completion later wakes nothing (D-815),
/// and the process exits promptly with status 0 (debug and release
/// builds). The name predates the pool, when the read blocked its worker.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_skips_inline_io() {
    const CHILD: &str = "CHEBY_RT_TEST_TEARDOWN_INLINE_IO";
    const NAME: &str = "sched::lifecycle_tests::lifecycle_teardown_skips_inline_io";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, |me| {
            detach(me, |me| {
                // SAFETY: the running fiber's context.
                let line = unsafe { crate::io::cheby_io_sys_read_line(ctx_ptr(me)) };
                release(line);
                if unwinding(me) { 0 } else { values::nil() }
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| group.blocked() == 1);
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped, report.halted),
            (0, 1, false)
        );
        runtime.shutdown();
        eprintln!("skipped {}", report.skipped);
        return;
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("skipped 1\n"), "{stderr}");
}

/// D-786, D-804 (review finding CONC-W2-1 of gate 2): the blocked fiber is
/// not a member but a scoped child of one. The detached fiber waits in its
/// scope's close for a child blocked in `io::read_line` (on the blocking
/// pool, marked, D-814) on a pipe that stays open, and a
/// spinning sibling is cancelled and leaves. The
/// teardown skips the member, counts the blocked child, and the process
/// exits promptly with status 0. Before the fix the teardown polled until
/// the read returned.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_skips_a_scoped_child_blocked_in_inline_io() {
    const CHILD: &str = "CHEBY_RT_TEST_TEARDOWN_INLINE_IO_CHILD";
    const NAME: &str =
        "sched::lifecycle_tests::lifecycle_teardown_skips_a_scoped_child_blocked_in_inline_io";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, |me| {
            detach(me, |me| {
                let scope = open_scope(me);
                release(spawn_into(me, scope, |me| {
                    // SAFETY: the running fiber's context.
                    let line = unsafe { crate::io::cheby_io_sys_read_line(ctx_ptr(me)) };
                    release(line);
                    if unwinding(me) { 0 } else { values::nil() }
                }));
                release(spawn_into(me, scope, until_cancelled));
                assert!(!close_scope(me, scope), "the close returned to user code");
                0
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| group.blocked() == 1);
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped, report.halted),
            (0, 1, false)
        );
        assert_eq!(group.live(), 1, "the skipped member left its group");
        runtime.shutdown();
        eprintln!("skipped {}", report.skipped);
        return;
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("skipped 1\n"), "{stderr}");
}

/// D-786, D-804: a teardown does not skip a member whose subtree still
/// makes progress. The detached fiber waits in its scope's close for a
/// child blocked in `io::read_line` (on the blocking pool, marked) and for
/// a sibling that, once cancelled,
/// keeps running for 100 ms before it unwinds; the teardown waits for the
/// sibling to leave (it ran past the 100 ms) before it skips the member.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_waits_for_a_busy_sibling_of_a_blocked_fiber() {
    const CHILD: &str = "CHEBY_RT_TEST_TEARDOWN_INLINE_IO_BUSY";
    const NAME: &str =
        "sched::lifecycle_tests::lifecycle_teardown_waits_for_a_busy_sibling_of_a_blocked_fiber";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        // The sibling must have started before the teardown: a fiber
        // cancelled before its first run never calls its function (D-666),
        // so it would leave at once instead of running on.
        let running = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&running);
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
            detach(me, move |me| {
                let scope = open_scope(me);
                release(spawn_into(me, scope, |me| {
                    // SAFETY: the running fiber's context.
                    let line = unsafe { crate::io::cheby_io_sys_read_line(ctx_ptr(me)) };
                    release(line);
                    if unwinding(me) { 0 } else { values::nil() }
                }));
                release(spawn_into(me, scope, move |me| {
                    started.store(true, Ordering::SeqCst);
                    until_cancelled(me);
                    // Busy after the cancellation, with no entry check.
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_millis(100) {
                        std::hint::spin_loop();
                    }
                    done.store(true, Ordering::SeqCst);
                    0
                }));
                assert!(!close_scope(me, scope), "the close returned to user code");
                0
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| group.blocked() == 1 && running.load(Ordering::SeqCst));
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped, report.halted),
            (0, 1, false)
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "the teardown skipped while a sibling still ran"
        );
        runtime.shutdown();
        eprintln!("skipped {}", report.skipped);
        return;
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("skipped 1\n"), "{stderr}");
}

/// D-817, D-786: when the system refuses a blocking-pool
/// thread and none runs, `io::read_line` runs inline on its worker, marked,
/// with the hand-off: the teardown skips the reader and counts it,
/// `shutdown` leaves that worker running, never joined, and the process
/// exits promptly with status 0.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_teardown_skips_io_inline_after_a_refused_pool_thread() {
    const CHILD: &str = "CHEBY_RT_TEST_TEARDOWN_REFUSED_POOL";
    const NAME: &str =
        "sched::lifecycle_tests::lifecycle_teardown_skips_io_inline_after_a_refused_pool_thread";
    if std::env::var_os(CHILD).is_some() {
        let pool = crate::pool::Pool::global();
        pool.refuse_threads(true);
        let runtime = runtime(2);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, |me| {
            detach(me, |me| {
                // SAFETY: the running fiber's context.
                let line = unsafe { crate::io::cheby_io_sys_read_line(ctx_ptr(me)) };
                release(line);
                if unwinding(me) { 0 } else { values::nil() }
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| group.blocked() == 1);
        // The submission counts a thread while it tries to start one, so
        // both conditions are awaited, briefly.
        let started = Instant::now();
        while !(pool.threads() == 0 && (0..2).any(|index| runtime.shared.worker_blocked(index))) {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the read does not block a worker inline ({} pool threads)",
                pool.threads()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped, report.halted),
            (0, 1, false)
        );
        runtime.shutdown();
        eprintln!("skipped {}", report.skipped);
        return;
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("skipped 1\n"), "{stderr}");
}

/// D-073, D-708: the halt freezes a detached spinner and leaves a parked
/// detached fiber parked, joins every thread and returns; nothing is
/// retired, and the process exits promptly with status 0.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_halt_freezes_a_spinner_and_a_parked_fiber() {
    const CHILD: &str = "CHEBY_RT_TEST_HALT";
    const NAME: &str =
        "sched::lifecycle_tests::lifecycle_halt_freezes_a_spinner_and_a_parked_fiber";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        let shared = Arc::clone(&runtime.shared);
        let domain = Domain::new();
        let started = Arc::new(AtomicUsize::new(0));
        let (spinner, parked) = (Arc::clone(&started), Arc::clone(&started));
        let outcome = testing::start_root_with(&runtime, &config(&domain), move |me| {
            detach(me, move |me| {
                spinner.fetch_add(1, Ordering::SeqCst);
                spin(me)
            });
            let (tx, rx) = new_channel(me, 1);
            let (tx, rx) = (hold_owned(tx), hold_owned(rx));
            detach(me, move |me| {
                parked.fetch_add(1, Ordering::SeqCst);
                let _ = receive_on(me, rx.word());
                drop((tx, rx));
                0
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| started.load(Ordering::SeqCst) == 2);
        // The receiver is parked; the spinner is not.
        wait_until(|| shared.default_group().parked() == 1);
        let retired_before = shared.live.load(Ordering::SeqCst);
        runtime.halt();
        assert_eq!(
            shared.probe.frozen.load(Ordering::SeqCst),
            1,
            "not one fiber froze"
        );
        assert_eq!(
            shared.live.load(Ordering::SeqCst),
            retired_before,
            "a fiber was retired"
        );
        assert!(
            shared
                .workers
                .iter()
                .all(|worker| worker.running().is_null())
        );
        eprintln!("halted");
        return;
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("halted\n"), "{stderr}");
}

/// D-709: a termination fixes the program's status once; the first wins.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_terminate_first_status_wins() {
    let runtime = runtime(1);
    let shared = runtime.shared_handle();
    assert_eq!(shared.exit_status(), None);
    assert_eq!(shared.fix_status(3), 3);
    assert_eq!(shared.fix_status(4), 3);
    assert_eq!(shared.exit_status(), Some(3));
    runtime.shutdown();
}

/// D-709: a termination on a fiber that is not a root fixes the status,
/// halts the world and sends `Exited` to the host waiting for the root,
/// which is frozen or parked; a root started afterwards never runs and its
/// host gets `Exited` at once; the host completes the halt. Nothing ends
/// the process: this test runs in-process.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_terminate_from_a_non_root_fiber() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let shared = runtime.shared_handle();
        let domain = Domain::new();
        let outcome = testing::start_root_with(&runtime, &config(&domain), |me| {
            let (tx, rx) = new_channel(me, 1);
            detach(me, |_| crate::sched::terminate(3));
            // The root waits; the termination freezes or strands it.
            let got = receive_on(me, rx);
            release(tx);
            release(rx);
            panic!("the root went on after the termination: {got:?}");
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert_eq!(outcome, EntryOutcome::Exited(3));
        assert_eq!(shared.exit_status(), Some(3));
        assert!(shared.terminated() && shared.halting());
        let late = testing::start_root_with(&runtime, &config(&domain), |_| {
            panic!("a root ran after the termination")
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert_eq!(late, EntryOutcome::Exited(3));
        runtime.halt();
        assert!(
            shared
                .workers
                .iter()
                .all(|worker| worker.running().is_null())
        );
    }
}

/// A thunk that terminates its isolated entry with status 5.
unsafe extern "C" fn terminate_isolated(_ctx: *mut ExecCtx) -> u64 {
    crate::sched::terminate(5)
}

/// D-709: a termination on an isolated entry sends `Exited` to its host,
/// which returns without joining the entry's thread, parked for good.
#[test]
fn lifecycle_terminate_on_an_isolated_entry() {
    let domain = Domain::new();
    // SAFETY: a test thunk with the entry ABI.
    let outcome = unsafe { crate::entry::run_entry(terminate_isolated, &config(&domain)) };
    assert_eq!(outcome.unwrap(), EntryOutcome::Exited(5));
}

/// D-709, D-708: a detached fiber terminates while its group is torn down
/// (the teardown's cancellation reached it, but it fails a write before an
/// entry check delivers it): the teardown stops waiting, since a frozen
/// fiber never leaves its group, and reports `halted`; the host completes
/// the halt. This test runs in-process and survives.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_terminate_during_teardown() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let shared = runtime.shared_handle();
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let ready = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ready);
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
            detach(me, move |me| {
                flag.store(true, Ordering::SeqCst);
                // No entry check: the teardown's cancellation is not
                // delivered before the write fails.
                let started = Instant::now();
                while crate::fiber_ops::cancel::hooks::target(me).is_none() {
                    assert!(started.elapsed() < testing::hang());
                    std::hint::spin_loop();
                }
                crate::sched::terminate(1)
            });
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        wait_until(|| ready.load(Ordering::SeqCst));
        let report = runtime.teardown(&group);
        assert!(report.halted, "{report:?}");
        assert_eq!(group.live(), 1, "the frozen fiber left its group");
        runtime.halt();
        assert_eq!(shared.exit_status(), Some(1));
    }
}

/// D-708, D-709: `Drop` and `shutdown` of a runtime whose halt has started
/// (a termination) complete the halt and never tear down: a detached fiber
/// parked in `receive` stays parked, never woken by a teardown's walk.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_drop_after_terminate() {
    for threads in THREADS {
        for by_drop in [true, false] {
            let runtime = runtime(threads);
            let shared = runtime.shared_handle();
            let domain = Domain::new();
            let outcome = testing::start_root_with(&runtime, &config(&domain), |me| {
                let (tx, rx) = new_channel(me, 1);
                let (tx, rx) = (hold_owned(tx), hold_owned(rx));
                detach(me, move |me| {
                    let _ = receive_on(me, rx.word());
                    drop((tx, rx));
                    0
                });
                let group = group_of(me);
                yield_until(me, || group.parked() == 1);
                crate::sched::terminate(2)
            })
            .recv_timeout(testing::hang())
            .unwrap();
            assert_eq!(outcome, EntryOutcome::Exited(2));
            if by_drop {
                drop(runtime);
            } else {
                runtime.shutdown();
            }
            assert_eq!(shared.default_group().parked(), 1, "a teardown woke it");
            assert_eq!(shared.default_group().live(), 1);
        }
    }
}

/// `main`'s thunk for the `run_main` tests: spawns a detached fiber that
/// waits in `receive` on a channel it keeps open and a spinner, waits
/// until the first is parked and the second runs, and returns 0. (A
/// detached fiber torn down before it starts never runs its action, whose
/// guards would then keep the channel's ends: the harness's actions are
/// not the closure's captures.)
unsafe extern "C" fn main_with_a_parked_detached_fiber(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the root's context.
    let me = unsafe { &*ctx };
    let (tx, rx) = new_channel(me, 1);
    let (tx, rx) = (hold_owned(tx), hold_owned(rx));
    detach(me, move |me| {
        let _ = receive_on(me, rx.word());
        drop((tx, rx));
        0
    });
    let spinning = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&spinning);
    detach(me, move |me| {
        flag.store(true, Ordering::SeqCst);
        spin(me)
    });
    let group = group_of(me);
    yield_until(me, || {
        group.parked() == 1 && spinning.load(Ordering::SeqCst)
    });
    0
}

/// The configuration of the `run_main` tests: two workers, the test
/// trampoline, a fresh domain.
fn run_config(at_exit: crate::entry::AtExit) -> crate::entry::RunConfig {
    crate::entry::RunConfig {
        callbacks: crate::fiber_ops::detached_tests::callbacks(),
        domain: Domain::new(),
        threads: Some(2),
        at_exit,
        ..crate::entry::RunConfig::default()
    }
}

/// D-708, D-710: `run_main` tears `main`'s detached fibers down once the
/// status is fixed (a spinner and a parked receiver), then runs the debug
/// leak check, which finds nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_run_main_tears_mains_group_down() {
    let config = run_config(crate::entry::AtExit::Teardown);
    let domain = Arc::clone(&config.domain);
    // SAFETY: a test thunk with the entry ABI.
    let status = unsafe { crate::entry::run_main(main_with_a_parked_detached_fiber, config) };
    assert_eq!(status.unwrap(), 0);
    assert_eq!(domain.live_objects(), 0);
}

/// Ends this child process with `status`, as `cheby run` ends with
/// `run_main`'s status, without the test harness writing to a standard
/// output that may be closed.
fn exit_child(status: i32) -> ! {
    // SAFETY: `_exit` has no preconditions; the child's work is done.
    unsafe { libc::_exit(status) }
}

/// D-073, D-708: under `AtExit::Freeze` `run_main` halts instead of tearing
/// down: a spinner and a parked receiver are frozen where they stand, and
/// the process exits promptly with `main`'s status.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_halt_run_main_freezes_under_freeze() {
    const CHILD: &str = "CHEBY_RT_TEST_RUN_MAIN_FREEZE";
    const NAME: &str = "sched::lifecycle_tests::lifecycle_halt_run_main_freezes_under_freeze";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: a test thunk with the entry ABI.
        let status = unsafe {
            crate::entry::run_main(
                main_with_a_parked_detached_fiber,
                run_config(crate::entry::AtExit::Freeze),
            )
        };
        eprintln!("run_main returned {status:?}");
        exit_child(status.unwrap_or(99));
    }
    let (status, stderr) = child_within_limit(NAME, CHILD);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("run_main returned Ok(0)"), "{stderr}");
}

/// `main`'s thunk for [`lifecycle_terminate_after_the_flush_keeps_the_status`]:
/// spawns a detached fiber that waits, without an entry check, until
/// `run_main` has fixed the status, then prints lines until a write fails.
unsafe extern "C" fn main_with_a_late_writer(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the root's context.
    let me = unsafe { &*ctx };
    detach(me, |me| {
        let runtime = Arc::clone(me.sched.runtime().unwrap());
        while runtime.exit_status().is_none() {
            std::hint::spin_loop();
        }
        eprintln!("writing");
        let line = values::string(&"x".repeat(1000));
        loop {
            // SAFETY: a live `String`, borrowed. A write that fails is a
            // termination, which never returns.
            unsafe { crate::io::cheby_io_println(line) };
        }
    });
    0
}

/// D-785 (`cheby run`'s twin of [`lifecycle_terminate_during_teardown`],
/// through a closed pipe): a detached fiber's write fails after `main`
/// returned and `run_main` flushed and fixed the status 0; the failing
/// fiber is frozen, the teardown stops, the halt runs, nothing more is
/// printed, and the process exits promptly with the fixed status 0.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lifecycle_terminate_after_the_flush_keeps_the_status() {
    const CHILD: &str = "CHEBY_RT_TEST_LATE_WRITE";
    const NAME: &str =
        "sched::lifecycle_tests::lifecycle_terminate_after_the_flush_keeps_the_status";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: a test thunk with the entry ABI.
        let status = unsafe {
            crate::entry::run_main(
                main_with_a_late_writer,
                run_config(crate::entry::AtExit::Teardown),
            )
        };
        eprintln!("run_main returned {status:?}");
        exit_child(status.unwrap_or(99));
    }
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = process.stdout.take();
    let mut stderr = BufReader::new(process.stderr.take().unwrap());
    let mut seen = String::new();
    while !seen.contains("writing\n") {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
    }
    // Close the pipe: the writer's next write fails.
    drop(stdout);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > CHILD_LIMIT {
            process.kill().ok();
            process.wait().ok();
            panic!("the child process did not exit within {CHILD_LIMIT:?}: {seen}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    for line in stderr.lines() {
        seen.push_str(&line.unwrap_or_default());
        seen.push('\n');
    }
    assert_eq!(status.code(), Some(0), "{seen}");
    assert!(seen.contains("run_main returned Ok(0)"), "{seen}");
}
