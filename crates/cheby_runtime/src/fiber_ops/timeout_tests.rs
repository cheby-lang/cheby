//! Tests of the timed scope behind `fiber::timeout` (D-671, D-652, D-692;
//! spec §10.8; architecture §7.5): one test per row of survey-t1 §7's
//! table, the timer cancelled on each path out of the close, a firing that
//! loses the race to the scope's end, the lower bound on `TimedOut`, and
//! the rule without a runtime.
//!
//! The body of a timed scope runs on the root fiber, between the open and
//! the close, as `scope_run`'s compiled body does; it acts as generated
//! code does: after a kernel call it tests the unwind flag, and when it is
//! set, the close runs as `scope_run`'s pad would ([`close_body`]). Fibers
//! spawned into nested scopes are Rust jobs ([`start_job`]), which never
//! assert. Events are ordered by channels and by the fiber's own
//! cancellation target, never by sleeping, and time is asserted only as a
//! lower bound ("never earlier than the limit"), with limits of at least
//! 10 ms (plan §0 'Determinism').
//!
//! GAP(timer-cancels-only): a firing only cancels the scope, and the close
//! decides `TimedOut` from the body's unwinding (`scope.rs`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::cancel::{self, hooks};
use super::channel::cheby_channel_receive;
use super::scope::{ScopeState, cheby_rt_scope_close, cheby_rt_scope_open, state_of};
use super::tests::{
    channel, close, close_unwound, config, ctx_of, drop_word, dup, failure_of, int, open, spawn,
    string, yield_now,
};
use crate::abi::{PanicKind, Word, unwind_flag};
use crate::alloc::Domain;
use crate::ctx::{self, ExecCtx, reason};
use crate::entry::{Callbacks, EntryConfig, EntryOutcome, run_entry};
use crate::generic::tests::captures;
use crate::panic::cheby_rt_panic;
use crate::sched::testing::{self, HANG};
use crate::unwind::flag;
use crate::values;

// ---------------------------------------------------------------------------
// Jobs: Rust closures run as spawned fibers (as in `cancel_tests`).

/// A job: runs on the spawned fiber with the Cheby captures after the key,
/// borrowed, and returns its result, owned.
type JobFn = Box<dyn FnOnce(&ExecCtx, &[Word]) -> Word + Send>;

/// The jobs not started yet, by key.
static JOBS: Mutex<BTreeMap<i64, JobFn>> = Mutex::new(BTreeMap::new());

/// The next job key.
static NEXT_JOB: AtomicI64 = AtomicI64::new(1);

/// The trampoline of job closures, with `cb$tramp$0`'s ABI: the closure is
/// borrowed, the result owned.
unsafe extern "C" fn job_t0(ctx: *mut ExecCtx, function: u64) -> u64 {
    // SAFETY: the running fiber's control block.
    let me = unsafe { &*ctx };
    // SAFETY: a live job closure, borrowed.
    let words = unsafe { captures(function) };
    let job = words
        .first()
        .and_then(|key| JOBS.lock().unwrap().remove(&int(*key)));
    match job {
        Some(job) => job(me, words.get(1..).unwrap_or(&[])),
        None => values::nil(),
    }
}

/// Spawns `job` into `scope` with the Cheby values `captures`, each given
/// away to the closure, and drops the `Fiber` handle.
fn start_job(
    me: &ExecCtx,
    scope: Word,
    captures: &[Word],
    job: impl FnOnce(&ExecCtx, &[Word]) -> Word + Send + 'static,
) {
    let key = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    JOBS.lock().unwrap().insert(key, Box::new(job));
    let mut words = vec![values::int_word(key)];
    words.extend_from_slice(captures);
    drop_word(spawn(me, scope, &words));
}

