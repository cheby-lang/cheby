//! Tests of the scope of `process::until_interrupted` (D-1073, ADR-0071;
//! `fiber_ops/interrupt.rs`, `sched/signal.rs`): the `until_interrupted_*`
//! runtime tests.
//!
//! The body of the scope runs on a root fiber between the open and the
//! close, as `scope_run`'s compiled body does in `fiber::interruptible`,
//! and arms the scope first with the kernel. A signal is synthetic: one
//! byte written to the self-pipe, as the handler writes it
//! (`signal::write_synthetic`), except in the one test that raises a real
//! signal once the handler is installed. The handler is checked on
//! SIGTERM: a test binary started in the background by a non-interactive
//! shell (the amd64 container's steps) inherits SIGINT ignored, which the
//! runtime rightly leaves ignored. A signal that would take its
//! default action again is recorded (`signal::hooks::RERAISED`) instead
//! of ending the test binary. Fibers never assert: each sends what it saw
//! to the test thread, which compares it once the runtime has stopped. The
//! registry and the dispositions are the process's, so every test holds
//! `signal::hooks::SERIAL`. Signals reach no scope on Windows until M6,
//! so the module is Unix only.
#![cfg(unix)]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::cancel_tests::state_of;
use super::channel::cheby_channel_receive;
use super::interrupt::cheby_fiber_sys_arm_interrupt;
use super::tests::{
    channel, close, close_unwound, config, ctx_of, drop_word, failure_of, open, yield_now,
};
use crate::abi::unwind_flag;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, run_entry};
use crate::sched::signal::{self, hooks};
use crate::sched::{end_test_root, testing};
use crate::unwind::flag;

/// The thread counts the tests run at.
const THREADS: [usize; 2] = [1, 4];

/// The failure of a scope that a signal cancelled: kind 1, empty texts,
/// which `fiber::interruptible` reads as an interruption.
fn interrupted() -> (i64, String, String) {
    (1, String::new(), String::new())
}

/// `fiber::sys_arm_interrupt` on the borrowed `Scope` handle `scope`.
fn arm(me: &ExecCtx, scope: u64) {
    // SAFETY: the running fiber's context and a live `Scope` handle it
    // opened.
    unsafe { cheby_fiber_sys_arm_interrupt(ctx_of(me), scope) };
}

/// The running fiber's unwind flag.
fn raised(me: &ExecCtx) -> bool {
    flag::get(me) != unwind_flag::NONE
}

/// `channel::receive` on `rx`; true when it raised (a cancellation), else
/// its result is dropped.
fn receive_raises(me: &ExecCtx, rx: u64) -> bool {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let word = unsafe { cheby_channel_receive(ctx_of(me), rx) };
    if raised(me) {
        return true;
    }
    drop_word(word);
    false
}

/// Yields until `ready` holds, at most [`testing::hang`]; whether it held.
fn yield_until(me: &ExecCtx, ready: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() > testing::hang() {
            return false;
        }
        yield_now(me, 1);
    }
    true
}

/// Waits on the test thread until `ready` holds, at most
/// [`testing::hang`]; whether it held.
fn wait_until(ready: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() > testing::hang() {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

/// The registry's and the process's state: armed scopes, the handler by
/// the registry's account, and SIGTERM's disposition (module
/// documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    armed: usize,
    installed: bool,
    handler: bool,
}

fn state() -> State {
    State {
        armed: signal::armed(),
        installed: signal::installed(),
        handler: signal::handler_installed(libc::SIGTERM),
    }
}

/// One scope armed, the handler installed.
const ARMED: State = State {
    armed: 1,
    installed: true,
    handler: true,
};

/// Nothing armed, the dispositions as before.
const IDLE: State = State {
    armed: 0,
    installed: false,
    handler: false,
};

/// Runs `body` as a root fiber in test mode on `runtime`, requires that it
/// returns `7` with nothing leaked, and returns what it sent.
fn run_root<T: Send + 'static>(
    runtime: &crate::sched::Runtime,
    body: impl FnOnce(&ExecCtx, &mpsc::Sender<T>) -> u64 + Send + 'static,
) -> Vec<T> {
    let domain = Domain::new();
    let (tx, rx) = mpsc::channel();
    let outcome = testing::start_root_with(runtime, &config(&domain), move |me| body(me, &tx))
        .recv_timeout(testing::hang())
        .expect("the root fiber hung");
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    rx.try_iter().collect()
}

