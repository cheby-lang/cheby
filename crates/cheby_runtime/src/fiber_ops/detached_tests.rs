//! Tests of detached fibers (D-669, D-671, D-694, D-701, D-705, D-692),
//! and the harness the lifecycle tests share (`sched::lifecycle_tests`):
//! Rust-level fibers on a `Runtime`, no JIT, with the return flag (D-733).
//!
//! A spawned function is a test closure `[RUST, key]` whose key names a
//! Rust action in [`ACTIONS`], which the test trampoline [`t0`] takes out
//! and runs on the fiber, in place of the generated `cb$tramp$0`. An
//! action owns the Cheby words it captures through [`Owned`] guards, so
//! they are released on every path out, a cancellation included; an action
//! that unwinds returns 0 with the flag set, as generated code does.
//!
//! The runtimes read `CHEBY_RT_STRESS_MIGRATE` (D-687), and every test that
//! takes a thread count runs at 1, 2 and 4 workers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::Owned;
use super::detached::cheby_fiber_sys_spawn_detached;
use super::spawn::cheby_fiber_sys_spawn;
use super::tests::{
    channel, close, close_unwound, ctx_of, drop_word, failure_of, int, open, receive, send,
};
use crate::abi::{PanicKind, Word};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{Callbacks, EntryConfig, EntryMode, EntryOutcome};
use crate::generic::tests::{captures, closure};
use crate::sched::testing;
use crate::sched::{AfterSwitch, DetachedGroup, Runtime, RuntimeConfig, park};
use crate::values;

/// The operation code of a test closure that runs a Rust action.
const RUST: i64 = 1_000;

/// What a test fiber runs: its owned result, or 0 with the flag set.
pub(crate) type Action = Box<dyn FnOnce(&ExecCtx) -> Word + Send>;

/// The actions not run yet, by key.
static ACTIONS: Mutex<Option<HashMap<i64, Action>>> = Mutex::new(None);

/// The next action key.
static NEXT_KEY: AtomicI64 = AtomicI64::new(1);

/// A test closure `[RUST, key]` that runs `action`, owned.
pub(crate) fn action(action: impl FnOnce(&ExecCtx) -> Word + Send + 'static) -> Word {
    let key = NEXT_KEY.fetch_add(1, Ordering::SeqCst);
    ACTIONS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(key, Box::new(action));
    closure(&[values::int_word(RUST), values::int_word(key)])
}

/// The test trampoline, with `cb$tramp$0`'s ABI: the closure is borrowed,
/// the result owned.
unsafe extern "C" fn t0(ctx: *mut ExecCtx, function: u64) -> u64 {
    // SAFETY: the running fiber's control block.
    let me = unsafe { &*ctx };
    // SAFETY: a live test closure, borrowed.
    let words = unsafe { captures(function) };
    let (Some(&op), Some(&key)) = (words.first(), words.get(1)) else {
        return values::nil();
    };
    assert_eq!(int(op), RUST, "not a test action");
    let action = ACTIONS
        .lock()
        .as_mut()
        .and_then(|actions| actions.remove(&int(key)));
    action.map_or_else(values::nil, |action| action(me))
}

/// The trampolines of the harness: [`t0`] only.
pub(crate) fn callbacks() -> Callbacks {
    Callbacks {
        t0: Some(t0),
        ..Callbacks::default()
    }
}

