//! Tests of a pool thread acting for its parked fiber during a package's
//! `@blocking` call (D-950, `ffi_pool.rs`, `sched::blocking`'s
//! `cheby_rt_blocking_call`). The shims stand in for generated `cb$blk$`
//! shims (D-818): `extern "C"` functions over argument words that call
//! `cheby.h`'s functions as C code would. Tests named `ffi_pool_job_…` run
//! a job to its end; `ffi_pool_abandon_…` abandon one by a give-up or a
//! halt, most of them in a child process, since they leave a fiber parked
//! for good (as `pool::tests` does). Every name holds `pool_`, so
//! `cargo test -p cheby_runtime pool_` runs them all. The protocol is
//! modelled in `ffi_models.rs` (`ffi_pool_model_*`).

use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::abi::Word;
use crate::entry::EntryOutcome;
use crate::ffi::{
    cheby_ffi_drop, cheby_ffi_dup, cheby_ffi_int_new, cheby_ffi_string_bytes, cheby_ffi_string_new,
    cheby_ffi_write_stdout,
};
use crate::fiber_ops::detached_tests::{config, detach, runtime, unwinding};
use crate::handles::{HandleKind, HandlePayload, new_handle};
use crate::pool::THREAD_NAME;
use crate::sched::blocking::cheby_rt_blocking_call;
use crate::sched::{DetachedGroup, testing};
use crate::{layout, list, rc, string, values};

/// Waits on the host until `ready` holds, or panics after
/// [`testing::hang`].
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

/// The control block pointer of a running fiber.
fn ctx_ptr(me: &crate::ctx::ExecCtx) -> *mut crate::ctx::ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// The word of a `cheby_value *`.
fn word_of(value: *mut c_void) -> Word {
    u64::try_from(value.expose_provenance()).unwrap()
}

/// The `cheby_value *` of a word.
fn value_of(word: Word) -> *mut c_void {
    std::ptr::with_exposed_provenance_mut(usize::try_from(word).unwrap())
}

/// The text of a `String` word.
fn text(word: Word) -> String {
    // SAFETY: a live `String`.
    unsafe { string::to_rust_string(word) }.unwrap()
}

/// What a test's shim reports, and the gate it may wait at; its address
/// is the shim's first argument word.
#[derive(Default)]
struct Probe {
    /// Whether the shim ran on a pool thread.
    on_pool: AtomicBool,
    /// The address of the accounting domain the shim's thread charged.
    domain: AtomicUsize,
    /// Set by the shim once it runs.
    running: AtomicBool,
    /// Opened by the test.
    open: AtomicBool,
    /// Set by the shim once it is done.
    done: AtomicBool,
    /// What an abandoned job's calls returned: `cheby_ffi_string_new` (1
    /// when NULL), `cheby_ffi_write_stdout`'s result, `cheby_ffi_dup`'s
    /// result was its argument (1), the borrowed argument's text was
    /// readable (1).
    null_alloc: AtomicUsize,
    write: AtomicI64,
    dup_same: AtomicUsize,
    readable: AtomicUsize,
    /// Allocations the shim makes in a loop, and how many fibers must have
    /// started before it does.
    rounds: AtomicUsize,
    /// The thread the shim ran on.
    thread: Mutex<Option<thread::ThreadId>>,
    /// The reference a shim duplicated from its borrowed argument and keeps
    /// for a later call ([`keep_shim`], [`drop_kept_shim`]).
    kept: AtomicU64,
    /// The address of the control block of the fiber that owns the test's
    /// value of [`PoolNode`]'s type.
    owner: AtomicUsize,
    /// What [`drop_pool_node`] saw: how many times it ran, whether on a
    /// pool thread, whether with the owner fiber current, and whether after
    /// the shim that released the last reference had returned.
    drops: AtomicUsize,
    drop_on_pool: AtomicBool,
    drop_owner_current: AtomicBool,
    drop_after_shim: AtomicBool,
    /// What a [`ThreadProbe`] finalizer saw beyond those: its thread, that
    /// thread's name, and the address of the fiber current there (0 for
    /// none).
    drop_thread: Mutex<Option<thread::ThreadId>>,
    drop_thread_name: Mutex<Option<String>>,
    drop_fiber: AtomicUsize,
    /// How many drops had run when the second call returned to the fiber,
    /// and the domain's live objects then, less those at the root's start.
    drops_at_return: AtomicUsize,
    live_left: AtomicI64,
    /// Calls that ran inline after a give-up with their allocation and
    /// their output done, the abandoned jobs of their fiber afterwards, and
    /// whether the fiber was woken after a call that a halt marked.
    inline_ok: AtomicUsize,
    kept_jobs: AtomicUsize,
    woken: AtomicBool,
    /// Counts of the abandoned list a test reads on its fiber, in order.
    counts: Mutex<Vec<usize>>,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The shim's argument word: this probe's address.
    fn word(self: &Arc<Self>) -> u64 {
        u64::try_from(Arc::as_ptr(self).expose_provenance()).unwrap()
    }