/// Runs `body` as a root fiber on a new runtime of `threads` workers with
/// the job trampoline, requires that it returns `7`, and checks that
/// nothing leaked once the runtime has stopped.
fn run_root(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) {
    let runtime = testing::runtime(threads);
    let domain = Domain::new();
    let config = EntryConfig {
        callbacks: Callbacks {
            t0: Some(job_t0),
            ..Callbacks::default()
        },
        ..config(&domain)
    };
    let outcome = testing::start_root_with(&runtime, &config, body)
        .recv_timeout(HANG)
        .expect("the root fiber hung");
    runtime.shutdown();
    assert_eq!(
        outcome,
        EntryOutcome::Returned(7),
        "the root did not return at {threads} threads"
    );
    assert_eq!(
        domain.live_objects(),
        0,
        "objects leaked at {threads} threads"
    );
}

/// The thread counts the tests run at.
const THREADS: [usize; 2] = [1, 4];

// ---------------------------------------------------------------------------
// The body's operations, as generated code makes them.

/// `scope_run(True, seconds, nanoseconds, …)`'s open for `limit`, as
/// `fiber::timeout` computes the pair (D-652).
fn open_timed(me: &ExecCtx, limit: Duration) -> Word {
    let seconds = i64::try_from(limit.as_secs()).unwrap();
    let nanoseconds = i64::from(limit.subsec_nanos());
    // SAFETY: the running fiber's context.
    unsafe { cheby_rt_scope_open(ctx_of(me), true, seconds, nanoseconds) }
}

/// The running fiber's unwind flag.
fn raised(me: &ExecCtx) -> bool {
    flag::get(me) != unwind_flag::NONE
}

/// Closes a body's scope as `scope_run` does: through the pad when the body
/// unwound, else normally. Returns the close's result.
fn close_body(me: &ExecCtx, scope: Word) -> bool {
    if raised(me) {
        close_unwound(me, scope)
    } else {
        close(me, scope)
    }
}

/// What `timeout` makes of a closed scope: `Ok`, or the failure that
/// `take_failure` reports as `(kind, message, location)`.
fn verdict(me: &ExecCtx, succeeded: bool) -> Result<(), (i64, String, String)> {
    if succeeded {
        Ok(())
    } else {
        Err(failure_of(me))
    }
}

/// The failure of an expired limit: kind 1, empty texts.
fn timed_out() -> Result<(), (i64, String, String)> {
    Err((1, String::new(), String::new()))
}

/// The failure of a panic.
fn panicked(message: &str, location: &str) -> Result<(), (i64, String, String)> {
    Err((0, message.to_owned(), location.to_owned()))
}

/// `channel::receive` on `rx`; true when it raised (a cancellation), else
/// its result is dropped.
fn receive_raises(me: &ExecCtx, rx: Word) -> bool {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let word = unsafe { cheby_channel_receive(ctx_of(me), rx) };
    if raised(me) {
        return true;
    }
    drop_word(word);
    false
}

/// `panic as message` at `location`, as generated code raises it.
fn panic_with(me: &ExecCtx, message: &str, location: &str) {
    let (message, location) = (string(message), string(location));
    // SAFETY: the running fiber's context and live `String`s.
    unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), message, location) };
    drop_word(message);
    drop_word(location);
}

/// The running fiber's runtime's live timers.
fn pending_timers(me: &ExecCtx) -> usize {
    me.sched
        .runtime()
        .map_or(0, |runtime| runtime.timers_pending())
}

/// Waits, with no entry check, until a cancellation has reached the running
/// fiber (its target is set): work during which no cancellation is
/// delivered (D-102). Yields to the scheduler between looks when `yields`
/// is set, else spins on its worker. Returns false after [`HANG`].
fn wait_until_reached(me: &ExecCtx, yields: bool) -> bool {
    let started = Instant::now();
    while hooks::target(me).is_none() {
        if started.elapsed() > HANG {
            return false;
        }
        if yields {
            yield_now(me, 1);
        } else {
            std::hint::spin_loop();
        }
    }
    true
}

/// Whether a cancellation is still pending for the running fiber: a target
/// or the CANCEL reason.
fn cancellation_pending(me: &ExecCtx) -> bool {
    hooks::target(me).is_some() || ctx::pending(me, reason::CANCEL)
}

/// The location of the tests' panics.
const AT: &str = "src/main.cheby:2:3";

