//! Cancellation's runtime tests (D-666, D-667, D-668, D-670, D-688, D-706,
//! D-707; architecture §5.3, §7.5 and §7.6): the `cancel_*` tests of
//! survey-u5 §4.3 that pin the protocol from inside the runtime, besides the
//! hazard tests, the member-exit race and the raise-bound test, which live
//! next to the code in `fiber_ops::tests`.
//!
//! Each test runs Rust jobs as fibers on a real runtime with poisoning on,
//! through `fiber_ops::tests`' helpers (plan §2.2's test API), orders its
//! events with channels, joins and gates that one fiber opens after an
//! event (never with timing), and ends with the domain's live count at
//! zero. A test's result never depends on which fiber runs first.
//!
//! A job is a Rust closure registered under a key; the spawned Cheby
//! closure captures the key first, then the Cheby values the job borrows,
//! which are released with the closure (D-670). The trampoline [`job_t0`]
//! stands in for `cb$tramp$0`. A job acts as generated code does: after a
//! kernel call it tests the unwind flag, and when it is set, runs its
//! cleanup (drops) and returns. A job never calls an entry check unless the
//! test says so, so a job that only computes and yields is work during
//! which no cancellation is delivered (D-102: delivery happens at an entry
//! check or a suspension).
//!
//! Fibers never assert: they write what they saw into [`Notes`], and the
//! test thread compares it with what the design requires once the runtime
//! has stopped, so one failing test does not abort the others.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use super::base::{JoinCell, Outcome};
use super::cancel::{Cancelled, check};
use super::channel::{cheby_channel_receive, cheby_channel_send};
use super::join::cheby_fiber_sys_join;
use super::scope::{ScopeHandle, ScopeState};
use super::tests::{
    cell_of, channel, close, config, ctx_of, drop_word, dup, failure_of, int, open, result, spawn,
    string, wait_for_waiters, wait_until_failed, yield_now,
};
use crate::abi::{PanicKind, Word, unwind_flag};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{Callbacks, EntryConfig, EntryOutcome};
use crate::generic::tests::captures;
use crate::handles::{self, HandleKind};
use crate::panic::{PanicReport, cheby_rt_panic};
use crate::sched::Runtime;
use crate::sched::testing::{self, HANG};
use crate::unwind::flag;
use crate::values;

// ---------------------------------------------------------------------------
// Jobs: Rust closures run as spawned fibers.

/// A job: runs on the spawned fiber with the Cheby captures after the key,
/// borrowed, and returns its result, owned.
type JobFn = Box<dyn FnOnce(&ExecCtx, &[Word]) -> Word + Send>;

/// The jobs not started yet, by key.
static JOBS: Mutex<BTreeMap<i64, JobFn>> = Mutex::new(BTreeMap::new());

/// The next job key.
static NEXT_JOB: AtomicI64 = AtomicI64::new(1);

/// Registers `job` and returns its key.
fn register(job: impl FnOnce(&ExecCtx, &[Word]) -> Word + Send + 'static) -> i64 {
    let key = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    JOBS.lock().unwrap().insert(key, Box::new(job));
    key
}

/// Removes the job `key` if it never started; returns whether it was still
/// waiting, that is, whether its function was never called.
fn discard(key: i64) -> bool {
    JOBS.lock().unwrap().remove(&key).is_some()
}

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
/// away to the closure; returns the `Fiber` handle, owned, and the job's
/// key.
pub(super) fn spawn_job(
    me: &ExecCtx,
    scope: Word,
    captures: &[Word],
    job: impl FnOnce(&ExecCtx, &[Word]) -> Word + Send + 'static,
) -> (Word, i64) {
    let key = register(job);
    let mut words = vec![values::int_word(key)];
    words.extend_from_slice(captures);
    (spawn(me, scope, &words), key)
}

/// [`spawn_job`] that drops the `Fiber` handle at once.
pub(super) fn start_job(
    me: &ExecCtx,
    scope: Word,
    captures: &[Word],
    job: impl FnOnce(&ExecCtx, &[Word]) -> Word + Send + 'static,
) {
    let (fiber, _) = spawn_job(me, scope, captures, job);
    drop_word(fiber);
}

// ---------------------------------------------------------------------------
// Running roots.

/// An entry configuration with the job trampoline and the domain `domain`.
fn jobs_config(domain: &Arc<Domain>) -> EntryConfig {
    EntryConfig {
        callbacks: Callbacks {
            t0: Some(job_t0),
            ..Callbacks::default()
        },
        ..config(domain)
    }
}

/// Runs `body` as a root fiber on `runtime` with a fresh domain; returns its
/// outcome and the domain.
pub(super) fn outcome_on(
    runtime: &Runtime,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> (EntryOutcome, Arc<Domain>) {
    let domain = Domain::new();
    let outcome = testing::start_root_with(runtime, &jobs_config(&domain), body)
        .recv_timeout(HANG)
        .expect("the root fiber hung");
    (outcome, domain)
}

/// Runs `body` as a root fiber on a new runtime of `threads` workers,
/// requires that it returns `7` and checks that nothing leaked.
pub(super) fn run_jobs(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) {
    let runtime = testing::runtime(threads);
    let (outcome, domain) = outcome_on(&runtime, body);
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

/// The thread counts most tests run at.
pub(super) const THREADS: [usize; 3] = [1, 2, 4];

// ---------------------------------------------------------------------------
// What fibers saw.

/// What the fibers of one run saw, by name, compared with the design by
/// the test thread.
#[derive(Debug, Clone, Default)]
pub(super) struct Notes(Arc<Mutex<BTreeMap<String, String>>>);

impl Notes {
    /// Notes `value`, in its `Debug` form, under `key`.
    pub(super) fn set(&self, key: &str, value: impl Debug) {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_owned(), format!("{value:?}"));
    }

    /// Requires every `(key, value)` of `expected`, where `value` is the
    /// `Debug` form of what was noted; reports every mismatch at once.
    /// What was noted under `key`, in its `Debug` form.
    pub(super) fn get(&self, key: &str) -> Option<String> {
        self.0.lock().unwrap().get(key).cloned()
    }

    pub(super) fn require(&self, threads: usize, expected: &[(&str, String)]) {
        let seen = self.0.lock().unwrap();
        let wrong: Vec<String> = expected
            .iter()
            .filter(|(key, value)| seen.get(*key) != Some(value))
            .map(|(key, value)| {
                format!(
                    "{key}: expected {value}, saw {}",
                    seen.get(*key).map_or("nothing", String::as_str)
                )
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "at {threads} threads:\n  {}",
            wrong.join("\n  ")
        );
    }
}

/// The `Debug` form of `value`, for [`Notes::require`].
pub(super) fn shown(value: impl Debug) -> String {
    format!("{value:?}")
}

/// A scope's failure as `take_failure` reports a panic.
pub(super) fn panic_failure(message: &str, location: &str) -> String {
    shown((0_i64, message, location))
}

/// A gate one fiber opens after an event and others wait for, yielding:
/// ordering by an event, never by timing.
#[derive(Debug, Clone, Default)]
pub(super) struct Gate(Arc<AtomicBool>);

impl Gate {
    pub(super) fn open(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Yields until the gate is open. Yielding is not an entry check, so no
    /// cancellation is delivered meanwhile: this stands for work the fiber
    /// does without calling a function.
    pub(super) fn wait(&self, me: &ExecCtx) {
        for _ in 0..10_000_000 {
            if self.0.load(Ordering::SeqCst) {
                return;
            }
            yield_now(me, 1);
        }
        panic!("the gate never opened");
    }
}

// ---------------------------------------------------------------------------
// Kernel calls that may unwind, as generated code makes them.

/// A kernel's result, or the unwind flag it left set.
pub(super) type Step<T> = Result<T, u32>;

/// The running fiber's unwind flag.
pub(super) fn flag_of(me: &ExecCtx) -> u32 {
    flag::get(me)
}

/// `channel::receive`: `Ok` with the kernel's `Result` taken apart, or the
/// flag when it raised.
pub(super) fn receive_step(me: &ExecCtx, rx: Word) -> Step<Result<Word, Word>> {
    // SAFETY: the running fiber's context and a live `Receiver`.
    let word = unsafe { cheby_channel_receive(ctx_of(me), rx) };
    match flag_of(me) {
        unwind_flag::NONE => Ok(result(word)),
        raised => Err(raised),
    }
}

/// `channel::send` of the borrowed `value`, as [`receive_step`].
pub(super) fn send_step(me: &ExecCtx, tx: Word, value: Word) -> Step<Result<Word, Word>> {
    // SAFETY: the running fiber's context, a live `Sender` and value.
    let word = unsafe { cheby_channel_send(ctx_of(me), tx, value) };
    match flag_of(me) {
        unwind_flag::NONE => Ok(result(word)),
        raised => Err(raised),
    }
}

/// `fiber::join`: the value, owned, or the flag when it raised.
fn join_step(me: &ExecCtx, fiber: Word) -> Step<Word> {
    // SAFETY: the running fiber's context and a live `Fiber`.
    let word = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
    match flag_of(me) {
        unwind_flag::NONE => Ok(word),
        raised => Err(raised),
    }
}

/// A note's text, noted as it is (its `Debug` form has no quotes).
pub(super) struct Shape(String);

impl Debug for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a step gave, for notes: `raised N` with the flag when it raised,
/// else `Ok(…)` or `Err(…)` with the payload as an integer when it is one,
/// `Nil`, or `a value`. Releases the payload.
pub(super) fn step_shape(step: Step<Result<Word, Word>>) -> Shape {
    Shape(match step {
        Err(raised) => format!("raised {raised}"),
        Ok(Ok(word)) => {
            let text = describe(word);
            drop_word(word);
            format!("Ok({text})")
        }
        Ok(Err(word)) => {
            let text = describe(word);
            drop_word(word);
            format!("Err({text})")
        }
    })
}

/// A short description of a borrowed value.
fn describe(word: Word) -> String {
    if word == values::nil() {
        "Nil".to_owned()
    } else if crate::abi::is_tagged_int(word) {
        int(word).to_string()
    } else {
        "a value".to_owned()
    }
}

/// `panic as message` at `location`, as generated code raises it; returns
/// the flag it left.
pub(super) fn panic_with(me: &ExecCtx, message: &str, location: &str) -> u32 {
    let (message, location) = (string(message), string(location));
    // SAFETY: the running fiber's context and live `String`s.
    unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), message, location) };
    drop_word(message);
    drop_word(location);
    flag_of(me)
}