    /// Waits until the test opens the gate, at most [`testing::hang`].
    fn pass(&self) {
        let started = Instant::now();
        while !self.open.load(Ordering::SeqCst) && started.elapsed() < testing::hang() {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The probe a shim's first argument word names.
///
/// # Safety
///
/// `args` holds at least one word, a [`Probe`]'s address that the test
/// keeps alive while the shim runs.
unsafe fn probe<'a>(args: *const u64) -> &'a Probe {
    // SAFETY: forwarded from the caller.
    unsafe { &*std::ptr::with_exposed_provenance::<Probe>(usize::try_from(*args).unwrap()) }
}

/// Records where the shim runs and which domain its thread charges.
fn record_thread(probe: &Probe) {
    probe.on_pool.store(
        thread::current().name() == Some(THREAD_NAME),
        Ordering::SeqCst,
    );
    probe.domain.store(
        Arc::as_ptr(&alloc::current_domain()).addr(),
        Ordering::SeqCst,
    );
    *probe.thread.lock() = Some(thread::current().id());
}

/// A shim that builds a `String` with `cheby_ffi_string_new` and returns
/// it, as a package's `@blocking` C function would (D-950).
unsafe extern "C" fn build_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    probe.running.store(true, Ordering::SeqCst);
    probe.pass();
    let text = b"built on the pool";
    // SAFETY: valid bytes; the result is the call's owned result.
    let made = unsafe { cheby_ffi_string_new(text.as_ptr().cast(), text.len()) };
    probe.done.store(true, Ordering::SeqCst);
    // SAFETY: the runtime's result cell.
    unsafe { out.write(word_of(made)) };
}

/// Runs `body` as a root fiber of a runtime of `threads` workers in
/// `domain` and returns its outcome; shuts the runtime down.
fn on_root(
    threads: usize,
    domain: &Arc<Domain>,
    body: impl FnOnce(&crate::ctx::ExecCtx) -> u64 + Send + 'static,
) -> EntryOutcome {
    let runtime = runtime(threads);
    let outcome = testing::start_root_with(&runtime, &config(domain), body)
        .recv_timeout(testing::hang())
        .expect("the root hung");
    runtime.shutdown();
    outcome
}

/// D-950: a job's calls enter until the mark, and none after it; the mark
/// keeps the job's domain.
#[test]
fn ffi_pool_job_enters_until_the_mark() {
    let domain = Domain::new();
    let job = FfiJob::new(Arc::clone(&domain), std::ptr::null());
    assert!(job.enter());
    assert_eq!(job.abandon(), Some(1), "one call was in flight");
    job.leave();
    assert!(!job.enter());
    assert!(job.abandoned());
    assert_eq!(job.abandon(), None, "marked once");
    assert!(keeps_domain(&domain));
}

/// D-950: the shim runs on a pool thread that acts for the parked fiber:
/// the `String` it builds is charged to the fiber's domain, the fiber reads
/// it after the call, and its release on the fiber leaves the domain empty.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_builds_values_charged_to_the_fibers_domain() {
    let domain = Domain::new();
    let probe = Probe::new();
    probe.open.store(true, Ordering::SeqCst);
    let p = Arc::clone(&probe);
    let d = Arc::clone(&domain);
    let outcome = on_root(2, &domain, move |me| {
        let args = [p.word()];
        // SAFETY: the running fiber's context, one argument word for a
        // shim whose probe this closure keeps alive, and static code.
        let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), build_shim, args.as_ptr(), 1) };
        assert!(!unwinding(me), "the call raised");
        assert_eq!(text(word), "built on the pool");
        assert_eq!(d.live_objects(), 1, "the String is the fiber's");
        // SAFETY: the call's owned result.
        unsafe { rc::drop(word) };
        drop(p);
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    assert_eq!(
        probe.domain.load(Ordering::SeqCst),
        Arc::as_ptr(&domain).addr(),
        "the pool thread charged another domain"
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    assert_eq!(domain.live_bytes(), 0, "bytes leaked");
    assert!(current_job().is_none(), "the host slot keeps a job");
}

/// A shim whose first allocation fails on the pool thread: it and the
/// next allocation return NULL (D-946), and it returns NULL.
unsafe extern "C" fn failing_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    crate::alloc::refuse_next_system_allocation();
    let text = b"never built";
    // SAFETY: valid bytes.
    let first = unsafe { cheby_ffi_string_new(text.as_ptr().cast(), text.len()) };
    // SAFETY: callable with any argument.
    let second = unsafe { cheby_ffi_int_new(1) };
    probe.null_alloc.store(
        usize::from(first.is_null() && second.is_null()),
        Ordering::SeqCst,
    );
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// D-950, D-946: an allocation that fails on the pool thread returns NULL
/// there, and the `out of memory` panic is raised on the fiber when it
/// resumes.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_a_failed_allocation_raises_on_the_fiber() {
    let _serial = crate::alloc::tests::serial();
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let outcome = on_root(1, &domain, move |me| {
        let args = [p.word()];
        // SAFETY: as in `ffi_pool_job_builds_values_charged_to_the_fibers_domain`.
        let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), failing_shim, args.as_ptr(), 1) };
        drop(p);
        let flagged = unwinding(me);
        // The catch point's work: the record the flag stands for.
        let record = crate::unwind::record::take_record(me);
        crate::unwind::flag::clear(me);
        let raised = matches!(
            record,
            Some(crate::unwind::record::UnwindRecord::Panic(report))
                if report.message == alloc::OUT_OF_MEMORY
        );
        u64::from(word == 0 && flagged && raised)
    });
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    assert_eq!(
        probe.null_alloc.load(Ordering::SeqCst),
        1,
        "an allocation succeeded"
    );
    assert_eq!(
        outcome,
        EntryOutcome::Returned(1),
        "the failure was not raised on the fiber as `out of memory`"
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-950, D-670: a fiber cancelled while its shim runs gets the call's
/// owned result back with the flag set, so it releases it on the fiber
/// (the wrapper's helper does, `cheby_rt_ffi_value`'s cleanup), and the
/// domain ends empty.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_a_cancelled_call_hands_its_result_back() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let probe = Probe::new();
    // 1: unwound with the built String; 2: anything else.
    let outcome = Arc::new(AtomicUsize::new(0));
    let (p, o) = (Arc::clone(&probe), Arc::clone(&outcome));
    let root = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        detach(me, move |me| {
            let args = [p.word()];
            // SAFETY: as above.
            let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), build_shim, args.as_ptr(), 1) };
            let unwound = unwinding(me);
            let code = if unwound && word != 0 && text(word) == "built on the pool" {
                1
            } else {
                2
            };
            // What the wrapper's cleanup does with the owned result.
            // SAFETY: the call's owned result, or 0.
            unsafe { rc::drop(word) };
            o.store(code, Ordering::SeqCst);
            drop(p);
            if unwound { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
    wait_until(|| group.blocked() == 1 && probe.running.load(Ordering::SeqCst));
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (0, 1));
    probe.open.store(true, Ordering::SeqCst);
    wait_until(|| outcome.load(Ordering::SeqCst) != 0);
    assert_eq!(
        outcome.load(Ordering::SeqCst),
        1,
        "the result was not handed back with the flag"
    );
    wait_until(|| group.live() == 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// How many fibers of the domain-counter test must have started before
/// any of them allocates: the shim's root and three others.
const PARTICIPANTS: usize = 4;

/// Waits until `PARTICIPANTS` have started, at most [`testing::hang`].
fn all_started(started: &AtomicUsize) {
    started.fetch_add(1, Ordering::SeqCst);
    let at = Instant::now();
    while started.load(Ordering::SeqCst) < PARTICIPANTS && at.elapsed() < testing::hang() {
        thread::yield_now();
    }
}

/// The started count of the domain-counter test, shared with its shim.
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// A shim that builds and drops `rounds` strings on its pool thread once
/// every participant started.
unsafe extern "C" fn churning_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    all_started(&STARTED);
    let text = b"churn";
    for _ in 0..probe.rounds.load(Ordering::SeqCst) {
        // SAFETY: valid bytes; the value is dropped at once.
        unsafe {
            let made = cheby_ffi_string_new(text.as_ptr().cast(), text.len());
            cheby_ffi_drop(made);
        }
    }
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// D-950, D-691: a pool job charges its fiber's domain while three other
/// fibers of the same domain allocate and free on their workers at the
/// same time; the domain's counts end at zero.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_charges_beside_other_fibers_of_its_domain() {
    const ROUNDS: usize = 5_000;
    STARTED.store(0, Ordering::SeqCst);
    let runtime = runtime(PARTICIPANTS);
    let domain = Domain::new();
    let probe = Probe::new();
    probe.rounds.store(ROUNDS, Ordering::SeqCst);
    let p = Arc::clone(&probe);
    let mut roots = vec![testing::start_root_with(
        &runtime,
        &config(&domain),
        move |me| {
            let args = [p.word()];
            // SAFETY: as above.
            let word =
                unsafe { cheby_rt_blocking_call(ctx_ptr(me), churning_shim, args.as_ptr(), 1) };
            drop(p);
            if unwinding(me) { 1 } else { word }
        },
    )];
    for _ in 1..PARTICIPANTS {
        roots.push(testing::start_root_with(&runtime, &config(&domain), |_| {
            all_started(&STARTED);
            for _ in 0..ROUNDS {
                let made = values::string("churn");
                // SAFETY: the value made above.
                unsafe { rc::drop(made) };
            }
            0
        }));
    }
    for root in roots {
        let outcome = root.recv_timeout(testing::hang()).expect("a root hung");
        assert_eq!(outcome, EntryOutcome::Returned(0));
    }
    runtime.shutdown();
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    assert_eq!(
        domain.live_objects(),
        0,
        "the domain's object count drifted"
    );
    assert_eq!(domain.live_bytes(), 0, "the domain's byte count drifted");
}

// Another fiber's job, run inline (child processes) -------------------------

/// What one root of [`refused_pair`] ended with, and its probe and domain.
type Ran = (EntryOutcome, Arc<Probe>, Arc<Domain>);

/// Runs two roots of a two-worker runtime, each in a domain of its own,
/// that both call `shim` while the process's pool refuses every thread.
/// The first refusal waits until both jobs are queued, so one root gets
/// both back and runs them inline, its own and the other's (D-817). Each
/// root returns what `after` makes of its call's result word. Only in a
/// child process: the refusal is the global pool's.
fn refused_pair(
    shim: unsafe extern "C" fn(*const u64, *mut u64),
    after: fn(&crate::ctx::ExecCtx, u64) -> u64,
) -> [Ran; 2] {
    let pool = crate::pool::Pool::global();
    pool.refuse_until_queued(2);
    pool.refuse_threads(true);
    let runtime = runtime(2);
    let start = || {
        let domain = Domain::new();
        let probe = Probe::new();
        probe.open.store(true, Ordering::SeqCst);
        let p = Arc::clone(&probe);
        let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
            let args = [p.word()];
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose probe this closure keeps alive, and static code.
            let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), shim, args.as_ptr(), 1) };
            drop(p);
            after(me, word)
        });
        (root, probe, domain)
    };
    let (a, b) = (start(), start());
    let ended = |(root, probe, domain): (std::sync::mpsc::Receiver<EntryOutcome>, _, _)| {
        let outcome = root.recv_timeout(testing::hang()).expect("a root hung");
        (outcome, probe, domain)
    };
    let pair = [ended(a), ended(b)];
    runtime.shutdown();
    pool.refuse_threads(false);
    pair
}

/// Checks that one root ran both jobs inline and that each job charged
/// its own fiber's domain, which ends empty.
fn check_inline_pair(pair: &[Ran; 2]) {
    for (outcome, probe, domain) in pair {
        assert_eq!(*outcome, EntryOutcome::Returned(1), "{outcome:?}");
        assert!(
            !probe.on_pool.load(Ordering::SeqCst),
            "ran on a pool thread"
        );
        assert_eq!(
            probe.domain.load(Ordering::SeqCst),
            Arc::as_ptr(domain).addr(),
            "a job charged another fiber's domain"
        );
        assert_eq!(domain.live_objects(), 0, "the domain's objects drifted");
        assert_eq!(domain.live_bytes(), 0, "the domain's bytes drifted");
    }
    let threads: Vec<_> = pair
        .iter()
        .map(|(_, probe, _)| *probe.thread.lock())
        .collect();
    assert!(threads[0].is_some(), "a shim did not run");
    assert_eq!(threads[0], threads[1], "no root ran the other's job");
    assert!(current_job().is_none(), "the host slot keeps a job");
    eprintln!("inline ok");
}

