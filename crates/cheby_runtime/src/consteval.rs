//! The runtime side of constant evaluation (E6, plan §7, D-535 to D-539).
//!
//! The host (`cheby_consteval`) evaluates each constant on an entry of its
//! own (D-457) and stores the result in the constant's `cb$const$` slot.
//! This module holds what generated code and the host share:
//!
//! - [`ConstTable`]: every constant of the build by slot address, with its
//!   initializer's entry thunk, its hash seed (D-538) and its state, and
//!   the stack of constants in progress. The host registers the constants
//!   and passes the table to each constant entry ([`EntryConfig::consts`]).
//! - `cheby_rt_const_missing(ctx, slot)`: what a `LoadConst` calls when
//!   it finds its slot empty. During constant evaluation it runs that
//!   constant's initializer on the current entry under the constant's own
//!   seed ([`entry::with_seed`](crate::entry::with_seed)), stores the
//!   result in the slot and returns it borrowed (lazy forcing, D-535). A
//!   constant met while in progress panics `ConstCycle` (E0069), and one
//!   that already failed ends the reader too, without a report of its own.
//!   Outside constant evaluation an empty slot is a fatal error.
//! - `cheby_rt_effect_check(ctx, symbol)`: the start of the wrapper of
//!   every external that constant evaluation may not run (D-537); panics
//!   `ForbiddenEffect` during constant evaluation.
//! - `cheby_rt_unresolved()`: bound to every unresolved native symbol of a
//!   `cheby check` build (D-532); panics `ForbiddenEffect`.
//! - [`const_seed`] and [`source_hash`]: a constant's D-269 seed (D-538).
//!
//! - The limits (D-536): an entry that runs out of time or memory records
//!   its [`Limit`] in the table before it is abandoned
//!   ([`Abandoned::limit`]), so the host reports E0076 rather than a panic.
//! - [`rebuild_collections`]: the rebuild of every `Map` and `Set` in the
//!   results with the per-process seed before code runs (D-196, D-539).
//! - [`find_out_of_js_range`]: the JS range check of a result (E0078,
//!   D-540), walking the value by its type.
//!
//! Freezing results in place is [`crate::alloc::freeze`] (D-539).
//!
//! [`EntryConfig::consts`]: crate::entry::EntryConfig::consts

use std::collections::{HashMap, HashSet};
use std::hash::Hasher as _;
use std::sync::{Arc, Mutex, PoisonError};

use siphasher::sip::SipHasher13;

use crate::abi::{self, PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::entry::{self, EntryFn};
use crate::layout::{self, FieldKind, LayoutKind};
use crate::object::{load_rc, read_field, word_to_ptr};
use crate::panic::{cheby_rt_panic, fatal, panic_current};
use crate::{alloc, hamt, list, values};

/// The kernels this module registers: none.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose kernels this module registers completely: none.
pub(crate) const COMPLETE: &[&str] = &[];

/// Where a constant's evaluation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstState {
    /// Not evaluated yet: its slot is empty.
    Pending,
    /// Its initializer is running, at the top level or forced.
    InProgress,
    /// Evaluated: its slot holds the value.
    Done,
    /// Its evaluation panicked, directly or in a constant it forced.
    Failed,
}

/// One constant of the build.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The constant's entry thunk (`EntryKind::Const`), which returns the
    /// owned value.
    thunk: EntryFn,
    seed: u64,
    state: ConstState,
}

#[derive(Debug, Default)]
struct Table {
    /// By slot address.
    entries: HashMap<usize, Entry>,
    /// The constants in progress, outermost first.
    stack: Vec<usize>,
    /// The constant a cycle met, until [`ConstTable::abandon`].
    cycle: Option<usize>,
    /// The failed constant a reader met, until [`ConstTable::abandon`].
    failed: Option<usize>,
    /// The limit that ended the running entry, until
    /// [`ConstTable::abandon`].
    limit: Option<Limit>,
}

/// A limit of constant evaluation (D-195, D-536), reported as E0076.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The wall-clock time of one top-level evaluation.
    Time,
    /// The live bytes of the build while a constant is evaluated.
    Memory,
}

impl Limit {
    /// The message of the (captured, never printed) panic report that
    /// abandons the entry.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Time => "constant evaluation exceeded its time limit",
            Self::Memory => "constant evaluation exceeded its memory limit",
        }
    }
}