/// The entry check generated code makes; returns the flag after it.
pub(super) fn entry_check(me: &ExecCtx) -> u32 {
    testing::entry_check(me);
    flag_of(me)
}

/// Closes `scope` from `scope_run`'s pad, after its body unwound: clears
/// the flag and closes with `unwound` (`fiber_ops::tests::close_unwound`).
/// When no unwinding is in flight, which the test's notes then report as a
/// mismatch, it closes as the body's return would instead, so that a broken
/// expectation fails the test and does not abort the test process.
pub(super) fn close_unwound(me: &ExecCtx, scope: Word) -> bool {
    if flag_of(me) == NONE {
        close(me, scope)
    } else {
        super::tests::close_unwound(me, scope)
    }
}

/// The state of a borrowed `Scope` handle.
pub(super) fn state_of(scope: Word) -> Arc<ScopeState> {
    // SAFETY: a live `Scope` handle, borrowed.
    Arc::clone(&unsafe { handles::payload::<ScopeHandle>(scope, HandleKind::Scope) }.0)
}

/// Yields until `cell` has `count` parked joiners: waiting for an event,
/// never for a time. Gives up, noting nothing, after many yields, so that a
/// joiner that never parks shows as a mismatch, not as a hang.
fn wait_for_joiners(me: &ExecCtx, cell: &JoinCell, count: usize) -> bool {
    for _ in 0..1_000_000 {
        if cell.joiners() == count {
            return true;
        }
        yield_now(me, 1);
    }
    false
}

/// The outcome of a join cell, for notes.
fn ended(cell: &JoinCell) -> Outcome {
    cell.outcome()
}

/// What the running fiber's last closed scope left for `take_failure`,
/// taken: `None` when the close returned nothing.
fn last_failure(me: &ExecCtx) -> Option<super::Failure> {
    me.conc.take_last_failure()
}

pub(super) const CANCEL: u32 = unwind_flag::CANCEL;
pub(super) const PANIC: u32 = unwind_flag::PANIC;
pub(super) const NONE: u32 = unwind_flag::NONE;

// ---------------------------------------------------------------------------
// I-7: an inner close waits, then passes through.