/// What a root of the building test makes of its call: 1 when it got the
/// `String` without the flag; the `String` is released on the fiber.
fn kept_built(me: &crate::ctx::ExecCtx, word: u64) -> u64 {
    let built = !unwinding(me) && word != 0 && text(word) == "built on the pool";
    // SAFETY: the call's owned result, or 0.
    unsafe { rc::drop(word) };
    u64::from(built)
}

/// What a root of the failing test makes of its call: 1 when it got 0
/// with the `out of memory` panic raised on it.
fn raised_out_of_memory(me: &crate::ctx::ExecCtx, word: u64) -> u64 {
    let flagged = unwinding(me);
    // The catch point's work: the record the flag stands for.
    let record = crate::unwind::record::take_record(me);
    crate::unwind::flag::clear(me);
    let raised = matches!(
        record,
        Some(crate::unwind::record::UnwindRecord::Panic(report))
            if report.message == alloc::OUT_OF_MEMORY
    );
    u64::from(word == 0 && flagged && raised)
}

/// D-950, D-817, D-1268: after a refused pool
/// thread, a fiber that runs another fiber's queued job inline acts for
/// that fiber as a pool thread would: the `String` the other fiber's C
/// code builds is charged to the other fiber's domain, and both domains
/// end empty.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_inline_for_another_fiber_charges_its_domain() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_INLINE_OTHER_BUILD";
    if std::env::var_os(CHILD).is_some() {
        check_inline_pair(&refused_pair(build_shim, kept_built));
        return;
    }
    let (status, stderr) = child(
        "ffi_pool_job_inline_for_another_fiber_charges_its_domain",
        CHILD,
    );
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("inline ok\n"), "{stderr}");
}

/// D-950, D-946, D-817, D-1268: a failed allocation
/// of another fiber's job run inline is recorded in that job and raised
/// on its own fiber when it resumes, never on the fiber that ran it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_inline_for_another_fiber_raises_on_it() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_INLINE_OTHER_FAIL";
    if std::env::var_os(CHILD).is_some() {
        let _serial = crate::alloc::tests::serial();
        let pair = refused_pair(failing_shim, raised_out_of_memory);
        for (_, probe, _) in &pair {
            assert_eq!(
                probe.null_alloc.load(Ordering::SeqCst),
                1,
                "an allocation succeeded"
            );
        }
        check_inline_pair(&pair);
        return;
    }
    let (status, stderr) = child("ffi_pool_job_inline_for_another_fiber_raises_on_it", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("inline ok\n"), "{stderr}");
}

// The last reference released on a pool thread (wave 9's FFI item, part (i))

/// The handle of the test external type `PoolNode`: the probe its drop
/// function reports to.
struct PoolNode {
    probe: Arc<Probe>,
}

/// The drop function of the test external type `PoolNode`: records where
/// it ran and frees the handle. It asserts nothing, since it runs inside
/// the runtime's free.
unsafe extern "C" fn drop_pool_node(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: every handle of `PoolNode` is a leaked `Box<PoolNode>`, and
    // its drop function runs once per value.
    let node = unsafe { Box::from_raw(handle.cast::<PoolNode>()) };
    let probe = &node.probe;
    probe.drop_on_pool.store(
        thread::current().name() == Some(THREAD_NAME),
        Ordering::SeqCst,
    );
    probe.drop_owner_current.store(
        tls::current_fiber().addr() == probe.owner.load(Ordering::SeqCst),
        Ordering::SeqCst,
    );
    probe
        .drop_after_shim
        .store(probe.done.load(Ordering::SeqCst), Ordering::SeqCst);
    probe.drops.fetch_add(1, Ordering::SeqCst);
}

/// The key of the test external type `PoolNode`, registered once.
fn pool_node_key() -> u64 {
    static KEY: OnceLock<u64> = OnceLock::new();
    *KEY.get_or_init(|| {
        let key = layout::type_key("ffi_pool_tests", "PoolNode");
        layout::register_external_with_drop("PoolNode", key, Some(drop_pool_node)).unwrap();
        key
    })
}

/// A new value of `PoolNode`, owned by the running fiber `me`, whose drop
/// function reports to `probe`.
fn pool_node(me: &crate::ctx::ExecCtx, probe: &Arc<Probe>) -> Word {
    let handle = Box::into_raw(Box::new(PoolNode {
        probe: Arc::clone(probe),
    }));
    let handle = Word::try_from(handle.expose_provenance()).unwrap();
    // SAFETY: the running fiber's control block and a handle of `PoolNode`.
    unsafe { crate::ffi::cheby_rt_ffi_external(ctx_ptr(me), handle, pool_node_key()) }
}

/// A shim that duplicates its borrowed second argument and keeps the copy
/// in the probe, as C code that keeps a value for a later call does: C
/// code reaches a value's last reference only so (D-187, D-945).
unsafe extern "C" fn keep_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    // SAFETY: the second word is a value the parked fiber holds.
    let borrowed = value_of(unsafe { *args.add(1) });
    // SAFETY: a live borrowed value.
    let copy = unsafe { cheby_ffi_dup(borrowed) };
    probe.kept.store(word_of(copy), Ordering::SeqCst);
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// A shim that releases the reference [`keep_shim`] kept, the last one of
/// its value, then records that it is done.
unsafe extern "C" fn drop_kept_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    // SAFETY: the reference the first call kept, which C code owns.
    unsafe { cheby_ffi_drop(value_of(probe.kept.swap(0, Ordering::SeqCst))) };
    probe.done.store(true, Ordering::SeqCst);
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// [`last_reference_of`] for a value of `PoolNode`.
fn last_reference_on_the_pool(in_list: bool) -> Arc<Probe> {
    pool_node_key();
    last_reference_of(in_list, pool_node)
}

/// On a root fiber: builds a value with `make`, held by itself or, with
/// `in_list`, by a `List` of it; a first `@blocking` call keeps a
/// duplicate of what the fiber holds, the fiber releases its own
/// reference, and a second call releases the duplicate on its pool
/// thread, the last reference. Returns the probe.
fn last_reference_of(
    in_list: bool,
    make: impl FnOnce(&crate::ctx::ExecCtx, &Arc<Probe>) -> Word + Send + 'static,
) -> Arc<Probe> {
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let d = Arc::clone(&domain);
    let outcome = on_root(2, &domain, move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        let before = d.live_objects();
        let node = make(me, &p);
        let held = if in_list {
            list::from_iter([node])
        } else {
            node
        };
        let first = [p.word(), held];
        // SAFETY: the running fiber's context, two words for a shim that
        // reads them while this closure keeps the probe and the value
        // alive, and static code.
        let kept = unsafe { cheby_rt_blocking_call(ctx_ptr(me), keep_shim, first.as_ptr(), 2) };
        // SAFETY: the fiber's own reference; the shim kept another.
        unsafe { rc::drop(held) };
        let second = [p.word()];
        // SAFETY: as above, with one word.
        let dropped =
            unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_kept_shim, second.as_ptr(), 1) };
        p.drops_at_return
            .store(p.drops.load(Ordering::SeqCst), Ordering::SeqCst);
        p.live_left
            .store(d.live_objects().saturating_sub(before), Ordering::SeqCst);
        let clean = !unwinding(me) && kept == 0 && dropped == 0;
        drop(p);
        u64::from(clean)
    });
    assert_eq!(
        outcome,
        EntryOutcome::Returned(1),
        "a call failed or raised"
    );
    probe
}

/// What D-171 asks of the drop function of a value whose last reference a
/// pool thread released for its parked fiber (D-950): it ran once, on the
/// fiber, after the shim returned and before the call returned to the
/// fiber's code, and nothing of the value is left in the fiber's domain.
fn check_dropped_on_the_fiber(probe: &Probe) {
    assert!(
        probe.on_pool.load(Ordering::SeqCst),
        "the second call did not run on a pool thread"
    );
    assert_eq!(
        probe.drops.load(Ordering::SeqCst),
        1,
        "the drop function did not run exactly once"
    );
    assert_eq!(
        probe.drops_at_return.load(Ordering::SeqCst),
        1,
        "the drop function had not run when the call returned to the fiber"
    );
    assert!(
        !probe.drop_on_pool.load(Ordering::SeqCst),
        "the drop function ran on the pool thread (D-171)"
    );
    assert!(
        probe.drop_owner_current.load(Ordering::SeqCst),
        "the drop function ran without its owner fiber current"
    );
    assert!(
        probe.drop_after_shim.load(Ordering::SeqCst),
        "the drop function ran before the releasing shim returned"
    );
    assert_eq!(
        probe.live_left.load(Ordering::SeqCst),
        0,
        "objects of the value are left in the fiber's domain"
    );
}