/// What a panicked evaluation left in progress ([`ConstTable::abandon`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Abandoned {
    /// The slots of the constants in progress when the entry panicked,
    /// outermost first: the last one's initializer panicked.
    pub stack: Vec<*mut u64>,
    /// The constant whose read closed a cycle (`ConstCycle`).
    pub cycle: Option<*mut u64>,
    /// The already failed constant a reader met: the panic only ends the
    /// reader, and is not reported again.
    pub failed: Option<*mut u64>,
    /// The limit that ended the entry (E0076), if one did.
    pub limit: Option<Limit>,
}

/// The constants of one build, shared by the host and the constant
/// entries that run on their own threads.
#[derive(Debug, Default)]
pub struct ConstTable {
    table: Mutex<Table>,
}

impl ConstTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the constant whose slot is `slot`, with its entry thunk
    /// and its hash seed (D-538), as [`ConstState::Pending`].
    ///
    /// # Safety
    ///
    /// `slot` is the constant's 8-byte writable slot and `thunk` its
    /// constant entry thunk (C ABI, `ctx` in, the owned value out), both
    /// valid for as long as any entry that gets this table runs.
    pub unsafe fn register(&self, slot: *mut u64, thunk: EntryFn, seed: u64) {
        self.lock().entries.insert(
            slot.addr(),
            Entry {
                thunk,
                seed,
                state: ConstState::Pending,
            },
        );
    }

    /// The state of the constant at `slot`, if it is registered.
    #[must_use]
    pub fn state(&self, slot: *mut u64) -> Option<ConstState> {
        self.lock().entries.get(&slot.addr()).map(|e| e.state)
    }

    /// Marks the constant at `slot` in progress, as the host starts its
    /// evaluation at the top level. Returns `false`, and changes nothing,
    /// unless it is registered and [`ConstState::Pending`].
    pub fn begin(&self, slot: *mut u64) -> bool {
        let mut table = self.lock();
        match table.entries.get_mut(&slot.addr()) {
            Some(e) if e.state == ConstState::Pending => {
                e.state = ConstState::InProgress;
                table.stack.push(slot.addr());
                true
            }
            _ => false,
        }
    }

    /// Marks the constant at `slot` evaluated: its slot holds its value.
    pub fn finish(&self, slot: *mut u64) {
        let mut table = self.lock();
        if let Some(e) = table.entries.get_mut(&slot.addr()) {
            e.state = ConstState::Done;
        }
        if table.stack.last() == Some(&slot.addr()) {
            table.stack.pop();
        }
    }

    /// After an entry panicked: marks every constant in progress failed and
    /// returns them, with the constant a cycle met or the failed constant a
    /// reader met.
    pub fn abandon(&self) -> Abandoned {
        let mut table = self.lock();
        let stack = std::mem::take(&mut table.stack);
        for slot in &stack {
            if let Some(e) = table.entries.get_mut(slot) {
                e.state = ConstState::Failed;
            }
        }
        Abandoned {
            stack: stack
                .into_iter()
                .map(std::ptr::with_exposed_provenance_mut)
                .collect(),
            cycle: table
                .cycle
                .take()
                .map(std::ptr::with_exposed_provenance_mut),
            failed: table
                .failed
                .take()
                .map(std::ptr::with_exposed_provenance_mut),
            limit: table.limit.take(),
        }
    }

    /// Records that `limit` ends the running entry.
    fn exceed(&self, limit: Limit) {
        self.lock().limit = Some(limit);
    }

    /// Starts forcing the constant at `slot`: its thunk and seed if it is
    /// pending (now in progress), or what to do instead.
    fn force(&self, slot: usize) -> Forced {
        let mut table = self.lock();
        let Some(e) = table.entries.get_mut(&slot) else {
            return Forced::Unknown;
        };
        match e.state {
            ConstState::Pending => {
                e.state = ConstState::InProgress;
                let (thunk, seed) = (e.thunk, e.seed);
                table.stack.push(slot);
                Forced::Run { thunk, seed }
            }
            ConstState::InProgress => {
                table.cycle = Some(slot);
                Forced::Cycle
            }
            ConstState::Failed => {
                table.failed = Some(slot);
                Forced::Failed
            }
            ConstState::Done => Forced::Done,
        }
    }
}

