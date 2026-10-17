//! Tests of the blocking pool and of the fiber side that parks on it
//! (D-683, D-786, D-708; survey-io §4.3). Every test is named `pool_…`.
//! The pool's own tests use private pools ([`Pool::private`]), so their
//! thread counts are not shared with the tests running beside them; the
//! fiber tests use the harness of `fiber_ops::detached_tests` (Rust
//! actions as fibers of a `Runtime`). A test that leaves a fiber parked
//! for good (a halt, a stop with a pending job) runs in a child process,
//! as the lifecycle tests do. A test that waits ends through a gate, a
//! channel or a timeout, never by hanging.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::{Pool, Submitted, THREAD_NAME};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome};
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::fiber_ops::detached_tests::{config, detach, runtime, unwinding};
use crate::sched::blocking::{
    Hooks, PARKING_WOKEN, Probe, cheby_rt_blocking_call, run_blocking, run_blocking_in,
};
use crate::sched::testing;
use crate::sched::{AfterSwitch, CodeToken, DetachedGroup, FiberRef, park};
use crate::values;

/// How long a child process may take, at most, to exit.
const CHILD_LIMIT: Duration = Duration::from_secs(10);

/// Waits until `ready` holds on the host, or panics after [`testing::hang()`].
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "the host waited too long"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn ctx_ptr(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// A gate a job waits at until the test opens it, and what a gated
/// `@blocking` shim reports.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    /// Set by the shim once it runs.
    running: AtomicBool,
    /// Set when the code token that names this gate is dropped.
    code_dropped: AtomicBool,
    /// Whether the shim saw the token dropped while it still ran.
    saw_dropped: AtomicBool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn open(&self) {
        *self.open.lock() = true;
        self.opened.notify_all();
    }

    /// Waits until the gate is open; false after [`testing::hang()`].
    fn wait(&self) -> bool {
        let started = Instant::now();
        let mut open = self.open.lock();
        while !*open {
            if started.elapsed() > testing::hang() {
                return false;
            }
            self.opened.wait_for(&mut open, Duration::from_millis(100));
        }
        true
    }

    /// The word a gated shim gets: the gate's address.
    fn word(self: &Arc<Self>) -> u64 {
        u64::try_from(Arc::as_ptr(self).expose_provenance()).unwrap()
    }
}

/// An `@blocking` shim (D-818) whose one argument word is a
/// [`Gate`]'s address: it waits at the gate, then returns 7.
unsafe extern "C" fn gated_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test passes one word, the address of a gate it keeps
    // alive until the shim has returned.
    let gate =
        unsafe { &*std::ptr::with_exposed_provenance::<Gate>(usize::try_from(*args).unwrap()) };
    gate.running.store(true, Ordering::SeqCst);
    assert!(gate.wait(), "the gate never opened");
    gate.saw_dropped
        .store(gate.code_dropped.load(Ordering::SeqCst), Ordering::SeqCst);
    // SAFETY: the runtime's result cell.
    unsafe { out.write(7) };
}

/// The owner of a JIT's code, standing in for it: records its drop.
struct KeepAlive(Arc<Gate>);

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.0.code_dropped.store(true, Ordering::SeqCst);
    }
}

/// Runs `body` as a root fiber of a runtime of `threads` workers and
/// returns its word; panics if it panics or hangs. Shuts the runtime down
/// and checks that nothing leaked.
fn on_root(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) -> u64 {
    let runtime = runtime(threads);
    let domain = Domain::new();
    let outcome = testing::start_root_with(&runtime, &config(&domain), body)
        .recv_timeout(testing::hang())
        .expect("the root hung");
    let EntryOutcome::Returned(word) = outcome else {
        panic!("the root did not return: {outcome:?}");
    };
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    word
}