/// D-171, D-949, D-950: a `cheby_ffi_drop` on a pool thread that releases
/// the last reference to a value of an external type leaves its drop
/// function to the fiber the pool thread acts for, which runs it when the
/// call returns.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_of_the_last_reference_runs_on_the_fiber() {
    check_dropped_on_the_fiber(&last_reference_on_the_pool(false));
}

/// D-171, D-949, D-950: the same when the pool thread releases a `List`
/// holding the last reference, so that the free cascades through the list
/// to the value.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_of_a_list_holding_the_last_reference_runs_on_the_fiber() {
    check_dropped_on_the_fiber(&last_reference_on_the_pool(true));
}

// Abandoned jobs (child processes) -----------------------------------------

/// How long a child process may take, at most, to exit.
fn child_limit() -> Duration {
    testing::scaled(Duration::from_secs(20))
}

/// Runs this test binary's `ffi_pool::tests::<name>` alone with the
/// variable `child` set; returns its status and standard error, or panics
/// if it does not exit within [`child_limit`]. Its standard output must be
/// empty: an abandoned job's output functions write nothing.
fn child(name: &str, child: &str) -> (std::process::ExitStatus, String) {
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("ffi_pool::tests::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(child, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > child_limit() {
            process.kill().ok();
            process.wait().ok();
            panic!("the child process did not exit within {:?}", child_limit());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = process.stderr.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stderr).ok();
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = process.stdout.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stdout).ok();
    }
    assert!(
        !stdout.contains("abandoned output"),
        "an abandoned job wrote: {stdout}"
    );
    (status, stderr)
}

/// A shim that waits at its gate, then calls `cheby.h` as an abandoned job
/// would: an allocation, an output, a dup and a drop of its borrowed
/// second argument, and a read of it.
unsafe extern "C" fn abandoned_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    // SAFETY: the second word is a `String` the parked fiber holds.
    let kept = value_of(unsafe { *args.add(1) });
    record_thread(probe);
    probe.running.store(true, Ordering::SeqCst);
    probe.pass();
    let text = b"abandoned output\n";
    // SAFETY: valid bytes and a live borrowed `String`.
    unsafe {
        let made = cheby_ffi_string_new(text.as_ptr().cast(), text.len());
        probe
            .null_alloc
            .store(usize::from(made.is_null()), Ordering::SeqCst);
        probe.write.store(
            i64::from(cheby_ffi_write_stdout(text.as_ptr().cast(), text.len())),
            Ordering::SeqCst,
        );
        let dup = cheby_ffi_dup(kept);
        probe
            .dup_same
            .store(usize::from(dup == kept), Ordering::SeqCst);
        cheby_ffi_drop(kept);
        cheby_ffi_drop(kept);
        let mut len = 0_usize;
        let bytes = cheby_ffi_string_bytes(kept, &raw mut len);
        let read = std::slice::from_raw_parts(bytes.cast::<u8>(), len);
        probe
            .readable
            .store(usize::from(read == b"borrowed"), Ordering::SeqCst);
        out.write(word_of(made));
    }
    probe.done.store(true, Ordering::SeqCst);
}

/// A runtime of one worker whose root waits on [`abandoned_shim`] in a
/// fresh domain, which it returns with the probe once the shim runs.
fn abandoned_job() -> (crate::sched::Runtime, Arc<Domain>, Arc<Probe>) {
    let runtime = runtime(1);
    let (domain, probe) = abandoned_root(&runtime);
    (runtime, domain, probe)
}

/// Starts [`abandoned_job`]'s root on `runtime`; returns its domain and
/// probe once its shim runs.
fn abandoned_root(runtime: &crate::sched::Runtime) -> (Arc<Domain>, Arc<Probe>) {
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let _root = testing::start_root_with(runtime, &config(&domain), move |me| {
        let kept = values::string("borrowed");
        let args = [p.word(), kept];
        // SAFETY: the running fiber's context, a probe this closure keeps
        // alive and a `String` it holds, and static code.
        unsafe { cheby_rt_blocking_call(ctx_ptr(me), abandoned_shim, args.as_ptr(), 2) }
    });
    wait_until(|| probe.running.load(Ordering::SeqCst));
    (domain, probe)
}

/// What an abandoned job's calls did, checked in the child.
fn check_abandoned(domain: &Arc<Domain>, probe: &Probe) {
    probe.open.store(true, Ordering::SeqCst);
    wait_until(|| probe.done.load(Ordering::SeqCst));
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    assert_eq!(
        probe.null_alloc.load(Ordering::SeqCst),
        1,
        "an allocation succeeded"
    );
    assert_eq!(
        probe.write.load(Ordering::SeqCst),
        -1,
        "an output was written"
    );
    assert_eq!(
        probe.dup_same.load(Ordering::SeqCst),
        1,
        "dup changed its argument"
    );
    assert_eq!(
        probe.readable.load(Ordering::SeqCst),
        1,
        "the borrowed argument was not readable"
    );
    // The two drops did nothing: the fiber still holds its `String`, and
    // the domain is kept until the process exits.
    assert_eq!(domain.live_objects(), 1, "a drop released the argument");
    assert!(keeps_domain(domain), "teardown may free the domain");
    eprintln!("abandoned ok");
}

/// D-950, D-815: after a stop gave up on the pool, the job's later calls
/// return NULL, -1 or nothing, its borrowed argument stays readable, and
/// the process ends without freeing the domain.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_after_a_give_up() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_ABANDON_GIVE_UP";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, domain, probe) = abandoned_job();
        runtime.shutdown();
        check_abandoned(&domain, &probe);
        return;
    }
    let (status, stderr) = child("ffi_pool_abandon_after_a_give_up", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("abandoned ok\n"), "{stderr}");
}

/// D-950, D-073: a halt abandons the job as a give-up does.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_after_a_halt() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_ABANDON_HALT";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, domain, probe) = abandoned_job();
        runtime.halt();
        check_abandoned(&domain, &probe);
        return;
    }
    let (status, stderr) = child("ffi_pool_abandon_after_a_halt", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("abandoned ok\n"), "{stderr}");
}

// Calls that register after a give-up or a halt (wave 9's FFI item, part (ii))

/// A shim that waits at its gate, then returns 0: a call that blocks
/// inline on its worker after a refused pool thread (D-786, D-817).
unsafe extern "C" fn gated_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    probe.running.store(true, Ordering::SeqCst);
    probe.pass();
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// A shim that builds a `String`, writes a line through the output
/// function and returns the `String`; it counts the call in `inline_ok`
/// when it ran with the probe's owner fiber current and both worked.
unsafe extern "C" fn inline_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    let own = tls::current_fiber().addr() == probe.owner.load(Ordering::SeqCst);
    let text = b"inline output\n";
    // SAFETY: valid bytes; the result is the call's owned result.
    let made = unsafe { cheby_ffi_string_new(text.as_ptr().cast(), text.len()) };
    // SAFETY: valid bytes.
    let wrote = unsafe { cheby_ffi_write_stdout(text.as_ptr().cast(), text.len()) };
    if own && !made.is_null() && wrote == 0 {
        probe.inline_ok.fetch_add(1, Ordering::SeqCst);
    }
    // SAFETY: the runtime's result cell.
    unsafe { out.write(word_of(made)) };
}

/// How many calls the fiber makes after the give-up.
const CALLS_AFTER: usize = 3;

/// The child's body of [`ffi_pool_abandon_calls_after_a_give_up_are_not_kept`]:
/// on a runtime of two workers, while the process's pool refuses every
/// thread and none runs, a root blocks inline in a call on its worker
/// (D-786, D-817); then, with threads allowed again, a second root waits
/// on the pool as in [`abandoned_job`]. The stop detaches the first
/// root's worker and gives up on the pool (D-815). The first root then
/// goes on running and makes [`CALLS_AFTER`] calls, which run inline on
/// its own fiber (D-1268).
fn calls_after_a_give_up() {
    let runtime = runtime(2);
    let pool = crate::pool::Pool::global();
    pool.refuse_threads(true);
    let domain = Domain::new();
    let probe = Probe::new();
    let hold = Probe::new();
    let (p, h) = (Arc::clone(&probe), Arc::clone(&hold));
    let _root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        let args = [p.word()];
        // SAFETY: the running fiber's context, one argument word for a shim
        // whose probe this closure keeps alive, and static code.
        unsafe { cheby_rt_blocking_call(ctx_ptr(me), gated_shim, args.as_ptr(), 1) };
        for _ in 0..CALLS_AFTER {
            // SAFETY: as above.
            let word =
                unsafe { cheby_rt_blocking_call(ctx_ptr(me), inline_shim, args.as_ptr(), 1) };
            // SAFETY: the call's owned result, or 0.
            unsafe { rc::drop(word) };
        }
        p.kept_jobs
            .store(abandoned_by(ctx_ptr(me).addr()), Ordering::SeqCst);
        p.done.store(true, Ordering::SeqCst);
        // The runtime has stopped: the fiber waits here until the process
        // ends.
        h.pass();
        values::nil()
    });
    wait_until(|| probe.running.load(Ordering::SeqCst));
    pool.refuse_threads(false);
    let (_waiting, waiting_probe) = abandoned_root(&runtime);
    runtime.shutdown();
    probe.open.store(true, Ordering::SeqCst);
    wait_until(|| probe.done.load(Ordering::SeqCst));
    waiting_probe.open.store(true, Ordering::SeqCst);
    assert!(
        !probe.on_pool.load(Ordering::SeqCst),
        "a call after the give-up ran on a pool thread"
    );
    assert_eq!(
        probe.inline_ok.load(Ordering::SeqCst),
        CALLS_AFTER,
        "a call after the give-up did not run on its own fiber with its allocation and output"
    );
    assert_eq!(
        probe.kept_jobs.load(Ordering::SeqCst),
        0,
        "the abandoned list keeps jobs of calls that ran inline on their own fiber"
    );
    eprintln!("calls after a give-up ok");
}