/// I-7, D-667, D-706 (architecture §5.3, §7.5 step 3): an outer scope is
/// cancelled while an inner body has returned and an inner member still
/// runs. The inner close waits for the member, which the walk cancelled,
/// then passes the cancellation through: it raises it again and the inner
/// scope never returns `Err`. The outer scope returns its first cause.
///
/// The root opens `outer` with a member `m1` and, inside it, `inner` with a
/// member `m2`; `m2` lets `m1` go and waits forever; `m1` panics, which
/// cancels `outer` and everything below it. The root reaches the inner
/// close before or after the panic: the result is the same.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_inner_close_passes_through() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (go_tx, go_rx) = channel(me, 0);
            let (never_tx, never_rx) = channel(me, 0);
            let outer = open(me);
            let (m1, _) = spawn_job(me, outer, &[go_rx], |me, words| {
                if receive_step(me, words[0]).is_ok() {
                    panic_with(me, "the outer member's", "src/m1.cheby:1:1");
                }
                0
            });
            let inner = open(me);
            let inner_state = state_of(inner);
            let m2_notes = seen.clone();
            let (m2, _) = spawn_job(me, inner, &[go_tx, never_rx], move |me, words| {
                m2_notes.set(
                    "m2 send",
                    step_shape(send_step(me, words[0], values::nil())),
                );
                m2_notes.set("m2 receive", step_shape(receive_step(me, words[1])));
                0
            });
            let (m1_cell, m2_cell) = (cell_of(m1), cell_of(m2));
            // The inner body has returned: its close.
            close(me, inner);
            seen.set("inner close flag", flag_of(me));
            seen.set("inner close failure", last_failure(me));
            seen.set("m2 after the inner close", ended(&m2_cell));
            seen.set("inner ended", inner_state.ended());
            seen.set("open scopes", me.conc.open_scope_count());
            // `outer`'s pad.
            seen.set("outer close", close_unwound(me, outer));
            seen.set("outer failure", failure_of(me));
            seen.set("m1", ended(&m1_cell));
            seen.set("entry check after", entry_check(me));
            for word in [m1, m2, never_tx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("m2 send", "Ok(Nil)".to_owned()),
                ("m2 receive", format!("raised {CANCEL}")),
                ("inner close flag", shown(CANCEL)),
                ("inner close failure", shown(None::<super::Failure>)),
                ("m2 after the inner close", shown(Outcome::Failed)),
                ("inner ended", shown(true)),
                ("open scopes", shown(1_usize)),
                ("outer close", shown(false)),
                (
                    "outer failure",
                    panic_failure("the outer member's", "src/m1.cheby:1:1"),
                ),
                ("m1", shown(Outcome::Failed)),
                ("entry check after", shown(NONE)),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// D-667: the outermost target wins, and extends an unwinding.

/// What [`outer_and_inner`] builds: inside the root's scope `outer`, a
/// member `m` that opens `inner` and runs `m_body` there.
///
/// `p`, a member of `outer`, waits on a channel whose only `Sender` the
/// returned closure gets; once it is dropped, `p` panics, which cancels
/// `outer`, and then opens `gate`. `k`, a member of `inner`, tells `k2` it
/// has started, then waits for `gate` without an entry check, so `inner`'s
/// close cannot end before `outer` is cancelled; then it runs an entry
/// check. `k2`, a member of `inner`, panics once `k` has started, which
/// cancels `inner`.
struct OuterAndInner {
    notes: Notes,
    gate: Gate,
}

impl OuterAndInner {
    /// Runs the shape at `threads` threads. `m_body` gets `m`'s context,
    /// `inner`'s handle and the `Sender` that lets `p` go (owned), and `k2`
    /// gets that `Sender` too when `k2_drops_it` (then `m_body` gets 0).
    fn run(
        threads: usize,
        k2_drops_it: bool,
        m_body: impl FnOnce(&ExecCtx, Word, Word, &Self) + Send + 'static,
    ) -> Notes {
        let shape = Self {
            notes: Notes::default(),
            gate: Gate::default(),
        };
        let notes = shape.notes.clone();
        run_jobs(threads, move |me| {
            let outer = open(me);
            let seen = shape.notes.clone();
            let (m, _) = spawn_job(me, outer, &[dup(outer)], move |me, words| {
                shape.m(me, words[0], k2_drops_it, m_body);
                0
            });
            let m_cell = cell_of(m);
            drop_word(m);
            seen.set("outer close", close(me, outer));
            seen.set("outer failure", failure_of(me));
            seen.set("m", ended(&m_cell));
            seen.set("root entry check after", entry_check(me));
            7
        });
        notes
    }

    /// `m`'s part: spawns `p`, opens `inner`, spawns `k` and `k2`, then runs
    /// `m_body`.
    fn m(
        &self,
        me: &ExecCtx,
        outer: Word,
        k2_drops_it: bool,
        m_body: impl FnOnce(&ExecCtx, Word, Word, &Self),
    ) {
        let (p_tx, p_rx) = channel(me, 0);
        let (gate, notes) = (self.gate.clone(), self.notes.clone());
        start_job(me, outer, &[p_rx], move |me, words| {
            notes.set("p receive", step_shape(receive_step(me, words[0])));
            notes.set(
                "p panic",
                panic_with(me, "the outer cause", "src/p.cheby:3:3"),
            );
            gate.open();
            0
        });
        let inner = open(me);
        let (started_tx, started_rx) = channel(me, 1);
        let (gate, notes) = (self.gate.clone(), self.notes.clone());
        start_job(me, inner, &[started_tx], move |me, words| {
            notes.set(
                "k started",
                step_shape(send_step(me, words[0], values::nil())),
            );
            gate.wait(me);
            notes.set("k entry check", entry_check(me));
            0
        });
        let notes = self.notes.clone();
        let (k2_captures, p_tx) = if k2_drops_it {
            (vec![started_rx, p_tx], 0)
        } else {
            (vec![started_rx], p_tx)
        };
        start_job(me, inner, &k2_captures, move |me, words| {
            notes.set("k2 receive", step_shape(receive_step(me, words[0])));
            notes.set(
                "k2 panic",
                panic_with(me, "the inner cause", "src/k2.cheby:2:2"),
            );
            // Unwinding: the closure, and the `Sender` it may hold, goes
            // when the fiber's function is released (D-670).
            0
        });
        m_body(me, inner, p_tx, self);
    }

    /// What the root and the fibers other than `m` see in both tests.
    fn common() -> Vec<(&'static str, String)> {
        vec![
            ("p receive", "Err(Nil)".to_owned()),
            ("p panic", shown(PANIC)),
            ("k started", "Ok(Nil)".to_owned()),
            ("k entry check", shown(CANCEL)),
            ("k2 receive", "Ok(Nil)".to_owned()),
            ("k2 panic", shown(PANIC)),
            ("outer close", shown(false)),
            (
                "outer failure",
                panic_failure("the outer cause", "src/p.cheby:3:3"),
            ),
            ("m", shown(Outcome::Failed)),
            ("root entry check after", shown(NONE)),
        ]
    }
}

/// D-667, D-706: a cancellation that arrives while a fiber unwinds for an
/// inner target extends the unwinding to the outer one. `m` is cancelled
/// for `inner` (its member `k2` panicked) while it waits in a receive, and
/// starts unwinding; only its cleanup then lets `p` panic, which cancels
/// `outer`. `inner`'s close, which waits for `k` until `outer` is
/// cancelled, finds the outer target and passes it through instead of
/// returning `inner`'s `Err`, and `m` unwinds completely.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_outer_target_extends_an_unwinding() {
    for threads in THREADS {
        let notes = OuterAndInner::run(threads, false, |me, inner, p_tx, shape| {
            let (m_tx, m_rx) = channel(me, 0);
            let received = receive_step(me, m_rx);
            shape.notes.set("m receive", step_shape(received));
            // Cleanup pads, unwinding for `inner`: dropping the `Sender`
            // lets `p` panic.
            for word in [m_rx, m_tx, p_tx] {
                drop_word(word);
            }
            close_unwound(me, inner);
            shape.notes.set("m inner close flag", flag_of(me));
            shape.notes.set("m inner close failure", last_failure(me));
        });
        let mut expected = OuterAndInner::common();
        expected.extend([
            ("m receive", format!("raised {CANCEL}")),
            ("m inner close flag", shown(CANCEL)),
            ("m inner close failure", shown(None::<super::Failure>)),
        ]);
        notes.require(threads, &expected);
    }
}

/// D-667, D-706: a fiber with two pending cancellations unwinds to the
/// outermost target. `m` works without an entry check while `k2`'s panic
/// cancels `inner` and then `p`'s panic cancels `outer` (`k2`'s released
/// closure drops the `Sender` `p` waits on). Its next entry check delivers
/// the cancellation, and `inner`'s close passes it through to `outer`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_outermost_target_wins() {
    for threads in THREADS {
        let notes = OuterAndInner::run(threads, true, |me, inner, _, shape| {
            shape.gate.wait(me);
            shape.notes.set("m check", check(me));
            shape.notes.set("m entry check", entry_check(me));
            close_unwound(me, inner);
            shape.notes.set("m inner close flag", flag_of(me));
            shape.notes.set("m inner close failure", last_failure(me));
        });
        let mut expected = OuterAndInner::common();
        expected.extend([
            ("m check", shown(Err::<(), _>(Cancelled))),
            ("m entry check", shown(CANCEL)),
            ("m inner close flag", shown(CANCEL)),
            ("m inner close failure", shown(None::<super::Failure>)),
        ]);
        notes.require(threads, &expected);
    }
}

/// Architecture §7.5 step 3, D-706: a close whose target is its own scope
/// clears the target and the CANCEL reason under the fiber lock and returns
/// the scope's `Err`; the fiber goes on uncancelled. A later cancellation of
/// an outer scope then sets a new target, and the next entry check
/// delivers it: nothing is lost.
///
/// `m`, a member of the root's `outer`, opens `inner`, whose member `k`
/// panics; `m` is cancelled in a receive and unwinds to `inner`'s close,
/// which returns `k`'s panic. `m` then lets `p`, a member of `outer`, panic
/// and works without an entry check until `p`'s panic has started.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_close_clears_its_own_target_and_a_later_outer_cancel_is_delivered() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let gate = Gate::default();
        run_jobs(threads, move |me| {
            let outer = open(me);
            let m_notes = seen.clone();
            let (m, _) = spawn_job(me, outer, &[dup(outer)], move |me, words| {
                clear_then_outer(me, words[0], &m_notes, &gate);
                0
            });
            let m_cell = cell_of(m);
            drop_word(m);
            seen.set("outer close", close(me, outer));
            seen.set("outer failure", failure_of(me));
            seen.set("m", ended(&m_cell));
            7
        });
        notes.require(
            threads,
            &[
                ("m receive", format!("raised {CANCEL}")),
                ("m inner close", shown(false)),
                ("m inner close flag", shown(NONE)),
                (
                    "m inner failure",
                    panic_failure("the inner cause", "src/k.cheby:1:1"),
                ),
                ("m check after the close", shown(Ok::<(), Cancelled>(()))),
                ("m entry check after the close", shown(NONE)),
                ("m send", "Ok(Nil)".to_owned()),
                ("p panic", shown(PANIC)),
                (
                    "m check after the outer cancel",
                    shown(Err::<(), _>(Cancelled)),
                ),
                ("m entry check after the outer cancel", shown(CANCEL)),
                ("outer close", shown(false)),
                (
                    "outer failure",
                    panic_failure("the outer cause", "src/p.cheby:3:3"),
                ),
                ("m", shown(Outcome::Failed)),
            ],
        );
    }
}

