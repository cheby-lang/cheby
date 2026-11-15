//! A fiber's context (D-1571, D-1573): the bindings of `std::context`, a
//! persistent map in the runtime's own tries (`hamt::bindings`), held by
//! the fiber control block, with the swap kernels that `provide` and
//! `get` are written over and the save and restore of a scope.
//!
//! **The field.** [`FiberContext`] holds one owned reference to the
//! fiber's map, or 0 for the empty context, written by the owning fiber
//! only, with relaxed ordering (the module documentation of `ctx`). Every
//! control block starts empty (`ExecCtx::new`), so a root, a test, a REPL
//! entry and an isolated entry start from their own context (D-1573). A
//! word handed to Cheby is never 0: the empty context is the static empty
//! map there (D-733, I-F).
//!
//! **Swaps.** `provide` is Cheby over three kernels (D-1573):
//! [`cheby_context_sys_bind`] gives the fiber a new map with one more
//! binding and returns the outer one, which `provide`'s frame holds while
//! `body` runs; [`cheby_context_sys_restore`] puts it back when `body`
//! returns. A bind never updates a map in place, so the outer map keeps
//! what it held (I-D). [`cheby_context_sys_get`] reads a binding, or the
//! key's default. None of them waits, so they work on an isolated entry
//! too (D-692). [`kernels`] registers the three, and [`COMPLETE`] names
//! `std::context`, which declares them (I-L).
//!
//! **Scopes.** When `body` unwinds, the swap back does not run: the
//! catch point puts the context back instead. The only catch point after
//! which Cheby code runs again on the fiber is a scope's close (I-C, the
//! source test `catch_points_are_pinned` in `unwind`), so a scope records
//! the fiber's bindings when it opens ([`ScopeSaved::save`]) and its close
//! restores them on every way out ([`ScopeSaved::restore`], I-B).
//!
//! **Spawns.** A spawned fiber starts with its spawner's bindings
//! (D-1044, D-1571): the spawn kernels mark them shared before anything is
//! reserved ([`share_bindings`], I-E), and `sched::create_fiber` copies
//! the pointer once the fiber exists ([`inherit`]).
//!
//! **Release.** The field cannot be released by dropping the control
//! block, which happens with no fiber current (I-A). Every base frame
//! marks itself ([`set_based`]) and releases the bindings while its fiber
//! is current, before its last touch of shared state ([`release`]):
//! `fiber_main` and `detached_main` before they leave their scope or group,
//! a root and an isolated entry before they report their outcome, so a
//! drop function of a value still bound runs before a panic's report on
//! every host (D-1594). `fiber_start` checks that it was done
//! ([`release_at_exit`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::running;
use crate::hamt::bindings;
use crate::object::{load_rc, word_to_ptr};
use crate::panic::fatal;
use crate::rc;
use crate::unwind::guard::guarded;

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["context"];

/// One kernel: symbol, address, C parameter types (after the context
/// pointer of a `@std_context` kernel) and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The kernels of `std::context`, every one `@std_context` (D-692,
/// D-1573). A key's identity, the tuple `(U64, String, String)`, is one
/// word.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 3] {
    [
        (
            "cheby_context_sys_get",
            cheby_context_sys_get as *const u8,
            &["Word", "Word"],
            "Word",
        ),
        (
            "cheby_context_sys_bind",
            cheby_context_sys_bind as *const u8,
            &["Word", "Word"],
            "Word",
        ),
        (
            "cheby_context_sys_restore",
            cheby_context_sys_restore as *const u8,
            &["Word"],
            "()",
        ),
    ]
}

/// Appends every `std::context` kernel symbol and its address to `out`
/// (plan §4.3.3).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The C signatures of the kernels, without the context pointer, for the
/// cross-check against `std/context.cheby` (`symbols.rs`).
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

/// The kernels that take the running fiber's context (`@std_context`,
/// D-692): all of them.
#[cfg(test)]
pub(crate) const CONTEXT_KERNELS: [&str; 3] = [
    "cheby_context_sys_get",
    "cheby_context_sys_bind",
    "cheby_context_sys_restore",
];

/// A fiber's context (D-1571, D-1574). Owner-written, `Relaxed`.
#[derive(Debug, Default)]
pub(crate) struct FiberContext {
    /// An owned reference to the fiber's bindings map, or 0 for the empty
    /// context.
    bindings: AtomicU64,
    /// Whether a base frame runs this fiber, which releases the bindings
    /// itself ([`set_based`]).
    based: AtomicBool,
}

/// The fiber's bindings word, borrowed: 0 for the empty context.
pub(crate) fn bindings_of(me: &ExecCtx) -> Word {
    me.context.bindings.load(Ordering::Relaxed)
}