/// D-815, D-786, D-1268, D-1269 as D-1308 refines it:
/// calls that a fiber makes after a stop gave
/// up on the pool run inline on that fiber, their allocations and output
/// work, and the abandoned list keeps no job of them, nor of the call the
/// give-up marked while it ran inline.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_calls_after_a_give_up_are_not_kept() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_CALLS_AFTER_A_GIVE_UP";
    if std::env::var_os(CHILD).is_some() {
        calls_after_a_give_up();
        return;
    }
    let (status, stderr) = child("ffi_pool_abandon_calls_after_a_give_up_are_not_kept", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("calls after a give-up ok\n"), "{stderr}");
}

/// D-1269 as D-1308 refines it: a job that a
/// give-up marked between its registration and its run, the race of
/// `register_ffi_job` with `enter`'s test of the give-up, and that then
/// ran inline on its own fiber, leaves the abandoned list once it is
/// unregistered; a marked job run from a thread that is no fiber, as a
/// pool thread runs it, stays there.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_a_job_marked_while_it_ran_inline_is_released() {
    let domain = Domain::new();
    let probe = Probe::new();
    probe.open.store(true, Ordering::SeqCst);
    let p = Arc::clone(&probe);
    let outcome = on_root(1, &domain, move |me| {
        let owner = ctx_ptr(me).addr();
        let mut counts = vec![abandoned_by(owner)];
        let job = FfiJob::new(Arc::clone(me.domain()), ctx_ptr(me));
        let marked = job.abandon().is_some();
        counts.push(abandoned_by(owner));
        let args = [p.word()];
        let mut out = 0_u64;
        // SAFETY: a shim given its probe's word and a result cell, on the
        // job's own fiber.
        run_as_job(&job, || unsafe { build_shim(args.as_ptr(), &raw mut out) });
        // SAFETY: the shim's owned result, or 0.
        unsafe { rc::drop(out) };
        release_if_ran_inline(&job);
        counts.push(abandoned_by(owner));
        let other = FfiJob::new(Domain::new(), ctx_ptr(me));
        let other_marked = other.abandon().is_some();
        let o = Arc::clone(&other);
        let ran = thread::spawn(move || run_as_job(&o, || true))
            .join()
            .unwrap_or(false);
        release_if_ran_inline(&other);
        counts.push(abandoned_by(owner));
        *p.counts.lock() = counts;
        drop(p);
        u64::from(marked && other_marked && ran)
    });
    assert_eq!(outcome, EntryOutcome::Returned(1), "{outcome:?}");
    assert!(
        !probe.on_pool.load(Ordering::SeqCst),
        "the inline shim ran on a pool thread"
    );
    let counts = probe.counts.lock().clone();
    let &[before, marked, released, kept] = counts.as_slice() else {
        panic!("the root read {counts:?}");
    };
    assert_eq!(
        marked,
        before.saturating_add(1),
        "the mark did not keep the job"
    );
    assert_eq!(
        released, before,
        "a job marked while it ran inline on its own fiber stayed in the abandoned list"
    );
    assert_eq!(
        kept,
        before.saturating_add(1),
        "a marked job run off its fiber left the abandoned list"
    );
}

/// The child's body of [`ffi_pool_abandon_a_call_after_a_halt_runs_marked`]:
/// a root starts a halt itself, as a termination does (D-073, D-708),
/// runs on until its next entry check, and makes a call first.
fn call_after_a_halt() {
    let runtime = runtime(1);
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let _root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        let kept = values::string("borrowed");
        let args = [p.word(), kept];
        if let Some(shared) = me.sched.runtime() {
            shared.begin_halt();
        }
        // SAFETY: the running fiber's context, a probe this closure keeps
        // alive and a `String` it holds, and static code.
        let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), abandoned_shim, args.as_ptr(), 2) };
        p.woken.store(true, Ordering::SeqCst);
        word
    });
    wait_until(|| probe.running.load(Ordering::SeqCst));
    check_abandoned(&domain, &probe);
    assert_eq!(
        abandoned_by(probe.owner.load(Ordering::SeqCst)),
        1,
        "the halt's list does not keep the call's job"
    );
    // A completion during a halt wakes nothing (D-815, D-073).
    thread::sleep(testing::scaled(Duration::from_millis(50)));
    runtime.halt();
    assert!(
        !probe.woken.load(Ordering::SeqCst),
        "the fiber was woken during the halt"
    );
    eprintln!("call after a halt ok");
}

/// D-1269, D-073, D-708, D-815: a `@blocking` call that registers its job
/// after a halt began is submitted with its job already marked: its
/// allocation returns NULL, its output returns -1, its job is kept until
/// the process exits, and its completion does not wake the fiber. This
/// pins D-1269's logged rule ("marks it at once when the runtime already
/// halts"), which D-1308 keeps.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_a_call_after_a_halt_runs_marked() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_CALL_AFTER_A_HALT";
    if std::env::var_os(CHILD).is_some() {
        call_after_a_halt();
        return;
    }
    let (status, stderr) = child("ffi_pool_abandon_a_call_after_a_halt_runs_marked", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("call after a halt ok\n"), "{stderr}");
}

/// A shim that reads the abandoned list for its probe's owner while its
/// call runs, then runs as [`inline_shim`]: a job marked at its registration
/// is on the list by then.
unsafe extern "C" fn listed_inline_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    probe
        .counts
        .lock()
        .push(abandoned_by(probe.owner.load(Ordering::SeqCst)));
    // SAFETY: forwarded from the runtime.
    unsafe { inline_shim(args, out) };
}

/// The child's body of [`ffi_pool_abandon_a_call_after_a_give_up_is_not_marked`]:
/// a root gives up on the pool as a stop does, with no halt, and makes
/// [`CALLS_AFTER`] calls.
fn calls_after_a_give_up_without_a_halt() {
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let outcome = on_root(1, &domain, move |me| {
        let owner = ctx_ptr(me).addr();
        p.owner.store(owner, Ordering::SeqCst);
        let Some(shared) = me.sched.runtime() else {
            return 0;
        };
        crate::sched::blocking::give_up_without_a_halt(shared);
        let args = [p.word()];
        for _ in 0..CALLS_AFTER {
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose probe this closure keeps alive, and static code.
            let word = unsafe {
                cheby_rt_blocking_call(ctx_ptr(me), listed_inline_shim, args.as_ptr(), 1)
            };
            // SAFETY: the call's owned result, or 0.
            unsafe { rc::drop(word) };
        }
        p.kept_jobs.store(abandoned_by(owner), Ordering::SeqCst);
        let halting = shared.halting();
        drop(p);
        u64::from(!halting)
    });
    assert_eq!(
        outcome,
        EntryOutcome::Returned(1),
        "the runtime halted: this test is of a give-up alone"
    );
    assert!(
        !probe.on_pool.load(Ordering::SeqCst),
        "a call after the give-up ran on a pool thread"
    );
    assert_eq!(
        probe.inline_ok.load(Ordering::SeqCst),
        CALLS_AFTER,
        "a call after the give-up did not run on its own fiber with its allocation and output"
    );
    assert_eq!(
        *probe.counts.lock(),
        [0; CALLS_AFTER],
        "a call after a give-up was marked at its registration"
    );
    assert_eq!(probe.kept_jobs.load(Ordering::SeqCst), 0, "a job was kept");
    eprintln!("not marked ok");
}

/// D-1269 as D-1308 refines it: a job that
/// registers after a give-up with no halt is not marked at its registration
/// (a halt still marks one, [`ffi_pool_abandon_a_call_after_a_halt_runs_marked`]),
/// so it is never on the abandoned list, even while its call runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_a_call_after_a_give_up_is_not_marked() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_GIVE_UP_NOT_MARKED";
    if std::env::var_os(CHILD).is_some() {
        calls_after_a_give_up_without_a_halt();
        return;
    }
    let (status, stderr) = child(
        "ffi_pool_abandon_a_call_after_a_give_up_is_not_marked",
        CHILD,
    );
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("not marked ok\n"), "{stderr}");
}

