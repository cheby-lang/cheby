//! Tests of a fiber's context (D-1571, D-1573, `crate::context`): the
//! swap kernels on a root and on an isolated entry, a scope's save and
//! restore, inheritance by scoped, detached and test fibers, the release
//! at every base frame, running out of memory in a bind or a spawn, and
//! captured contexts with the stream's frames (D-1566, D-1567, D-1574).
//! Rust-level fibers on a `Runtime`, no JIT, with the return flag
//! (D-733), as `fiber_ops::tests`; poisoning is on, and every test finds
//! its domain's live count at its baseline at the end.
//!
//! The module is `context_tests`, not `context::tests`: Miri's `text`
//! group selects the tests whose path holds `text::`, and these run
//! fibers.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::abi::{PanicKind, Word};
use crate::alloc::Domain;
use crate::context::{
    self, ScopeSaved, bindings_of, cheby_context_sys_bind, cheby_context_sys_capture,
    cheby_context_sys_empty, cheby_context_sys_enter, cheby_context_sys_get,
    cheby_context_sys_leave, cheby_context_sys_restore, derive, pop_stream, push_stream,
    stream_parts,
};
use crate::ctx::ExecCtx;
use crate::entry::{Callbacks, EntryConfig, EntryMode, EntryOutcome, run_entry};
use crate::fiber_ops::base::{FiberHandle, Outcome};
use crate::fiber_ops::detached::cheby_fiber_sys_spawn_detached;
use crate::fiber_ops::join::cheby_fiber_sys_join;
use crate::fiber_ops::scope::{
    cheby_fiber_take_failure, cheby_rt_scope_close, cheby_rt_scope_open,
};
use crate::fiber_ops::spawn::cheby_fiber_sys_spawn;
use crate::generic::tests::{captures, closure};
use crate::hamt;
use crate::handles::{self, HandleKind};
use crate::panic::cheby_rt_panic;
use crate::rc;
use crate::sched::{AfterSwitch, park, testing};
use crate::unwind::record::{UnwindRecord, take_unwound};
use crate::values;

// ---------------------------------------------------------------------
// Test closures and their trampoline

/// The operation codes of test closures: their first capture.
mod op {
    /// `[op, identity, default]`: returns `get(identity)`, owned.
    pub(super) const GET: i64 = 0;
    /// `[op, identity, default]`: stores the `Int` that `get(identity)`
    /// returns in [`super::SEEN`]; returns `Nil`.
    pub(super) const GET_INTO_SEEN: i64 = 1;
    /// `[op]`: panics, which under the flag backend returns with the flag
    /// set, as generated code's trampoline would.
    pub(super) const PANIC: i64 = 2;
    /// `[op, value]`: returns `value`.
    pub(super) const RETURN: i64 = 3;
}

/// What the last `GET_INTO_SEEN` read, or -1.
static SEEN: AtomicI64 = AtomicI64::new(-1);

/// The trampoline of test closures, with `cb$tramp$0`'s ABI: the closure
/// is borrowed, the result owned.
unsafe extern "C" fn t0(ctx: *mut ExecCtx, function: u64) -> u64 {
    // SAFETY: a live test closure, borrowed.
    let words = unsafe { captures(function) };
    let word = |index: usize| words.get(index).copied().unwrap_or(0);
    // SAFETY: each operation reads the captures its code says it has, and
    // `ctx` is the running fiber's.
    unsafe {
        match values::word_to_i64(word(0)) {
            op::GET => cheby_context_sys_get(ctx, word(1), word(2)),
            op::GET_INTO_SEEN => {
                let value = cheby_context_sys_get(ctx, word(1), word(2));
                SEEN.store(values::word_to_i64(value), Ordering::SeqCst);
                rc::drop(value);
                values::nil()
            }
            op::PANIC => {
                cheby_rt_panic(ctx, PanicKind::Explicit.code(), 0, 0);
                0
            }
            op::RETURN => {
                rc::dup(word(1));
                word(1)
            }
            _ => values::nil(),
        }
    }
}