/// Runs this test binary's test `name` alone in a child process with the
/// environment variable `child` set and standard input piped and kept
/// open; returns its status and standard error, or panics if it does not
/// exit within [`CHILD_LIMIT`].
fn child(name: &str, child: &str) -> (std::process::ExitStatus, String) {
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
        thread::sleep(Duration::from_millis(5));
    };
    drop(stdin);
    let mut stderr = String::new();
    if let Some(mut pipe) = process.stderr.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stderr).ok();
    }
    (status, stderr)
}

// ---------------------------------------------------------------------
// The pool alone (private pools, no fibers)

/// D-683: with the cap reached, later jobs queue; they all run once the
/// running ones return.
#[test]
fn pool_a_lowered_cap_queues_jobs() {
    let pool = Pool::private(2, Duration::from_secs(10));
    let gate = Gate::new();
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..5 {
        let (gate, done) = (Arc::clone(&gate), Arc::clone(&done));
        let submitted = pool.submit(Box::new(move || {
            assert!(gate.wait());
            done.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    }
    wait_until(|| pool.queued() == 3);
    assert_eq!(pool.threads(), 2, "the cap was passed");
    gate.open();
    wait_until(|| done.load(Ordering::SeqCst) == 5);
    assert!(pool.threads() <= 2);
}

/// D-683: jobs run in the order they were submitted (one thread).
#[test]
fn pool_jobs_run_in_fifo_order() {
    let pool = Pool::private(1, Duration::from_secs(10));
    let gate = Gate::new();
    let order = Arc::new(Mutex::new(Vec::new()));
    for id in 0..6_usize {
        let (gate, order) = (Arc::clone(&gate), Arc::clone(&order));
        let submitted = pool.submit(Box::new(move || {
            if id == 0 {
                assert!(gate.wait());
            }
            order.lock().push(id);
        }));
        assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    }
    gate.open();
    wait_until(|| order.lock().len() == 6);
    assert_eq!(*order.lock(), [0, 1, 2, 3, 4, 5]);
}

/// D-683: a thread ends after its idle timeout with no job, never
/// earlier, and a later job starts a thread again.
#[test]
fn pool_idle_threads_exit_after_the_timeout() {
    const IDLE: Duration = Duration::from_millis(20);
    let pool = Pool::private(4, IDLE);
    let finished: Arc<Mutex<Option<Instant>>> = Arc::default();
    let at = Arc::clone(&finished);
    let submitted = pool.submit(Box::new(move || {
        assert_eq!(thread::current().name(), Some(THREAD_NAME));
        *at.lock() = Some(Instant::now());
    }));
    assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    wait_until(|| finished.lock().is_some());
    wait_until(|| pool.threads() == 0);
    let idle = finished.lock().map(|at| at.elapsed()).unwrap();
    assert!(idle >= IDLE, "a thread ended after {idle:?} idle");
    let again = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&again);
    let submitted = pool.submit(Box::new(move || ran.store(true, Ordering::SeqCst)));
    assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    wait_until(|| again.load(Ordering::SeqCst));
}

/// D-817: a refused thread while another pool thread
/// runs queues the job behind it, never inline.
#[test]
fn pool_a_refused_thread_queues_behind_a_running_one() {
    let pool = Pool::private(4, Duration::from_secs(10));
    let gate = Gate::new();
    let first = Arc::clone(&gate);
    let submitted = pool.submit(Box::new(move || assert!(first.wait())));
    assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    wait_until(|| pool.threads() == 1 && pool.queued() == 0);
    pool.refuse_threads(true);
    let ran = Arc::new(AtomicBool::new(false));
    let second = Arc::clone(&ran);
    let submitted = pool.submit(Box::new(move || second.store(true, Ordering::SeqCst)));
    assert!(matches!(submitted, Submitted::Queued), "{submitted:?}");
    assert_eq!((pool.threads(), pool.queued()), (1, 1));
    gate.open();
    wait_until(|| ran.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------
// The fiber side

/// D-683: the job runs on a pool thread, not on the fiber's worker.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_job_runs_on_a_pool_thread() {
    let ok = on_root(1, |me| {
        let worker = thread::current().id();
        let ran = run_blocking(me, || {
            let here = thread::current();
            (here.id(), here.name().map(str::to_owned))
        });
        let Ok((thread, name)) = ran else {
            return 0;
        };
        u64::from(thread != worker && name.as_deref() == Some(THREAD_NAME))
    });
    assert_eq!(ok, 1, "the job ran on the worker or an unnamed thread");
}

/// D-683, D-786's limit lifted: at one worker, another fiber runs while a
/// job waits, and the job waits for that fiber.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_another_fiber_runs_while_a_job_waits() {
    let ok = on_root(1, |me| {
        let (tx, rx) = mpsc::channel();
        testing::spawn(me, false, move |_| {
            tx.send(()).ok();
        });
        let got = run_blocking(me, move || rx.recv_timeout(Duration::from_secs(20)).is_ok());
        u64::from(got == Ok(true))
    });
    assert_eq!(ok, 1, "the job waited for a fiber that never ran");
}

/// survey-io §3.2: a job that completes before its fiber has switched
/// away finds it `PARKING`, turns it into `PARKING_WOKEN`, and the worker
/// requeues it: the wake is not lost. The mark is clear at the wake.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_job_that_completes_before_the_park_is_not_lost() {
    let probe = Arc::new(Probe::default());
    probe.complete_before_park.store(true, Ordering::SeqCst);
    let hooks = Hooks {
        probe: Some(Arc::clone(&probe)),
    };
    let word = on_root(1, move |me| {
        run_blocking_in(Pool::global(), me, || 42_u64, &hooks).unwrap_or(0)
    });
    assert_eq!(word, 42);
    assert_eq!(
        probe.state_before_park.load(Ordering::SeqCst),
        PARKING_WOKEN
    );
    assert_eq!(probe.woke.load(Ordering::SeqCst), 1);
    assert_eq!(probe.skipped.load(Ordering::SeqCst), 0);
    assert!(
        !probe.marked_at_wake.load(Ordering::SeqCst),
        "marked at the wake"
    );
}

/// A result that counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// D-670, D-683: a cancellation that reaches a fiber while its job runs
/// waits for the job (the teardown skips the marked fiber, D-786), then
/// the fiber gets `Err(Cancelled)` and the result is dropped.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_cancellation_during_a_job_waits_for_it_and_drops_the_result() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let gate = Gate::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    // 1: cancelled after the job finished; 2: something else.
    let outcome = Arc::new(AtomicUsize::new(0));
    let (g, d, f, o) = (
        Arc::clone(&gate),
        Arc::clone(&drops),
        Arc::clone(&finished),
        Arc::clone(&outcome),
    );
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let done = Arc::clone(&f);
            let result = run_blocking(me, move || {
                assert!(g.wait());
                done.store(true, Ordering::SeqCst);
                Counted(d)
            });
            let cancelled = matches!(result, Err(Cancelled)) && f.load(Ordering::SeqCst);
            drop(result);
            o.store(if cancelled { 1 } else { 2 }, Ordering::SeqCst);
            values::nil()
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1);
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (0, 1));
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "dropped before the job ended"
    );
    gate.open();
    wait_until(|| outcome.load(Ordering::SeqCst) != 0);
    assert_eq!(
        outcome.load(Ordering::SeqCst),
        1,
        "not cancelled after the job"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the result was not dropped once"
    );
    wait_until(|| group.live() == 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-816: a fiber already targeted by a
/// cancellation starts no job.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_cancellation_pending_before_submission_starts_no_job() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let ready = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    // 1: refused with `Cancelled`; 2: something else.
    let outcome = Arc::new(AtomicUsize::new(0));
    let (r, j, o) = (Arc::clone(&ready), Arc::clone(&ran), Arc::clone(&outcome));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            r.store(true, Ordering::SeqCst);
            let started = Instant::now();
            while cancel::check(me).is_ok() {
                assert!(started.elapsed() < testing::hang(), "never cancelled");
                park(me, AfterSwitch::Requeue);
            }
            let result = run_blocking(me, move || j.store(true, Ordering::SeqCst));
            o.store(
                if result == Err(Cancelled) { 1 } else { 2 },
                Ordering::SeqCst,
            );
            values::nil()
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| ready.load(Ordering::SeqCst));
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (1, 0));
    assert_eq!(outcome.load(Ordering::SeqCst), 1);
    assert!(
        !ran.load(Ordering::SeqCst),
        "a job ran for a cancelled fiber"
    );
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-817: when the system refuses a pool thread and
/// none runs, the job runs inline on the fiber's worker, with D-786's
/// mark set while it runs and cleared after.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_refused_thread_runs_the_job_inline_with_the_mark() {
    let pool = Pool::private(4, Duration::from_secs(10));
    pool.refuse_threads(true);
    let ok = on_root(1, move |me| {
        let worker = thread::current().id();
        let fiber = FiberRef::from_ctx(me);
        let ran = run_blocking_in(
            pool,
            me,
            move || {
                // SAFETY: the fiber runs this job itself, inline.
                let marked = unsafe { fiber.ctx() }.sched.blocking();
                (thread::current().id(), marked)
            },
            &Hooks::default(),
        );
        u64::from(ran == Ok((worker, true)) && !me.sched.blocking())
    });
    assert_eq!(ok, 1, "the job did not run inline, marked");
    assert_eq!(pool.threads(), 0);
}