/// Replaces the fiber's bindings word with `word`, whose reference the
/// field takes, and returns the old one, whose reference the caller gets.
fn swap_bindings(me: &ExecCtx, word: Word) -> Word {
    me.context.bindings.swap(word, Ordering::Relaxed)
}

/// Gives up an owned reference to a bindings word, 0 included. Never
/// raises (D-171); freeing the map may run drop functions of the values
/// it held, which are foreign code.
fn drop_bindings(word: Word) {
    // SAFETY: the caller owns the reference; 0 is a no-op.
    unsafe { rc::drop(word) };
}

/// The value `identity` is bound to in the fiber's context, or `default`,
/// owned (one reference added). It allocates no object, but the key
/// comparison uses scratch memory ([`bindings::lookup`]) and may raise
/// `out of memory` (D-642), before any reference is taken, so nothing is
/// left to release. `cheby_context_sys_get` must therefore not be listed
/// in `NEVER_RAISE_KERNELS` while it compares through `generic::equal`.
///
/// # Safety
///
/// `identity` is a live key identity and `default` a live value, both
/// borrowed.
pub(crate) unsafe fn get(me: &ExecCtx, identity: Word, default: Word) -> Word {
    let map = bindings_of(me);
    // SAFETY: the fiber's map is 0 or live while it holds it; the identity
    // is forwarded from the caller.
    let found = unsafe { bindings::lookup(map, identity) };
    let value = found.unwrap_or(default);
    // SAFETY: a live value, borrowed from the map or the caller.
    unsafe { rc::dup(value) };
    value
}

/// Whether the fiber's context binds `identity` (D-1597): for the empty
/// context one load of the field and one test, otherwise one probe of the
/// map. It allocates no object and takes no reference, but it may raise
/// `out of memory` as [`get`] does, with nothing to release.
///
/// # Safety
///
/// `identity` is a live key identity, borrowed.
pub(crate) unsafe fn binds(me: &ExecCtx, identity: Word) -> bool {
    let map = bindings_of(me);
    // SAFETY: the fiber's map is live while it holds it; the identity is
    // forwarded from the caller.
    map != 0 && unsafe { bindings::lookup(map, identity) }.is_some()
}

/// Binds `identity` to `value` on top of the fiber's context (D-1571):
/// the fiber gets a new map, and the outer one is returned, owned (the
/// fiber's reference passes to the caller), as the static empty map when
/// the context was empty (I-F). Raises `out of memory` (D-642) with the
/// fiber's context unchanged.
///
/// # Safety
///
/// As for [`get`], `value` in place of `default`.
pub(crate) unsafe fn bind(me: &ExecCtx, identity: Word, value: Word) -> Word {
    let outer = bindings_of(me);
    // SAFETY: as in `get`; `value` is forwarded from the caller.
    let new = unsafe { bindings::bind(outer, identity, value) };
    let old = swap_bindings(me, new);
    if old == 0 { bindings::empty() } else { old }
}

/// Makes `outer`, a word [`bind`] returned, the fiber's context again,
/// with one reference added for the fiber, and releases what the fiber
/// held. Never raises.
///
/// # Safety
///
/// `outer` is the static empty map or a live bindings map, borrowed.
pub(crate) unsafe fn restore(me: &ExecCtx, outer: Word) {
    let word = if outer == bindings::empty() {
        0
    } else {
        // SAFETY: a live map, borrowed.
        unsafe { rc::dup(outer) };
        outer
    };
    drop_bindings(swap_bindings(me, word));
}

/// What a scope recorded when it opened (D-1573): a field of
/// `fiber_ops::scope::ScopeState`, which only the opening fiber touches.
/// The scope state outlives its close, so the close takes the word.
#[derive(Debug, Default)]
pub(crate) struct ScopeSaved {
    /// An owned reference to the fiber's bindings at the open, or 0.
    bindings: AtomicU64,
}

impl ScopeSaved {
    /// Records the fiber's bindings, with one reference added: when the
    /// body unwinds, the frame that held the outer map may release it in
    /// its cleanup, and this copy is then the only one. Never raises.
    pub(crate) fn save(&self, me: &ExecCtx) {
        let word = bindings_of(me);
        // SAFETY: the fiber's map, 0 or live.
        unsafe { rc::dup(word) };
        self.bindings.store(word, Ordering::Relaxed);
    }