/// An entry configuration in test mode with the test trampoline and the
/// domain `domain`.
fn config(domain: &Arc<Domain>) -> EntryConfig {
    EntryConfig {
        stack_size: 1 << 20,
        callbacks: Callbacks {
            t0: Some(t0),
            ..Callbacks::default()
        },
        domain: Arc::clone(domain),
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// Runs `body` as a root fiber on a runtime of `threads` workers and
/// returns its outcome and its domain, after the runtime has stopped.
fn outcome(
    threads: usize,
    body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static,
) -> (EntryOutcome, Arc<Domain>) {
    let runtime = testing::runtime(threads);
    let domain = Domain::new();
    let outcome = testing::start_root_with(&runtime, &config(&domain), body)
        .recv_timeout(testing::hang())
        .expect("the root fiber hung");
    runtime.shutdown();
    (outcome, domain)
}

/// Runs `body` as a root and checks that it returned 1 and left nothing
/// live in its domain.
fn on_a_root(threads: usize, body: impl FnOnce(&ExecCtx) + Send + 'static) {
    let (outcome, domain) = outcome(threads, move |me| {
        body(me);
        1
    });
    assert_eq!(outcome, EntryOutcome::Returned(1));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    ptr::from_ref(me).cast_mut()
}

/// A key identity `(hash, path, type)`, owned.
fn identity(hash: u64, path: &str) -> Word {
    values::tuple(&[
        values::u64_word(hash),
        values::string(path),
        values::string("Int"),
    ])
}

/// `context::sys_get`, as its wrapper calls it.
fn get(me: &ExecCtx, identity: Word, default: Word) -> Word {
    // SAFETY: the running fiber's context and live borrowed words.
    unsafe { cheby_context_sys_get(ctx_of(me), identity, default) }
}

/// `context::sys_bind`, as its wrapper calls it: the outer bindings,
/// owned.
fn bind(me: &ExecCtx, identity: Word, value: Word) -> Word {
    // SAFETY: as above.
    unsafe { cheby_context_sys_bind(ctx_of(me), identity, value) }
}

/// [`bind`] of a `value` the caller owns, which it gives up after the
/// call, as the kernel's wrapper does with its parameters.
fn bind_taking(me: &ExecCtx, identity: Word, value: Word) -> Word {
    let outer = bind(me, identity, value);
    drop_word(value);
    outer
}

/// `context::sys_restore` of `outer`, then the release of `outer` that
/// `provide`'s frame does after it.
fn restore(me: &ExecCtx, outer: Word) {
    // SAFETY: as above.
    unsafe { cheby_context_sys_restore(ctx_of(me), outer) };
    drop_word(outer);
}

/// `word` with one more reference, for a closure to take.
fn dup(word: Word) -> Word {
    // SAFETY: a live value.
    unsafe { rc::dup(word) };
    word
}

fn drop_word(word: Word) {
    // SAFETY: an owned reference to a live value.
    unsafe { rc::drop(word) };
}

fn int(word: Word) -> i64 {
    // SAFETY: tagged integers only.
    unsafe { values::word_to_i64(word) }
}

fn text(word: Word) -> String {
    // SAFETY: a live `String`, borrowed.
    unsafe { values::str_of(word) }.to_owned()
}

/// The count of a borrowed object (`rc::count_of`, a test diagnostic).
fn count(word: Word) -> i32 {
    // SAFETY: a live object.
    unsafe { rc::count_of(word) }.unwrap()
}

/// Lets the fiber `me` allocate at most `extra` more bytes, scratch
/// included (D-536), as `fiber_ops::tests::limit_to`.
fn limit_to(me: &ExecCtx, extra: u64) {
    let live = u64::try_from(me.domain().live_bytes()).unwrap_or(u64::MAX);
    let scratch = me.scratch().0.load(Ordering::Relaxed);
    me.set_budget(live.saturating_add(scratch).saturating_add(extra));
}

fn open(me: &ExecCtx) -> Word {
    // SAFETY: the running fiber's context.
    unsafe { cheby_rt_scope_open(ctx_of(me), false, 0, 0) }
}

fn close(me: &ExecCtx, scope: Word) -> bool {
    // SAFETY: the running fiber's context and the handle its open gave.
    unsafe { cheby_rt_scope_close(ctx_of(me), scope, false) }
}

/// Closes a scope whose body unwound, as `scope_run`'s pad does.
fn close_unwound(me: &ExecCtx, scope: Word) -> bool {
    assert_ne!(
        me.unwind.load(Ordering::Acquire),
        0,
        "the body is not unwinding"
    );
    crate::unwind::flag::clear(me);
    // SAFETY: the running fiber's context and the handle its open gave.
    unsafe { cheby_rt_scope_close(ctx_of(me), scope, true) }
}

/// `take_failure` after a failed close: its message.
fn failure_message(me: &ExecCtx) -> String {
    // SAFETY: the running fiber's context, right after a failed close.
    let failure = unsafe { cheby_fiber_take_failure(ctx_of(me)) };
    // SAFETY: a live tuple.
    let message = text(unsafe { values::read_tuple(failure) }[1]);
    drop_word(failure);
    message
}

fn spawn(me: &ExecCtx, scope: Word, captures: &[Word]) -> Word {
    let function = closure(captures);
    // SAFETY: the running fiber's context, a live scope and closure.
    let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
    drop_word(function);
    fiber
}

fn yield_now(me: &ExecCtx) {
    park(me, AfterSwitch::Requeue);
}

// ---------------------------------------------------------------------
// The kernels

/// D-1044, D-1573: a new root's context is empty: `get` returns the
/// default, owned, and the fiber holds no map.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn get_on_the_empty_context_returns_the_default() {
    on_a_root(1, |me| {
        assert_eq!(bindings_of(me), 0);
        let key = identity(1, "app::port");
        let default = values::string("the default");
        let got = get(me, key, default);
        assert_eq!(got, default);
        assert_eq!(count(default), 2, "the result is owned");
        for word in [got, default, key] {
            drop_word(word);
        }
    });
}

/// D-1044, D-1571: a bind is read by `get`, returns the outer bindings
/// (the static empty map for the empty context, never 0, I-F), and the
/// restore puts them back; a nested bind of the same key shadows it and
/// leaves the outer map as it was (I-D).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn bind_get_and_restore() {
    on_a_root(1, |me| {
        let port = identity(1, "app::port");
        let host = identity(2, "app::host");
        let none = values::int_word(-1);
        let outer = bind(me, port, values::int_word(8080));
        assert_eq!(outer, hamt::bindings::empty());
        assert_ne!(bindings_of(me), 0);
        assert_eq!(int(get(me, port, none)), 8080);
        assert_eq!(int(get(me, host, none)), -1);
        let first = bindings_of(me);
        let middle = bind(me, port, values::int_word(9090));
        assert_eq!(middle, first, "the outer bindings, handed back");
        assert_eq!(int(get(me, port, none)), 9090);
        // SAFETY: the live outer map, borrowed.
        let unchanged = unsafe { hamt::bindings::lookup(middle, port) };
        assert_eq!(unchanged.map(int), Some(8080));
        restore(me, middle);
        assert_eq!(bindings_of(me), first);
        assert_eq!(int(get(me, port, none)), 8080);
        restore(me, outer);
        assert_eq!(bindings_of(me), 0);
        assert_eq!(int(get(me, port, none)), -1);
        drop_word(port);
        drop_word(host);
    });
}