/// D-683, D-692: without a runtime (an isolated entry) the job runs
/// inline on the calling thread, marked while it runs.
#[test]
fn pool_without_a_runtime_the_job_runs_inline() {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let seen: Arc<Mutex<Option<(bool, bool, bool)>>> = Arc::default();
    let out = Arc::clone(&seen);
    let task = move || {
        let me = crate::entry::current_fiber().unwrap();
        let here = thread::current().id();
        let fiber = FiberRef::from_ctx(me);
        let ran: Result<(ThreadId, bool), Cancelled> = run_blocking(me, move || {
            // SAFETY: the entry runs this job itself, inline.
            let marked = unsafe { fiber.ctx() }.sched.blocking();
            (thread::current().id(), marked)
        });
        let (thread, marked) = ran.unwrap();
        *out.lock() = Some((thread == here, marked, me.sched.blocking()));
        0
    };
    // SAFETY: the task makes no Cheby value and uses no trampoline.
    let outcome = unsafe { crate::entry::run_task(&config, task) }.unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(*seen.lock(), Some((true, true, false)));
}

/// D-814, survey-io risk 2: the pool thread clears the
/// mark before it wakes the fiber, so a woken fiber left in a run queue
/// for longer than the teardown's 5 ms poll (its only worker is busy for
/// 200 ms) is not counted as blocked, and the teardown waits for it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_woken_fiber_left_queued_is_not_counted_as_blocked() {
    let runtime = runtime(1);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let (tx, rx) = mpsc::channel::<()>();
    let left = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let (gone, job_done) = (Arc::clone(&left), Arc::clone(&returned));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let _ = run_blocking(me, move || {
                let got = rx.recv_timeout(testing::hang()).is_ok();
                job_done.store(true, Ordering::SeqCst);
                got
            });
            gone.store(true, Ordering::SeqCst);
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1);
    // A root on the default group keeps the only worker busy with no entry
    // check for 200 ms.
    let spinning = Arc::new(AtomicBool::new(false));
    let spun = Arc::new(AtomicBool::new(false));
    let (busy, done) = (Arc::clone(&spinning), Arc::clone(&spun));
    let hog = testing::start_root_with(&runtime, &config(&domain), move |_| {
        busy.store(true, Ordering::SeqCst);
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(200) {
            std::hint::spin_loop();
        }
        done.store(true, Ordering::SeqCst);
        values::nil()
    });
    wait_until(|| spinning.load(Ordering::SeqCst));
    tx.send(()).unwrap();
    // The job returns; its pool thread clears the mark, then queues the
    // fiber behind the hog. The teardown's two polls, 5 ms apart, must not
    // find it blocked.
    wait_until(|| returned.load(Ordering::SeqCst));
    assert!(
        !spun.load(Ordering::SeqCst),
        "the hog ended too early to test"
    );
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (1, 0));
    assert!(left.load(Ordering::SeqCst));
    assert!(spun.load(Ordering::SeqCst), "the teardown did not wait");
    let hogged = hog.recv_timeout(testing::hang()).unwrap();
    assert!(matches!(hogged, EntryOutcome::Returned(_)), "{hogged:?}");
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-670, D-683, D-734, D-818: an `@blocking` call whose
/// fiber is cancelled while the shim runs returns once the shim returns
/// and raises through the flag (the call site tests the flag, never the
/// result word).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_blocking_call_raises_a_cancellation_through_the_flag() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let gate = Gate::new();
    // 1: unwound with the flag set; 2 + word: returned the word.
    let outcome = Arc::new(AtomicUsize::new(0));
    let (g, o) = (Arc::clone(&gate), Arc::clone(&outcome));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let args = [g.word()];
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose gate this closure keeps alive, and static code.
            let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), gated_shim, args.as_ptr(), 1) };
            let unwound = unwinding(me);
            let code = if unwound {
                1
            } else {
                2_usize.saturating_add(usize::try_from(word).unwrap_or(0))
            };
            o.store(code, Ordering::SeqCst);
            drop(g);
            if unwound { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1 && gate.running.load(Ordering::SeqCst));
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (0, 1));
    gate.open();
    wait_until(|| outcome.load(Ordering::SeqCst) != 0);
    assert_eq!(
        outcome.load(Ordering::SeqCst),
        1,
        "the cancellation was lost"
    );
    wait_until(|| group.live() == 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// An `@blocking` call returns the shim's word unchanged (D-818).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_blocking_call_returns_the_shims_word() {
    let gate = Gate::new();
    gate.open();
    let g = Arc::clone(&gate);
    let word = on_root(1, move |me| {
        let args = [g.word()];
        // SAFETY: the running fiber's context, one argument word for a shim
        // whose gate this closure keeps alive, and static code.
        unsafe { cheby_rt_blocking_call(ctx_ptr(me), gated_shim, args.as_ptr(), 1) }
    });
    assert_eq!(word, 7);
}