// Every path that resumes a fiber runs what its job left to it (wave 12's
// review of part (i))

/// Gives the probe a duplicate of `value`, then releases the fiber's own
/// reference, so that the probe's is the last.
fn keep_the_last_reference(probe: &Probe, value: Word) {
    // SAFETY: a live value of the running fiber.
    let copy = unsafe { cheby_ffi_dup(value_of(value)) };
    probe.kept.store(word_of(copy), Ordering::SeqCst);
    // SAFETY: the fiber's own reference.
    unsafe { rc::drop(value) };
}

/// [`drop_kept_shim`] behind the test's gate.
unsafe extern "C" fn gated_drop_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    probe.running.store(true, Ordering::SeqCst);
    probe.pass();
    // SAFETY: forwarded from the runtime.
    unsafe { drop_kept_shim(args, out) };
}

/// [`drop_kept_shim`], then [`failing_shim`]: the job holds a drop function
/// when its allocation fails.
unsafe extern "C" fn drop_then_fail_shim(args: *const u64, out: *mut u64) {
    // SAFETY: forwarded from the runtime; the second shim writes the result
    // cell last.
    unsafe {
        drop_kept_shim(args, out);
        failing_shim(args, out);
    }
}

/// Releases the kept reference, then waits at the test's gate: the job
/// holds a drop function when a give-up marks it.
unsafe extern "C" fn drop_then_wait_shim(args: *const u64, out: *mut u64) {
    // SAFETY: the test's probe.
    let probe = unsafe { probe(args) };
    record_thread(probe);
    // SAFETY: the reference the test kept, which C code owns.
    unsafe { cheby_ffi_drop(value_of(probe.kept.swap(0, Ordering::SeqCst))) };
    probe.running.store(true, Ordering::SeqCst);
    probe.pass();
    probe.done.store(true, Ordering::SeqCst);
    // SAFETY: the runtime's result cell.
    unsafe { out.write(0) };
}

/// D-171, D-816, D-950, D-1307: a call cancelled while its
/// shim runs still runs the drop function its job left to the fiber, once,
/// before the cancellation unwinds the fiber.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_of_a_cancelled_call_runs_on_the_fiber() {
    use crate::fiber_ops::detached_tests::{
        close_scope, open_scope, release, spawn_into, yield_until,
    };
    pool_node_key();
    let domain = Domain::new();
    let probe = Probe::new();
    let raised = Arc::new(AtomicBool::new(false));
    let (p, r) = (Arc::clone(&probe), Arc::clone(&raised));
    let outcome = on_root(2, &domain, move |me| {
        let scope = open_scope(me);
        let q = Arc::clone(&p);
        let fiber = spawn_into(me, scope, move |me| {
            q.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
            keep_the_last_reference(&q, pool_node(me, &q));
            let args = [q.word()];
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose probe this closure keeps alive, and static code.
            unsafe { cheby_rt_blocking_call(ctx_ptr(me), gated_drop_shim, args.as_ptr(), 1) };
            q.drops_at_return
                .store(q.drops.load(Ordering::SeqCst), Ordering::SeqCst);
            let unwound = unwinding(me);
            r.store(unwound, Ordering::SeqCst);
            drop(q);
            if unwound { 0 } else { values::nil() }
        });
        yield_until(me, || p.running.load(Ordering::SeqCst));
        // SAFETY: a live `Scope` handle, borrowed.
        let state = unsafe { crate::fiber_ops::scope::state_of(scope) };
        crate::fiber_ops::cancel::cancel(&state);
        p.open.store(true, Ordering::SeqCst);
        close_scope(me, scope);
        release(fiber);
        drop(p);
        1
    });
    assert_eq!(outcome, EntryOutcome::Returned(1), "{outcome:?}");
    assert!(
        raised.load(Ordering::SeqCst),
        "the cancellation was not raised on the fiber"
    );
    check_dropped_on_the_fiber(&probe);
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-171, D-946, D-950, D-1307: a call whose allocation fails
/// after its release still runs the drop function on the fiber, before the
/// `out of memory` panic is raised there.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_of_a_call_whose_allocation_failed_runs_on_the_fiber() {
    let _serial = crate::alloc::tests::serial();
    pool_node_key();
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let outcome = on_root(1, &domain, move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        keep_the_last_reference(&p, pool_node(me, &p));
        let args = [p.word()];
        // SAFETY: as in `ffi_pool_job_a_failed_allocation_raises_on_the_fiber`.
        let word =
            unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_then_fail_shim, args.as_ptr(), 1) };
        p.drops_at_return
            .store(p.drops.load(Ordering::SeqCst), Ordering::SeqCst);
        drop(p);
        raised_out_of_memory(me, word)
    });
    assert_eq!(
        outcome,
        EntryOutcome::Returned(1),
        "the failure was not raised on the fiber as `out of memory`"
    );
    assert_eq!(
        probe.null_alloc.load(Ordering::SeqCst),
        1,
        "an allocation succeeded"
    );
    check_dropped_on_the_fiber(&probe);
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// As [`refused_pair`], each root keeps the last reference to a value of its
/// own that `make` builds, and its call releases it. One root runs both
/// jobs. Each root returns 1 when its call neither raised nor returned a
/// value.
fn refused_releases(
    make: impl FnOnce(&crate::ctx::ExecCtx, &Arc<Probe>) -> Word + Copy + Send + 'static,
) -> [Ran; 2] {
    let pool = crate::pool::Pool::global();
    pool.refuse_until_queued(2);
    pool.refuse_threads(true);
    let runtime = runtime(2);
    let start = || {
        let domain = Domain::new();
        let probe = Probe::new();
        let p = Arc::clone(&probe);
        let root = testing::start_root_with(&runtime, &config(&domain), move |me| {
            p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
            keep_the_last_reference(&p, make(me, &p));
            let args = [p.word()];
            // SAFETY: the running fiber's context, one argument word for a
            // shim whose probe this closure keeps alive, and static code.
            let word =
                unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_kept_shim, args.as_ptr(), 1) };
            p.drops_at_return
                .store(p.drops.load(Ordering::SeqCst), Ordering::SeqCst);
            let clean = !unwinding(me) && word == 0;
            drop(p);
            u64::from(clean)
        });
        (root, probe, domain)
    };
    let (a, b) = (start(), start());
    let ended = |(root, probe, domain): (std::sync::mpsc::Receiver<EntryOutcome>, _, _)| {
        let outcome = root.recv_timeout(testing::hang()).expect("a root hung");
        (outcome, probe, domain)
    };
    let pair = [ended(a), ended(b)];
    runtime.shutdown();
    pool.refuse_threads(false);
    pair
}

/// The child's body of
/// [`ffi_pool_job_drop_inline_for_another_fiber_runs_on_that_fiber`]: one
/// root runs both jobs of [`refused_releases`].
fn inline_drops() {
    pool_node_key();
    let mut threads = Vec::new();
    let mut after_shim = 0_usize;
    for (outcome, probe, domain) in refused_releases(pool_node) {
        assert_eq!(outcome, EntryOutcome::Returned(1), "{outcome:?}");
        assert_eq!(
            (
                probe.drops.load(Ordering::SeqCst),
                probe.drops_at_return.load(Ordering::SeqCst)
            ),
            (1, 1),
            "the drop function did not run once before the call returned"
        );
        assert!(
            !probe.on_pool.load(Ordering::SeqCst),
            "a shim ran on a pool thread"
        );
        assert!(
            probe.drop_owner_current.load(Ordering::SeqCst),
            "the drop function ran without its owner fiber current"
        );
        after_shim =
            after_shim.saturating_add(usize::from(probe.drop_after_shim.load(Ordering::SeqCst)));
        threads.push(*probe.thread.lock());
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
    assert!(threads[0].is_some(), "a shim did not run");
    assert_eq!(threads[0], threads[1], "no root ran the other's job");
    assert_eq!(
        after_shim, 1,
        "the fiber that ran both jobs runs its own drop function inline and leaves the other's"
    );
    eprintln!("inline drops ok");
}

/// D-171, D-817, D-950, D-1268, D-1307: a fiber that runs
/// another fiber's job inline acts for that fiber as a pool thread would:
/// the drop function waits for the job's own fiber, while the job that ran
/// inline on its own fiber runs its drop function there at once.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_inline_for_another_fiber_runs_on_that_fiber() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_INLINE_OTHER_DROP";
    if std::env::var_os(CHILD).is_some() {
        inline_drops();
        return;
    }
    let (status, stderr) = child(
        "ffi_pool_job_drop_inline_for_another_fiber_runs_on_that_fiber",
        CHILD,
    );
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("inline drops ok\n"), "{stderr}");
}