/// I-A: `release` takes the fiber's bindings and releases them; a second
/// call does nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn release_twice_releases_once() {
    on_a_root(1, |me| {
        let key = identity(1, "app::name");
        let outer = bind_taking(me, key, values::string("a value"));
        assert_eq!(outer, hamt::bindings::empty());
        let live = me.domain().live_objects();
        context::release(me);
        assert_eq!(bindings_of(me), 0);
        // The root, its node and the value.
        assert_eq!(me.domain().live_objects(), live - 3);
        context::release(me);
        assert_eq!(me.domain().live_objects(), live - 3);
        drop_word(key);
    });
}

/// I-A: a root whose body ends while it still holds bindings, as one
/// that panicked inside `provide` does, has them released by its base
/// frame before its host counts its domain.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_root_that_ends_inside_a_bind_releases_it() {
    on_a_root(1, |me| {
        let key = identity(1, "app::name");
        let outer = bind_taking(me, key, values::string("a value"));
        drop_word(outer);
        drop_word(key);
    });
}

// ---------------------------------------------------------------------
// Scopes

/// D-1573, I-B: what a scope recorded at its open is put back at its
/// close: here a bind whose outer word was released, as a cleanup pad
/// releases it when `body` unwinds, so the scope's reference is the only
/// one; the fiber then has the recorded map, and nothing leaks.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scope_save_and_restore_put_the_recorded_map_back() {
    on_a_root(1, |me| {
        let key = identity(1, "app::name");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        let recorded = bindings_of(me);
        let live = me.domain().live_objects();
        let saved = ScopeSaved::default();
        saved.save(me);
        let inner = bind(me, key, values::int_word(2));
        assert_eq!(inner, recorded);
        drop_word(inner);
        assert_eq!(int(get(me, key, none)), 2);
        saved.restore(me, false);
        assert_eq!(bindings_of(me), recorded);
        assert_eq!(int(get(me, key, none)), 1);
        assert_eq!(me.domain().live_objects(), live);
        // Nothing recorded any more: a second restore empties the context.
        saved.restore(me, false);
        assert_eq!(bindings_of(me), 0);
        drop_word(outer);
        drop_word(key);
    });
}

/// D-1573, I-B, I-C: a scope whose body panicked inside a bind gives the
/// fiber back the context it opened with, as `scope_run`'s pad and close
/// do; one whose body returned after its swap back finds the context
/// unchanged.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scopes_close_restores_the_context_of_its_open() {
    on_a_root(2, |me| {
        let key = identity(1, "app::request");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        let at_open = bindings_of(me);
        // A body that panics inside `provide`.
        let scope = open(me);
        let inner = bind(me, key, values::int_word(2));
        // SAFETY: the running fiber's context; no message or location.
        unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), 0, 0) };
        // `provide`'s cleanup pad releases the outer word it held.
        drop_word(inner);
        assert!(!close_unwound(me, scope));
        assert_eq!(failure_message(me), PanicKind::Explicit.default_message());
        assert_eq!(bindings_of(me), at_open);
        assert_eq!(int(get(me, key, none)), 1);
        // A body that returns after its swap back.
        let scope = open(me);
        let inner = bind(me, key, values::int_word(3));
        restore(me, inner);
        assert!(close(me, scope));
        assert_eq!(bindings_of(me), at_open);
        restore(me, outer);
        assert_eq!(bindings_of(me), 0);
        drop_word(key);
    });
}

/// D-667, I-B: the third way out of a scope's close, the pass-through of
/// an enclosing scope's cancellation, restores the context too. Scope B,
/// opened inside scope A, closes while a failing member cancels A: B's
/// close re-raises with the fiber back on what it had when B opened, and
/// A's close returns `Err` with the fiber back on what it had when A
/// opened. Nothing leaks: B's recorded reference was taken.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_close_that_passes_a_cancellation_through_restores_the_context() {
    on_a_root(1, |me| {
        let key = identity(1, "app::request");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        let at_a = bindings_of(me);
        let live = me.domain().live_objects();
        let a = open(me);
        let in_a = bind(me, key, values::int_word(2));
        let at_b = bindings_of(me);
        let failing = spawn(me, a, &[values::int_word(op::PANIC)]);
        // SAFETY: a live `Fiber` handle, borrowed.
        let cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(failing, HandleKind::Fiber) }.0);
        drop_word(failing);
        let b = open(me);
        let in_b = bind(me, key, values::int_word(3));
        while cell.outcome() != Outcome::Failed {
            yield_now(me);
        }
        // B's body panics inside `provide`, whose pad releases the outer
        // word it held.
        // SAFETY: the running fiber's context; no message or location.
        unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), 0, 0) };
        drop_word(in_b);
        assert!(!close_unwound(me, b));
        assert_ne!(
            me.unwind.load(Ordering::Acquire),
            0,
            "B's close did not pass the cancellation through"
        );
        assert_eq!(bindings_of(me), at_b, "B's close restored nothing");
        // A's body unwinds through the `provide` inside it.
        drop_word(in_a);
        assert!(!close_unwound(me, a));
        assert_eq!(failure_message(me), PanicKind::Explicit.default_message());
        assert_eq!(bindings_of(me), at_a);
        assert_eq!(int(get(me, key, none)), 1);
        assert_eq!(me.domain().live_objects(), live, "objects leaked");
        restore(me, outer);
        drop_word(key);
    });
}