/// A limit that never passes during a test.
const LONG: Duration = Duration::from_secs(10);

/// The shortest limit the tests use (plan §0 'Determinism').
const SHORT: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// survey-t1 §7's rows.

/// Row 1: `f` returns before the timer fires: `Ok`, and the close cancels
/// the timer on the return path, so no timer is left.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_f_returns_first_and_the_close_cancels_the_timer() {
    for threads in THREADS {
        run_root(threads, |me| {
            let before = pending_timers(me);
            let scope = open_timed(me, LONG);
            // SAFETY: a live `Scope` handle.
            let state = unsafe { state_of(scope) };
            assert!(state.timed());
            assert_eq!(pending_timers(me), before + 1);
            yield_now(me, 3);
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), Ok(()));
            assert!(!state.timed());
            assert_eq!(pending_timers(me), before);
            assert!(!cancellation_pending(me));
            assert!(!testing::entry_check(me));
            7
        });
    }
}

/// Row 2: the timer fires, the cancellation is delivered to `f` parked in
/// `receive`, which takes nothing and unwinds: `TimedOut` (kind 1, empty
/// texts), never earlier than the limit after the call.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_fired_and_delivered_is_timed_out() {
    for threads in THREADS {
        run_root(threads, |me| {
            let (tx, rx) = channel(me, 0);
            let started = Instant::now();
            let scope = open_timed(me, Duration::from_millis(20));
            assert!(receive_raises(me, rx));
            let succeeded = close_body(me, scope);
            assert!(started.elapsed() >= Duration::from_millis(20));
            assert_eq!(verdict(me, succeeded), timed_out());
            assert_eq!(pending_timers(me), 0);
            assert!(!cancellation_pending(me));
            drop_word(tx);
            drop_word(rx);
            7
        });
    }
}

/// Row 3: the timer fired and reached `f`, which returned before its next
/// entry check: the cancellation never reached `f`, so `Ok` (D-671: "even
/// when the limit has passed"), and the close clears the target and the
/// CANCEL reason, so nothing is delivered afterwards.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_fired_but_f_returned_before_its_next_entry_check_is_ok() {
    for threads in THREADS {
        run_root(threads, |me| {
            let started = Instant::now();
            let scope = open_timed(me, SHORT);
            assert!(wait_until_reached(me, false), "the timer never fired");
            assert!(started.elapsed() >= SHORT);
            assert_eq!(pending_timers(me), 0);
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), Ok(()));
            assert!(!cancellation_pending(me));
            assert!(!testing::entry_check(me));
            7
        });
    }
}

/// Row 4: `f` panics before the timer fires: the panic is the scope's
/// first cause, so `Panicked`, and the close cancels the timer on the
/// panic path.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_f_panics_before_firing_is_panicked_and_the_close_cancels_the_timer() {
    for threads in THREADS {
        run_root(threads, |me| {
            let scope = open_timed(me, LONG);
            assert_eq!(pending_timers(me), 1);
            panic_with(me, "first", AT);
            assert!(raised(me));
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), panicked("first", AT));
            assert_eq!(pending_timers(me), 0);
            assert!(!cancellation_pending(me));
            7
        });
    }
}

/// Row 4, the timer firing later: `f` panicked first and the timer fires
/// while `f` unwinds, finding the scope cancelled already: still
/// `Panicked`, with the first cause.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_a_firing_after_fs_panic_leaves_it_panicked() {
    for threads in THREADS {
        run_root(threads, |me| {
            let scope = open_timed(me, SHORT);
            panic_with(me, "first", AT);
            assert!(raised(me));
            // The unwinding takes its time: the timer fires meanwhile.
            let started = Instant::now();
            while pending_timers(me) != 0 {
                assert!(started.elapsed() < HANG, "the timer never fired");
                std::hint::spin_loop();
            }
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), panicked("first", AT));
            assert!(!cancellation_pending(me));
            7
        });
    }
}