/// An entry configuration in test mode with the test trampoline and
/// `domain`.
pub(crate) fn config(domain: &Arc<Domain>) -> EntryConfig {
    EntryConfig {
        stack_size: 1 << 20,
        callbacks: callbacks(),
        domain: Arc::clone(domain),
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// A runtime of `threads` workers that reads `CHEBY_RT_STRESS_MIGRATE`.
pub(crate) fn runtime(threads: usize) -> Runtime {
    Runtime::new(&RuntimeConfig {
        threads: Some(threads),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// Spawns a detached fiber that runs `body`.
pub(crate) fn detach(me: &ExecCtx, body: impl FnOnce(&ExecCtx) -> Word + Send + 'static) {
    let function = action(body);
    // SAFETY: the running fiber's context and a live closure, borrowed.
    unsafe { cheby_fiber_sys_spawn_detached(ctx_of(me), function) };
    drop_word(function);
}

/// Spawns a fiber into `scope` that runs `body`; returns its `Fiber`
/// handle, owned.
pub(crate) fn spawn_into(
    me: &ExecCtx,
    scope: Word,
    body: impl FnOnce(&ExecCtx) -> Word + Send + 'static,
) -> Word {
    let function = action(body);
    // SAFETY: the running fiber's context, a live scope and closure.
    let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
    drop_word(function);
    fiber
}

/// Whether the fiber is unwinding: its flag is set.
pub(crate) fn unwinding(me: &ExecCtx) -> bool {
    crate::unwind::flag::get(me) != crate::abi::unwind_flag::NONE
}

/// Runs entry checks, yielding between them, until one delivers a
/// cancellation; returns 0 with the flag set.
pub(crate) fn until_cancelled(me: &ExecCtx) -> Word {
    while !testing::entry_check(me) {
        park(me, AfterSwitch::Requeue);
    }
    0
}

/// Runs entry checks without yielding until one delivers a cancellation:
/// a spinner that only the monitor preempts.
pub(crate) fn spin(me: &ExecCtx) -> Word {
    while !testing::entry_check(me) {
        std::hint::spin_loop();
    }
    0
}

/// Takes ownership of one reference to `word`, dup'd for the guard.
///
/// The guard is usually moved into a Rust closure that another fiber runs,
/// where `mark_shared` at a spawn cannot reach it (D-705), so `word` is
/// marked shared here first: two fibers on different workers then release
/// it through the shared count (a data race under the thread sanitizer with
/// migration otherwise, board items DLs/SZa-1 and SZa-2).
pub(crate) fn hold(word: Word) -> Owned {
    // SAFETY: a live value, either still confined to the calling fiber or
    // already marked shared by an earlier `hold` (marking skips it then);
    // the guard owns the reference taken here.
    unsafe {
        crate::rc::mark_shared(word);
        crate::rc::dup(word);
        Owned::new(word)
    }
}

/// The group of the running fiber.
pub(crate) fn group_of(me: &ExecCtx) -> Arc<DetachedGroup> {
    me.conc.group().unwrap()
}

/// Yields until `ready` holds, or panics after [`testing::hang`].
pub(crate) fn yield_until(me: &ExecCtx, ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "a fiber waited too long"
        );
        park(me, AfterSwitch::Requeue);
    }
}

/// Runs `body` as a root fiber of `runtime` with `domain` and waits for
/// its outcome.
pub(crate) fn root_outcome(
    runtime: &Runtime,
    domain: &Arc<Domain>,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> EntryOutcome {
    testing::start_root_with(runtime, &config(domain), body)
        .recv_timeout(testing::hang())
        .expect("the root fiber hung")
}

/// The two ends of a new channel of `capacity`, owned.
pub(crate) fn new_channel(me: &ExecCtx, capacity: i64) -> (Word, Word) {
    channel(me, capacity)
}

/// `channel::send`, with `value` borrowed: the payload of the result, or
/// `None` when the kernel raised (a cancellation), with the flag set.
pub(crate) fn send_on(me: &ExecCtx, tx: Word, value: Word) -> Option<Result<Word, Word>> {
    // SAFETY: the running fiber's context, a live `Sender` and value.
    let word = unsafe { super::channel::cheby_channel_send(ctx_of(me), tx, value) };
    (!unwinding(me)).then(|| super::tests::result(word))
}

/// `channel::receive`: the payload of the result, or `None` when the
/// kernel raised (a cancellation), with the flag set.
pub(crate) fn receive_on(me: &ExecCtx, rx: Word) -> Option<Result<Word, Word>> {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let word = unsafe { super::channel::cheby_channel_receive(ctx_of(me), rx) };
    (!unwinding(me)).then(|| super::tests::result(word))
}

/// Opens a scope.
pub(crate) fn open_scope(me: &ExecCtx) -> Word {
    open(me)
}

/// Closes a scope, consuming its handle, as generated code does: after
/// an unwinding, through the pad's path; returns whether the scope
/// returned `Ok` to user code (it did not unwind and did not fail).
pub(crate) fn close_scope(me: &ExecCtx, scope: Word) -> bool {
    if unwinding(me) {
        close_unwound(me, scope);
        false
    } else {
        close(me, scope)
    }
}

/// Releases an owned reference.
pub(crate) fn release(word: Word) {
    drop_word(word);
}

/// Runs `body` as a root fiber, requires that it returns, then shuts the
/// runtime down (tearing its groups down) and checks that nothing leaked.
fn run_and_shutdown(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) {
    let runtime = runtime(threads);
    let domain = Domain::new();
    let outcome = root_outcome(&runtime, &domain, body);
    assert!(
        matches!(outcome, EntryOutcome::Returned(_)),
        "the root did not return: {outcome:?}"
    );
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

const THREADS: [usize; 3] = [1, 2, 4];

/// D-669: a detached fiber runs on its own after the spawner moved on, and
/// its result is released when it returns; it is a member of the root's
/// group until it ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_runs_on_its_own_and_releases_its_result() {
    for threads in THREADS {
        run_and_shutdown(threads, |me| {
            let (tx, rx) = channel(me, 0);
            let out = hold(tx);
            detach(me, move |me| {
                let sent = send(me, out.word(), values::int_word(7));
                drop(out);
                if unwinding(me) {
                    return 0;
                }
                assert_eq!(sent, Ok(values::nil()));
                // The result goes at once (D-669): a fresh object.
                values::string("released by the base frame")
            });
            drop_word(tx);
            assert_eq!(receive(me, rx).map(int), Ok(7));
            drop_word(rx);
            let group = group_of(me);
            yield_until(me, || group.live() == 0);
            values::nil()
        });
    }
}

/// D-669, D-671: a detached fiber spawned inside `fiber::timeout` survives
/// the timeout's cancellation of its spawner's scope, and later sends on a
/// channel the root receives from.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_survives_its_spawners_timeout() {
    for threads in THREADS {
        run_and_shutdown(threads, |me| {
            let (go_tx, go_rx) = channel(me, 0);
            let (out_tx, out_rx) = channel(me, 0);
            // `fiber::timeout(20 ms, f)`: a timed scope whose body spawns the
            // detached fiber, then waits to be cancelled.
            // SAFETY: the running fiber's context.
            let scope =
                unsafe { super::scope::cheby_rt_scope_open(ctx_of(me), true, 0, 20_000_000) };
            let (go, out) = (hold(go_rx), hold(out_tx));
            detach(me, move |me| {
                let got = receive(me, go.word());
                drop(go);
                if unwinding(me) {
                    return 0;
                }
                assert_eq!(got.map(int), Ok(1));
                let sent = send(me, out.word(), values::int_word(42));
                drop(out);
                if unwinding(me) {
                    return 0;
                }
                assert_eq!(sent, Ok(values::nil()));
                values::nil()
            });
            until_cancelled(me);
            assert!(!close_unwound(me, scope), "the timeout did not fail");
            assert_eq!(failure_of(me).0, 1, "not an expired limit");
            // The detached fiber was not cancelled: it goes on.
            assert_eq!(send(me, go_tx, values::int_word(1)), Ok(values::nil()));
            assert_eq!(receive(me, out_rx).map(int), Ok(42));
            for word in [go_tx, go_rx, out_tx, out_rx] {
                drop_word(word);
            }
            values::nil()
        });
    }
}

/// D-669: a detached fiber spawned by a member of a scope that fails (a
/// sibling panics) survives the scope's cancellation of its members.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_survives_its_spawners_failed_scope() {
    for threads in THREADS {
        run_and_shutdown(threads, |me| {
            let (go_tx, go_rx) = channel(me, 0);
            let (out_tx, out_rx) = channel(me, 0);
            let detached = Arc::new(AtomicBool::new(false));
            let scope = open(me);
            let (go, out, flag) = (hold(go_rx), hold(out_tx), Arc::clone(&detached));
            let spawner = spawn_into(me, scope, move |me| {
                detach(me, move |me| {
                    let got = receive(me, go.word());
                    drop(go);
                    if unwinding(me) {
                        return 0;
                    }
                    assert_eq!(got.map(int), Ok(1));
                    let sent = send(me, out.word(), values::int_word(42));
                    drop(out);
                    if unwinding(me) {
                        return 0;
                    }
                    assert_eq!(sent, Ok(values::nil()));
                    values::nil()
                });
                flag.store(true, Ordering::SeqCst);
                until_cancelled(me)
            });
            let flag = Arc::clone(&detached);
            let sibling = spawn_into(me, scope, move |me| {
                yield_until(me, || flag.load(Ordering::SeqCst));
                let message = values::string("sibling failed");
                let location = values::string("a.cheby:1:2");
                // SAFETY: the running fiber's context and live strings.
                unsafe {
                    crate::panic::cheby_rt_panic(
                        ctx_of(me),
                        PanicKind::Explicit.code(),
                        message,
                        location,
                    );
                }
                drop_word(message);
                drop_word(location);
                0
            });
            drop_word(spawner);
            drop_word(sibling);
            assert!(!close(me, scope), "the scope did not fail");
            assert_eq!(failure_of(me).1, "sibling failed");
            assert_eq!(send(me, go_tx, values::int_word(1)), Ok(values::nil()));
            assert_eq!(receive(me, out_rx).map(int), Ok(42));
            for word in [go_tx, go_rx, out_tx, out_rx] {
                drop_word(word);
            }
            values::nil()
        });
    }
}

/// D-799: a detached fiber that a detached fiber spawns
/// joins the same group, and a teardown waits for both.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_spawned_by_a_detached_fiber_joins_its_group() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let both = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&both);
        let (tx, rx) = mpsc::channel();
        let outcome = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
            let inner = Arc::clone(&seen);
            detach(me, move |me| {
                detach(me, move |me| {
                    inner.fetch_add(1, Ordering::SeqCst);
                    spin(me)
                });
                seen.fetch_add(1, Ordering::SeqCst);
                spin(me)
            });
            tx.send(group_of(me)).unwrap();
            values::nil()
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        let root_group = rx.recv().unwrap();
        assert!(Arc::ptr_eq(&root_group, &group));
        let started = Instant::now();
        while both.load(Ordering::SeqCst) < 2 {
            assert!(started.elapsed() < testing::hang());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(group.live(), 2);
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped, report.halted),
            (2, 0, false)
        );
        assert_eq!(group.live(), 0);
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// An isolated entry (no runtime) that calls `spawn_detached`.
unsafe extern "C" fn isolated_spawn_detached(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's context.
    let me = unsafe { &*ctx };
    detach(me, |_| values::nil());
    values::nil()
}

/// D-692: without a runtime, `spawn_detached` ends the program with a
/// fatal error naming it. Runs itself in a child process.
#[test]
fn detached_spawn_without_a_runtime_needs_the_scheduler() {
    const CHILD: &str = "CHEBY_RT_TEST_SPAWN_DETACHED_ISOLATED";
    const NAME: &str =
        "fiber_ops::detached_tests::detached_spawn_without_a_runtime_needs_the_scheduler";
    if std::env::var_os(CHILD).is_some() {
        let domain = Domain::new();
        // SAFETY: a test thunk with the entry ABI.
        let outcome = unsafe { crate::entry::run_entry(isolated_spawn_detached, &config(&domain)) };
        panic!("the isolated entry ended: {outcome:?}");
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
            "fatal runtime error: fiber::spawn_detached needs the fiber scheduler, \
             which this entry does not run\n"
        ),
        "{stderr}"
    );
}