/// D-692: on an isolated entry, which has no runtime, the kernels, a
/// scope's save and restore and the entry's release work as on a root.
#[test]
fn an_isolated_entry_binds_and_restores() {
    unsafe extern "C" fn entry(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        assert!(me.sched.runtime().is_none());
        let key = identity(1, "app::name");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        let scope = open(me);
        let inner = bind(me, key, values::int_word(2));
        assert_eq!(int(get(me, key, none)), 2);
        restore(me, inner);
        assert!(close(me, scope));
        assert_eq!(int(get(me, key, none)), 1);
        restore(me, outer);
        assert_eq!(int(get(me, key, none)), -1);
        // Ends holding a binding, which the entry releases (I-A).
        drop_word(bind_taking(me, key, values::string("left behind")));
        drop_word(key);
        7
    }
    let domain = Domain::new();
    // SAFETY: a test thunk with the entry ABI.
    let outcome = unsafe { run_entry(entry, &config(&domain)) };
    assert_eq!(outcome.unwrap(), EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

// ---------------------------------------------------------------------
// Spawns

/// D-1044, D-1571, I-E: a fiber spawned under a binding reads it; the
/// spawner's map is shared once the spawn returns; every object is free
/// once both fibers have ended. A detached fiber reads it the same way.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_spawned_fiber_reads_its_spawners_bindings() {
    on_a_root(2, |me| {
        let key = identity(1, "app::request");
        let outer = bind_taking(me, key, values::string("request 7"));
        let map = bindings_of(me);
        assert!(count(map) > 0, "a new map is local");
        let scope = open(me);
        let fiber = spawn(
            me,
            scope,
            &[values::int_word(op::GET), dup(key), values::string("none")],
        );
        assert!(count(map) < 0, "the map is shared");
        // SAFETY: the running fiber's context and a live `Fiber`.
        let value = unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) };
        assert_eq!(text(value), "request 7");
        drop_word(value);
        drop_word(fiber);
        assert!(close(me, scope));
        // A detached fiber.
        SEEN.store(-1, Ordering::SeqCst);
        let inner = bind(me, key, values::int_word(42));
        let function = closure(&[
            values::int_word(op::GET_INTO_SEEN),
            dup(key),
            values::int_word(-2),
        ]);
        // SAFETY: the running fiber's context and a live closure.
        unsafe { cheby_fiber_sys_spawn_detached(ctx_of(me), function) };
        drop_word(function);
        restore(me, inner);
        while SEEN.load(Ordering::SeqCst) == -1 {
            yield_now(me);
        }
        assert_eq!(SEEN.load(Ordering::SeqCst), 42);
        restore(me, outer);
        drop_word(key);
    });
}

/// I-A: a fiber spawned into a scope that is being cancelled never calls
/// its function, and its base frame still releases the bindings it
/// inherited (`fiber_main`'s first step skips only the call).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_fiber_cancelled_before_it_starts_releases_what_it_inherited() {
    on_a_root(2, |me| {
        let key = identity(1, "app::request");
        let outer = bind(me, key, values::int_word(5));
        let scope = open(me);
        let failing = spawn(me, scope, &[values::int_word(op::PANIC)]);
        // SAFETY: a live `Fiber` handle, borrowed.
        let cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(failing, HandleKind::Fiber) }.0);
        while cell.outcome() != Outcome::Failed {
            yield_now(me);
        }
        SEEN.store(-1, Ordering::SeqCst);
        let late = spawn(
            me,
            scope,
            &[
                values::int_word(op::GET_INTO_SEEN),
                dup(key),
                values::int_word(0),
            ],
        );
        // SAFETY: a live `Fiber` handle, borrowed.
        let late_cell =
            Arc::clone(&unsafe { handles::payload::<FiberHandle>(late, HandleKind::Fiber) }.0);
        drop_word(failing);
        drop_word(late);
        assert!(!close(me, scope));
        assert_eq!(failure_message(me), PanicKind::Explicit.default_message());
        assert_eq!(late_cell.outcome(), Outcome::Failed);
        assert_eq!(SEEN.load(Ordering::SeqCst), -1, "the function was called");
        // Only the root holds the map again.
        assert!(count(bindings_of(me)) == -1, "a child kept its reference");
        restore(me, outer);
        drop_word(key);
    });
}

/// The budget of the marking tests: room for a `Fiber` handle (at most
/// 64 bytes) and one worklist of `rc::mark_shared`'s first size (8 steps
/// of 16 bytes), not for its growth to 16 steps (256 bytes more, while
/// the old buffer is still counted). A spawned closure of two integers
/// needs 3 steps, so its walk fits; the walk of [`bind_marked`]'s map
/// needs 10 and grows.
const ONE_WORKLIST: u64 = 64 + 128;