/// One run of [`drop_chain_node`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per claim the chain's test compares"
)]
struct ChainRun {
    id: usize,
    on_pool: bool,
    owner_current: bool,
    after_shim: bool,
    in_mark: bool,
}

/// The handle of the test external type `ChainNode`: a value that owns a
/// reference to `next`, or 0, which its drop function gives up.
struct ChainNode {
    id: usize,
    probe: Arc<Probe>,
    runs: Arc<Mutex<Vec<ChainRun>>>,
    next: Word,
}

/// The drop function of the test external type `ChainNode`: records where
/// it ran and calls `cheby_ffi_drop` on the next value, as a C drop
/// function may (D-949).
unsafe extern "C" fn drop_chain_node(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: every handle of `ChainNode` is a leaked `Box<ChainNode>`, and
    // its drop function runs once per value.
    let node = unsafe { Box::from_raw(handle.cast::<ChainNode>()) };
    node.runs.lock().push(ChainRun {
        id: node.id,
        on_pool: thread::current().name() == Some(THREAD_NAME),
        owner_current: tls::current_fiber().addr() == node.probe.owner.load(Ordering::SeqCst),
        after_shim: node.probe.done.load(Ordering::SeqCst),
        in_mark: crate::ffi::in_drop_function(),
    });
    if node.next != 0 {
        // SAFETY: the reference this handle owned.
        unsafe { cheby_ffi_drop(value_of(node.next)) };
    }
}

/// The key of the test external type `ChainNode`, registered once.
fn chain_node_key() -> u64 {
    static KEY: OnceLock<u64> = OnceLock::new();
    *KEY.get_or_init(|| {
        let key = layout::type_key("ffi_pool_tests", "ChainNode");
        layout::register_external_with_drop("ChainNode", key, Some(drop_chain_node)).unwrap();
        key
    })
}

/// A new value of `ChainNode`, owned by the running fiber `me`, that owns
/// the reference `next`.
fn chain_node(
    me: &crate::ctx::ExecCtx,
    id: usize,
    probe: &Arc<Probe>,
    runs: &Arc<Mutex<Vec<ChainRun>>>,
    next: Word,
) -> Word {
    let handle = Box::into_raw(Box::new(ChainNode {
        id,
        probe: Arc::clone(probe),
        runs: Arc::clone(runs),
        next,
    }));
    let handle = Word::try_from(handle.expose_provenance()).unwrap();
    // SAFETY: the running fiber's control block and a handle of `ChainNode`.
    unsafe { crate::ffi::cheby_rt_ffi_external(ctx_ptr(me), handle, chain_node_key()) }
}

/// D-171, D-949, D-1307: the drop function a job left to its
/// fiber runs under the drop-function mark, so a `cheby_ffi_drop` inside it
/// queues onto the running worklist: a chain of three values ends in order,
/// all on the fiber.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_of_a_chain_runs_on_the_fiber_in_order() {
    let domain = Domain::new();
    let probe = Probe::new();
    let runs = Arc::new(Mutex::new(Vec::new()));
    let (p, l, d) = (Arc::clone(&probe), Arc::clone(&runs), Arc::clone(&domain));
    let outcome = on_root(2, &domain, move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        let before = d.live_objects();
        let third = chain_node(me, 3, &p, &l, 0);
        let second = chain_node(me, 2, &p, &l, third);
        keep_the_last_reference(&p, chain_node(me, 1, &p, &l, second));
        let args = [p.word()];
        // SAFETY: as in `ffi_pool_job_builds_values_charged_to_the_fibers_domain`.
        let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_kept_shim, args.as_ptr(), 1) };
        let ended = l.lock().len();
        p.live_left
            .store(d.live_objects().saturating_sub(before), Ordering::SeqCst);
        let clean = !unwinding(me) && word == 0;
        drop(p);
        drop(l);
        u64::from(clean && ended == 3)
    });
    assert_eq!(outcome, EntryOutcome::Returned(1), "{:?}", runs.lock());
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    let expected = [1, 2, 3].map(|id| ChainRun {
        id,
        on_pool: false,
        owner_current: true,
        after_shim: true,
        in_mark: true,
    });
    assert_eq!(*runs.lock(), expected);
    assert_eq!(probe.live_left.load(Ordering::SeqCst), 0, "objects leaked");
}

/// D-710, D-949, D-1307: a fiber whose group is being torn
/// down does not run the drop functions its job left to it, as the
/// finalizer runs none there; the value's block is freed all the same.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_drop_is_not_run_while_the_group_is_torn_down() {
    pool_node_key();
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    group.set_tearing_down(true);
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let outcome = testing::start_root_in(&runtime, &config(&domain), &group, move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        keep_the_last_reference(&p, pool_node(me, &p));
        let args = [p.word()];
        // SAFETY: as in `ffi_pool_job_builds_values_charged_to_the_fibers_domain`.
        let word = unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_kept_shim, args.as_ptr(), 1) };
        p.drops_at_return
            .store(p.drops.load(Ordering::SeqCst), Ordering::SeqCst);
        let clean = !unwinding(me) && word == 0;
        drop(p);
        u64::from(clean)
    })
    .recv_timeout(testing::hang())
    .expect("the root hung");
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(1), "{outcome:?}");
    assert!(probe.on_pool.load(Ordering::SeqCst), "not on a pool thread");
    assert!(probe.done.load(Ordering::SeqCst), "the shim did not run");
    assert_eq!(
        probe.drops.load(Ordering::SeqCst),
        0,
        "a drop function ran during a teardown (D-710)"
    );
    assert_eq!(domain.live_objects(), 0, "the value's block was not freed");
}

/// The child's body of [`ffi_pool_abandon_a_job_that_deferred_keeps_its_drop_unrun`]:
/// a root's call releases the last reference on its pool thread and waits;
/// a stop gives up on the pool, and the shim then returns.
fn deferred_then_abandoned() {
    pool_node_key();
    let runtime = runtime(1);
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let _root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
        keep_the_last_reference(&p, pool_node(me, &p));
        let args = [p.word()];
        // SAFETY: the running fiber's context, one argument word for a shim
        // whose probe this closure keeps alive, and static code.
        unsafe { cheby_rt_blocking_call(ctx_ptr(me), drop_then_wait_shim, args.as_ptr(), 1) }
    });
    wait_until(|| probe.running.load(Ordering::SeqCst));
    runtime.shutdown();
    probe.open.store(true, Ordering::SeqCst);
    wait_until(|| probe.done.load(Ordering::SeqCst));
    // A completion after a give-up wakes nothing (D-815).
    thread::sleep(testing::scaled(Duration::from_millis(50)));
    assert_eq!(
        abandoned_by(probe.owner.load(Ordering::SeqCst)),
        1,
        "the abandoned list does not keep the job"
    );
    assert_eq!(
        probe.drops.load(Ordering::SeqCst),
        0,
        "the drop function of an abandoned job ran"
    );
    eprintln!("kept unrun ok");
}

/// D-815, D-949, D-1307: a job that a give-up abandoned after
/// it took a drop function never resumes its fiber, so the handle stays
/// with the job, unrun, on every thread.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_abandon_a_job_that_deferred_keeps_its_drop_unrun() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_ABANDON_DEFERRED";
    if std::env::var_os(CHILD).is_some() {
        deferred_then_abandoned();
        return;
    }
    let (status, stderr) = child(
        "ffi_pool_abandon_a_job_that_deferred_keeps_its_drop_unrun",
        CHILD,
    );
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("kept unrun ok\n"), "{stderr}");
}

// The last reference to a std handle released off its fiber (the M3 CP4
// review's Open list: D-1307 covers user external types only). These tests
// read which thread a std handle's finalizer runs on, for each actor of a
// `@blocking` call: `handles::handle_finalize` has no job test.

/// The kinds with a payload, 40 to 43 and 45 to 47: every std handle that
/// has a finalizer.
const STD_KINDS: [HandleKind; 7] = [
    HandleKind::Sender,
    HandleKind::Receiver,
    HandleKind::Fiber,
    HandleKind::Scope,
    HandleKind::File,
    HandleKind::Listener,
    HandleKind::Connection,
];

/// A test payload for any of [`STD_KINDS`]. A real `Sender`'s finalizer
/// shows no thread, so this one goes through the same path, the kind, the
/// vtable, `handle_finalize` and `finalize_payload`, and records where it
/// ran in the probe.
struct ThreadProbe {
    probe: Arc<Probe>,
}