/// Ends the constant evaluation running on this thread because it
/// exceeded `limit` (D-536): records the limit in the build's table, then
/// abandons the entry like a panic, whose report the host never prints.
/// Called by the stack check's slow path (time) and by allocation
/// (memory).
pub(crate) fn limit_exceeded(limit: Limit) -> ! {
    if let Some(table) = entry::current_consts() {
        table.exceed(limit);
    }
    panic_current(PanicKind::Explicit, Some(limit.message()))
}

/// What [`ConstTable::force`] found.
enum Forced {
    Run { thunk: EntryFn, seed: u64 },
    Cycle,
    Failed,
    Done,
    Unknown,
}

/// `cheby_rt_effect_check(ctx, symbol)`: called at the start of the
/// wrapper of every external that constant evaluation may not run (plan
/// §7.4). During constant evaluation it panics `ForbiddenEffect` with the
/// symbol's name as the (internal) message, which constant evaluation turns
/// into E0077; otherwise it does nothing.
///
/// # Safety
///
/// `ctx` is the running entry's context, and `symbol` a borrowed `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_effect_check(ctx: *mut ExecCtx, symbol: Word) {
    // SAFETY: forwarded from the caller.
    let Some(context) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "an effect check ran without an execution context"
        ));
    };
    let mode = context.mode.load(std::sync::atomic::Ordering::Relaxed);
    if mode & abi::CTX_MODE_CONST_EVAL != 0 {
        // SAFETY: forwarded from the caller.
        unsafe { cheby_rt_panic(ctx, PanicKind::ForbiddenEffect.code(), symbol, 0) }
    }
}

/// `cheby_rt_const_missing(ctx, slot)`: called by a `LoadConst` that found
/// the constant's slot empty. During constant evaluation it forces the
/// constant (module docs) and returns its value borrowed from the slot;
/// outside constant evaluation, or for a slot the build did not register,
/// it is a fatal error.
///
/// # Safety
///
/// `ctx` is the running entry's context and `slot` the address of a
/// constant's slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_const_missing(ctx: *mut ExecCtx, slot: u64) -> Word {
    let Some(table) = entry::current_consts() else {
        fatal(format_args!(
            "the constant slot at {slot:#x} is empty outside constant evaluation"
        ));
    };
    let Ok(address) = usize::try_from(slot) else {
        fatal(format_args!(
            "the constant slot {slot:#x} is not an address"
        ));
    };
    let slot_ptr: *mut u64 = std::ptr::with_exposed_provenance_mut(address);
    match table.force(address) {
        Forced::Run { thunk, seed } => {
            // SAFETY: the host registered `thunk` as this constant's entry
            // thunk, valid while the entry runs; `ctx` is the running
            // entry's, so the initializer runs on the current entry, its
            // stack checks included.
            let word = entry::with_seed(seed, || unsafe { thunk(ctx) });
            // SAFETY: the registered slot is an 8-aligned writable word of
            // the JIT's data, and only this entry's thread evaluates.
            unsafe { slot_ptr.write(word) };
            table.finish(slot_ptr);
            word
        }
        // SAFETY: as above; a done constant's slot holds its value.
        Forced::Done => unsafe { slot_ptr.read() },
        Forced::Cycle => panic_current(PanicKind::ConstCycle, None),
        Forced::Failed => panic_current(
            PanicKind::ConstCycle,
            Some("a constant this initializer reads failed to evaluate"),
        ),
        Forced::Unknown => fatal(format_args!(
            "the constant slot at {slot:#x} belongs to no constant of this build"
        )),
    }
}

/// The panic message of [`cheby_rt_unresolved`], by which constant
/// evaluation tells a missing native symbol from a forbidden external.
pub const UNRESOLVED_MESSAGE: &str =
    "constant evaluation called a native function this build cannot resolve";

/// `cheby_rt_unresolved()`: the code every unresolved native symbol of a
/// `cheby check` build is bound to (D-532). It takes no `ctx`, like every
/// foreign function, ignores whatever arguments it was called with, and
/// panics the running entry with `ForbiddenEffect`.
///
/// # Safety
///
/// Called from generated code on an entry's thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_unresolved() -> Word {
    panic_current(PanicKind::ForbiddenEffect, Some(UNRESOLVED_MESSAGE))
}