/// Binds four keys whose hashes differ in their low five bits, so the
/// map's top node holds all four inline and the walk that marks it needs
/// 2 + 2 × 4 steps. Returns the keys and the outer words, owned, for
/// [`unbind_marked`].
fn bind_marked(me: &ExecCtx) -> Vec<(Word, Word)> {
    (1..=4)
        .map(|hash| {
            let key = identity(hash, "app::marked");
            (key, bind(me, key, values::int_word(0)))
        })
        .collect()
}

/// Undoes [`bind_marked`], innermost first.
fn unbind_marked(me: &ExecCtx, bound: Vec<(Word, Word)>) {
    for (key, outer) in bound.into_iter().rev() {
        restore(me, outer);
        drop_word(key);
    }
}

/// A closure for the marking tests: returns 1, or `Nil` for a detached
/// fiber.
fn returning(value: Word) -> Word {
    closure(&[values::int_word(op::RETURN), value])
}

/// I-E, review finding MEM-1: marking the spawner's bindings shared runs
/// in step 1, before a member slot is reserved, so a walk that runs out
/// of memory leaves the scope without a member: its close returns at once
/// (a reserved slot would make it wait for a fiber that never exists),
/// and nothing leaks. The control spawn shows that the budget holds the
/// handle and the function's own walk, so the raise is the bindings'.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_spawn_whose_marking_of_the_bindings_raises_leaves_no_member() {
    on_a_root(1, |me| {
        // The control: the empty context, under the same budget.
        let scope = open(me);
        let function = returning(values::int_word(1));
        limit_to(me, ONE_WORKLIST);
        // SAFETY: the running fiber's context, a live scope and closure.
        let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
        me.set_budget(u64::MAX);
        assert_ne!(fiber, 0, "the control spawn ran out of memory");
        drop_word(fiber);
        drop_word(function);
        assert!(close(me, scope));
        // Under four bindings.
        let bound = bind_marked(me);
        let scope = open(me);
        let function = returning(values::int_word(1));
        limit_to(me, ONE_WORKLIST);
        // SAFETY: as above.
        let fiber = unsafe { cheby_fiber_sys_spawn(ctx_of(me), scope, function) };
        me.set_budget(u64::MAX);
        assert_eq!(fiber, 0, "the bindings' walk fitted");
        drop_word(function);
        assert!(!close_unwound(me, scope));
        assert_eq!(
            failure_message(me),
            crate::consteval::Limit::Memory.message()
        );
        unbind_marked(me, bound);
    });
}

/// I-E, review finding MEM-1, for `spawn_detached`: the bindings are
/// marked before a slot of the spawner's group is reserved, so a walk
/// that runs out of memory leaves the group without a member (a teardown
/// would wait for a reserved one), and nothing leaks. The control spawn
/// shows that the function's own walk fits the budget.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_detached_spawn_whose_marking_of_the_bindings_raises_reserves_no_slot() {
    on_a_root(1, |me| {
        let group = me.conc.group().unwrap_or_else(|| {
            Arc::clone(
                me.sched
                    .runtime()
                    .expect("a root has a runtime")
                    .default_group(),
            )
        });
        assert_eq!(group.live(), 0);
        // The control: the empty context, under the same budget.
        let function = returning(values::nil());
        limit_to(me, ONE_WORKLIST);
        // SAFETY: the running fiber's context and a live closure.
        unsafe { cheby_fiber_sys_spawn_detached(ctx_of(me), function) };
        me.set_budget(u64::MAX);
        assert!(take_unwound(me).is_none(), "the control spawn raised");
        drop_word(function);
        while group.live() != 0 {
            yield_now(me);
        }
        // Under four bindings.
        let bound = bind_marked(me);
        let function = returning(values::nil());
        limit_to(me, ONE_WORKLIST);
        // SAFETY: as above.
        unsafe { cheby_fiber_sys_spawn_detached(ctx_of(me), function) };
        me.set_budget(u64::MAX);
        match take_unwound(me) {
            Some(UnwindRecord::Panic(report)) => {
                assert_eq!(report.message, crate::consteval::Limit::Memory.message());
            }
            other => panic!("the bindings' walk fitted: {other:?}"),
        }
        assert_eq!(group.live(), 0, "a slot was reserved");
        drop_word(function);
        unbind_marked(me, bound);
    });
}

/// I-A: a test fiber, whose start function is not a base frame, gets its
/// spawner's bindings too, and `fiber_start` releases them when it ends.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_fiber_without_a_base_frame_is_released_when_it_ends() {
    on_a_root(2, |me| {
        let key = identity(1, "app::request");
        let outer = bind(me, key, values::int_word(9));
        context::share_bindings(me);
        let map = bindings_of(me);
        SEEN.store(-1, Ordering::SeqCst);
        let child_key = key;
        testing::spawn(me, true, move |child| {
            assert_eq!(bindings_of(child), map);
            let got = get(child, child_key, values::int_word(0));
            SEEN.store(int(got), Ordering::SeqCst);
        });
        while SEEN.load(Ordering::SeqCst) == -1 || count(map) != -1 {
            yield_now(me);
        }
        assert_eq!(SEEN.load(Ordering::SeqCst), 9);
        restore(me, outer);
        drop_word(key);
    });
}

// ---------------------------------------------------------------------
// Running out of memory

