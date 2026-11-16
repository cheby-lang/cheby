//! The tests of `std::time`'s kernels (`time.rs`, D-985, D-1021, D-1058):
//! `time_kernel_*` for the clocks, the clock in context (D-1597) and the
//! declarations in `std/time.cheby`,
//! `time_sleep_*` for `sys_sleep` on Rust-level fibers of a runtime (the
//! harness of `fiber_ops::detached_tests`, no JIT). The deadlock report of
//! an endless sleep is tested by `sched::deadlock_tests`'s
//! `deadlock_sleep_*`, and the races by `fiber_ops::sleep_models`.
//!
//! Every time is measured on the fiber that waits, right where the event
//! happens, and only lower bounds are asserted, except that a cancelled
//! sleep of an hour must return within [`testing::hang`].

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::abi::Word;
use crate::alloc::Domain;
use crate::context::{cheby_context_sys_bind, cheby_context_sys_restore};
use crate::ctx::ExecCtx;
use crate::entry::{EntryOutcome, run_entry};
use crate::fiber_ops::detached_tests::{self, close_scope, release, root_outcome, unwinding};
use crate::fiber_ops::scope::cheby_rt_scope_open;
use crate::sched::testing;
use crate::time::{
    ASYNC_KERNELS, CONTEXT_KERNELS, PROVIDED, cheby_time_sys_monotonic,
    cheby_time_sys_monotonic_unless_provided, cheby_time_sys_now,
    cheby_time_sys_now_unless_provided, cheby_time_sys_sleep, origin, signatures, since_epoch,
    since_origin,
};
use crate::values;

/// The thread counts of the tests that start a runtime.
const THREADS: [usize; 2] = [1, 4];

/// The nanoseconds in a second.
const BILLION: i64 = 1_000_000_000;

fn ctx_ptr(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// Runs `body` as the root fiber of a new runtime of `threads` workers,
/// requires that it returns and that nothing leaked once the runtime has
/// stopped, and gives back what `body` computed, for the test thread to
/// assert.
fn on_root<T: Send + 'static>(
    threads: usize,
    body: impl FnOnce(&ExecCtx) -> T + Send + 'static,
) -> T {
    let runtime = testing::runtime(threads);
    let domain = Domain::new();
    let (sender, received) = mpsc::channel();
    let outcome = root_outcome(&runtime, &domain, move |me| {
        let value = body(me);
        sender.send(value).ok();
        7
    });
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    received.recv().expect("the root sent nothing")
}

/// A clock kernel's `(Int, Int)`, read and released.
fn pair(word: Word) -> (i64, i64) {
    // SAFETY: a live tuple of two `Int`s that the kernel returned, owned;
    // its fields are read before it is released.
    let pair = unsafe {
        let fields = values::read_tuple(word);
        (
            values::word_to_i64(fields[0]),
            values::word_to_i64(fields[1]),
        )
    };
    release(word);
    pair
}

/// The monotonic kernel's reading.
fn monotonic() -> (i64, i64) {
    // SAFETY: the kernel takes no argument.
    pair(unsafe { cheby_time_sys_monotonic() })
}

/// `(seconds, nanoseconds)` as nanoseconds.
fn nanos((seconds, nanoseconds): (i64, i64)) -> i128 {
    i128::from(seconds)
        .saturating_mul(i128::from(BILLION))
        .saturating_add(i128::from(nanoseconds))
}

/// Nanoseconds as a `Duration`'s `(seconds, nanoseconds)`, the nanoseconds
/// from 0 to 999 999 999 (D-650), as `std::time` passes a span.
fn parts(total: i128) -> (i64, i64) {
    let billion = i128::from(BILLION);
    (
        i64::try_from(total.div_euclid(billion)).unwrap(),
        i64::try_from(total.rem_euclid(billion)).unwrap(),
    )
}

/// `sys_sleep` for the span `(seconds, nanoseconds)`: whether it raised (a
/// cancellation), with the flag set then.
fn sleep(me: &ExecCtx, (seconds, nanoseconds): (i64, i64)) -> bool {
    // SAFETY: the running fiber's context.
    unsafe { cheby_time_sys_sleep(ctx_ptr(me), seconds, nanoseconds) };
    unwinding(me)
}