/// Takes the serial lock and starts with no recorded signal and nothing
/// armed, as in a fresh process (`hooks::reset`; each loop over the thread
/// counts resets again).
fn serial() -> parking_lot::MutexGuard<'static, ()> {
    let guard = hooks::SERIAL.lock();
    assert_eq!(state(), IDLE, "a scope is armed before the test");
    hooks::reset();
    guard
}

/// D-1073: a signal event on the poller cancels an open scope with the
/// scope as the target; its body, blocked in a `receive` nobody answers,
/// unwinds, and the close fails it as kind 1 with empty texts. While it is
/// armed the handler is installed; the walk restores the default action,
/// and the close disarms it.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_a_synthetic_signal_cancels_an_open_scope() {
    let _serial = serial();
    for threads in THREADS {
        hooks::reset();
        let runtime = testing::with_signals(threads, false);
        let seen = run_root(&runtime, |me, out| {
            let scope = open(me);
            arm(me, scope);
            let _ = out.send(("armed", format!("{:?}", state())));
            let (tx, rx) = channel(me, 0);
            let wrote = signal::write_synthetic(0);
            let _ = out.send(("wrote", format!("{wrote}")));
            let unwound = receive_raises(me, rx);
            let _ = out.send(("unwound", format!("{unwound}")));
            let _ = out.send(("cancelled", format!("{:?}", state())));
            let succeeded = if unwound {
                close_unwound(me, scope)
            } else {
                close(me, scope)
            };
            let failure = (!succeeded).then(|| failure_of(me));
            let _ = out.send(("failure", format!("{failure:?}")));
            let _ = out.send(("closed", format!("{:?}", state())));
            drop_word(tx);
            drop_word(rx);
            7
        });
        runtime.shutdown();
        let expected = [
            ("armed", format!("{ARMED:?}")),
            ("wrote", "true".to_owned()),
            ("unwound", "true".to_owned()),
            ("cancelled", format!("{:?}", State { armed: 1, ..IDLE })),
            ("failure", format!("{:?}", Some(interrupted()))),
            ("closed", format!("{IDLE:?}")),
        ];
        assert_eq!(seen, expected, "at {threads} threads");
        assert!(hooks::RERAISED.lock().is_empty());
    }
}

/// D-1073: the first signal restores the default action, so a second one
/// while the shutdown is stuck takes it: with the handler, a second byte
/// drained while the scope is still open is sent again to the process.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_the_default_is_back_after_the_first() {
    let _serial = serial();
    for threads in THREADS {
        hooks::reset();
        let runtime = testing::with_signals(threads, false);
        let seen = run_root(&runtime, |me, out| {
            let scope = open(me);
            arm(me, scope);
            let (tx, rx) = channel(me, 0);
            signal::write_synthetic(0);
            let unwound = receive_raises(me, rx);
            // Still open: its body is unwinding, a stuck shutdown.
            let _ = out.send(("first", format!("{unwound} {:?}", state())));
            signal::write_synthetic(1);
            let again = yield_until(me, || !hooks::RERAISED.lock().is_empty());
            let _ = out.send(("second", format!("{again} {:?}", hooks::RERAISED.lock())));
            let succeeded = close_unwound(me, scope);
            let failure = (!succeeded).then(|| failure_of(me));
            let _ = out.send(("failure", format!("{failure:?}")));
            drop_word(tx);
            drop_word(rx);
            7
        });
        runtime.shutdown();
        let expected = [
            ("first", format!("true {:?}", State { armed: 1, ..IDLE })),
            ("second", format!("true {:?}", vec![libc::SIGTERM])),
            ("failure", format!("{:?}", Some(interrupted()))),
        ];
        assert_eq!(seen, expected, "at {threads} threads");
        assert_eq!(state(), IDLE);
    }
}

/// D-1073: with no scope open the handler is not installed, so a signal
/// takes the default action; one that reached the pipe just before the
/// last scope closed is sent to the process again. A body that returned
/// gives `Ok` and leaves nothing armed.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_none_open_takes_the_default() {
    let _serial = serial();
    for threads in THREADS {
        hooks::reset();
        let runtime = testing::with_signals(threads, false);
        let seen = run_root(&runtime, |me, out| {
            let scope = open(me);
            arm(me, scope);
            let succeeded = close(me, scope);
            let _ = out.send(("closed", format!("{succeeded} {:?}", state())));
            signal::write_synthetic(0);
            let again = yield_until(me, || !hooks::RERAISED.lock().is_empty());
            let _ = out.send(("late", format!("{again} {:?}", hooks::RERAISED.lock())));
            7
        });
        runtime.shutdown();
        let expected = [
            ("closed", format!("true {IDLE:?}")),
            ("late", format!("true {:?}", vec![libc::SIGINT])),
        ];
        assert_eq!(seen, expected, "at {threads} threads");
    }
}