/// `m`'s part of [`cancel_close_clears_its_own_target_and_a_later_outer_cancel_is_delivered`].
fn clear_then_outer(me: &ExecCtx, outer: Word, notes: &Notes, gate: &Gate) {
    let (go_tx, go_rx) = channel(me, 0);
    let (p_notes, p_gate) = (notes.clone(), gate.clone());
    start_job(me, outer, &[go_rx], move |me, words| {
        if receive_step(me, words[0]).is_ok() {
            p_notes.set(
                "p panic",
                panic_with(me, "the outer cause", "src/p.cheby:3:3"),
            );
        }
        p_gate.open();
        0
    });
    let inner = open(me);
    start_job(me, inner, &[], |me, _| {
        panic_with(me, "the inner cause", "src/k.cheby:1:1");
        0
    });
    let (m_tx, m_rx) = channel(me, 0);
    notes.set("m receive", step_shape(receive_step(me, m_rx)));
    drop_word(m_rx);
    drop_word(m_tx);
    notes.set("m inner close", close_unwound(me, inner));
    notes.set("m inner close flag", flag_of(me));
    notes.set("m inner failure", failure_of(me));
    notes.set("m check after the close", check(me));
    notes.set("m entry check after the close", entry_check(me));
    notes.set("m send", step_shape(send_step(me, go_tx, values::nil())));
    gate.wait(me);
    notes.set("m check after the outer cancel", check(me));
    notes.set("m entry check after the outer cancel", entry_check(me));
    // Unwinding completely: the cleanup pads.
    drop_word(go_tx);
}

// ---------------------------------------------------------------------------
// D-668, D-707: `join` and failed fibers.

/// D-668's "always", D-666, D-707: a member joining a sibling that failed
/// is being cancelled, so it unwinds and never panics with `joined fiber
/// failed`: the walk at the panic's start ends before the sibling's join
/// cell becomes `Failed`. `before` joins while the sibling `f` still runs
/// and is parked in its cell when `f` panics, `after` waits for `f`'s cell to be `Failed`
/// without an entry check and then joins. The scope's `Err` is `f`'s.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_member_joining_a_failed_sibling_unwinds() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (go_tx, go_rx) = channel(me, 0);
            let (ready_tx, ready_rx) = channel(me, 2);
            let scope = open(me);
            let (f, _) = spawn_job(me, scope, &[go_rx], |me, words| {
                if receive_step(me, words[0]).is_ok() {
                    panic_with(me, "the sibling's", "src/f.cheby:5:5");
                }
                0
            });
            let f_cell = cell_of(f);
            let before_notes = seen.clone();
            start_job(me, scope, &[dup(ready_tx), dup(f)], move |me, words| {
                before_notes.set(
                    "before ready",
                    step_shape(send_step(me, words[0], values::nil())),
                );
                let joined = join_step(me, words[1]);
                before_notes.set("before join", joined.map(drop_word));
                0
            });
            let (after_notes, after_cell) = (seen.clone(), Arc::clone(&f_cell));
            start_job(me, scope, &[ready_tx, dup(f)], move |me, words| {
                after_notes.set(
                    "after ready",
                    step_shape(send_step(me, words[0], values::nil())),
                );
                wait_until_failed(me, &after_cell);
                let joined = join_step(me, words[1]);
                after_notes.set("after join", joined.map(drop_word));
                0
            });
            // Both joiners have started before `f` may panic.
            for _ in 0..2 {
                drop_shape(receive_step(me, ready_rx));
            }
            // `before` is parked in `f`'s cell when `f` panics.
            seen.set("before parked", wait_for_joiners(me, &f_cell, 1));
            seen.set("go", step_shape(send_step(me, go_tx, values::nil())));
            seen.set("close", close(me, scope));
            seen.set("failure", failure_of(me));
            seen.set("f", ended(&f_cell));
            for word in [f, go_tx, ready_rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("before ready", "Ok(Nil)".to_owned()),
                ("after ready", "Ok(Nil)".to_owned()),
                ("before parked", shown(true)),
                ("go", "Ok(Nil)".to_owned()),
                ("before join", shown(Err::<(), _>(CANCEL))),
                ("after join", shown(Err::<(), _>(CANCEL))),
                ("close", shown(false)),
                ("failure", panic_failure("the sibling's", "src/f.cheby:5:5")),
                ("f", shown(Outcome::Failed)),
            ],
        );
    }
}

/// Drops what a step gave, if anything.
pub(super) fn drop_shape(step: Step<Result<Word, Word>>) {
    if let Ok(Ok(word) | Err(word)) = step {
        drop_word(word);
    }
}

/// D-668, D-670, D-707 (architecture §7.6): a joiner parked on a running
/// fiber whose own scope is cancelled unwinds, takes nothing and is not
/// left in the join cell: when the joined fiber `x`, in another scope,
/// returns later and wakes its joiners, no retired fiber is woken, and the
/// value is still there for the next joiner.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_joiner_unwinds_and_is_not_left_in_the_cell() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (x_tx, x_rx) = channel(me, 0);
            let (p_tx, p_rx) = channel(me, 0);
            let (ready_tx, ready_rx) = channel(me, 0);
            let a = open(me);
            let (x, _) = spawn_job(me, a, &[x_rx], |me, words| {
                match receive_step(me, words[0]) {
                    Ok(Ok(value)) => value,
                    _ => 0,
                }
            });
            let b = open(me);
            let j_notes = seen.clone();
            let (j, _) = spawn_job(me, b, &[ready_tx, dup(x)], move |me, words| {
                j_notes.set(
                    "j ready",
                    step_shape(send_step(me, words[0], values::nil())),
                );
                let joined = join_step(me, words[1]);
                j_notes.set("j join", joined.map(drop_word));
                0
            });
            let j_cell = cell_of(j);
            start_job(me, b, &[p_rx], |me, words| {
                if receive_step(me, words[0]).is_ok() {
                    panic_with(me, "the cause", "src/p.cheby:3:3");
                }
                0
            });
            let x_cell = cell_of(x);
            seen.set("ready", step_shape(receive_step(me, ready_rx)));
            // `j` is parked in `x`'s cell before its scope is cancelled.
            seen.set("j parked", wait_for_joiners(me, &x_cell, 1));
            seen.set("go", step_shape(send_step(me, p_tx, values::nil())));
            seen.set("b close", close(me, b));
            seen.set("b failure", failure_of(me));
            seen.set("j", ended(&j_cell));
            seen.set("joiners left", x_cell.joiners());
            // `x` returns now, after `j` has left.
            let value = string("x's value");
            seen.set("x send", step_shape(send_step(me, x_tx, value)));
            seen.set("a close", close(me, a));
            let joined = join_step(me, x);
            seen.set("root join", joined.map(text_of));
            if let Ok(word) = joined {
                drop_word(word);
            }
            for word in [value, x, j, x_tx, p_tx, ready_rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("j ready", "Ok(Nil)".to_owned()),
                ("ready", "Ok(Nil)".to_owned()),
                ("j parked", shown(true)),
                ("go", "Ok(Nil)".to_owned()),
                ("j join", shown(Err::<(), _>(CANCEL))),
                ("b close", shown(false)),
                ("b failure", panic_failure("the cause", "src/p.cheby:3:3")),
                ("j", shown(Outcome::Failed)),
                ("joiners left", shown(0_usize)),
                ("x send", "Ok(Nil)".to_owned()),
                ("a close", shown(true)),
                ("root join", shown(Ok::<_, u32>("x's value".to_owned()))),
            ],
        );
    }
}

/// The text of a borrowed `String`.
fn text_of(word: Word) -> String {
    super::tests::text(word)
}