/// The running fiber's runtime's live timers.
fn pending_timers(me: &ExecCtx) -> usize {
    me.sched
        .runtime()
        .map_or(0, |runtime| runtime.timers_pending())
}

// ---------------------------------------------------------------------
// The clocks (D-1021)
// ---------------------------------------------------------------------

/// The wall clock reads the system's clock, as the whole seconds since the
/// Unix epoch and the nanoseconds after them.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_kernel_wall_clock_is_the_system_clock() {
    let (before, read, after) = on_root(1, |_| {
        let before = since_epoch(SystemTime::now());
        // SAFETY: the kernel takes no argument.
        let read = pair(unsafe { cheby_time_sys_now() });
        let after = since_epoch(SystemTime::now());
        (before, read, after)
    });
    assert!(
        before <= read && read <= after,
        "{before:?} {read:?} {after:?}"
    );
    assert!((0..BILLION).contains(&read.1), "{read:?}");
    // After 2020-01-01.
    assert!(read.0 > 1_577_836_800, "{read:?}");
}

/// A point before 1970 has negative seconds and nanoseconds from 0 to
/// 999 999 999, `Duration`'s one representation (D-650). `SystemTime`
/// counts in steps of 100 ns on Windows, so the offsets here are multiples
/// of that.
#[test]
fn time_kernel_wall_clock_before_1970_is_normalized() {
    let at = |offset: Duration, before: bool| {
        since_epoch(if before {
            UNIX_EPOCH - offset
        } else {
            UNIX_EPOCH + offset
        })
    };
    assert_eq!(at(Duration::ZERO, false), (0, 0));
    assert_eq!(at(Duration::from_millis(1_250), false), (1, 250_000_000));
    assert_eq!(at(Duration::from_millis(1_500), true), (-2, 500_000_000));
    assert_eq!(at(Duration::from_secs(2), true), (-2, 0));
    assert_eq!(at(Duration::from_nanos(100), true), (-1, 999_999_900));
}

/// The monotonic clock counts from one origin per process, read through
/// `Instant`, the timer heap's clock: readings taken in two runtimes are
/// comparable, and never go backwards (D-1021).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_kernel_monotonic_counts_from_one_process_origin() {
    assert_eq!(since_origin(origin()), (0, 0));
    assert_eq!(
        since_origin(origin() + Duration::from_millis(1_500)),
        (1, 500_000_000)
    );
    let first = on_root(1, |_| monotonic());
    let second = on_root(4, |_| {
        let a = monotonic();
        let b = monotonic();
        (a, b)
    });
    let last = since_origin(Instant::now());
    assert!(first <= second.0, "{first:?} {second:?}");
    assert!(second.0 <= second.1, "{second:?}");
    assert!(second.1 <= last, "{second:?} {last:?}");
    assert!((0..BILLION).contains(&first.1), "{first:?}");
}

// ---------------------------------------------------------------------
// The clock in context (D-1597)
// ---------------------------------------------------------------------

/// A key identity `(hash, path, type)`, owned, as a key constant holds it.
fn identity(hash: u64, path: &str, ty: &str) -> Word {
    values::tuple(&[
        values::u64_word(hash),
        values::string(path),
        values::string(ty),
    ])
}

/// What both clock kernels read, in this order, `now` then `monotonic`,
/// with `clock` as the clock key's identity.
fn both_readings(me: &ExecCtx, clock: Word) -> [(i64, i64); 2] {
    // SAFETY: the running fiber's context and a live identity, borrowed.
    unsafe {
        [
            pair(cheby_time_sys_now_unless_provided(ctx_ptr(me), clock)),
            pair(cheby_time_sys_monotonic_unless_provided(ctx_ptr(me), clock)),
        ]
    }
}