/// Row 5: the timer fired and reached `f`, which panics before the
/// cancellation is delivered: the scope, cancelled, records no cause, and
/// the body's own record decides: `Panicked` (the cancellation had not
/// reached `f`, D-671).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_f_panics_after_firing_before_delivery_is_panicked() {
    for threads in THREADS {
        run_root(threads, |me| {
            let scope = open_timed(me, SHORT);
            assert!(wait_until_reached(me, false), "the timer never fired");
            panic_with(me, "late", AT);
            assert!(raised(me));
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), panicked("late", AT));
            assert!(!cancellation_pending(me));
            7
        });
    }
}

/// Row 6: a fiber in a scope that `f` opened panics while the timeout's
/// cancellation unwinds it: its scope, cancelled from outside, records
/// nothing (D-178), the nested close passes the cancellation through, and
/// the result is `TimedOut`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_a_panic_during_the_unwinding_does_not_change_the_result() {
    for threads in THREADS {
        run_root(threads, |me| {
            let started = Instant::now();
            let scope = open_timed(me, SHORT);
            let inner = open(me);
            start_job(me, inner, &[], |me, _| {
                // Work with no entry check until the cancellation reaches
                // this fiber, then a panic before it is delivered.
                if wait_until_reached(me, true) {
                    panic_with(me, "during the unwinding", AT);
                    0
                } else {
                    values::nil()
                }
            });
            // The inner close waits for the job, then passes the timeout's
            // cancellation through: it never returns to `f`.
            let inner_closed = close(me, inner);
            assert!(raised(me), "the inner scope returned {inner_closed}");
            let succeeded = close_body(me, scope);
            assert!(started.elapsed() >= SHORT);
            assert_eq!(verdict(me, succeeded), timed_out());
            assert!(!cancellation_pending(me));
            7
        });
    }
}

/// Row 7: the timer fires while `f` waits in a nested scope's close for a
/// fiber parked in `receive`: the fiber is cancelled and takes nothing,
/// the inner scope never returns (spec §10.7), and the result is
/// `TimedOut` (fiber.md, "a timeout cancels the scopes inside it without
/// their returning").
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_firing_while_f_waits_in_a_nested_scopes_close_is_timed_out() {
    for threads in THREADS {
        run_root(threads, |me| {
            let (tx, rx) = channel(me, 1);
            let started = Instant::now();
            let scope = open_timed(me, SHORT);
            let inner = open(me);
            start_job(me, inner, &[dup(rx)], |me, words| {
                let rx = words.first().copied().unwrap_or(0);
                if receive_raises(me, rx) {
                    0
                } else {
                    values::nil()
                }
            });
            let inner_closed = close(me, inner);
            assert!(raised(me), "the inner scope returned {inner_closed}");
            let succeeded = close_body(me, scope);
            assert!(started.elapsed() >= SHORT);
            assert_eq!(verdict(me, succeeded), timed_out());
            // The cancelled receive took nothing: the channel still has
            // room for one value.
            assert!(super::tests::send(me, tx, values::int_word(1)).is_ok());
            drop_word(tx);
            drop_word(rx);
            7
        });
    }
}

// ---------------------------------------------------------------------------
// The close's paths and the firing's race.

/// The pass-through path: an outer scope is cancelled while `f` runs; the
/// timed scope's close passes the cancellation through (D-667) and cancels
/// its timer on the way, and the outer scope fails as cancelled from its
/// own target.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_the_close_cancels_the_timer_when_an_outer_cancellation_passes_through() {
    for threads in THREADS {
        run_root(threads, |me| {
            let outer = open(me);
            let scope = open_timed(me, LONG);
            assert_eq!(pending_timers(me), 1);
            // SAFETY: a live `Scope` handle.
            let outer_state = unsafe { state_of(outer) };
            cancel::cancel(&outer_state);
            drop(outer_state);
            assert!(
                testing::entry_check(me),
                "the cancellation was not delivered"
            );
            let succeeded = close_body(me, scope);
            assert!(!succeeded);
            assert!(raised(me), "the timed scope did not pass the cancellation");
            assert_eq!(pending_timers(me), 0);
            let outer_succeeded = close_body(me, outer);
            assert_eq!(verdict(me, outer_succeeded), timed_out());
            assert!(!cancellation_pending(me));
            7
        });
    }
}