/// D-668, D-732: a joiner that is not being cancelled panics with `joined
/// fiber failed` at `<unknown location>` when the fiber it joins was
/// cancelled, not panicked. `x` is cancelled in a receive when its sibling
/// `p` panics. `j1`, a member of a scope `t` that `h` opens beside `x`'s
/// scope, is parked in `x`'s cell when `x` is cancelled and is woken by
/// `x`'s failure; its panic is `t`'s `Err`. Then
/// the root, outside `x`'s scope and uncancelled, joins `x` itself, and its
/// panic is its own scope's `Err`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_outside_joiner_of_a_cancelled_fiber_panics() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (p_tx, p_rx) = channel(me, 0);
            let (never_tx, never_rx) = channel(me, 0);
            let (ready_tx, ready_rx) = channel(me, 0);
            let r0 = open(me);
            let s = open(me);
            let x_notes = seen.clone();
            let (x, _) = spawn_job(me, s, &[never_rx, dup(ready_tx)], move |me, words| {
                drop_shape(send_step(me, words[1], values::nil()));
                x_notes.set("x receive", step_shape(receive_step(me, words[0])));
                0
            });
            let x_cell = cell_of(x);
            start_job(me, s, &[p_rx], |me, words| {
                if receive_step(me, words[0]).is_ok() {
                    panic_with(me, "the cause", "src/p.cheby:3:3");
                }
                0
            });
            let h_notes = seen.clone();
            let (h, _) = spawn_job(me, r0, &[dup(x), ready_tx], move |me, words| {
                outside_joiner_scope(me, words[0], words[1], &h_notes);
                values::nil()
            });
            // `x` and `j1` have started before `p` may panic.
            for name in ["ready 1", "ready 2"] {
                seen.set(name, step_shape(receive_step(me, ready_rx)));
            }
            // `j1` is parked in `x`'s cell when `x` is cancelled.
            seen.set("j1 parked", wait_for_joiners(me, &x_cell, 1));
            seen.set("go", step_shape(send_step(me, p_tx, values::nil())));
            seen.set("s close", close(me, s));
            seen.set("s failure", failure_of(me));
            seen.set("x", ended(&x_cell));
            let joined_h = join_step(me, h);
            seen.set("h join", joined_h.map(describe));
            let joined_x = join_step(me, x);
            seen.set("root join", joined_x.map(drop_word));
            // The cleanup pads, then `r0`'s pad.
            for word in [x, h, p_tx, never_tx, ready_rx] {
                drop_word(word);
            }
            seen.set("r0 close", close_unwound(me, r0));
            seen.set("r0 failure", failure_of(me));
            7
        });
        let joined_failed = panic_failure("joined fiber failed", "<unknown location>");
        notes.require(
            threads,
            &[
                ("ready 1", "Ok(Nil)".to_owned()),
                ("ready 2", "Ok(Nil)".to_owned()),
                ("j1 parked", shown(true)),
                ("go", "Ok(Nil)".to_owned()),
                ("x receive", format!("raised {CANCEL}")),
                ("s close", shown(false)),
                ("s failure", panic_failure("the cause", "src/p.cheby:3:3")),
                ("x", shown(Outcome::Failed)),
                ("j1 join", shown(Err::<(), _>(PANIC))),
                ("t close", shown(false)),
                ("t failure", joined_failed.clone()),
                ("h join", shown(Ok::<_, u32>("Nil"))),
                ("root join", shown(Err::<(), _>(PANIC))),
                ("r0 close", shown(false)),
                ("r0 failure", joined_failed),
            ],
        );
    }
}

/// `h`'s part of [`cancel_outside_joiner_of_a_cancelled_fiber_panics`]:
/// opens `t` with the joiner `j1`, which joins `x` after telling the root
/// it has started, and closes `t`.
fn outside_joiner_scope(me: &ExecCtx, x: Word, ready_tx: Word, notes: &Notes) {
    let t = open(me);
    let j1_notes = notes.clone();
    start_job(me, t, &[dup(x), dup(ready_tx)], move |me, words| {
        drop_shape(send_step(me, words[1], values::nil()));
        let joined = join_step(me, words[0]);
        j1_notes.set("j1 join", joined.map(drop_word));
        0
    });
    notes.set("t close", close(me, t));
    notes.set("t failure", failure_of(me));
}

// ---------------------------------------------------------------------------
// D-667, D-668: below a scope the walk has not reached yet.

/// Makes entry checks in a loop, yielding between them, as a fiber working
/// in a loop of calls does, until one delivers a cancellation; returns
/// whether one did (the flag is then set), giving up after many checks.
pub(super) fn until_cancelled(me: &ExecCtx) -> bool {
    for _ in 0..10_000_000 {
        if entry_check(me) != NONE {
            return true;
        }
        yield_now(me, 1);
    }
    false
}

/// D-666, D-667, D-668, D-670 (review finding CONC-W2-1): the walk sets
/// targets one scope at a time, parent first, and a fiber it has reached
/// can unwind while the walk is still on its way, so the effects of a
/// cancellation can reach fibers below a scope the walk has not visited
/// yet. Those fibers are being cancelled all the same (D-667: the subtree
/// is cancelled at the moment its scope fails), so what they see must not
/// depend on how far the walk has come, which would make it depend on the
/// thread count.
///
/// The root opens `s` with the members `m2`, which makes entry checks in a
/// loop and holds the only `Sender` of `c`, and `o`, which opens `s2` with
/// three members and then waits: `j1` joins `m2`, `j2` receives on `c`,
/// and `j3` opens `c3` with a member `k` that receives on `c`, and closes
/// `c3` as soon as its body returns. Then the root runs only the first step
/// of `s`'s walk (`hooks::cancel_first_step`), so `s2` stays unvisited, as
/// it is while the walk wakes `s`'s fibers: `m2` unwinds, which releases
/// `c`'s `Sender` and fails `m2`. `j1` unwinds instead of panicking with
/// `joined fiber failed`, `j2` and `k` unwind instead of returning
/// `Err(Nil)`, and `j3`'s close of `c3` passes the cancellation through
/// instead of returning. Only then does the root run the whole walk.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_fibers_below_an_unvisited_scope_are_cancelled() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (c_tx, c_rx) = channel(me, 0);
            let (ready_tx, ready_rx) = channel(me, 0);
            let (never_tx, never_rx) = channel(me, 0);
            let s = open(me);
            let s_state = state_of(s);
            let m2_notes = seen.clone();
            let (m2, _) = spawn_job(me, s, &[dup(c_tx)], move |me, _| {
                m2_notes.set("m2 delivered", until_cancelled(me));
                0
            });
            let m2_cell = cell_of(m2);
            let gates: [Gate; 3] = Default::default();
            let (o_notes, o_gates) = (seen.clone(), gates.clone());
            start_job(
                me,
                s,
                &[never_rx, ready_tx, dup(m2), c_rx],
                move |me, words| {
                    below_an_unvisited_scope(me, words, &o_notes, o_gates);
                    0
                },
            );
            for name in ["ready 1", "ready 2", "ready 3"] {
                seen.set(name, step_shape(receive_step(me, ready_rx)));
            }
            // `j1` is parked in `m2`'s cell, `j2` and `k` on `c`, whose
            // only `Sender` is `m2`'s once the root lets its own go.
            seen.set("j1 parked", wait_for_joiners(me, &m2_cell, 1));
            wait_for_waiters(me, c_tx, 0, 2);
            drop_word(c_tx);
            super::cancel::hooks::cancel_first_step(&s_state);
            for gate in &gates {
                gate.wait(me);
            }
            // The rest of the walk, which finds the fibers below `s2`
            // cancelled already.
            super::cancel::cancel(&s_state);
            for word in [m2, ready_rx, never_tx] {
                drop_word(word);
            }
            // The root owns `s`, which the test cancelled from outside
            // without a cause: the close clears the target and succeeds.
            seen.set("s close", close(me, s));
            seen.set("m2", ended(&m2_cell));
            7
        });
        let raised = format!("raised {CANCEL}");
        notes.require(
            threads,
            &[
                ("ready 1", "Ok(Nil)".to_owned()),
                ("ready 2", "Ok(Nil)".to_owned()),
                ("ready 3", "Ok(Nil)".to_owned()),
                ("j1 parked", shown(true)),
                ("m2 delivered", shown(true)),
                ("j1 join", shown(Err::<(), _>(CANCEL))),
                ("j2 receive", raised.clone()),
                ("k receive", raised.clone()),
                ("j3 c3 close flag", shown(CANCEL)),
                ("o receive", raised),
                ("o s2 close flag", shown(CANCEL)),
                ("s close", shown(true)),
                ("m2", shown(Outcome::Failed)),
            ],
        );
    }
}