/// Rebuilds every `Map` and `Set` that `roots` reach with `seed`, the
/// per-process seed, before code reads them (D-196, D-539).
///
/// One memoized walk covers all roots together: each object is visited
/// once, whichever roots share it, and a collection is rebuilt after
/// everything it holds (post-order), so a `Set` used as a `Map` key is
/// rebuilt before the map. Each collection is rebuilt in place
/// ([`hamt::reseed_in_place`]): its root object keeps its address and
/// gets the new trie, so every parent field and slot that pointed to it
/// points to the rebuilt collection, with no count to adjust. Static
/// objects (frozen results of earlier batches, constructor statics) are
/// skipped. The walk is a worklist on the heap (plan §0, deep data).
///
/// Run it on an entry ([`entry::run_task`]): hashing a key may call a
/// type's `@std_hooks` closure.
///
/// # Safety
///
/// Every root is a tagged integer, 0 or a live object; no other thread
/// uses the objects they reach; and the current domain is the one they
/// are charged to.
pub unsafe fn rebuild_collections(roots: &[Word], seed: u64) {
    let mut work: Vec<(Word, bool)> = roots.iter().map(|&word| (word, false)).collect();
    work.reverse();
    let mut visited: HashSet<Word> = HashSet::new();
    while let Some((word, children_done)) = work.pop() {
        if children_done {
            // SAFETY: a live object reached from the roots, visited once.
            if unsafe { hamt::collection_of(word) }.is_some() {
                // SAFETY: as above; no other reference is in use.
                unsafe { hamt::reseed_in_place(word, seed) };
            }
            continue;
        }
        if abi::is_tagged_int(word) || word == 0 {
            continue;
        }
        // SAFETY: a live object reached from the roots. In poisoning mode,
        // a freed one ends the process.
        unsafe { alloc::check_live(word, "rebuilding a constant") };
        // SAFETY: as above.
        if unsafe { load_rc(word_to_ptr(word)) } == abi::RC_STATIC || !visited.insert(word) {
            continue;
        }
        work.push((word, true));
        let first_child = work.len();
        // SAFETY: as above.
        unsafe { alloc::for_each_child(word, &mut |child| work.push((child, false))) };
        // Children in their own order, first child on top.
        if let Some(children) = work.get_mut(first_child..) {
            children.reverse();
        }
    }
}

/// The largest `Int` magnitude JS represents exactly, 2^53 - 1 (D-037).
pub const JS_MAX_SAFE_INT: i64 = (1 << 53) - 1;

/// A type as the JS range check of constants sees it (E0078, D-540): only
/// what may hold an `Int` matters. Nodes refer to each other by index into
/// one slice, so recursive types stay finite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeNode {
    /// Holds no `Int`: other scalars, strings, bytes, `Nil`, `Bool`.
    Opaque,
    /// `Int` (never `I64`, which JS represents exactly as a `BigInt`).
    Int,
    /// A tuple, by element.
    Tuple(Box<[usize]>),
    /// `List<T>`.
    List(usize),
    /// `Map<K, V>`.
    Map(usize, usize),
    /// `Set<T>`.
    Set(usize),
    /// A type with constructors: for each constructor, by tag, the nodes of
    /// its fields in declaration order (with the type's arguments in
    /// place).
    Adt(Box<[Box<[usize]>]>),
}

