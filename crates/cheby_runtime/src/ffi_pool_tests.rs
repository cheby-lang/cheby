//! Tests of a pool thread acting for its parked fiber during a package's
//! `@blocking` call (D-950, `ffi_pool.rs`, `sched::blocking`'s
//! `cheby_rt_blocking_call`). The shims stand in for generated `cb$blk$`
//! shims (D-818): `extern "C"` functions over argument words that call
//! `cheby.h`'s functions as C code would. Tests named `ffi_pool_job_…` run
//! a job to its end; `ffi_abandon_…` abandon one by a give-up or a halt and
//! run in a child process, since they leave a fiber parked for good (as
//! `pool::tests` does). The protocol is modelled in `ffi_models.rs`
//! (`ffi_pool_model_*`).

use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
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
use crate::pool::THREAD_NAME;
use crate::sched::blocking::cheby_rt_blocking_call;
use crate::sched::{DetachedGroup, testing};
use crate::{rc, string, values};

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
    let domain = Domain::new();
    let probe = Probe::new();
    let p = Arc::clone(&probe);
    let _root = testing::start_root_with(&runtime, &config(&domain), move |me| {
        let kept = values::string("borrowed");
        let args = [p.word(), kept];
        // SAFETY: the running fiber's context, a probe this closure keeps
        // alive and a `String` it holds, and static code.
        unsafe { cheby_rt_blocking_call(ctx_ptr(me), abandoned_shim, args.as_ptr(), 2) }
    });
    wait_until(|| probe.running.load(Ordering::SeqCst));
    (runtime, domain, probe)
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
fn ffi_abandon_after_a_give_up() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_ABANDON_GIVE_UP";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, domain, probe) = abandoned_job();
        runtime.shutdown();
        check_abandoned(&domain, &probe);
        return;
    }
    let (status, stderr) = child("ffi_abandon_after_a_give_up", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("abandoned ok\n"), "{stderr}");
}

/// D-950, D-073: a halt abandons the job as a give-up does.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_abandon_after_a_halt() {
    const CHILD: &str = "CHEBY_RT_TEST_FFI_ABANDON_HALT";
    if std::env::var_os(CHILD).is_some() {
        let (runtime, domain, probe) = abandoned_job();
        runtime.halt();
        check_abandoned(&domain, &probe);
        return;
    }
    let (status, stderr) = child("ffi_abandon_after_a_halt", CHILD);
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("abandoned ok\n"), "{stderr}");
}