/// D-1073 (the review's RB10-1): a signal that came while one scope was
/// open is never charged to a scope armed after it. Its byte is drained
/// after the first scope closed, or while that scope closes with `Ok`
/// because its body returned first; either way it reached nothing and takes
/// its action again. The next scope, armed after the signal, is no target
/// and installs nothing: the default action stays for the rest of the
/// process, so a second Ctrl-C would end it.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_a_signal_is_never_charged_to_a_later_scope() {
    let _serial = serial();
    for threads in THREADS {
        hooks::reset();
        let runtime = testing::with_signals(threads, false);
        let seen = run_root(&runtime, |me, out| {
            let first = open(me);
            arm(me, first);
            signal::write_synthetic(0);
            let succeeded = close(me, first);
            let _ = out.send(("first", format!("{succeeded}")));
            let second = open(me);
            arm(me, second);
            let _ = out.send(("second armed", format!("{:?}", state())));
            let again = yield_until(me, || !hooks::RERAISED.lock().is_empty());
            let succeeded = close(me, second);
            let _ = out.send((
                "second",
                format!("{again} {succeeded} {:?}", hooks::RERAISED.lock()),
            ));
            let third = open(me);
            arm(me, third);
            let _ = out.send(("third armed", format!("{:?}", state())));
            let succeeded = close(me, third);
            let _ = out.send(("closed", format!("{succeeded} {:?}", state())));
            7
        });
        runtime.shutdown();
        let expected = [
            ("first", "true".to_owned()),
            ("second armed", format!("{:?}", State { armed: 1, ..IDLE })),
            ("second", format!("true true {:?}", vec![libc::SIGINT])),
            ("third armed", format!("{:?}", State { armed: 1, ..IDLE })),
            ("closed", format!("true {IDLE:?}")),
        ];
        assert_eq!(seen, expected, "at {threads} threads");
    }
}

/// D-1073, D-984: an open scope is not pending work, so a root whose only
/// fiber blocks inside one on a channel is still reported deadlocked; the
/// test's root unwinds through the scope, whose close disarms it.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_a_deadlock_inside_an_open_scope_is_still_reported() {
    let _serial = serial();
    for threads in THREADS {
        let runtime = testing::with_signals(threads, true);
        let domain = Domain::new();
        let root = testing::start_root_with(&runtime, &config(&domain), |me| {
            let scope = open(me);
            arm(me, scope);
            let (tx, rx) = channel(me, 0);
            if receive_raises(me, rx) {
                close_unwound(me, scope);
            } else {
                close(me, scope);
            }
            drop_word(tx);
            drop_word(rx);
            0
        });
        let outcome = root
            .recv_timeout(testing::hang())
            .expect("the root did not end");
        runtime.shutdown();
        let EntryOutcome::Deadlocked(report) = outcome else {
            panic!("not reported at {threads} threads: {outcome:?}");
        };
        assert!(report.contains("blocked in channel::receive"), "{report}");
        assert_eq!(domain.live_objects(), 0, "objects leaked");
        assert_eq!(state(), IDLE);
    }
}

/// D-1073: under `cheby test` (a runtime that gives its scopes no signal)
/// the kernel arms nothing and installs nothing, so Ctrl-C keeps the
/// host's behavior and the body runs as in any scope.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_no_signal_reaches_a_scope_under_cheby_test() {
    let _serial = serial();
    let runtime = testing::runtime(2);
    let seen = run_root(&runtime, |me, out| {
        let scope = open(me);
        arm(me, scope);
        let armed = state_of(scope).interrupt_armed();
        let _ = out.send(format!("{armed} {:?}", state()));
        let succeeded = close(me, scope);
        let _ = out.send(format!("{succeeded}"));
        7
    });
    runtime.shutdown();
    assert_eq!(seen, [format!("false {IDLE:?}"), "true".to_owned()]);
}

/// The isolated entry's state, for [`isolated`].
static ISOLATED: parking_lot::Mutex<Option<(bool, State, bool)>> = parking_lot::Mutex::new(None);