/// D-1597: both kernels that `time::now` and `time::monotonic` call read
/// their clock on the empty context and with an unrelated key bound, and
/// return exactly [`PROVIDED`] and read nothing once the clock's key is
/// bound, by an equal identity at another address too; after the
/// bindings are restored, with the unrelated key still bound and then
/// with none, they read the clock again. The unrelated key has the
/// clock's hash, so the probe compares the texts.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_kernel_clock_in_context_reads_unless_the_clock_is_provided() {
    for threads in THREADS {
        let readings = on_root(threads, |me| {
            let clock = identity(0x5EED_C10C, "std::time::clock", "std::time::Clock");
            let unrelated = identity(0x5EED_C10C, "app::port", "Int");
            let empty = both_readings(me, clock);
            // SAFETY: the running fiber's context and live words, borrowed;
            // each outer bindings word is restored and then released.
            let (with_unrelated, with_clock, with_equal_clock, restored) = unsafe {
                let ctx = ctx_ptr(me);
                let outer = cheby_context_sys_bind(ctx, unrelated, values::int_word(8080));
                let with_unrelated = both_readings(me, clock);
                let middle = cheby_context_sys_bind(ctx, clock, values::int_word(1));
                let with_clock = both_readings(me, clock);
                let equal = identity(0x5EED_C10C, "std::time::clock", "std::time::Clock");
                let with_equal_clock = both_readings(me, equal);
                release(equal);
                cheby_context_sys_restore(ctx, middle);
                release(middle);
                let after_one = both_readings(me, clock);
                cheby_context_sys_restore(ctx, outer);
                release(outer);
                let restored = both_readings(me, clock);
                (
                    with_unrelated,
                    with_clock,
                    with_equal_clock,
                    [after_one, restored],
                )
            };
            release(clock);
            release(unrelated);
            (
                empty,
                with_unrelated,
                with_clock,
                with_equal_clock,
                restored,
            )
        });
        let (empty, with_unrelated, with_clock, with_equal_clock, restored) = readings;
        let [after_one, restored] = restored;
        for reading in [empty, with_unrelated, after_one, restored]
            .into_iter()
            .flatten()
        {
            assert!((0..BILLION).contains(&reading.1), "{reading:?}");
        }
        assert_eq!(with_clock, [PROVIDED, PROVIDED]);
        assert_eq!(with_equal_clock, [PROVIDED, PROVIDED]);
        // The monotonic readings never go back, across the branches.
        let monotonic = [empty[1], with_unrelated[1], after_one[1], restored[1]];
        assert!(monotonic.is_sorted(), "{monotonic:?}");
    }
}

/// The parameter list and the rest of a `fn` line after its name: the
/// list ends at the `)` that closes its `(`, so a tuple type inside it
/// stays in it.
fn split_params(after_name: &str) -> (&str, &str) {
    let mut depth = 0_u32;
    for (i, c) in after_name.char_indices() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let params = after_name.get(1..i).unwrap();
                    let rest = after_name.get(i.saturating_add(1)..).unwrap();
                    return (params, rest);
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parameters: {after_name}");
}