impl HandlePayload for ThreadProbe {
    fn finalize(self: Box<Self>, _release: &mut dyn FnMut(Word)) {
        let probe = &self.probe;
        let on_pool = thread::current().name() == Some(THREAD_NAME);
        probe.drop_on_pool.store(on_pool, Ordering::SeqCst);
        *probe.drop_thread.lock() = Some(thread::current().id());
        *probe.drop_thread_name.lock() = thread::current().name().map(str::to_owned);
        let fiber = tls::current_fiber().addr();
        probe.drop_fiber.store(fiber, Ordering::SeqCst);
        probe.drop_owner_current.store(
            fiber == probe.owner.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        probe
            .drop_after_shim
            .store(probe.done.load(Ordering::SeqCst), Ordering::SeqCst);
        probe.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// A new handle of `kind`, owned by the running fiber `me`, whose finalizer
/// reports to `probe`.
fn std_handle(me: &crate::ctx::ExecCtx, probe: &Arc<Probe>, kind: HandleKind) -> Word {
    let payload = Box::new(ThreadProbe {
        probe: Arc::clone(probe),
    });
    // SAFETY: the running fiber's control block.
    unsafe { new_handle(ctx_ptr(me), kind, payload) }
}

/// What the finalizer of a handle of `kind` read, for the test's output.
fn read_line(actor: &str, kind: HandleKind, probe: &Probe) -> String {
    let fiber = probe.drop_fiber.load(Ordering::SeqCst);
    let owner = probe.drop_owner_current.load(Ordering::SeqCst);
    let fiber = match (fiber, owner) {
        (0, _) => "none",
        (_, true) => "owner",
        (_, false) => "another",
    };
    format!(
        "std-handle read: {actor}: {kind:?}: thread {:?}, fiber current: {fiber}, after the shim: {}",
        probe.drop_thread_name.lock(),
        probe.drop_after_shim.load(Ordering::SeqCst),
    )
}

/// What the code does with a std handle whose last reference a pool thread
/// released for its parked fiber: its finalizer ran once, on the pool thread
/// that ran the shim, inside the release, with no fiber current (D-1311),
/// and nothing of the value is left in the fiber's domain. D-1307's deferral
/// does not reach it.
fn check_finalized_on_the_pool(kind: HandleKind, probe: &Probe) {
    assert!(
        probe.on_pool.load(Ordering::SeqCst),
        "{kind:?}: the second call did not run on a pool thread"
    );
    assert_eq!(
        probe.drops.load(Ordering::SeqCst),
        1,
        "{kind:?}: not finalized exactly once"
    );
    assert_eq!(
        probe.drops_at_return.load(Ordering::SeqCst),
        1,
        "{kind:?}: not finalized when the call returned to the fiber"
    );
    assert!(
        probe.drop_on_pool.load(Ordering::SeqCst),
        "{kind:?}: the finalizer did not run on a pool thread"
    );
    assert_eq!(
        *probe.drop_thread.lock(),
        *probe.thread.lock(),
        "{kind:?}: the finalizer ran on another thread than the shim"
    );
    assert!(
        !probe.drop_after_shim.load(Ordering::SeqCst),
        "{kind:?}: the finalizer ran after the shim returned"
    );
    assert!(
        !probe.drop_owner_current.load(Ordering::SeqCst),
        "{kind:?}: the finalizer ran with the owner fiber current"
    );
    assert_eq!(
        probe.drop_fiber.load(Ordering::SeqCst),
        0,
        "{kind:?}: a fiber was current on the pool thread (D-1311)"
    );
    assert_eq!(
        probe.live_left.load(Ordering::SeqCst),
        0,
        "{kind:?}: objects of the value are left in the fiber's domain"
    );
}

/// D-171, D-703, D-950, D-1307: a `cheby_ffi_drop` on a pool thread that
/// releases the last reference to a std handle of any kind runs its
/// finalizer there.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_std_handle_on_the_pool_finalizer_thread() {
    for kind in STD_KINDS {
        let probe = last_reference_of(false, move |me, p| std_handle(me, p, kind));
        check_finalized_on_the_pool(kind, &probe);
        eprintln!("{}", read_line("pool", kind, &probe));
    }
}

/// D-171, D-703, D-950, D-1307: the same when the pool thread releases a
/// `List` that holds the last reference, so that the free cascades through
/// the list to the handle.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_std_handle_in_a_list_on_the_pool_finalizer_thread() {
    for kind in STD_KINDS {
        let probe = last_reference_of(true, move |me, p| std_handle(me, p, kind));
        check_finalized_on_the_pool(kind, &probe);
        eprintln!("{}", read_line("pool, in a list", kind, &probe));
    }
}

/// The control of the two tests above: the fiber's own `rc::drop` of the
/// last reference, with no job installed, finalizes on the fiber, so the
/// probe reads both answers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_std_handle_on_its_fiber_finalizer_thread() {
    for kind in STD_KINDS {
        let domain = Domain::new();
        let probe = Probe::new();
        let p = Arc::clone(&probe);
        let outcome = on_root(2, &domain, move |me| {
            p.owner.store(ctx_ptr(me).addr(), Ordering::SeqCst);
            let handle = std_handle(me, &p, kind);
            let no_job = current_job().is_none();
            // SAFETY: the fiber's own reference, the last one.
            unsafe { rc::drop(handle) };
            drop(p);
            u64::from(no_job)
        });
        assert_eq!(outcome, EntryOutcome::Returned(1), "{kind:?}: {outcome:?}");
        assert_eq!(
            probe.drops.load(Ordering::SeqCst),
            1,
            "{kind:?}: not finalized exactly once"
        );
        assert!(
            !probe.drop_on_pool.load(Ordering::SeqCst),
            "{kind:?}: the finalizer ran on a pool thread"
        );
        assert!(
            probe.drop_owner_current.load(Ordering::SeqCst),
            "{kind:?}: the finalizer ran without its owner fiber current"
        );
        assert_eq!(domain.live_objects(), 0, "{kind:?}: objects leaked");
        eprintln!("{}", read_line("the fiber's own drop", kind, &probe));
    }
}

/// The child's body of
/// [`ffi_pool_job_std_handle_inline_for_another_fiber_finalizer_thread`]:
/// per kind, one root runs both jobs of [`refused_releases`]. Each handle is
/// finalized inside its release on that root's thread. The job of the fiber
/// that ran both has its owner current. The other's finalizer, the second
/// actor of D-1307, runs with no fiber current at all: the fiber that runs
/// another's job stops being current for the shim (D-1268).
fn inline_std_handles() {
    for kind in STD_KINDS {
        let pair = refused_releases(move |me, p| std_handle(me, p, kind));
        let mut for_another = 0_usize;
        for (outcome, probe, domain) in &pair {
            assert_eq!(*outcome, EntryOutcome::Returned(1), "{kind:?}: {outcome:?}");
            assert_eq!(
                (
                    probe.drops.load(Ordering::SeqCst),
                    probe.drops_at_return.load(Ordering::SeqCst)
                ),
                (1, 1),
                "{kind:?}: not finalized once before the call returned"
            );
            assert!(
                !probe.on_pool.load(Ordering::SeqCst),
                "{kind:?}: a shim ran on a pool thread"
            );
            assert!(
                !probe.drop_on_pool.load(Ordering::SeqCst),
                "{kind:?}: the finalizer ran on a pool thread"
            );
            assert_eq!(
                *probe.drop_thread.lock(),
                *probe.thread.lock(),
                "{kind:?}: the finalizer ran on another thread than the shim"
            );
            assert!(
                !probe.drop_after_shim.load(Ordering::SeqCst),
                "{kind:?}: the finalizer ran after the shim returned"
            );
            assert_eq!(domain.live_objects(), 0, "{kind:?}: objects leaked");
            if !probe.drop_owner_current.load(Ordering::SeqCst) {
                for_another = for_another.saturating_add(1);
                assert_eq!(
                    probe.drop_fiber.load(Ordering::SeqCst),
                    0,
                    "{kind:?}: the fiber that ran the job was still current (D-1268)"
                );
                eprintln!("{}", read_line("inline for another fiber", kind, probe));
            }
        }
        assert_eq!(
            *pair[0].1.thread.lock(),
            *pair[1].1.thread.lock(),
            "{kind:?}: no root ran the other's job"
        );
        assert_eq!(
            for_another, 1,
            "{kind:?}: one job ran on its own fiber, the other for another fiber"
        );
    }
    eprintln!("inline std handles ok");
}

/// D-171, D-703, D-817, D-1268, D-1307: the second actor. A fiber that runs
/// another fiber's job inline after a refused thread acts for that fiber,
/// and the release of a std handle's last reference finalizes it there.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_pool_job_std_handle_inline_for_another_fiber_finalizer_thread() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_INLINE_OTHER_STD_HANDLE";
    if std::env::var_os(CHILD).is_some() {
        inline_std_handles();
        return;
    }
    let (status, stderr) = child(
        "ffi_pool_job_std_handle_inline_for_another_fiber_finalizer_thread",
        CHILD,
    );
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("inline std handles ok\n"), "{stderr}");
    for line in stderr
        .lines()
        .filter(|l| l.starts_with("std-handle read: "))
    {
        eprintln!("{line}");
    }
}