/// An entry thunk run without a runtime, as an isolated entry (D-713) and,
/// until M6, every entry on Windows: opens a scope, arms it and closes it.
unsafe extern "C" fn isolated(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's live context.
    let me = unsafe { &*ctx };
    let scope = open(me);
    arm(me, scope);
    let armed = state_of(scope).interrupt_armed();
    let seen = state();
    let succeeded = close(me, scope);
    *ISOLATED.lock() = Some((armed, seen, succeeded));
    0
}

/// D-1073, D-713: in an entry that runs without a runtime no signal
/// reaches the scope. (The REPL's session runtime gives none either: it is
/// made with `RuntimeConfig::signals` false, as `cheby test`'s, which
/// `until_interrupted_no_signal_reaches_a_scope_under_cheby_test` covers.)
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_no_signal_reaches_a_scope_without_a_runtime() {
    let _serial = serial();
    // SAFETY: `isolated` is an entry thunk with the `EntryFn` ABI.
    let outcome = unsafe { run_entry(isolated, &EntryConfig::new(EntryMode::Test)) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(0));
    assert_eq!(ISOLATED.lock().take(), Some((false, IDLE, true)));
}

/// D-1023 (S7a's `process::exit` under `cheby test`): `end_test_root` ends
/// the calling test as a deadlock does: the first report wins, the caller
/// must unwind, every scope passes the cancellation through, and the host
/// gets `Deadlocked` with the report.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn end_test_root_fails_the_calling_test_once() {
    for threads in THREADS {
        let runtime = testing::runtime(threads);
        let domain = Domain::new();
        let (out, seen) = mpsc::channel();
        let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
            let scope = open(me);
            let (tx, rx) = channel(me, 0);
            let first = end_test_root(me, "process::exit called with status 3\n".to_owned());
            let second = end_test_root(me, "process::exit called with status 4\n".to_owned());
            let unwound = receive_raises(me, rx);
            let _ = out.send(format!("{first} {second} {unwound}"));
            if unwound {
                close_unwound(me, scope);
            } else {
                close(me, scope);
            }
            let _ = out.send(format!("{}", raised(me)));
            drop_word(tx);
            drop_word(rx);
            0
        });
        let outcome = root
            .recv_timeout(testing::hang())
            .expect("the root did not end");
        runtime.shutdown();
        assert_eq!(
            outcome,
            EntryOutcome::Deadlocked("process::exit called with status 3\n".to_owned()),
            "at {threads} threads"
        );
        let seen: Vec<String> = seen.try_iter().collect();
        assert_eq!(seen, ["true true true", "true"], "at {threads} threads");
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// D-1073: a real signal, raised on the root's thread once the handler is
/// installed, reaches the poller through the handler's byte and cancels
/// the scope; the kernel's `SA_RESETHAND` and the walk leave the default
/// action in place, and the close disarms. SIGINT is raised when its
/// disposition is the handler, else SIGTERM (SIGINT ignored by the test's
/// environment, module documentation); a signal is raised only when the
/// handler is its disposition, so the test binary never takes the default
/// action.
#[test]
#[cfg_attr(windows, ignore = "signals on Windows are M6 (D-545, D-714)")]
fn until_interrupted_a_real_signal_cancels_an_open_scope() {
    let _serial = serial();
    let runtime = testing::with_signals(2, false);
    let seen = run_root(&runtime, |me, out| {
        let scope = open(me);
        arm(me, scope);
        let (tx, rx) = channel(me, 0);
        let raised = [libc::SIGINT, libc::SIGTERM]
            .into_iter()
            .find(|signal| signal::handler_installed(*signal));
        let _ = out.send(format!("handler {}", raised.is_some()));
        let unwound = raised.is_some_and(|signal| {
            // SAFETY: the signal's disposition is the runtime's handler,
            // which only writes to the pipe.
            unsafe { libc::raise(signal) };
            receive_raises(me, rx)
        });
        let succeeded = if unwound {
            close_unwound(me, scope)
        } else {
            close(me, scope)
        };
        let failure = (!succeeded).then(|| failure_of(me));
        let _ = out.send(format!("{unwound} {failure:?} {:?}", state()));
        drop_word(tx);
        drop_word(rx);
        7
    });
    runtime.shutdown();
    assert_eq!(
        seen,
        [
            "handler true".to_owned(),
            format!("true {:?} {IDLE:?}", Some(interrupted()))
        ]
    );
    assert!(hooks::RERAISED.lock().is_empty());
    assert!(wait_until(|| state() == IDLE));
}