/// The parameters of a list, split at the commas outside parentheses.
fn top_level_params(params: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (i, c) in params.char_indices() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(params.get(start..i).unwrap());
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    out.push(params.get(start..).unwrap());
    out.into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Each kernel has the C signature, the `@std_context`, the `@async` and
/// the bindings of its declaration in `std/time.cheby` (D-985, D-986,
/// D-692): `sys_now` and `sys_monotonic` are ordinary kernels returning a
/// pair; `sys_now_unless_provided` and `sys_monotonic_unless_provided`
/// take the context and the clock key's identity, one word, and return a
/// pair (D-1597); `sys_sleep` takes the context and is `@async` on JS;
/// every binding is to `./time.mjs`. `std/time/instant.cheby` imports only
/// `std::duration` and declares no kernel (D-1057).
#[test]
fn time_kernel_declarations_match_std() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
    let source = std::fs::read_to_string(root.join("time.cheby")).unwrap();
    let mut declared = Vec::new();
    let mut attributes: Vec<&str> = Vec::new();
    for line in source.lines() {
        if line.starts_with('@') {
            attributes.push(line);
            continue;
        }
        let Some(rest) = line.strip_prefix("fn ") else {
            attributes.clear();
            continue;
        };
        let native = attributes.iter().find_map(|a| {
            a.strip_prefix("@external(native, \"")
                .and_then(|r| r.strip_suffix("\")"))
        });
        if let Some(native) = native {
            let open = rest.find('(').unwrap();
            let name = rest.get(..open).unwrap();
            let js = format!("@external(js, \"./time.mjs\", \"{name}\")");
            assert!(attributes.contains(&js.as_str()), "{name}: {attributes:?}");
            let context = attributes.contains(&"@std_context");
            let asynchronous = attributes.contains(&"@async");
            let (params, result) = split_params(rest.get(open..).unwrap());
            let params: Vec<&str> = top_level_params(params)
                .into_iter()
                .map(|p| match p.split_once(':').unwrap().1.trim() {
                    "Int" => "i64",
                    _ => "Word",
                })
                .collect();
            let result = match result.trim() {
                "" => "()",
                _ => "Word",
            };
            declared.push((native.to_owned(), params, result, context, asynchronous));
        }
        attributes.clear();
    }
    let registered: Vec<_> = signatures()
        .into_iter()
        .map(|(name, params, result)| {
            let context = CONTEXT_KERNELS.contains(&name);
            let asynchronous = ASYNC_KERNELS.contains(&name);
            (
                name.to_owned(),
                params.to_vec(),
                result,
                context,
                asynchronous,
            )
        })
        .collect();
    assert_eq!(declared, registered);
    assert_eq!(registered.len(), 5);
    let instant = std::fs::read_to_string(root.join("time/instant.cheby")).unwrap();
    assert!(!instant.contains("@external"));
    let imports: Vec<&str> = instant
        .lines()
        .filter(|line| line.starts_with("import "))
        .collect();
    assert_eq!(imports, ["import std::duration"]);
}

// ---------------------------------------------------------------------
// Sleeping (D-652, D-891, D-983, D-1020)
// ---------------------------------------------------------------------

/// A span of zero or less returns at once: the fiber does not park and no
/// timer is armed (D-652).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_sleep_of_zero_or_less_returns_at_once() {
    for threads in THREADS {
        let (raised, parked, armed) = on_root(threads, |me| {
            let resumes = me.sched.resumes();
            let timers = pending_timers(me);
            let mut raised = false;
            for span in [(0, 0), (0, -1), (-1, 0), (-3, 500_000_000), (i64::MIN, 0)] {
                raised |= sleep(me, span);
            }
            (
                raised,
                me.sched.resumes() != resumes,
                pending_timers(me) != timers,
            )
        });
        assert!(!raised && !parked && !armed, "at {threads} threads");
    }
}

/// An expired sleep wakes its fiber, never before its span, and leaves no
/// timer behind.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_sleep_an_expired_sleep_wakes() {
    const SPAN: Duration = Duration::from_millis(20);
    for threads in THREADS {
        let (raised, slept, timers_left) = on_root(threads, |me| {
            let timers = pending_timers(me);
            let start = Instant::now();
            let raised = sleep(me, (0, i64::try_from(SPAN.as_nanos()).unwrap()));
            let slept = start.elapsed();
            (raised, slept, pending_timers(me) - timers)
        });
        assert!(!raised, "at {threads} threads");
        assert!(slept >= SPAN, "woke after {slept:?} at {threads} threads");
        assert_eq!(timers_left, 0, "at {threads} threads");
    }
}

/// A sleep that a cancellation reaches unwinds at once, taking nothing,
/// and cancels its timer: here a timed scope of 10 ms around a sleep of an
/// hour, and around an endless one (D-983, D-891).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_sleep_a_cancelled_sleep_returns_at_once() {
    const LIMIT: Duration = Duration::from_millis(10);
    for threads in THREADS {
        for span in [(3_600, 0), (i64::MAX, 0)] {
            let (raised, slept, succeeded, timers_left) = on_root(threads, move |me| {
                let timers = pending_timers(me);
                let limit = i64::try_from(LIMIT.as_nanos()).unwrap();
                // SAFETY: the running fiber's context.
                let scope = unsafe { cheby_rt_scope_open(ctx_ptr(me), true, 0, limit) };
                let start = Instant::now();
                let raised = sleep(me, span);
                let slept = start.elapsed();
                let succeeded = close_scope(me, scope);
                // `fiber::timeout` takes the scope's failure; nothing else
                // reads it here.
                let _ = me.conc.take_last_failure();
                (raised, slept, succeeded, pending_timers(me) - timers)
            });
            let shown = format!("{span:?} at {threads} threads");
            assert!(raised, "{shown}: the sleep returned");
            assert!(!succeeded, "{shown}: the scope did not time out");
            assert!(slept >= LIMIT, "{shown}: woke after {slept:?}");
            assert!(slept < testing::hang(), "{shown}: woke after {slept:?}");
            assert_eq!(timers_left, 0, "{shown}");
        }
    }
}

