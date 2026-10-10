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
//! Freezing results in place is [`crate::alloc::freeze`] (D-539). W4-C adds
//! the limits (D-536) and the rebuild of maps and sets (D-196).
//!
//! [`EntryConfig::consts`]: crate::entry::EntryConfig::consts

use std::collections::HashMap;
use std::hash::Hasher as _;
use std::sync::{Arc, Mutex, PoisonError};

use siphasher::sip::SipHasher13;

use crate::abi::{self, PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::entry::{self, EntryFn};
use crate::panic::{cheby_rt_panic, fatal, panic_current};

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
        }
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