/// The shapes of the out-of-memory tests: the keys, owned, and for each
/// shape the key bound first (if any), the key bound under the budget,
/// and the number of objects that bind allocates. Into the empty context
/// (the node and the root); into a free slot of a map (the root and the
/// top node's copy); where the new key clashes with the stored one down
/// to the last level, bit 63 (the root, the top node's copy, the bottom
/// node and `merge`'s 11 one-child nodes between); where it has the
/// stored one's hash (the same and a collision node at the bottom, below
/// 12 one-child nodes).
struct Shapes {
    keys: [Word; 4],
    cases: [(Option<Word>, Word, u32); 4],
}

fn shapes() -> Shapes {
    let hash = 0x0123_4567_89AB_CDEF_u64;
    let first = identity(hash, "app::first");
    let free = identity(hash ^ 1, "app::free");
    let deep = identity(hash ^ (1 << 63), "app::deep");
    let same = identity(hash, "app::same");
    Shapes {
        keys: [first, free, deep, same],
        cases: [
            (None, first, 2),
            (Some(first), free, 2),
            (Some(first), deep, 14),
            (Some(first), same, 15),
        ],
    }
}

/// D-642, I-D: a bind that runs out of its memory budget leaves the
/// fiber's context as it was and the domain's live objects at their
/// baseline. The budget sweep reaches the scratch worklists of the key
/// comparison (`generic::equal`, in the walk before anything is
/// allocated) and, for the empty context and the free slot, the node
/// allocations; the comparison needs more scratch than the whole chain
/// of a clash or a collision needs objects, so the allocations of those
/// two shapes are swept by [`a_bind_refused_at_any_allocation_changes_nothing`].
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_bind_that_runs_out_of_memory_changes_nothing() {
    on_a_root(1, |me| {
        let Shapes { keys, cases } = shapes();
        let value = values::string("a value");
        let none = values::int_word(-1);
        let mut outers = Vec::new();
        for (setup, key, _) in cases {
            if let Some(setup) = setup {
                outers.push(bind(me, setup, values::int_word(1)));
            }
            let before = bindings_of(me);
            let mut raised = 0;
            for extra in (0..=1024).step_by(8) {
                let live = me.domain().live_objects();
                limit_to(me, extra);
                let outer = bind(me, key, value);
                me.set_budget(u64::MAX);
                if outer == 0 {
                    raised += 1;
                    match take_unwound(me) {
                        Some(UnwindRecord::Panic(report)) => {
                            assert_eq!(report.message, crate::consteval::Limit::Memory.message());
                        }
                        other => panic!("not out of memory: {other:?}"),
                    }
                    assert_eq!(bindings_of(me), before, "the context changed");
                    assert_eq!(me.domain().live_objects(), live, "objects leaked");
                    continue;
                }
                let got = get(me, key, none);
                assert_eq!(text(got), "a value");
                drop_word(got);
                restore(me, outer);
                assert_eq!(bindings_of(me), before);
                assert_eq!(me.domain().live_objects(), live);
                break;
            }
            assert!(raised > 0, "no bind ran out of memory");
        }
        for outer in outers.into_iter().rev() {
            restore(me, outer);
        }
        for word in keys.into_iter().chain([value]) {
            drop_word(word);
        }
    });
}

/// D-642, D-771, I-D: a bind that the system refuses an object at any of
/// its allocations leaves the fiber's context as it was and the domain's
/// live objects at their baseline. The sweep refuses the k-th allocation
/// for k = 0, 1, … until the bind succeeds, and the number of refused
/// binds is the number of objects the bind allocates, so it reaches every
/// node of `merge`'s chain and the guard around `put_owned`. Scratch is
/// not refused (`alloc::refuse_system_allocation_after`).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_bind_refused_at_any_allocation_changes_nothing() {
    let _serial = crate::alloc::tests::serial();
    on_a_root(1, |me| {
        let Shapes { keys, cases } = shapes();
        let value = values::string("a value");
        let none = values::int_word(-1);
        let mut outers = Vec::new();
        for (setup, key, allocations) in cases {
            if let Some(setup) = setup {
                outers.push(bind(me, setup, values::int_word(1)));
            }
            let before = bindings_of(me);
            let mut refused = 0;
            loop {
                let live = me.domain().live_objects();
                crate::alloc::refuse_system_allocation_after(refused);
                let outer = bind(me, key, value);
                if outer != 0 {
                    // The hook is still armed for the next allocation:
                    // let every later one through.
                    crate::alloc::refuse_system_allocation_after(u32::MAX);
                    let got = get(me, key, none);
                    assert_eq!(text(got), "a value");
                    drop_word(got);
                    restore(me, outer);
                    assert_eq!(bindings_of(me), before);
                    assert_eq!(me.domain().live_objects(), live);
                    break;
                }
                match take_unwound(me) {
                    Some(UnwindRecord::Panic(report)) => {
                        assert_eq!(report.message, crate::alloc::OUT_OF_MEMORY);
                    }
                    other => panic!("not out of memory: {other:?}"),
                }
                assert_eq!(bindings_of(me), before, "the context changed");
                assert_eq!(me.domain().live_objects(), live, "objects leaked");
                refused = refused.saturating_add(1);
                assert!(
                    refused <= allocations,
                    "more allocations than {allocations}"
                );
            }
            assert_eq!(refused, allocations, "the bind's allocations");
        }
        for outer in outers.into_iter().rev() {
            restore(me, outer);
        }
        for word in keys.into_iter().chain([value]) {
            drop_word(word);
        }
    });
}

// ---------------------------------------------------------------------
// Captured contexts and the stream's frames