/// The first `Int` outside ±[`JS_MAX_SAFE_INT`] in `value`, a value of the
/// type `nodes[root]`, or `None`. Raw `Int` fields of constructors are
/// checked through the layout table, whose field kinds tell `Int` from the
/// sized integers. Each (object, type) pair is visited once, with a
/// worklist on the heap (plan §0, deep data).
///
/// # Safety
///
/// `value` is a live value of the type `nodes` describes (a constant's
/// evaluated result), not changed during the call.
#[must_use]
pub unsafe fn find_out_of_js_range(value: Word, nodes: &[RangeNode], root: usize) -> Option<i64> {
    let out = |n: i64| n.unsigned_abs() > JS_MAX_SAFE_INT.unsigned_abs();
    let mut work: Vec<(Word, usize)> = vec![(value, root)];
    let mut seen: HashSet<(Word, usize)> = HashSet::new();
    while let Some((word, node)) = work.pop() {
        let Some(node_kind) = nodes.get(node) else {
            fatal(format_args!("the range node {node} does not exist"));
        };
        if matches!(node_kind, RangeNode::Opaque) {
            continue;
        }
        if let RangeNode::Int = node_kind {
            // SAFETY: an `Int` of the caller's value.
            let n = unsafe { values::word_to_i64(word) };
            if out(n) {
                return Some(n);
            }
            continue;
        }
        if abi::is_tagged_int(word) || word == 0 || !seen.insert((word, node)) {
            continue;
        }
        match node_kind {
            RangeNode::Opaque | RangeNode::Int => {}
            RangeNode::Tuple(parts) => {
                // SAFETY: a tuple of the caller's value.
                let elements = unsafe { values::read_tuple(word) };
                work.extend(elements.into_iter().zip(parts.iter().copied()));
            }
            RangeNode::List(element) => {
                // SAFETY: a list of the caller's value; the elements stay
                // alive with it.
                for item in unsafe { list::iter(word) } {
                    work.push((item, *element));
                }
            }
            RangeNode::Map(key, value) => {
                // SAFETY: a map of the caller's value.
                unsafe {
                    hamt::for_each_entry(word, &mut |k, v| {
                        work.push((k, *key));
                        if let Some(v) = v {
                            work.push((v, *value));
                        }
                    });
                }
            }
            RangeNode::Set(element) => {
                // SAFETY: a set of the caller's value.
                unsafe { hamt::for_each_entry(word, &mut |k, _| work.push((k, *element))) };
            }
            RangeNode::Adt(ctors) => {
                // SAFETY: a constructor value of the caller's value.
                let id = unsafe { values::layout_of(word) };
                let Some(info) = id.and_then(layout::get) else {
                    fatal(format_args!("{word:#x} is not a constructor value"));
                };
                let LayoutKind::Ctor { tag, .. } = info.kind else {
                    fatal(format_args!("{word:#x} is not a constructor value"));
                };
                let fields = usize::try_from(tag).ok().and_then(|tag| ctors.get(tag));
                let Some(fields) = fields else {
                    fatal(format_args!("the constructor {tag} has no range nodes"));
                };
                let obj = word_to_ptr(word);
                for (field, &field_node) in info.fields.iter().zip(fields.iter()) {
                    let offset = usize::try_from(field.offset).unwrap_or(usize::MAX);
                    // SAFETY: the layout's field lies within the object.
                    let raw = unsafe { read_field(obj, offset) };
                    match field.kind {
                        FieldKind::Value => work.push((raw, field_node)),
                        FieldKind::Int => {
                            let n = raw.cast_signed();
                            if out(n) {
                                return Some(n);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    None
}

/// The SipHash-1-3 with a zero key of a module's source bytes, its part of
/// a constant's seed (D-538).
#[must_use]
pub fn source_hash(bytes: &[u8]) -> u64 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    h.write(bytes);
    h.finish()
}

/// A constant's hash seed (D-269, D-538):
/// SipHash-1-3 with a zero key over the package name, the module path and
/// the constant's name, then, for each module the initializer reaches in
/// the order given (sorted by module path), the module path and the
/// [`source_hash`] of its source. Each text is followed by a `0xff` byte,
/// which UTF-8 never contains, so the fields cannot run together.
#[must_use]
pub fn const_seed(package: &str, module_path: &str, name: &str, deps: &[(&str, u64)]) -> u64 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    for text in [package, module_path, name] {
        h.write(text.as_bytes());
        h.write_u8(0xff);
    }
    for (path, source) in deps {
        h.write(path.as_bytes());
        h.write_u8(0xff);
        h.write_u64(*source);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::entry::{ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome, run_entry};
    use crate::string::tests::StaticImage;

    unsafe extern "C" fn checks_an_effect(ctx: *mut ExecCtx) -> u64 {
        thread_local! {
            static SYMBOL: StaticImage = StaticImage::new("cheby_io_println");
        }
        // SAFETY: the running entry's context and a live string.
        unsafe { cheby_rt_effect_check(ctx, SYMBOL.with(StaticImage::word)) };
        5
    }

    unsafe extern "C" fn calls_unresolved(_: *mut ExecCtx) -> u64 {
        // SAFETY: called on an entry's thread.
        unsafe { cheby_rt_unresolved() }
    }

    fn run(entry: crate::entry::EntryFn, mode: EntryMode) -> EntryOutcome {
        // SAFETY: test thunks with the `EntryFn` ABI and static code.
        unsafe { run_entry(entry, &EntryConfig::new(mode)) }.unwrap()
    }

    fn const_eval() -> EntryMode {
        EntryMode::ConstEval(ConstEvalLimits::default())
    }

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
    fn effects_panic_only_during_constant_evaluation() {
        assert_eq!(
            run(checks_an_effect, EntryMode::Run),
            EntryOutcome::Returned(5)
        );
        let EntryOutcome::Panicked(report) = run(checks_an_effect, const_eval()) else {
            panic!("the effect check did not panic");
        };
        assert_eq!(report.kind, PanicKind::ForbiddenEffect);
        assert_eq!(report.message, "cheby_io_println");
    }

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
    fn unresolved_symbols_panic_forbidden_effect() {
        let EntryOutcome::Panicked(report) = run(calls_unresolved, const_eval()) else {
            panic!("the stub returned");
        };
        assert_eq!(report.kind, PanicKind::ForbiddenEffect);
        assert_eq!(report.location, crate::panic::UNKNOWN_LOCATION);
    }

    // Three constants by slot: A = B + 1 (reads B), B = 41 (records the
    // seed it ran under), C = C (reads itself). D reads A.
    static SLOT_A: AtomicU64 = AtomicU64::new(0);
    static SLOT_B: AtomicU64 = AtomicU64::new(0);
    static SLOT_C: AtomicU64 = AtomicU64::new(0);
    static SEED_IN_B: AtomicU64 = AtomicU64::new(0);

    fn slot(s: &AtomicU64) -> *mut u64 {
        s.as_ptr()
    }

    /// What a `LoadConst` does.
    unsafe fn load(ctx: *mut ExecCtx, s: &AtomicU64) -> u64 {
        let word = s.load(Ordering::Relaxed);
        if word != 0 {
            return word;
        }
        // SAFETY: the running entry's context and a registered slot.
        unsafe { cheby_rt_const_missing(ctx, u64::try_from(slot(s).expose_provenance()).unwrap()) }
    }

    unsafe extern "C" fn init_a(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the running entry's context.
        let b = unsafe { load(ctx, &SLOT_B) };
        let seed_here = entry::current_seed();
        // The reader's seed is back after forcing.
        assert_eq!(seed_here, 1);
        abi::tag_int(abi::untag_int(b).wrapping_add(1)).unwrap_or(0)
    }

    unsafe extern "C" fn init_b(_: *mut ExecCtx) -> u64 {
        SEED_IN_B.store(entry::current_seed(), Ordering::Relaxed);
        abi::tag_int(41).unwrap_or(0)
    }

    unsafe extern "C" fn init_c(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the running entry's context.
        unsafe { load(ctx, &SLOT_C) }
    }

    fn eval(table: &Arc<ConstTable>, thunk: EntryFn, seed: u64) -> EntryOutcome {
        let config = EntryConfig {
            consts: Some(Arc::clone(table)),
            ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
                seed,
                ..ConstEvalLimits::default()
            }))
        };
        // SAFETY: test thunks with the `EntryFn` ABI and static code.
        unsafe { run_entry(thunk, &config) }.unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
    fn forcing_evaluates_once_under_the_forced_seed_and_finds_cycles() {
        let table = ConstTable::new();
        // SAFETY: static slots and static thunks.
        unsafe {
            table.register(slot(&SLOT_A), init_a, 1);
            table.register(slot(&SLOT_B), init_b, 2);
            table.register(slot(&SLOT_C), init_c, 3);
        }
        // A first: it forces B under B's seed.
        assert!(table.begin(slot(&SLOT_A)));
        let EntryOutcome::Returned(a) = eval(&table, init_a, 1) else {
            panic!("A panicked");
        };
        SLOT_A.store(a, Ordering::Relaxed);
        table.finish(slot(&SLOT_A));
        assert_eq!(abi::untag_int(a), 42);
        assert_eq!(SEED_IN_B.load(Ordering::Relaxed), 2);
        assert_eq!(abi::untag_int(SLOT_B.load(Ordering::Relaxed)), 41);
        assert_eq!(table.state(slot(&SLOT_B)), Some(ConstState::Done));
        // B is done, so it is not evaluated again.
        assert!(!table.begin(slot(&SLOT_B)));
        // C reads itself.
        assert!(table.begin(slot(&SLOT_C)));
        let EntryOutcome::Panicked(report) = eval(&table, init_c, 3) else {
            panic!("C did not panic");
        };
        assert_eq!(report.kind, PanicKind::ConstCycle);
        let abandoned = table.abandon();
        assert_eq!(abandoned.stack, [slot(&SLOT_C)]);
        assert_eq!(abandoned.cycle, Some(slot(&SLOT_C)));
        assert_eq!(table.state(slot(&SLOT_C)), Some(ConstState::Failed));
    }

    fn set_of(items: &[i64], seed: u64) -> Word {
        let mut set = hamt::cheby_set_new();
        for &item in items {
            // SAFETY: a live set, replaced by the kernel's owned result.
            let next = unsafe { hamt::cheby_set_insert(set, values::int_word(item)) };
            // SAFETY: the previous version, owned here.
            unsafe { crate::rc::drop(set) };
            set = next;
        }
        // SAFETY: an owned set, given up for the rebuilt one.
        unsafe { hamt::rebuild_with_seed(set, seed) }
    }

    /// D-196, D-539: one walk over all results rebuilds each collection
    /// once, inner ones first, in place, so two constants sharing a map of
    /// sets both see it rebuilt and lookups by value still work.
    #[test]
    fn rebuilding_reseeds_shared_and_nested_collections_once() {
        let domain = alloc::Domain::new();
        alloc::with_domain(Arc::clone(&domain), || {
            let key = set_of(&[1, 2, 3], 0xA);
            let mut map = hamt::cheby_map_new();
            for (k, v) in [(key, 10), (set_of(&[4], 0xA), 20)] {
                // SAFETY: live words; the kernel dups what it keeps.
                let next = unsafe { hamt::cheby_map_insert(map, k, values::int_word(v)) };
                // SAFETY: owned words given up.
                unsafe {
                    crate::rc::drop(map);
                    crate::rc::drop(k);
                }
                map = next;
            }
            // SAFETY: an owned map, given up for the rebuilt one.
            map = unsafe { hamt::rebuild_with_seed(map, 0xA) };
            // Two "constants": a tuple holding the map, and the map itself.
            // SAFETY: a live map, shared.
            unsafe { crate::rc::dup(map) };
            let first = values::tuple(&[map, values::int_word(1)]);
            let second = map;
            let live = domain.live_objects();
            // SAFETY: live results of this thread.
            unsafe {
                rebuild_collections(&[first, second, abi::tag_int(5).unwrap_or(0)], 0xB);
                assert_eq!(hamt::seed_of(second), Some(0xB));
                assert_eq!(values::read_tuple(first).first().copied(), Some(second));
                let mut keys = Vec::new();
                hamt::for_each_entry(second, &mut |k, _| keys.push(k));
                for k in keys {
                    assert_eq!(hamt::seed_of(k), Some(0xB));
                }
                let probe = set_of(&[3, 2, 1], 0xC);
                let found = hamt::cheby_map_get(second, probe);
                assert_eq!(values::read_option(found), Some(values::int_word(10)));
                crate::rc::drop(found);
                crate::rc::drop(probe);
                // The old tries were freed: the same number of objects.
                assert_eq!(domain.live_objects(), live);
                crate::rc::drop(first);
                crate::rc::drop(second);
            }
            assert_eq!(domain.live_objects(), 0);
        });
    }

    /// Plan §0 (deep data): a million-deep result is rebuilt, range-checked
    /// and frozen on a small stack.
    #[test]
    fn deep_results_need_no_deep_stack() {
        let worker = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(|| {
                let domain = alloc::Domain::new();
                alloc::with_domain(Arc::clone(&domain), || {
                    // (((… (big, set) …, 1), 1), 1): a node is a tuple of a
                    // node and an `Int`; the bottom holds a set and an
                    // `Int` outside the JS range.
                    let set = set_of(&[1, 2], 0xA);
                    let mut chain = values::tuple(&[set, values::int_word(1 << 60)]);
                    for _ in 0..1_000_000 {
                        chain = values::tuple(&[chain, values::int_word(1)]);
                    }
                    let nodes = [RangeNode::Tuple(Box::new([0, 1])), RangeNode::Int];
                    // SAFETY: live values of this thread.
                    unsafe {
                        rebuild_collections(&[chain], 0xB);
                        assert_eq!(hamt::seed_of(set), Some(0xB));
                        assert_eq!(find_out_of_js_range(chain, &nodes, 0), Some(1 << 60));
                        alloc::freeze(chain);
                    }
                    assert_eq!(domain.live_objects(), 0);
                });
            })
            .unwrap();
        worker.join().unwrap();
    }

    /// E0078, D-540: only `Int`s are checked, wherever the type puts them:
    /// raw constructor fields, tuples, lists, map keys and values, set
    /// elements; `I64` and other words are not.
    #[test]
    fn the_range_walk_follows_the_type() {
        let big = (1_i64 << 53) + 1;
        let domain = alloc::Domain::new();
        alloc::with_domain(Arc::clone(&domain), || {
            // SAFETY: live values of this thread.
            unsafe {
                // Nodes: 0 Int, 1 Opaque, 2 List<Int>, 3 Map<Int, Int>,
                // 4 Set<Int>, 5 (Int, Int), 6 Option<Int>.
                let nodes = [
                    RangeNode::Int,
                    RangeNode::Opaque,
                    RangeNode::List(0),
                    RangeNode::Map(0, 0),
                    RangeNode::Set(0),
                    RangeNode::Tuple(Box::new([1, 0])),
                    // `Some` is tag 0, `None` tag 1.
                    RangeNode::Adt(Box::new([Box::new([0]), Box::new([])])),
                ];
                let safe = values::int_word(JS_MAX_SAFE_INT);
                let unsafe_word = values::int_word(-big);
                assert_eq!(find_out_of_js_range(safe, &nodes, 0), None);
                assert_eq!(
                    find_out_of_js_range(values::int_word(-JS_MAX_SAFE_INT), &nodes, 0),
                    None
                );
                assert_eq!(find_out_of_js_range(unsafe_word, &nodes, 0), Some(-big));
                assert_eq!(find_out_of_js_range(unsafe_word, &nodes, 1), None);
                let list = list::from_iter([values::int_word(1), values::int_word(big)]);
                assert_eq!(find_out_of_js_range(list, &nodes, 2), Some(big));
                let set = set_of(&[3, big], 1);
                assert_eq!(find_out_of_js_range(set, &nodes, 4), Some(big));
                let map = hamt::cheby_map_insert(
                    hamt::cheby_map_new(),
                    values::int_word(1),
                    values::int_word(big),
                );
                assert_eq!(find_out_of_js_range(map, &nodes, 3), Some(big));
                // The first element is not an `Int` (say an `I64`).
                let pair = values::tuple(&[values::int_word(big), values::int_word(2)]);
                assert_eq!(find_out_of_js_range(pair, &nodes, 5), None);
                let some = values::some(values::int_word(big));
                assert_eq!(find_out_of_js_range(some, &nodes, 6), Some(big));
                assert_eq!(find_out_of_js_range(values::none(), &nodes, 6), None);
                for word in [unsafe_word, list, set, map, pair, some] {
                    crate::rc::drop(word);
                }
            }
            assert_eq!(domain.live_objects(), 0);
        });
    }

    #[test]
    fn seeds_depend_on_every_input() {
        let base = const_seed("app", "app::a", "x", &[("app::a", source_hash(b"1"))]);
        assert_eq!(
            base,
            const_seed("app", "app::a", "x", &[("app::a", source_hash(b"1"))])
        );
        for other in [
            const_seed("ap", "papp::a", "x", &[("app::a", source_hash(b"1"))]),
            const_seed("app", "app::a", "y", &[("app::a", source_hash(b"1"))]),
            const_seed("app", "app::a", "x", &[("app::a", source_hash(b"2"))]),
            const_seed("app", "app::a", "x", &[]),
        ] {
            assert_ne!(base, other);
        }
    }
}