/// `o`'s part of [`cancel_fibers_below_an_unvisited_scope_are_cancelled`],
/// with the captures `[never_rx, ready_tx, m2, c_rx]`: opens `s2` with
/// `j1`, `j2` and `j3`, waits until the walk cancels it, and closes `s2`.
/// Each of the three opens its gate once it has noted what it saw.
fn below_an_unvisited_scope(me: &ExecCtx, words: &[Word], notes: &Notes, gates: [Gate; 3]) {
    let (never_rx, ready_tx, m2, c_rx) = (words[0], words[1], words[2], words[3]);
    let [g1, g2, g3] = gates;
    let s2 = open(me);
    let j1_notes = notes.clone();
    start_job(me, s2, &[dup(ready_tx), dup(m2)], move |me, words| {
        drop_shape(send_step(me, words[0], values::nil()));
        let joined = join_step(me, words[1]);
        j1_notes.set("j1 join", joined.map(drop_word));
        g1.open();
        0
    });
    let j2_notes = notes.clone();
    start_job(me, s2, &[dup(ready_tx), dup(c_rx)], move |me, words| {
        drop_shape(send_step(me, words[0], values::nil()));
        j2_notes.set("j2 receive", step_shape(receive_step(me, words[1])));
        g2.open();
        0
    });
    let j3_notes = notes.clone();
    start_job(me, s2, &[dup(ready_tx), dup(c_rx)], move |me, words| {
        let c3 = open(me);
        let k_notes = j3_notes.clone();
        start_job(me, c3, &[dup(words[0]), dup(words[1])], move |me, words| {
            drop_shape(send_step(me, words[0], values::nil()));
            k_notes.set("k receive", step_shape(receive_step(me, words[1])));
            0
        });
        // The body returns at once; the close waits for `k`.
        let _ = close(me, c3);
        j3_notes.set("j3 c3 close flag", flag_of(me));
        g3.open();
        0
    });
    notes.set("o receive", step_shape(receive_step(me, never_rx)));
    close_unwound(me, s2);
    notes.set("o s2 close flag", flag_of(me));
}

/// One round of [`cancel_nested_joiner_while_the_walk_wakes_many`]: `s`
/// has `parked` members waiting on a channel nobody sends to, which makes
/// the walk's wake loop long, `m2`, which makes entry checks in a loop,
/// and `o`, which opens `s2` with a member `j`; `j` opens `c` and joins
/// `m2` in it. Then a member of `s` panics. Returns what the round saw.
fn nested_joiner_round(runtime: &Runtime, parked: usize) -> (EntryOutcome, i64, Notes) {
    let notes = Notes::default();
    let seen = notes.clone();
    let (outcome, domain) = outcome_on(runtime, move |me| {
        let (never_tx, never_rx) = channel(me, 0);
        let (ready_tx, ready_rx) = channel(me, 0);
        let (go_tx, go_rx) = channel(me, 0);
        let s = open(me);
        for _ in 0..parked {
            start_job(me, s, &[dup(never_rx)], |me, words| {
                drop_shape(receive_step(me, words[0]));
                0
            });
        }
        let (m2, _) = spawn_job(me, s, &[], |me, _| {
            until_cancelled(me);
            0
        });
        let j_notes = seen.clone();
        start_job(
            me,
            s,
            &[dup(never_rx), ready_tx, dup(m2)],
            move |me, words| {
                let s2 = open(me);
                start_job(me, s2, &[dup(words[1]), dup(words[2])], move |me, words| {
                    let c = open(me);
                    drop_shape(send_step(me, words[0], values::nil()));
                    let joined = join_step(me, words[1]);
                    j_notes.set("j join", joined.map(drop_word));
                    close_unwound(me, c);
                    0
                });
                drop_shape(receive_step(me, words[0]));
                close_unwound(me, s2);
                0
            },
        );
        start_job(me, s, &[go_rx], |me, words| {
            if receive_step(me, words[0]).is_ok() {
                panic_with(me, "first", "src/p.cheby:1:1");
            }
            0
        });
        seen.set("ready", step_shape(receive_step(me, ready_rx)));
        seen.set("go", step_shape(send_step(me, go_tx, values::nil())));
        // The body waits until the walk cancels it.
        seen.set("body receive", step_shape(receive_step(me, never_rx)));
        seen.set("close", close_unwound(me, s));
        seen.set("failure", failure_of(me));
        for word in [m2, never_tx, never_rx, ready_rx, go_tx] {
            drop_word(word);
        }
        7
    });
    (outcome, domain.live_objects(), notes)
}

/// `rounds` rounds of [`nested_joiner_round`] with `parked` parked members
/// at `threads` threads, with the migration stress mode `stress`.
fn nested_joiner_rounds(threads: usize, stress: bool, rounds: u64, parked: usize) {
    let runtime = testing::tuned(threads, stress, true);
    for round in 0..rounds {
        let (outcome, live, notes) = nested_joiner_round(&runtime, parked);
        let context = format!("round {round} at {threads} threads, stress {stress}");
        assert_eq!(outcome, EntryOutcome::Returned(7), "{context}");
        assert_eq!(live, 0, "objects leaked in {context}");
        notes.require(
            threads,
            &[
                ("ready", "Ok(Nil)".to_owned()),
                ("go", "Ok(Nil)".to_owned()),
                ("body receive", format!("raised {CANCEL}")),
                ("j join", shown(Err::<(), _>(CANCEL))),
                ("close", shown(false)),
                ("failure", panic_failure("first", "src/p.cheby:1:1")),
            ],
        );
    }
    runtime.shutdown();
}

/// D-667, D-668 (review finding CONC-W2-1), with a real walk: a joiner in a
/// scope nested in a failing scope, joining a sibling of that scope's
/// opener that the cancellation ended, unwinds and never panics with
/// `joined fiber failed`, however long the walk takes to reach it. Many
/// parked members make the walk's wake loop long. At 1, 2 and 4 threads,
/// with the migration stress mode off and on.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_nested_joiner_while_the_walk_wakes_many() {
    for stress in [false, true] {
        for threads in THREADS {
            nested_joiner_rounds(threads, stress, 5, 200);
        }
    }
}

/// [`cancel_nested_joiner_while_the_walk_wakes_many`] with 100 rounds of
/// 400 parked members per setting. Ignored in debug builds; run it with
/// `cargo test -p cheby_runtime --release cancel_nested_joiner -- --include-ignored`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[cfg_attr(
    all(debug_assertions, not(windows)),
    ignore = "100 rounds per setting: run with --release and --include-ignored"
)]
fn cancel_nested_joiner_while_the_walk_wakes_many_100_rounds() {
    for stress in [false, true] {
        for threads in THREADS {
            nested_joiner_rounds(threads, stress, 100, 400);
        }
    }
}

// ---------------------------------------------------------------------------
// D-666: the first cause.