/// A firing that loses the race to the scope's end does nothing: the walk
/// skips an ended scope (`ScopeState::cancel_step`), so the owner gets no
/// target and no CANCEL reason.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_a_firing_that_loses_the_race_to_ended_is_harmless() {
    run_root(1, |me| {
        let scope = open_timed(me, LONG);
        // SAFETY: a live `Scope` handle.
        let state = unsafe { state_of(scope) };
        assert!(close_body(me, scope));
        assert!(state.ended());
        ScopeState::fire_timer_target(&state);
        assert!(!cancellation_pending(me));
        assert!(!testing::entry_check(me));
        drop(state);
        7
    });
}

/// The race for real: a body that returns right at its deadline, before or
/// after the timer fires, always gives `Ok` and leaves no cancellation and
/// no timer behind (D-671).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_returning_at_the_deadline_is_always_ok() {
    for threads in THREADS {
        run_root(threads, |me| {
            for round in 0..200_u32 {
                let scope = open_timed(me, Duration::from_micros(u64::from(round % 50)));
                if round % 2 == 1 {
                    yield_now(me, 1);
                }
                let succeeded = close_body(me, scope);
                assert_eq!(verdict(me, succeeded), Ok(()), "round {round}");
                assert!(!cancellation_pending(me), "round {round}");
                assert_eq!(pending_timers(me), 0, "round {round}");
            }
            assert!(!testing::entry_check(me));
            7
        });
    }
}

/// A limit of zero or less never reaches the runtime (`timeout` returns
/// `Err(TimedOut)` without calling `f`, D-671); if one did, it is a
/// deadline of now: the body is cancelled at its next suspension.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_an_elapsed_limit_is_a_deadline_of_now() {
    for threads in THREADS {
        run_root(threads, |me| {
            let (tx, rx) = channel(me, 0);
            let scope = open_timed(me, Duration::ZERO);
            assert!(receive_raises(me, rx));
            let succeeded = close_body(me, scope);
            assert_eq!(verdict(me, succeeded), timed_out());
            drop_word(tx);
            drop_word(rx);
            7
        });
    }
}

/// A limit beyond the clock's range never passes (D-652): the scope is
/// untimed and arms nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_a_limit_past_the_clock_arms_nothing() {
    run_root(2, |me| {
        // SAFETY: the running fiber's context.
        let scope = unsafe { cheby_rt_scope_open(ctx_of(me), true, i64::MAX, 999_999_999) };
        // SAFETY: a live `Scope` handle.
        let state = unsafe { state_of(scope) };
        assert!(!state.timed());
        assert_eq!(pending_timers(me), 0);
        assert!(close_body(me, scope));
        drop(state);
        7
    });
}

// ---------------------------------------------------------------------------
// The lower bound.

/// D-671: `Err(TimedOut)` never comes earlier than the limit after the
/// call. 1000 timeouts of 10 ms around a parked `receive`, run by ten
/// fibers at once on four workers, each measured from before the open to
/// after the close. Only the lower bound is asserted (plan §0
/// 'Determinism'); the slowest one is printed.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn timeout_never_returns_before_its_limit() {
    const FIBERS: usize = 10;
    const ROUNDS: usize = 100;
    let seen: Arc<Mutex<Vec<(Duration, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&seen);
    run_root(4, move |me| {
        let (tx, rx) = channel(me, 0);
        let group = open(me);
        for _ in 0..FIBERS {
            let seen = Arc::clone(&shared);
            start_job(me, group, &[dup(rx)], move |me, words| {
                let rx = words.first().copied().unwrap_or(0);
                for _ in 0..ROUNDS {
                    let started = Instant::now();
                    let scope = open_timed(me, SHORT);
                    let cancelled = receive_raises(me, rx);
                    let succeeded = close_body(me, scope);
                    let took = started.elapsed();
                    let result = verdict(me, succeeded);
                    let timed_out = cancelled && result == timed_out();
                    seen.lock().unwrap().push((took, timed_out));
                }
                values::nil()
            });
        }
        assert!(close(me, group));
        drop_word(tx);
        drop_word(rx);
        7
    });
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), FIBERS * ROUNDS);
    assert!(
        seen.iter().all(|(_, timed_out)| *timed_out),
        "a timeout did not time out"
    );
    let shortest = seen.iter().map(|(took, _)| *took).min().unwrap();
    let longest = seen.iter().map(|(took, _)| *took).max().unwrap();
    println!("1000 timeouts of 10 ms: shortest {shortest:?}, longest {longest:?}");
    assert!(shortest >= SHORT, "a timeout returned after {shortest:?}");
}