/// `sleep_until`'s Cheby subtracts a monotonic reading from its deadline:
/// a deadline just passed gives a negative span, which returns at once,
/// and a deadline ahead is reached on the monotonic clock when the fiber
/// wakes, because the timers use the same clock (D-1021, D-1057).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn time_sleep_until_a_deadline_on_the_monotonic_clock() {
    const AHEAD: i128 = 20_000_000;
    for threads in THREADS {
        let (passed_parked, passed_raised, woke, deadline) = on_root(threads, |me| {
            // A deadline that has just passed.
            let passed = nanos(monotonic()) + 1_000_000;
            while nanos(since_origin(Instant::now())) <= passed {
                std::hint::spin_loop();
            }
            let resumes = me.sched.resumes();
            let passed_raised = sleep(me, parts(passed - nanos(monotonic())));
            let passed_parked = me.sched.resumes() != resumes;
            // A deadline ahead.
            let deadline = nanos(monotonic()) + AHEAD;
            let raised = sleep(me, parts(deadline - nanos(monotonic())));
            assert!(!raised);
            (passed_parked, passed_raised, nanos(monotonic()), deadline)
        });
        assert!(!passed_parked && !passed_raised, "at {threads} threads");
        assert!(woke >= deadline, "woke at {woke}, before {deadline}");
    }
}

/// Set in a child process that runs an isolated entry: which one.
const CHILD: &str = "CHEBY_RT_TEST_TIME_CHILD";

/// Runs `entry` as an isolated entry, which has no runtime (D-692).
fn run_isolated(entry: crate::entry::EntryFn) -> EntryOutcome {
    let domain = Domain::new();
    // SAFETY: a test thunk with the entry ABI.
    unsafe { run_entry(entry, &detached_tests::config(&domain)) }.unwrap()
}

/// Without a scheduler a sleep of zero or less still returns at once, as
/// every operation that completes without waiting does (D-692, D-652).
#[test]
fn time_sleep_of_zero_without_a_scheduler_returns() {
    unsafe extern "C" fn entry(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        let raised = sleep(me, (0, 0)) || sleep(me, (-1, 0));
        if raised { 0 } else { 7 }
    }
    assert_eq!(run_isolated(entry), EntryOutcome::Returned(7));
}

/// Without a scheduler any other sleep, endless or not, ends the program
/// with a fatal error that names `time::sleep` (D-692, D-714, D-983).
#[test]
fn time_sleep_without_a_scheduler_is_fatal() {
    const NAME: &str = "time_sleep_without_a_scheduler_is_fatal";
    unsafe extern "C" fn short(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        eprintln!("sleeping for 10 ms");
        sleep(me, (0, 10_000_000));
        0
    }
    unsafe extern "C" fn endless(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        eprintln!("sleeping for good");
        sleep(me, (i64::MAX, 0));
        0
    }
    if let Some(which) = std::env::var_os(CHILD) {
        let outcome = run_isolated(if which == "endless" { endless } else { short });
        panic!("the isolated entry ended: {outcome:?}");
    }
    for (which, said) in [
        ("short", "sleeping for 10 ms"),
        ("endless", "sleeping for good"),
    ] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("time_tests::{NAME}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, which)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert!(stderr.contains(&format!("{said}\n")), "{stderr}");
        assert!(
            stderr.contains(
                "fatal runtime error: time::sleep needs the fiber scheduler, \
                 which this entry does not run\n"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("the isolated entry ended"), "{stderr}");
    }
}