/// D-666, D-688, spec §10.3 item 3: the body panics while a member runs;
/// the walk at the panic's start requests the member's cancellation, but
/// the member works without an entry check and panics before the
/// cancellation is delivered. That later panic is an ordinary panic, not
/// recorded: the scope's `Err` carries the body's message and location.
///
/// The order is the channel's: the body's cleanup drops the channel's only
/// `Sender`, and the member's receive on it, made after the gate the body
/// opens next, finds it closed and returns without suspending (a receive
/// that does not suspend delivers nothing, D-102), so the member's panic
/// follows the body's.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_body_panic_first_wins_over_a_later_member_panic() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        let gate = Gate::default();
        run_jobs(threads, move |me| {
            let (tx, rx) = channel(me, 0);
            let (ready_tx, ready_rx) = channel(me, 0);
            let scope = open(me);
            let (m_notes, m_gate) = (seen.clone(), gate.clone());
            let (member, _) = spawn_job(me, scope, &[rx, ready_tx], move |me, words| {
                drop_shape(send_step(me, words[1], values::nil()));
                m_gate.wait(me);
                m_notes.set("member receive", step_shape(receive_step(me, words[0])));
                m_notes.set(
                    "member panic",
                    panic_with(me, "the member's", "src/member.cheby:1:1"),
                );
                0
            });
            let member_cell = cell_of(member);
            drop_word(member);
            // The member runs before the body panics.
            seen.set("ready", step_shape(receive_step(me, ready_rx)));
            seen.set(
                "body panic",
                panic_with(me, "the body's", "src/body.cheby:2:2"),
            );
            // The body's cleanup pads.
            drop_word(tx);
            drop_word(ready_rx);
            gate.open();
            seen.set("close", close_unwound(me, scope));
            seen.set("failure", failure_of(me));
            seen.set("member", ended(&member_cell));
            7
        });
        notes.require(
            threads,
            &[
                ("ready", "Ok(Nil)".to_owned()),
                ("body panic", shown(PANIC)),
                ("member receive", "Err(Nil)".to_owned()),
                ("member panic", shown(PANIC)),
                ("close", shown(false)),
                ("failure", panic_failure("the body's", "src/body.cheby:2:2")),
                ("member", shown(Outcome::Failed)),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// D-666, D-670: spawning into a failed or cancelled scope.

/// D-666, D-670: a fiber spawned into a scope that has failed (`s`, whose
/// member panicked) or that is cancelled (`inner`, nested in `s` and
/// reached by its walk) never calls its function, releases it, captures
/// included, before it leaves the scope, and its join cell is `Failed`.
/// `inner`'s close passes the cancellation through to `s`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_spawn_into_a_failed_scope_releases_its_function() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let domain = Arc::clone(me.domain());
            let s = open(me);
            let inner = open(me);
            let (p, _) = spawn_job(me, s, &[], |me, _| {
                panic_with(me, "the cause", "src/p.cheby:3:3");
                0
            });
            let p_cell = cell_of(p);
            drop_word(p);
            wait_until_failed(me, &p_cell);
            // The closes consume the two `Scope` handles, which count here.
            let before = domain.live_objects().wrapping_sub(2);
            let ran = Gate::default();
            let (into_failed, failed_key) = {
                let ran = ran.clone();
                spawn_job(me, s, &[string("captured 1")], move |_, _| {
                    ran.open();
                    values::nil()
                })
            };
            let (into_cancelled, cancelled_key) = {
                let ran = ran.clone();
                spawn_job(me, inner, &[string("captured 2")], move |_, _| {
                    ran.open();
                    values::nil()
                })
            };
            let cells = [cell_of(into_failed), cell_of(into_cancelled)];
            close(me, inner);
            seen.set("inner close flag", flag_of(me));
            seen.set("s close", close_unwound(me, s));
            seen.set("s failure", failure_of(me));
            seen.set("cells", cells.each_ref().map(|cell| ended(cell)));
            // Only the two `Fiber` handles are left of what the spawns made.
            seen.set(
                "left after the closes",
                domain.live_objects().wrapping_sub(before),
            );
            seen.set(
                "never called",
                [discard(failed_key), discard(cancelled_key)],
            );
            seen.set("ran", ran.0.load(Ordering::SeqCst));
            drop_word(into_failed);
            drop_word(into_cancelled);
            seen.set(
                "left at the end",
                domain.live_objects().wrapping_sub(before),
            );
            7
        });
        notes.require(
            threads,
            &[
                ("inner close flag", shown(CANCEL)),
                ("s close", shown(false)),
                ("s failure", panic_failure("the cause", "src/p.cheby:3:3")),
                ("cells", shown([Outcome::Failed, Outcome::Failed])),
                ("left after the closes", shown(2_usize)),
                ("never called", shown([true, true])),
                ("ran", shown(false)),
                ("left at the end", shown(0_usize)),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// D-666, D-670, D-706: what a member's panic reaches.

/// D-666, D-670, D-706 (architecture §7.2): a member's panic cancels a
/// sibling parked in `send`, one parked in `receive` and the owner parked
/// in `receive`. The cancelled `send` has not enqueued its value, which
/// its unwinding releases, and the cancelled receives take nothing. The
/// scope returns the panic, and nothing leaks. The panicking member waits
/// until all three are parked.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_member_panic_cancels_parked_siblings_and_the_owner() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let (a_tx, a_rx) = channel(me, 0);
            let (b_tx, b_rx) = channel(me, 0);
            let (o_tx, o_rx) = channel(me, 0);
            let scope = open(me);
            let a_notes = seen.clone();
            let value = string("never sent");
            let (a, _) = spawn_job(me, scope, &[dup(a_tx), value], move |me, words| {
                a_notes.set("a send", step_shape(send_step(me, words[0], words[1])));
                0
            });
            let b_notes = seen.clone();
            let (b, _) = spawn_job(me, scope, &[b_rx], move |me, words| {
                b_notes.set("b receive", step_shape(receive_step(me, words[0])));
                0
            });
            start_job(me, scope, &[a_tx, dup(b_tx), dup(o_tx)], |me, words| {
                wait_for_waiters(me, words[0], 1, 0);
                wait_for_waiters(me, words[1], 0, 1);
                wait_for_waiters(me, words[2], 0, 1);
                panic_with(me, "the cause", "src/p.cheby:3:3");
                0
            });
            let cells = [cell_of(a), cell_of(b)];
            seen.set("owner receive", step_shape(receive_step(me, o_rx)));
            seen.set("close", close_unwound(me, scope));
            seen.set("failure", failure_of(me));
            seen.set("cells", cells.each_ref().map(|cell| ended(cell)));
            // `a`'s value was never enqueued: its channel holds nothing.
            seen.set("a's channel", step_shape(receive_step(me, a_rx)));
            for word in [a, b, a_rx, b_tx, o_tx, o_rx] {
                drop_word(word);
            }
            7
        });
        notes.require(
            threads,
            &[
                ("a send", format!("raised {CANCEL}")),
                ("b receive", format!("raised {CANCEL}")),
                ("owner receive", format!("raised {CANCEL}")),
                ("close", shown(false)),
                ("failure", panic_failure("the cause", "src/p.cheby:3:3")),
                ("cells", shown([Outcome::Failed, Outcome::Failed])),
                ("a's channel", "Err(Nil)".to_owned()),
            ],
        );
    }
}

/// D-666: two members panic at about the same time: exactly one panic is
/// the scope's cause, with its own message and location, whichever starts
/// first; the other is not reported. Both members work without an entry
/// check from their start until a gate the root opens, then panic at once.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_two_member_panics_record_one_cause() {
    for threads in [2, 4] {
        for _ in 0..50 {
            let notes = Notes::default();
            let seen = notes.clone();
            run_jobs(threads, move |me| {
                let (ready_tx, ready_rx) = channel(me, 2);
                let gate = Gate::default();
                let scope = open(me);
                for (name, location) in [("a", "src/a.cheby:1:1"), ("b", "src/b.cheby:2:2")] {
                    let (gate, notes) = (gate.clone(), seen.clone());
                    start_job(me, scope, &[dup(ready_tx)], move |me, words| {
                        drop_shape(send_step(me, words[0], values::nil()));
                        gate.wait(me);
                        notes.set(name, panic_with(me, name, location));
                        0
                    });
                }
                for _ in 0..2 {
                    drop_shape(receive_step(me, ready_rx));
                }
                gate.open();
                seen.set("close", close(me, scope));
                seen.set("failure", failure_of(me));
                drop_word(ready_tx);
                drop_word(ready_rx);
                7
            });
            notes.require(
                threads,
                &[
                    ("a", shown(PANIC)),
                    ("b", shown(PANIC)),
                    ("close", shown(false)),
                ],
            );
            let failure = notes.0.lock().unwrap().get("failure").cloned();
            assert!(
                [
                    Some(panic_failure("a", "src/a.cheby:1:1")),
                    Some(panic_failure("b", "src/b.cheby:2:2"))
                ]
                .contains(&failure),
                "at {threads} threads the cause is {failure:?}"
            );
        }
    }
}

/// D-666, D-667, architecture §6.6: a scope cancelled from outside records
/// no cause. `inner`, nested in `outer`, is cancelled by `outer`'s walk when
/// `outer`'s member `p` panics. `inner`'s member `k`, working without an
/// entry check, then panics before the cancellation is delivered: an
/// ordinary panic, which `inner` does not record (`record_panic` refuses a
/// report too), and `inner`'s close passes the cancellation through; the
/// root's `outer` returns `p`'s panic.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_from_outside_records_no_cause() {
    for threads in THREADS {
        let notes = Notes::default();
        let seen = notes.clone();
        run_jobs(threads, move |me| {
            let outer = open(me);
            let m_notes = seen.clone();
            let (m, _) = spawn_job(me, outer, &[dup(outer)], move |me, words| {
                cancelled_from_outside(me, words[0], &m_notes);
                0
            });
            let m_cell = cell_of(m);
            drop_word(m);
            seen.set("outer close", close(me, outer));
            seen.set("outer failure", failure_of(me));
            seen.set("m", ended(&m_cell));
            7
        });
        notes.require(
            threads,
            &[
                ("k started", "Ok(Nil)".to_owned()),
                ("p panic", shown(PANIC)),
                ("k panic", shown(PANIC)),
                ("inner records a report", shown(false)),
                ("m receive", format!("raised {CANCEL}")),
                ("m inner close flag", shown(CANCEL)),
                ("m inner close failure", shown(None::<super::Failure>)),
                ("outer close", shown(false)),
                (
                    "outer failure",
                    panic_failure("the outer cause", "src/p.cheby:3:3"),
                ),
                ("m", shown(Outcome::Failed)),
            ],
        );
    }
}