// ---------------------------------------------------------------------
// Roots that block while the host stops the runtime

/// D-815, D-708 (gate 1's review finding
/// CONC-W1-1): a root that makes a blocking call while the host's stop
/// runs it runs to its end and reports. The call goes to the pool; the
/// workers wait for it, and its completion wakes the root for them. (The
/// stop's give-up is held here so that it waits however slow the machine
/// is; `pool_job_keeps_code_alive` and
/// `pool_a_runtime_dropped_with_a_pending_job_wakes_nothing` show a stop
/// that gives up.)
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_root_that_blocks_during_a_stop_reports() {
    let runtime = runtime(2);
    testing::hold_pool(&runtime.shared_handle());
    let domain = Domain::new();
    let started = Arc::new(AtomicBool::new(false));
    let begun = Arc::clone(&started);
    let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        begun.store(true, Ordering::SeqCst);
        // On its worker, with no entry check, until the stop set its flag.
        let waiting = Instant::now();
        while !testing::stopping(me) {
            if waiting.elapsed() > testing::hang() {
                return 0;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let worker = thread::current().id();
        match run_blocking(me, move || thread::current().id() == worker) {
            Ok(false) => 5,
            Ok(true) => 6,
            Err(Cancelled) => 7,
        }
    });
    wait_until(|| started.load(Ordering::SeqCst));
    runtime.shutdown();
    let outcome = root.recv_timeout(testing::hang());
    assert!(
        matches!(outcome, Ok(EntryOutcome::Returned(5))),
        "the root's call did not run on the pool and report: {outcome:?}"
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-815, D-708 (gate 1's review finding
/// CONC-W1-1): a root waiting on a pool job when the host's stop begins is
/// woken by the job's completion after the stop flag was set, and the
/// workers, which waited for it, run it to its end. (The give-up is held,
/// as above.)
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_job_pending_when_a_stop_begins_wakes_its_root() {
    let runtime = runtime(2);
    let shared = runtime.shared_handle();
    testing::hold_pool(&shared);
    let domain = Domain::new();
    let gate = Gate::new();
    let probe = Arc::new(Probe::default());
    let in_job = Arc::new(AtomicBool::new(false));
    let (g, p, j) = (Arc::clone(&gate), Arc::clone(&probe), Arc::clone(&in_job));
    let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        let hooks = Hooks { probe: Some(p) };
        let job = move || {
            j.store(true, Ordering::SeqCst);
            g.wait()
        };
        match run_blocking_in(Pool::global(), me, job, &hooks) {
            Ok(true) => 5,
            Ok(false) => 6,
            Err(Cancelled) => 7,
        }
    });
    wait_until(|| in_job.load(Ordering::SeqCst));
    let stopper = thread::spawn(move || runtime.shutdown());
    wait_until(|| testing::runtime_stopping(&shared));
    gate.open();
    let outcome = root.recv_timeout(testing::hang());
    stopper.join().unwrap();
    assert!(
        matches!(outcome, Ok(EntryOutcome::Returned(5))),
        "the root was not woken during the stop: {outcome:?}"
    );
    assert_eq!(
        (
            probe.woke.load(Ordering::SeqCst),
            probe.skipped.load(Ordering::SeqCst)
        ),
        (1, 0)
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-815, D-708 (gate 1's review finding
/// CONC-W1-1, its stress variant): eight roots that make quick blocking
/// calls in a loop while the host stops the runtime all run to their end
/// and report, at 1 to 4 workers: no completion's wake is lost to a worker
/// that ended (the give-up is held, as above).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_roots_blocking_in_a_loop_all_report_after_a_stop() {
    const ROOTS: usize = 8;
    const CALLS: u64 = 1000;
    let rounds: u64 = if testing::emulated() { 4 } else { 16 };
    let expected = CALLS * (CALLS - 1) / 2;
    for round in 0..rounds {
        let threads = usize::try_from(round % 4).unwrap() + 1;
        let runtime = runtime(threads);
        testing::hold_pool(&runtime.shared_handle());
        let domain = Domain::new();
        let started = Arc::new(AtomicUsize::new(0));
        let roots: Vec<_> = (0..ROOTS)
            .map(|_| {
                let started = Arc::clone(&started);
                testing::start_root_with(&runtime, &config(&domain), move |me| {
                    started.fetch_add(1, Ordering::SeqCst);
                    let mut sum = 0_u64;
                    for call in 0..CALLS {
                        match run_blocking(me, move || call) {
                            Ok(word) => sum += word,
                            Err(Cancelled) => return u64::MAX,
                        }
                    }
                    sum
                })
            })
            .collect();
        wait_until(|| started.load(Ordering::SeqCst) == ROOTS);
        thread::sleep(Duration::from_micros(200 + 300 * (round % 5)));
        runtime.shutdown();
        for (index, root) in roots.into_iter().enumerate() {
            let outcome = root.recv_timeout(testing::hang());
            assert!(
                matches!(outcome, Ok(EntryOutcome::Returned(sum)) if sum == expected),
                "round {round} ({threads} workers), root {index}: {outcome:?}"
            );
        }
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

// ---------------------------------------------------------------------
// Stops and halts with a pending job (child processes)

/// D-815: a halt with a pending job wakes nothing
/// when the job completes; the process exits promptly.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_halt_with_a_pending_job_wakes_nothing() {
    const CHILD: &str = "CHEBY_RT_TEST_POOL_HALT";
    const NAME: &str = "pool::tests::pool_a_halt_with_a_pending_job_wakes_nothing";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, gate, probe) = pending_job();
        runtime.halt();
        gate.open();
        wait_until(|| probe.completed.load(Ordering::SeqCst));
        wait_until(|| probe.skipped.load(Ordering::SeqCst) == 1);
        assert_eq!(
            probe.woke.load(Ordering::SeqCst),
            0,
            "a halted fiber was woken"
        );
        eprintln!("woke nothing");
        return;
    }
    let (status, stderr) = child(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("woke nothing\n"), "{stderr}");
}

/// D-815: a runtime stopped (dropped) with a
/// pending job wakes nothing when the job completes, and nothing touches
/// the freed runtime's code; the process exits promptly.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_a_runtime_dropped_with_a_pending_job_wakes_nothing() {
    const CHILD: &str = "CHEBY_RT_TEST_POOL_DROP";
    const NAME: &str = "pool::tests::pool_a_runtime_dropped_with_a_pending_job_wakes_nothing";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, gate, probe) = pending_job();
        drop(runtime);
        gate.open();
        wait_until(|| probe.completed.load(Ordering::SeqCst));
        wait_until(|| probe.skipped.load(Ordering::SeqCst) == 1);
        assert_eq!(
            probe.woke.load(Ordering::SeqCst),
            0,
            "a stopped fiber was woken"
        );
        eprintln!("woke nothing");
        return;
    }
    let (status, stderr) = child(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("woke nothing\n"), "{stderr}");
}

