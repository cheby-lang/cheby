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
//! the pointer once the fiber exists ([`inherit`]), where a seeded stream
//! is derived for the child as well (D-1048).
//!
//! **Captured contexts.** `capture`, `with` and `empty` (D-1566, D-1568)
//! are Cheby over four more kernels. [`cheby_context_sys_capture`] copies
//! the fiber's map pointer into a `Context` object (`handles::new_context`),
//! [`cheby_context_sys_enter`] puts a captured map in place of the fiber's
//! and returns the outer one, as a bind does, and
//! [`cheby_context_sys_leave`] puts it back; [`cheby_context_sys_empty`]
//! makes the empty context.
//!
//! **The stream.** A fiber's seeded stream of `std::random` (D-1048,
//! D-1574) is three fields beside the bindings: whether a stream is set,
//! its `SplitMix64` state, and the spawns and captures since the last draw.
//! `with` and `random::with_generator` keep the outer stream in the fiber,
//! in a stack of frames ([`push_stream`], [`pop_stream`]), so that a
//! scope's close puts back the stream that was current at its open in the
//! state it has reached since (D-1573): the open records the stack's
//! depth, and the close cuts the stack back to it. A capture under a
//! stream counts as a spawn and holds the state a child would start from
//! ([`derive`](fn@derive), D-1567). `std::random`'s kernels read the
//! stream ([`stream_on`], [`stream_state`]), write a draw's advanced state
//! back ([`set_stream`], which starts the spawn count again), and push and
//! pop the frames of `with_generator` (`random.rs`). A spawn under a
//! stream takes the next spawn number once the fiber exists (D-1585), and
//! the child starts from the derived state with no frames ([`inherit`]).
//! On the paths every fiber takes, the stream halves cost little when no
//! stream or frame is set: a spawn tests the stream flag (a load and a
//! branch), a scope's open copies the frame depth (a load and a store),
//! its close compares it (two loads and a branch), and a release tests two
//! fields (two loads and a branch); the work behind each branch is out of
//! line.
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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::running;
use crate::hamt::bindings;
use crate::handles;
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
fn all() -> [Entry; 7] {
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
        (
            "cheby_context_sys_capture",
            cheby_context_sys_capture as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_context_sys_enter",
            cheby_context_sys_enter as *const u8,
            &["Word"],
            "Word",
        ),
        (
            "cheby_context_sys_leave",
            cheby_context_sys_leave as *const u8,
            &["Word"],
            "()",
        ),
        (
            "cheby_context_sys_empty",
            cheby_context_sys_empty as *const u8,
            &[],
            "Word",
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
pub(crate) const CONTEXT_KERNELS: [&str; 7] = [
    "cheby_context_sys_get",
    "cheby_context_sys_bind",
    "cheby_context_sys_restore",
    "cheby_context_sys_capture",
    "cheby_context_sys_enter",
    "cheby_context_sys_leave",
    "cheby_context_sys_empty",
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
    /// Whether a seeded stream is set (D-1574).
    stream_on: AtomicBool,
    /// The stream's `SplitMix64` state, while one is set.
    stream_state: AtomicU64,
    /// The spawns and captures since the stream's last draw (D-1048,
    /// D-1567).
    stream_spawns: AtomicU64,
    /// The outer streams of `with` and `random::with_generator`, innermost
    /// last (D-1573). Only the owning fiber locks it.
    frames: Mutex<Vec<StreamFrame>>,
    /// `frames.len()`, readable without the lock: 0 unless a `with` or a
    /// `with_generator` runs on the fiber.
    depth: AtomicUsize,
}

/// A stream that `with` or `random::with_generator` put aside: the
/// fiber's three stream fields as they were when it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StreamFrame {
    /// Whether a stream was set.
    on: bool,
    /// Its state.
    state: u64,
    /// Its spawns and captures since the last draw.
    spawns: u64,
}

impl FiberContext {
    /// The fiber's current stream.
    fn stream(&self) -> StreamFrame {
        StreamFrame {
            on: self.stream_on.load(Ordering::Relaxed),
            state: self.stream_state.load(Ordering::Relaxed),
            spawns: self.stream_spawns.load(Ordering::Relaxed),
        }
    }

    /// Makes `frame` the fiber's current stream.
    fn set_stream_frame(&self, frame: StreamFrame) {
        self.stream_on.store(frame.on, Ordering::Relaxed);
        self.stream_state.store(frame.state, Ordering::Relaxed);
        self.stream_spawns.store(frame.spawns, Ordering::Relaxed);
    }

    /// Makes the frame at `depth` the current stream and drops it and
    /// every frame above it. Nothing when the stack is not deeper.
    fn cut_streams(&self, depth: usize) {
        let mut frames = self.frames.lock();
        if let Some(&frame) = frames.get(depth) {
            self.set_stream_frame(frame);
            frames.truncate(depth);
        }
        self.depth.store(frames.len(), Ordering::Relaxed);
    }
}

/// `SplitMix64`'s output function, the mixing of a step, as
/// `std/random.cheby`'s `mix` (D-655).
const fn mix(z: u64) -> u64 {
    let mixed = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let remixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    remixed ^ (remixed >> 31)
}

/// The state that the `n`-th spawn or capture since the last draw of a
/// stream in state `state` starts from (D-1048, D-1567):
/// `state + n × 0xD1B54A32D192ED03` modulo 2⁶⁴, mixed. It equals
/// `std::random`'s private `derived` bit for bit (I-J).
pub(crate) const fn derive(state: u64, n: u64) -> u64 {
    mix(state.wrapping_add(n.wrapping_mul(0xD1B5_4A32_D192_ED03)))
}

/// The fiber's stream, `(on, state, spawns)`, and the depth of its stream
/// frames, for the tests.
#[cfg(test)]
pub(crate) fn stream_parts(me: &ExecCtx) -> ((bool, u64, u64), usize) {
    let stream = me.context.stream();
    (
        (stream.on, stream.state, stream.spawns),
        me.context.depth.load(Ordering::Relaxed),
    )
}

/// Whether the fiber has a seeded stream (D-1574): the first thing each
/// host function of `std::random` asks. One load.
pub(crate) fn stream_on(me: &ExecCtx) -> bool {
    me.context.stream_on.load(Ordering::Relaxed)
}

/// The state of the fiber's seeded stream, which a draw starts from
/// (D-1048). Meaningful only while [`stream_on`] holds. One load.
pub(crate) fn stream_state(me: &ExecCtx) -> u64 {
    me.context.stream_state.load(Ordering::Relaxed)
}

/// Stores `state`, the state a draw advanced the stream to, and starts
/// the count of spawns and captures since the last draw again (D-1048,
/// D-1567). Two stores. Called only while [`stream_on`] holds.
pub(crate) fn set_stream(me: &ExecCtx, state: u64) {
    let fields = &me.context;
    fields.stream_state.store(state, Ordering::Relaxed);
    fields.stream_spawns.store(0, Ordering::Relaxed);
}

/// Puts the fiber's current stream aside, on top of its frames, and sets
/// the stream `(on, state)` with no spawns since a draw (D-1573): `with`
/// and `random::with_generator` call it before their body. Never raises.
pub(crate) fn push_stream(me: &ExecCtx, on: bool, state: u64) {
    let fields = &me.context;
    let mut frames = fields.frames.lock();
    frames.push(fields.stream());
    fields.depth.store(frames.len(), Ordering::Relaxed);
    fields.set_stream_frame(StreamFrame {
        on,
        state,
        spawns: 0,
    });
}

/// Makes the frame that [`push_stream`] put aside the current stream again,
/// as far as it had advanced when it was put aside, after the body
/// returned. Debug builds end the process when no frame is left. Never
/// raises.
pub(crate) fn pop_stream(me: &ExecCtx) {
    let fields = &me.context;
    let mut frames = fields.frames.lock();
    match frames.pop() {
        Some(frame) => fields.set_stream_frame(frame),
        None if cfg!(debug_assertions) => fatal(format_args!(
            "a stream was put back with no stream frame left (D-1573)"
        )),
        None => {}
    }
    fields.depth.store(frames.len(), Ordering::Relaxed);
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
    /// The depth of the fiber's stream frames at the open.
    depth: AtomicUsize,
}

impl ScopeSaved {
    /// Records the fiber's bindings, with one reference added: when the
    /// body unwinds, the frame that held the outer map may release it in
    /// its cleanup, and this copy is then the only one. Also records the
    /// depth of the stream frames, a load and a store. Never raises.
    pub(crate) fn save(&self, me: &ExecCtx) {
        let word = bindings_of(me);
        // SAFETY: the fiber's map, 0 or live.
        unsafe { rc::dup(word) };
        self.bindings.store(word, Ordering::Relaxed);
        self.depth
            .store(me.context.depth.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Puts the bindings recorded at the open back on the fiber and
    /// releases what it held, on every way out of the close (I-B). When
    /// the body `returned`, every swap inside it was undone, so the fiber
    /// holds the recorded map already; debug builds check that. When the
    /// fiber has more stream frames than at the open, a `with` or a
    /// `with_generator` inside the body unwound: the frame at the recorded
    /// depth, which holds the stream current when the first of them
    /// started, becomes the current stream again, in the state it had
    /// reached, and the frames above it go (D-1573). With no frame above
    /// the recorded depth, this half is a load, a load and a test. Never
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
        let depth = self.depth.load(Ordering::Relaxed);
        if me.context.depth.load(Ordering::Relaxed) > depth {
            cut_streams_at_close(me, depth, returned);
        }
    }
}

/// The stream half of a scope's close when a `with` or a
/// `with_generator` inside the body unwound ([`ScopeSaved::restore`]).
#[cold]
#[inline(never)]
fn cut_streams_at_close(me: &ExecCtx, depth: usize, returned: bool) {
    if cfg!(debug_assertions) && returned {
        fatal(format_args!(
            "a scope's body returned with more stream frames than it opened with \
             (D-1573, I-B)"
        ));
    }
    me.context.cut_streams(depth);
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
/// reference added, and, under a seeded stream, a stream of its own
/// ([`inherit_stream`]): `sched::create_fiber`, once the fiber exists and
/// before it is queued, so nothing else knows it, and a spawn that fails
/// takes no spawn number (D-1585). The spawn kernels have marked the map
/// shared ([`share_bindings`]); debug builds check that. With no stream,
/// the stream half is one load of the spawner's flag and a branch.
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
    if spawner.context.stream_on.load(Ordering::Relaxed) {
        inherit_stream(spawner, child);
    }
}

/// The stream half of [`inherit`] under a seeded stream (D-1048): the
/// spawn takes the spawner's next spawn number `n`, and the child's
/// stream starts from `derive(state, n)` with no spawns and no frames,
/// while the spawner's state stays where it was. The spawner is the
/// running fiber, so it writes its own fields; the child is not queued
/// yet and has no frames, being new.
#[cold]
#[inline(never)]
fn inherit_stream(spawner: &ExecCtx, child: &ExecCtx) {
    let fields = &spawner.context;
    let stream = fields.stream();
    let n = stream.spawns.wrapping_add(1);
    fields.stream_spawns.store(n, Ordering::Relaxed);
    child.context.set_stream_frame(StreamFrame {
        on: true,
        state: derive(stream.state, n),
        spawns: 0,
    });
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
/// own group (D-710). It also clears the stream and its frames, which a
/// base frame that unwound out of `with` or `with_generator` leaves set:
/// one test of two fields when there are none. A second call does
/// nothing. Never raises.
pub(crate) fn release(me: &ExecCtx) {
    drop_bindings(swap_bindings(me, 0));
    let fields = &me.context;
    if fields.stream_on.load(Ordering::Relaxed) || fields.depth.load(Ordering::Relaxed) != 0 {
        clear_streams(me);
    }
}

/// The stream half of [`release`] when a stream or a frame is set.
#[cold]
#[inline(never)]
fn clear_streams(me: &ExecCtx) {
    let fields = &me.context;
    let mut frames = fields.frames.lock();
    frames.clear();
    fields.depth.store(0, Ordering::Relaxed);
    fields.set_stream_frame(StreamFrame::default());
}

/// The fiber's whole context as a value (D-1566): a `Context` that holds
/// the fiber's map, and, while a seeded stream is set, the state a fiber
/// spawned here would start from (D-1567). The capture then takes the
/// next spawn number and leaves the stream's state alone. The object is
/// allocated first, which may raise `out of memory` (D-642), before the
/// map gains a reference and before the count moves.
pub(crate) fn capture(me: &ExecCtx) -> Word {
    let fields = &me.context;
    let stream = fields.stream();
    let n = stream.spawns.wrapping_add(1);
    let held = stream.on.then(|| derive(stream.state, n));
    // SAFETY: the fiber's map, 0 or live while it holds it.
    let word = unsafe { handles::new_context(bindings_of(me), held) };
    if stream.on {
        fields.stream_spawns.store(n, Ordering::Relaxed);
    }
    word
}

/// Runs the fiber under `captured` (D-1566, D-1567): its stream is put
/// aside ([`push_stream`]) and the captured one set, or none, and the
/// captured map replaces the fiber's bindings, with one reference added.
/// The outer bindings are returned, owned, as [`bind`] returns them, for
/// [`leave`]. Never raises.
///
/// # Safety
///
/// `captured` is a live captured context, borrowed.
pub(crate) unsafe fn enter(me: &ExecCtx, captured: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (map, stream) = unsafe { handles::context_parts(captured) };
    push_stream(me, stream.is_some(), stream.unwrap_or(0));
    // SAFETY: the context's map, 0 or live while the caller holds it.
    unsafe { rc::dup(map) };
    let old = swap_bindings(me, map);
    if old == 0 { bindings::empty() } else { old }
}

/// Undoes [`enter`] after the body returned: `outer`, which it returned,
/// is the fiber's context again, as [`restore`] makes it, and the stream
/// put aside is current again. Never raises.
///
/// # Safety
///
/// As for [`restore`].
pub(crate) unsafe fn leave(me: &ExecCtx, outer: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { restore(me, outer) };
    pop_stream(me);
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

/// `context::sys_capture`, private, which `context::capture` calls: the
/// calling fiber's context as a `Context`, owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_capture(ctx: *mut ExecCtx) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        capture(me)
    })
}

/// `context::sys_enter`, private, which `context::with` calls before
/// `body`: puts `captured` in place of the fiber's context and returns the
/// outer bindings, owned, which `with` gives back to
/// [`cheby_context_sys_leave`].
///
/// # Safety
///
/// `ctx` is the running fiber's context and `captured` a live `Context`,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_enter(ctx: *mut ExecCtx, captured: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: as above.
        unsafe { enter(me, captured) }
    })
}

/// `context::sys_leave`, private, which `context::with` calls after
/// `body` returned: makes `outer`, which [`cheby_context_sys_enter`]
/// returned, the fiber's bindings again, and its stream the one it had.
///
/// # Safety
///
/// `ctx` is the running fiber's context and `outer` a bindings word,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_leave(ctx: *mut ExecCtx, outer: Word) {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        // SAFETY: as above.
        unsafe { leave(me, outer) };
    });
}

/// `context::sys_empty`, private, which `context::empty` calls: the
/// context in which every key has its default and no stream is set
/// (D-1568), owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_context_sys_empty(ctx: *mut ExecCtx) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: 0 is the empty map.
        unsafe { handles::new_context(0, None) }
    })
}