/// `context::sys_capture`, owned.
fn capture(me: &ExecCtx) -> Word {
    // SAFETY: the running fiber's context.
    unsafe { cheby_context_sys_capture(ctx_of(me)) }
}

/// `context::sys_enter` of a borrowed captured context: the outer
/// bindings, owned.
fn enter(me: &ExecCtx, captured: Word) -> Word {
    // SAFETY: the running fiber's context and a live captured context.
    unsafe { cheby_context_sys_enter(ctx_of(me), captured) }
}

/// `context::sys_leave` of `outer`, then the release of `outer` that
/// `with`'s frame does after it.
fn leave(me: &ExecCtx, outer: Word) {
    // SAFETY: the running fiber's context and the word `enter` gave.
    unsafe { cheby_context_sys_leave(ctx_of(me), outer) };
    drop_word(outer);
}

/// The debug text of a live value, borrowed.
fn text_of(word: Word) -> String {
    // SAFETY: a live value.
    unsafe { crate::generic::debug_string(word) }
}

/// The map word and the stream of a captured context, borrowed.
fn parts(captured: Word) -> (Word, Option<u64>) {
    // SAFETY: a live captured context.
    unsafe { handles::context_parts(captured) }
}

/// D-1048, I-J: the Rust derivation equals `std::random`'s `derived`,
/// whose vector is `derive(42, 1)`.
#[test]
fn derive_follows_the_seeded_streams_vector() {
    assert_eq!(derive(42, 1), 814_261_035_509_592_648);
    assert_ne!(derive(42, 1), derive(42, 2));
    assert_ne!(derive(42, 1), derive(43, 1));
    // Wrapping: the largest state and spawn number do not overflow.
    let _ = derive(u64::MAX, u64::MAX);
}

/// D-1566, D-1568: a capture of the empty context and `empty` hold no
/// map and no stream; a capture holds the fiber's map, with a reference
/// of its own, and later binds do not change it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_capture_holds_the_fibers_map() {
    on_a_root(1, |me| {
        let empty_capture = capture(me);
        assert_eq!(parts(empty_capture), (0, None));
        // SAFETY: the running fiber's context.
        let empty = unsafe { cheby_context_sys_empty(ctx_of(me)) };
        assert_eq!(parts(empty), (0, None));
        assert_eq!(text_of(empty), "<Context>");
        let key = identity(1, "app::request");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        let map = bindings_of(me);
        let captured = capture(me);
        assert_eq!(parts(captured), (map, None));
        assert_eq!(count(map), 2, "the fiber's and the context's");
        let inner = bind(me, key, values::int_word(2));
        assert_eq!(parts(captured), (map, None));
        assert_eq!(int(get(me, key, none)), 2);
        restore(me, inner);
        restore(me, outer);
        assert_eq!(count(map), 1, "the context's only");
        for word in [captured, empty, empty_capture, key] {
            drop_word(word);
        }
    });
}

/// D-1566, D-072: a captured context sent through `rc::mark_shared`, as
/// a channel's send does, marks its map shared, and the last reference
/// frees the map.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_shared_capture_marks_its_map_and_frees_it_last() {
    on_a_root(1, |me| {
        let key = identity(1, "app::request");
        let outer = bind_taking(me, key, values::string("request 7"));
        let map = bindings_of(me);
        let captured = capture(me);
        assert!(count(map) > 0, "a new map is local");
        // SAFETY: a live value only this fiber holds.
        unsafe { rc::mark_shared(captured) };
        assert!(count(captured) < 0, "the context is shared");
        assert!(count(map) < 0, "the map is shared");
        let live = me.domain().live_objects();
        restore(me, outer);
        assert_eq!(
            me.domain().live_objects(),
            live,
            "the context keeps the map"
        );
        drop_word(captured);
        // The context, the root, its node and the value are freed.
        assert_eq!(me.domain().live_objects(), live - 4);
        drop_word(key);
    });
}

/// D-1567, D-1048: under a stream, each capture takes the next spawn
/// number and holds the state a child with that number starts from,
/// while the stream's state stays where it was; a capture with no stream
/// holds none and counts nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_capture_under_a_stream_takes_the_next_spawn_number() {
    on_a_root(1, |me| {
        let without = capture(me);
        assert_eq!(parts(without), (0, None));
        assert_eq!(stream_parts(me), ((false, 0, 0), 0));
        push_stream(me, true, 42);
        assert_eq!(stream_parts(me), ((true, 42, 0), 1));
        let first = capture(me);
        assert_eq!(parts(first), (0, Some(814_261_035_509_592_648)));
        assert_eq!(stream_parts(me), ((true, 42, 1), 1));
        let second = capture(me);
        assert_eq!(parts(second), (0, Some(derive(42, 2))));
        assert_eq!(stream_parts(me), ((true, 42, 2), 1));
        pop_stream(me);
        assert_eq!(stream_parts(me), ((false, 0, 0), 0));
        for word in [first, second, without] {
            drop_word(word);
        }
    });
}