/// A runtime of two workers whose detached fiber waits on a job gated by
/// the returned gate, with the returned probe; returns once the fiber is
/// marked.
fn pending_job() -> (crate::sched::Runtime, Arc<Gate>, Arc<Probe>) {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let gate = Gate::new();
    let probe = Arc::new(Probe::default());
    let (g, p) = (Arc::clone(&gate), Arc::clone(&probe));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let hooks = Hooks { probe: Some(p) };
            let _ = run_blocking_in(Pool::global(), me, move || g.wait(), &hooks);
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1);
    (runtime, gate, probe)
}

/// D-818, refines D-708: JIT code outlives every pool job
/// that runs it. A stop while a shim runs on a pool thread releases the
/// runtime's code tokens, but the job holds clones, so the code's owner is
/// dropped only after the job ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pool_job_keeps_code_alive() {
    const CHILD: &str = "CHEBY_RT_TEST_POOL_CODE";
    const NAME: &str = "pool::tests::pool_job_keeps_code_alive";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(1);
        let gate = Gate::new();
        runtime.attach(CodeToken::new(Arc::new(KeepAlive(Arc::clone(&gate)))));
        let domain = Domain::new();
        let g = Arc::clone(&gate);
        let _root = testing::start_root_with(&runtime, &config(&domain), move |me| {
            let args = [g.word()];
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose gate this closure keeps alive, and static code.
            unsafe { cheby_rt_blocking_call(ctx_ptr(me), gated_shim, args.as_ptr(), 1) }
        });
        wait_until(|| gate.running.load(Ordering::SeqCst));
        runtime.shutdown();
        assert!(
            !gate.code_dropped.load(Ordering::SeqCst),
            "the stop freed code a pool job runs"
        );
        gate.open();
        wait_until(|| gate.code_dropped.load(Ordering::SeqCst));
        assert!(
            !gate.saw_dropped.load(Ordering::SeqCst),
            "the code was freed while the shim ran"
        );
        eprintln!("kept alive");
        return;
    }
    let (status, stderr) = child(NAME, CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("kept alive\n"), "{stderr}");
}