// ---------------------------------------------------------------------------
// Without a runtime (D-692, architecture §5.1).

/// The environment variable that makes a test run its isolated entry.
const CHILD: &str = "CHEBY_RT_TEST_TIMEOUT_CHILD";

/// Runs the test `name` of this module again in a child process with
/// [`CHILD`] set, and returns its exit status, standard output and
/// standard error.
fn run_child(name: &str) -> (bool, String, String) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("fiber_ops::timeout_tests::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs `entry` as an isolated entry, which has no runtime, with the test
/// configuration.
fn run_isolated(entry: crate::entry::EntryFn) -> EntryOutcome {
    let domain = Domain::new();
    // SAFETY: a test thunk with the entry ABI.
    unsafe { run_entry(entry, &config(&domain)) }.unwrap()
}

/// A timed scope whose timer would be armed, in an entry without a
/// runtime, ends the program with a fatal error naming `fiber::timeout`
/// before anything is open (D-692).
#[test]
fn timeout_at_a_deadline_needs_the_scheduler_without_a_runtime() {
    const NAME: &str = "timeout_at_a_deadline_needs_the_scheduler_without_a_runtime";
    unsafe extern "C" fn entry(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        eprintln!("opening a timed scope");
        let scope = open_timed(me, SHORT);
        drop_word(scope);
        0
    }
    if std::env::var_os(CHILD).is_some() {
        let outcome = run_isolated(entry);
        panic!("the isolated entry ended: {outcome:?}");
    }
    let (success, _, stderr) = run_child(NAME);
    assert!(!success, "{stderr}");
    assert!(stderr.contains("opening a timed scope\n"), "{stderr}");
    assert!(
        stderr.contains(
            "fatal runtime error: fiber::timeout needs the fiber scheduler, \
             which this entry does not run\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("the isolated entry ended"), "{stderr}");
}

/// A timed scope whose deadline never comes arms nothing, so it works
/// without a runtime: it opens untimed, `f` runs and returns, and the
/// result is `Ok` (D-692: only an operation that would arm a timer needs
/// the scheduler; D-652).
#[test]
fn timeout_never_deadline_without_a_runtime() {
    const NAME: &str = "timeout_never_deadline_without_a_runtime";
    unsafe extern "C" fn entry(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        // SAFETY: the running fiber's context.
        let scope = unsafe { cheby_rt_scope_open(ctx, true, i64::MAX, 0) };
        println!("f ran");
        // SAFETY: the running fiber's context and the handle its open gave.
        let succeeded = unsafe { cheby_rt_scope_close(ctx, scope, false) };
        println!(
            "the scope returned {}",
            if succeeded { "Ok" } else { "Err" }
        );
        assert_eq!(me.conc.open_scope_count(), 0);
        7
    }
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(run_isolated(entry), EntryOutcome::Returned(7));
        println!("the isolated entry returned");
        return;
    }
    let (success, stdout, stderr) = run_child(NAME);
    assert!(success, "{stdout}\n{stderr}");
    assert!(stdout.contains("f ran\n"), "{stdout}");
    assert!(stdout.contains("the scope returned Ok\n"), "{stdout}");
    assert!(stdout.contains("the isolated entry returned\n"), "{stdout}");
    assert!(!stderr.contains("fatal runtime error"), "{stderr}");
}