/// D-1566, D-1567: `enter` puts the captured map and stream in place of
/// the fiber's, hiding its own bindings, and `leave` puts both back, the
/// outer stream as far as it had advanced; a context captured with no
/// stream turns the fiber's stream off inside.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn enter_and_leave_swap_the_bindings_and_the_stream() {
    on_a_root(1, |me| {
        let key = identity(1, "app::request");
        let none = values::int_word(-1);
        let unbound = capture(me);
        let outer = bind(me, key, values::int_word(1));
        push_stream(me, true, 5);
        let captured = capture(me);
        let at_capture = bindings_of(me);
        let inner = bind(me, key, values::int_word(2));
        let outside = bindings_of(me);
        // Under the captured context.
        let entered = enter(me, captured);
        assert_eq!(entered, outside, "the outer bindings, handed back");
        assert_eq!(bindings_of(me), at_capture);
        assert_eq!(int(get(me, key, none)), 1);
        assert_eq!(stream_parts(me), ((true, derive(5, 1), 0), 2));
        // `provide` binds on top, and a capture inside counts there.
        let on_top = bind(me, key, values::int_word(3));
        assert_eq!(int(get(me, key, none)), 3);
        drop_word(capture(me));
        assert_eq!(stream_parts(me), ((true, derive(5, 1), 1), 2));
        restore(me, on_top);
        leave(me, entered);
        assert_eq!(bindings_of(me), outside);
        assert_eq!(int(get(me, key, none)), 2);
        assert_eq!(stream_parts(me), ((true, 5, 1), 1));
        // A context with no stream and no bindings.
        let entered = enter(me, unbound);
        assert_eq!(entered, outside);
        assert_eq!(bindings_of(me), 0);
        assert_eq!(int(get(me, key, none)), -1);
        assert_eq!(stream_parts(me), ((false, 0, 0), 2));
        leave(me, entered);
        assert_eq!(stream_parts(me), ((true, 5, 1), 1));
        pop_stream(me);
        restore(me, inner);
        restore(me, outer);
        assert_eq!(bindings_of(me), 0);
        for word in [captured, unbound, key] {
            drop_word(word);
        }
    });
}

/// D-1573: a scope records the depth of the stream frames at its open;
/// when frames pushed inside its body are left behind, its close makes
/// the frame at the recorded depth current, in the state it had reached
/// when the first of them was pushed (here: two captures later), and
/// drops the frames above it. A close with no frame above does nothing to
/// the stream.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scopes_restore_cuts_the_stream_frames_back() {
    on_a_root(1, |me| {
        push_stream(me, true, 10);
        let saved = ScopeSaved::default();
        saved.save(me);
        // The outer stream advances inside the body.
        drop_word(capture(me));
        drop_word(capture(me));
        assert_eq!(stream_parts(me), ((true, 10, 2), 1));
        push_stream(me, true, 20);
        drop_word(capture(me));
        push_stream(me, false, 0);
        assert_eq!(stream_parts(me), ((false, 0, 0), 3));
        // The body unwinds: the close cuts back.
        saved.restore(me, false);
        assert_eq!(stream_parts(me), ((true, 10, 2), 1));
        // A scope whose body pushed nothing.
        saved.save(me);
        drop_word(capture(me));
        saved.restore(me, true);
        assert_eq!(stream_parts(me), ((true, 10, 3), 1));
        pop_stream(me);
        assert_eq!(stream_parts(me), ((false, 0, 0), 0));
    });
}

/// D-1573, I-B, I-C: a body that panics inside `with`, under a scope,
/// leaves the fiber on the captured context and stream; the scope's close
/// puts the bindings and the stream of its open back.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_scopes_close_after_a_panic_inside_with_puts_both_back() {
    on_a_root(2, |me| {
        let key = identity(1, "app::request");
        let none = values::int_word(-1);
        let outer = bind(me, key, values::int_word(1));
        push_stream(me, true, 7);
        let captured = provide_and_capture(me, key);
        let at_open = bindings_of(me);
        let scope = open(me);
        let entered = enter(me, captured);
        assert_eq!(int(get(me, key, none)), 2);
        push_stream(me, true, 99);
        // SAFETY: the running fiber's context; no message or location.
        unsafe { cheby_rt_panic(ctx_of(me), PanicKind::Explicit.code(), 0, 0) };
        // `with`'s cleanup pad releases the outer word it held.
        drop_word(entered);
        assert!(!close_unwound(me, scope));
        assert_eq!(failure_message(me), PanicKind::Explicit.default_message());
        assert_eq!(bindings_of(me), at_open);
        assert_eq!(int(get(me, key, none)), 1);
        assert_eq!(stream_parts(me), ((true, 7, 1), 1));
        pop_stream(me);
        restore(me, outer);
        drop_word(captured);
        drop_word(key);
    });
}

/// Binds `key` to 2, captures, and puts the fiber's bindings back: the
/// captured context, owned.
fn provide_and_capture(me: &ExecCtx, key: Word) -> Word {
    let outer = bind(me, key, values::int_word(2));
    let captured = capture(me);
    restore(me, outer);
    captured
}

/// I-A, D-1573: `release` clears the stream and its frames, which a base
/// frame that unwound out of `with` leaves set; a second call does
/// nothing. A root that ends inside `with` is released the same way.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn release_clears_the_stream_and_its_frames() {
    on_a_root(1, |me| {
        push_stream(me, true, 1);
        push_stream(me, true, 2);
        assert_eq!(stream_parts(me), ((true, 2, 0), 2));
        context::release(me);
        assert_eq!(stream_parts(me), ((false, 0, 0), 0));
        context::release(me);
        assert_eq!(stream_parts(me), ((false, 0, 0), 0));
    });
    on_a_root(1, |me| {
        let key = identity(1, "app::request");
        let captured = provide_and_capture(me, key);
        push_stream(me, true, 3);
        let entered = enter(me, captured);
        drop_word(entered);
        drop_word(captured);
        drop_word(key);
    });
}