/// D-669, D-701: a detached fiber's panic is printed as its report where
/// its unwinding ends, after standard output is flushed (text the program
/// printed before comes first), as one write; it cancels nothing: a
/// sibling detached fiber and the root go on. Runs itself in a child
/// process whose standard output and error share one pipe.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_panic_is_reported_after_flushing_and_cancels_nothing() {
    const CHILD: &str = "CHEBY_RT_TEST_DETACHED_PANIC";
    const NAME: &str =
        "fiber_ops::detached_tests::detached_panic_is_reported_after_flushing_and_cancels_nothing";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        let domain = Domain::new();
        let outcome = root_outcome(&runtime, &domain, |me| {
            let (tx, rx) = channel(me, 0);
            let out = hold(tx);
            // A sibling that outlives the panic.
            detach(me, move |me| {
                let sent = send(me, out.word(), values::int_word(5));
                drop(out);
                if unwinding(me) {
                    return 0;
                }
                assert_eq!(sent, Ok(values::nil()));
                values::nil()
            });
            let before = values::string("before the panic|");
            // SAFETY: a live string, borrowed.
            unsafe { crate::io::cheby_io_print(before) };
            drop_word(before);
            detach(me, |me| {
                let message = values::string("a detached failure");
                let location = values::string("d.cheby:3:4");
                // SAFETY: the running fiber's context and live strings.
                unsafe {
                    crate::panic::cheby_rt_panic(
                        ctx_of(me),
                        PanicKind::Explicit.code(),
                        message,
                        location,
                    );
                }
                drop_word(message);
                drop_word(location);
                0
            });
            let group = group_of(me);
            // The panicking fiber ends; the sibling still waits to send.
            yield_until(me, || group.live() == 1);
            assert_eq!(receive(me, rx).map(int), Ok(5));
            drop_word(tx);
            drop_word(rx);
            yield_until(me, || group.live() == 0);
            values::nil()
        });
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
        eprintln!("|child done");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exec \"$0\" --exact \"$1\" --nocapture --test-threads=1 -q 2>&1")
        .arg(exe)
        .arg(NAME)
        .env(CHILD, "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("before the panic|panicked at d.cheby:3:4:\na detached failure\n"),
        "{text}"
    );
    assert_eq!(text.matches("panicked at").count(), 1, "{text}");
    assert!(text.contains("|child done"), "{text}");
}

/// D-694: a detached fiber marks its function shared and releases it when
/// the call ends; a spawn that runs out of slots is `out of memory` and
/// leaves the group as it was.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn detached_spawn_out_of_memory_leaves_the_group_as_it_was() {
    run_and_shutdown(1, |me| {
        let group = group_of(me);
        me.sched
            .runtime()
            .unwrap()
            .fail_stack
            .store(true, Ordering::SeqCst);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        detach(me, move |_| {
            flag.store(true, Ordering::SeqCst);
            values::nil()
        });
        assert!(unwinding(me), "the spawn did not raise");
        let record = crate::unwind::record::take_unwound(me);
        assert!(matches!(
            record,
            Some(crate::unwind::record::UnwindRecord::Panic(ref report))
                if report.message == crate::alloc::OUT_OF_MEMORY
        ));
        assert_eq!(group.live(), 0);
        assert!(!ran.load(Ordering::SeqCst));
        values::nil()
    });
}