    /// Puts the bindings recorded at the open back on the fiber and
    /// releases what it held, on every way out of the close (I-B). When
    /// the body `returned`, every swap inside it was undone, so the fiber
    /// holds the recorded map already; debug builds check that. Never
    /// raises.
    pub(crate) fn restore(&self, me: &ExecCtx, returned: bool) {
        let saved = self.bindings.swap(0, Ordering::Relaxed);
        let old = swap_bindings(me, saved);
        if cfg!(debug_assertions) && returned && old != saved {
            fatal(format_args!(
                "a scope's body returned with a context other than the one it opened with \
                 (D-1573, I-B)"
            ));
        }
        drop_bindings(old);
    }
}

/// Marks the fiber's bindings shared, before a spawn makes another fiber
/// reach them (D-072, D-705, I-E): called in step 1 of both spawn
/// kernels, before anything is reserved, since the walk can raise
/// `out of memory` (D-642, review finding MEM-1). Nothing to do for the
/// empty context; `rc::mark_shared` skips a map that is shared already.
pub(crate) fn share_bindings(me: &ExecCtx) {
    let word = bindings_of(me);
    if word != 0 {
        // SAFETY: the fiber's own map: every object it reaches with a
        // positive count was made by this fiber and is confined to it
        // (I-1); one inherited from a spawner was shared already.
        unsafe { rc::mark_shared(word) };
    }
}

/// Gives the new fiber `child` its spawner's bindings (D-1044), one
/// reference added: `sched::create_fiber`, once the fiber exists and
/// before it is queued, so nothing else knows it. The spawn kernels have
/// marked the map shared ([`share_bindings`]); debug builds check that.
pub(crate) fn inherit(spawner: &ExecCtx, child: &ExecCtx) {
    let word = bindings_of(spawner);
    if word != 0 {
        // SAFETY: the spawner's live map, which it holds while it spawns.
        if cfg!(debug_assertions) && unsafe { load_rc(word_to_ptr(word)) } > 0 {
            fatal(format_args!(
                "a fiber was spawned with bindings that are not shared (D-705, I-E)"
            ));
        }
        // SAFETY: as above; a shared map is counted atomically.
        unsafe { rc::dup(word) };
    }
    drop_bindings(swap_bindings(child, word));
}

/// Marks the fiber as run by a base frame that calls [`release`] itself,
/// which [`release_at_exit`] then checks.
pub(crate) fn set_based(me: &ExecCtx) {
    me.context.based.store(true, Ordering::Relaxed);
}

/// Releases the fiber's bindings (I-A): its base frame calls it while the
/// fiber is current, before its last touch of shared state, so the
/// objects are counted in the fiber's domain before its owner counts it,
/// and a teardown's rule about drop functions is read from the fiber's
/// own group (D-710). A second call does nothing. Never raises.
pub(crate) fn release(me: &ExecCtx) {
    drop_bindings(swap_bindings(me, 0));
}

/// The check at the end of a fiber of a runtime, in `fiber_start` after
/// its start function returned, while it is still current. A fiber whose
/// base frame set [`set_based`] has released its bindings already; one
/// that still holds some is a broken invariant, ended in debug builds:
/// this point is after its last touch of shared state, so its owner may
/// have counted the domain's objects already. Test fibers without a base
/// frame, and every fiber in a release build, are released here.
pub(crate) fn release_at_exit(me: &ExecCtx) {
    if bindings_of(me) == 0 {
        return;
    }
    if cfg!(debug_assertions) && me.context.based.load(Ordering::Relaxed) {
        fatal(format_args!(
            "a fiber's base frame ended with its context unreleased (D-1573, I-A)"
        ));
    }
    release(me);
}

/// `context::sys_get`, private, which `context::get` calls: the value
/// bound to the key whose identity is `identity`, or `default`, owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `identity` a live key identity
/// and `default` a live value, both borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_get(
    ctx: *mut ExecCtx,
    identity: Word,
    default: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: as above.
        unsafe { get(me, identity, default) }
    })
}

/// `context::sys_bind`, private, which `context::provide` calls before
/// `body`: binds the key whose identity is `identity` to `value` and
/// returns the outer bindings, owned, which `provide` gives back to
/// [`cheby_context_sys_restore`].
///
/// # Safety
///
/// `ctx` is the running fiber's context; `identity` a live key identity
/// and `value` a live value, both borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_bind(
    ctx: *mut ExecCtx,
    identity: Word,
    value: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: as above.
        unsafe { bind(me, identity, value) }
    })
}

/// `context::sys_restore`, private, which `context::provide` calls after
/// `body` returned: makes `outer`, which [`cheby_context_sys_bind`]
/// returned, the fiber's bindings again.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `outer` a bindings word,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_restore(ctx: *mut ExecCtx, outer: Word) {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: as above.
        unsafe { restore(me, outer) };
    });
}