/// `m`'s part of [`cancel_from_outside_records_no_cause`].
fn cancelled_from_outside(me: &ExecCtx, outer: Word, notes: &Notes) {
    let gate = Gate::default();
    let (started_tx, started_rx) = channel(me, 0);
    let (p_notes, p_gate) = (notes.clone(), gate.clone());
    start_job(me, outer, &[started_rx], move |me, words| {
        if receive_step(me, words[0]).is_ok() {
            p_notes.set(
                "p panic",
                panic_with(me, "the outer cause", "src/p.cheby:3:3"),
            );
        }
        p_gate.open();
        0
    });
    let inner = open(me);
    let (k_notes, inner_state) = (notes.clone(), state_of(inner));
    start_job(me, inner, &[started_tx], move |me, words| {
        k_notes.set(
            "k started",
            step_shape(send_step(me, words[0], values::nil())),
        );
        gate.wait(me);
        k_notes.set(
            "k panic",
            panic_with(me, "the inner member's", "src/k.cheby:1:1"),
        );
        let report = PanicReport::new(PanicKind::Explicit, Some("probe"), "src/k.cheby:9:9");
        k_notes.set("inner records a report", inner_state.record_panic(&report));
        0
    });
    let (m_tx, m_rx) = channel(me, 0);
    notes.set("m receive", step_shape(receive_step(me, m_rx)));
    drop_word(m_rx);
    drop_word(m_tx);
    close_unwound(me, inner);
    notes.set("m inner close flag", flag_of(me));
    notes.set("m inner close failure", last_failure(me));
}

// ---------------------------------------------------------------------------
// Stress: 018's worker pool.

/// A small deterministic generator (xorshift64*), seeded per round.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// A number in `low..=high`.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let span = high.wrapping_sub(low).wrapping_add(1);
        low.wrapping_add(
            self.0
                .wrapping_mul(0x2545_F491_4F6C_DD1D)
                .checked_rem(span)
                .unwrap_or(0),
        )
    }
}

/// One round of 018's pool: a feeder sends `jobs` jobs over a capacity-0
/// channel to `workers` workers, which send each result back; job `fails`
/// panics. The panic cancels the feeder, the other workers and the body,
/// wherever each one waits; the scope returns the panic and nothing
/// leaks. Returns what the round saw, for its caller to compare.
fn pool_round(
    runtime: &Runtime,
    workers: u64,
    jobs: i64,
    fails: i64,
) -> (EntryOutcome, i64, Notes) {
    let notes = Notes::default();
    let seen = notes.clone();
    let (outcome, domain) = outcome_on(runtime, move |me| {
        let (jobs_tx, jobs_rx) = channel(me, 0);
        let (results_tx, results_rx) = channel(me, 0);
        let scope = open(me);
        start_job(me, scope, &[jobs_tx], move |me, words| {
            for job in 0..jobs {
                if !matches!(send_step(me, words[0], values::int_word(job)), Ok(Ok(_))) {
                    break;
                }
            }
            values::nil()
        });
        for _ in 0..workers {
            start_job(
                me,
                scope,
                &[dup(jobs_rx), dup(results_tx)],
                move |me, words| {
                    pool_worker(me, words[0], words[1], fails);
                    values::nil()
                },
            );
        }
        drop_word(jobs_rx);
        drop_word(results_tx);
        let mut received = 0_i64;
        let unwound = loop {
            match receive_step(me, results_rx) {
                Ok(Ok(_)) => received = received.wrapping_add(1),
                Ok(Err(_)) => break false,
                Err(_) => break true,
            }
        };
        // `unwound` is the scope's pad; a closed channel can come first.
        let closed = if unwound {
            close_unwound(me, scope)
        } else {
            close(me, scope)
        };
        seen.set("close", closed);
        seen.set("failure", failure_of(me));
        seen.set("all results", received >= jobs);
        seen.set("received", received);
        drop_word(results_rx);
        7
    });
    (outcome, domain.live_objects(), notes)
}

/// A worker of [`pool_round`]: takes jobs until the channel closes or a
/// cancellation unwinds it; job `fails` panics.
fn pool_worker(me: &ExecCtx, jobs_rx: Word, results_tx: Word, fails: i64) {
    while let Ok(Ok(job)) = receive_step(me, jobs_rx) {
        let job = int(job);
        if job == fails {
            panic_with(me, &format!("job {job} failed"), "src/pool.cheby:7:7");
            return;
        }
        if !matches!(
            send_step(me, results_tx, values::int_word(job.wrapping_mul(2))),
            Ok(Ok(_))
        ) {
            return;
        }
    }
}

/// `rounds` rounds of [`pool_round`] at `threads` threads, with the
/// migration stress mode `stress`: 4 to 64 workers, twice as many jobs and
/// one panicking job, all chosen by a generator seeded with the round.
fn pool_rounds(threads: usize, stress: bool, rounds: u64) {
    let runtime = testing::tuned(threads, stress, true);
    let mut results = 0_i64;
    for round in 0..rounds {
        let mut rng = Rng::new(round.wrapping_add(1));
        let workers = rng.between(4, 64);
        let jobs = i64::try_from(workers.wrapping_mul(2)).unwrap();
        let fails = i64::try_from(rng.between(0, workers.wrapping_mul(2).wrapping_sub(1))).unwrap();
        let (outcome, live, notes) = pool_round(&runtime, workers, jobs, fails);
        let context = format!(
            "round {round} ({workers} workers, job {fails} fails) at {threads} threads, stress {stress}"
        );
        assert_eq!(outcome, EntryOutcome::Returned(7), "{context}");
        assert_eq!(live, 0, "objects leaked in {context}");
        let failure = panic_failure(&format!("job {fails} failed"), "src/pool.cheby:7:7");
        notes.require(
            threads,
            &[
                ("close", shown(false)),
                ("failure", failure),
                ("all results", shown(false)),
            ],
        );
        let received = notes.0.lock().unwrap().get("received").cloned();
        results = results.wrapping_add(received.and_then(|n| n.parse().ok()).unwrap_or(0));
    }
    runtime.shutdown();
    // For the report: the pools did work before their panics.
    eprintln!(
        "stress pool: {rounds} rounds at {threads} threads, stress {stress}: {results} results"
    );
    assert!(results > 0, "no pool returned a result before its panic");
}

/// D-666, D-667, D-670, D-706: a short run of the stress pool, at 1, 2 and
/// 4 threads and under the migration stress mode
/// (`CHEBY_RT_STRESS_MIGRATE=1`'s mode), for every test run.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn cancel_stress_pool_quick() {
    for stress in [false, true] {
        for threads in THREADS {
            pool_rounds(threads, stress, 10);
        }
    }
}

/// D-666, D-667, D-670, D-706 (survey-u5 §4.3): 018-like pools of 4 to 64
/// workers with one panicking job, 200 rounds at 1, 2 and 4 threads, with
/// the migration stress mode off and on (`CHEBY_RT_STRESS_MIGRATE=1`'s
/// mode). Ignored in debug builds; run it with
/// `cargo test -p cheby_runtime --release cancel_stress_pool -- --include-ignored`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[cfg_attr(
    all(debug_assertions, not(windows)),
    ignore = "200 rounds per setting: run with --release and --include-ignored"
)]
fn cancel_stress_pool_200_rounds() {
    for stress in [false, true] {
        for threads in THREADS {
            pool_rounds(threads, stress, 200);
        }
    }
}
