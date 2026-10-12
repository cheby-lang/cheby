//! Compile-time evaluation of constants (E6, §4.5, §13.9, plan §7).
//!
//! A build compiles its code, its constant initializers and its entries
//! into one JIT (D-535). [`evaluate`] then evaluates the reachable
//! constants in that JIT, in the order it is given (the dependency order of
//! `Reach::constant_order`):
//!
//! 1. Every constant is registered in a [`ConstTable`] by its
//!    `cb$const$` slot, with its entry thunk and its D-269 seed (D-538).
//! 2. Each constant still pending runs its `EntryKind::Const` thunk on an
//!    entry of its own (D-457) with [`EntryMode::ConstEval`] and its seed,
//!    and the result goes into its slot ([`Jit::set_const`]). An
//!    initializer that reads a constant whose slot is still empty forces
//!    it on the same entry, under the forced constant's seed
//!    (`cheby_rt_const_missing`), so each constant is evaluated exactly
//!    once and its value does not depend on who reached it first.
//! 3. A panic becomes a diagnostic at the constant's name (D-533): E0069
//!    for a cycle through a function call, E0077 for a forbidden effect or
//!    a native symbol missing under `cheby check` (D-532), and E0075 for
//!    any other panic, with the panic's message and location in a note.
//!    The constants that the failed one was computing for are not
//!    reported again, and those that read a failed constant later stop
//!    without a report of their own. With the default unwinding backend
//!    the panicked entry's thread parks forever; under an unwinding
//!    backend (D-696) the evaluation unwinds through its thunk, a forced
//!    constant's included, and its thread ends (D-713). Either way the
//!    table holds the same constants in progress for
//!    [`ConstTable::abandon`].
//!    A top-level evaluation that runs out of time or memory (D-536) is
//!    E0076 at that constant: the host's watchdog interrupts it at its next
//!    function entry or tail call once its deadline passes, and an
//!    allocation that would take the bytes this evaluation keeps live past
//!    the memory limit ends it before memory is requested (D-617).
//! 4. When everything evaluated, each constant that exists on `js` is
//!    walked by its type for an `Int` outside ±(2⁵³ − 1) (E0078, D-540),
//!    if the caller passed the types ([`evaluate_with`]).
//! 5. When code runs afterwards ([`ConstMode::Run`]) and nothing failed,
//!    every `Map` and `Set` in the results is rebuilt with the per-process
//!    seed in one memoized, inner-first pass (D-196, D-539).
//! 6. Every result is frozen in place (count 0, out of the build's
//!    accounting domain, D-539, D-564), so code reads constants without
//!    counting and they never show up as leaks.

mod range;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, Ice, ItemSig};
use cheby_jit::{Jit, JitError};
use cheby_mir::EntryKind;
use cheby_package::{Target, Targets};
use cheby_runtime::abi::PanicKind;
use cheby_runtime::consteval::{Abandoned, ConstTable, Limit, UNRESOLVED_MESSAGE};
use cheby_runtime::{ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome, PanicReport};

pub use range::ConstTypes;

/// Which command evaluates: `cheby check` skips the D-196 rebuild of maps
/// and sets (D-539).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstMode {
    /// `cheby check`: evaluates and reports, never runs code afterwards.
    Check,
    /// `cheby run`, `cheby test` and the REPL: code runs afterwards.
    Run,
}

/// How constants are evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstEvalConfig {
    pub mode: ConstMode,
    /// Wall-clock time per top-level evaluation (D-536).
    pub time_limit: Duration,
    /// Peak live bytes of the build while a constant is evaluated (D-536).
    pub memory_limit: u64,
    /// The package's targets (E0078, D-540).
    pub targets: Targets,
}

/// One constant to evaluate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstInfo {
    pub def: DefId,
    /// The constant's name, for messages.
    pub name: Box<str>,
    /// The span of the constant's name, where its diagnostics point.
    pub name_span: Span,
    /// Its D-269 hash seed (D-538).
    pub seed: u64,
    /// Whether it exists on `js` (D-540).
    pub on_js: bool,
}

/// Evaluates `order` (from `Reach::constant_order`) in `jit`, storing each
/// result in its slot; returns the diagnostics (E0069, E0075-E0077). Every
/// constant of `order` must have its slot and its constant entry thunk in
/// `jit`. `Check` skips the D-196 rebuild (plan §7.6). Without the types
/// of [`evaluate_with`] there is no JS range check (E0078): a REPL session
/// runs only on native.
///
/// # Errors
///
/// An [`Ice`] if a constant has no slot or thunk, or the runtime cannot
/// run an entry.
pub fn evaluate(
    jit: &mut Jit,
    order: &[ConstInfo],
    config: &ConstEvalConfig,
) -> Result<Vec<Diagnostic>, Ice> {
    evaluate_with(jit, order, config, None)
}

/// [`evaluate`], and with `types` the JS range check of the constants that
/// exist on `js` when the package targets `js` (E0078, D-540).
///
/// # Errors
///
/// As for [`evaluate`], and an [`Ice`] if a constant's type names an item
/// `types` cannot see.
pub fn evaluate_with(
    jit: &mut Jit,
    order: &[ConstInfo],
    config: &ConstEvalConfig,
    types: Option<&ConstTypes<'_>>,
) -> Result<Vec<Diagnostic>, Ice> {
    let table = ConstTable::new();
    let mut slots = Vec::with_capacity(order.len());
    let mut by_slot: BTreeMap<usize, &ConstInfo> = BTreeMap::new();
    for info in order {
        let slot = jit
            .const_slot(info.def)
            .ok_or_else(|| Ice::new(format!("the constant `{}` has no slot", info.name)))?;
        let thunk = jit.entry(EntryKind::Const(info.def)).map_err(ice)?.thunk();
        // SAFETY: `slot` is the constant's slot in `jit` and `thunk` its
        // constant entry thunk, both valid while `jit` lives, which outlives
        // every entry run below; the table is not used after this function.
        unsafe { table.register(slot, thunk, info.seed) };
        slots.push(slot);
        by_slot.insert(slot.addr(), info);
    }
    let mut diagnostics = Vec::new();
    for (info, &slot) in order.iter().zip(&slots) {
        // Forced by an earlier constant, or failed with one.
        if !table.begin(slot) {
            continue;
        }
        // The deadline starts now; the runtime's watchdog and memory budget
        // enforce both limits (D-536).
        let entry_config = EntryConfig {
            consts: Some(Arc::clone(&table)),
            ..EntryConfig::new(EntryMode::ConstEval(limits(info, config)))
        };
        let outcome = jit
            .entry(EntryKind::Const(info.def))
            .map_err(ice)?
            .run(&entry_config)
            .map_err(ice)?;
        match outcome {
            EntryOutcome::Returned(word) => {
                jit.set_const(info.def, word).map_err(ice)?;
                table.finish(slot);
            }
            EntryOutcome::Panicked(report) => {
                let abandoned = table.abandon();
                let diagnostic = match abandoned.limit {
                    Some(limit) => Some(over_limit(limit, &abandoned, info, &by_slot, config)),
                    None => failure(&report, &abandoned, info, &by_slot),
                };
                diagnostics.extend(diagnostic);
            }
        }
    }
    if diagnostics.is_empty()
        && let Some(types) = types
        && config.targets.contains(Target::Js)
    {
        for (info, &slot) in order.iter().zip(&slots) {
            if info.on_js {
                diagnostics.extend(js_range(types, info, slot)?);
            }
        }
    }
    if diagnostics.is_empty() && config.mode == ConstMode::Run {
        rebuild(jit, &slots)?;
    }
    freeze(jit, &slots);
    Ok(diagnostics)
}

/// The debug text (§3.13) of each evaluated constant's value, in the order
/// of `defs`; `None` for a constant that has no value. Runs on an entry,
/// since printing may call a type's `@std_hooks` closure.
///
/// # Errors
///
/// An [`Ice`] if a constant has no slot, or printing panics or cannot run.
pub fn debug_texts(jit: &Jit, defs: &[DefId]) -> Result<Vec<Option<String>>, Ice> {
    let mut words = Vec::with_capacity(defs.len());
    for &def in defs {
        let slot = jit
            .const_slot(def)
            .ok_or_else(|| Ice::new(format!("the constant {def:?} has no slot")))?;
        // SAFETY: a constant's slot in `jit`, 8-aligned and readable; no
        // entry runs meanwhile.
        words.push(unsafe { slot.read() });
    }
    let texts: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let out = Arc::clone(&texts);
    let task = move || {
        let printed = words
            .iter()
            .map(|&word| {
                // SAFETY: an evaluated constant's frozen value, or 0.
                (word != 0).then(|| unsafe { cheby_runtime::generic::debug_string(word) })
            })
            .collect();
        *out.lock().unwrap_or_else(PoisonError::into_inner) = printed;
        0
    };
    // The task calls the JIT's trampolines, so the entry runs under the
    // JIT's unwinding backend (D-696).
    let config = EntryConfig {
        callbacks: jit.callbacks(),
        domain: jit.domain(),
        unwind: jit.unwind_backend(),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: the trampolines are `jit`'s, and the words its frozen
    // constants, valid while `jit` lives, which outlives the call.
    match unsafe { cheby_runtime::entry::run_task(&config, task) } {
        Ok(EntryOutcome::Returned(_)) => Ok(std::mem::take(
            &mut *texts.lock().unwrap_or_else(PoisonError::into_inner),
        )),
        Ok(EntryOutcome::Panicked(report)) => Err(Ice::new(format!(
            "printing a constant panicked: {}",
            report.message
        ))),
        Err(e) => Err(Ice::new(format!("printing constants: {e}"))),
    }
}

/// E0078 for `info`'s value if it holds an `Int` that JS cannot represent
/// exactly (D-540).
fn js_range(
    types: &ConstTypes<'_>,
    info: &ConstInfo,
    slot: *mut u64,
) -> Result<Option<Diagnostic>, Ice> {
    let item = (types.item)(info.def).ok_or_else(|| {
        Ice::new(format!(
            "the constant `{}` has no interface item",
            info.name
        ))
    })?;
    let ItemSig::Const(sig) = &item.sig else {
        return Ok(None);
    };
    let Some(nodes) = range::range_graph(types, sig.ty)? else {
        return Ok(None);
    };
    // SAFETY: the slot of an evaluated constant holds its live value,
    // which no entry changes meanwhile; `nodes` describes its type.
    let found = unsafe {
        let word = slot.read();
        cheby_runtime::consteval::find_out_of_js_range(word, &nodes, 0)
    };
    Ok(found.map(|n| {
        Diagnostic::new(
            codes::CONSTANT_OUT_OF_JS_RANGE,
            format!(
                "the constant `{}` holds an `Int` that JavaScript cannot represent exactly",
                info.name
            ),
        )
        .with_primary(info.name_span, "this constant exists on `js`")
        .with_note(format!(
            "it holds `{n}`, outside ±(2^53 - 1), the range of integers JavaScript represents exactly"
        ))
        .with_help(
            "keep its `Int`s between `int::min_safe` and `int::max_safe`, or mark the constant \
             `@target(native)`",
        )
    }))
}

/// Rebuilds every `Map` and `Set` in the results with the per-process seed
/// (D-196, D-539), on an entry with the JIT's trampolines, since hashing a
/// key may call a type's `@std_hooks` closure.
fn rebuild(jit: &Jit, slots: &[*mut u64]) -> Result<(), Ice> {
    // SAFETY: each slot is a constant's slot in `jit`, readable; it holds
    // 0 or this evaluation's value. No entry runs meanwhile.
    let words: Vec<u64> = slots.iter().map(|&slot| unsafe { slot.read() }).collect();
    let seed = cheby_runtime::generic::process_seed();
    let config = EntryConfig {
        callbacks: jit.callbacks(),
        domain: jit.domain(),
        unwind: jit.unwind_backend(),
        ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
            seed,
            ..ConstEvalLimits::default()
        }))
    };
    let task = move || {
        // SAFETY: the evaluated values, live and charged to the JIT's
        // domain, which is the entry's; nothing else uses them meanwhile.
        unsafe { cheby_runtime::consteval::rebuild_collections(&words, seed) };
        0
    };
    // SAFETY: the trampolines are `jit`'s, valid while `jit` lives, which
    // outlives the call.
    match unsafe { cheby_runtime::entry::run_task(&config, task) } {
        Ok(EntryOutcome::Returned(_)) => Ok(()),
        // Hashing ran the same code when the constants were evaluated, so
        // it cannot panic now.
        Ok(EntryOutcome::Panicked(report)) => Err(Ice::new(format!(
            "rebuilding the maps and sets of constants panicked: {}",
            report.message
        ))),
        Err(e) => Err(Ice::new(format!("rebuilding constants: {e}"))),
    }
}

/// E0076 for a top-level evaluation of `top` that ran out of time or
/// memory (D-536). The limit belongs to the top-level evaluation, which
/// includes the constants it forced, so the diagnostic is at `top`; a note
/// names the forced constant it was computing, if any (D-618).
fn over_limit(
    limit: Limit,
    abandoned: &Abandoned,
    top: &ConstInfo,
    by_slot: &BTreeMap<usize, &ConstInfo>,
    config: &ConstEvalConfig,
) -> Diagnostic {
    let name = &top.name;
    let (message, note, help) = match limit {
        Limit::Time => (
            format!(
                "evaluating the constant `{name}` took longer than the time limit of {}",
                duration_text(config.time_limit)
            ),
            "constant evaluation stops an initializer that runs past the limit".to_owned(),
            "compute less at compile time, or raise `time-limit` in the `[const-eval]` table of \
             `cheby.toml`",
        ),
        Limit::Memory => (
            format!(
                "evaluating the constant `{name}` used more than the memory limit of {}",
                bytes_text(config.memory_limit)
            ),
            "the limit counts the heap bytes that the evaluation keeps live at once".to_owned(),
            "compute less at compile time, or raise `memory-limit` in the `[const-eval]` table \
             of `cheby.toml`",
        ),
    };
    let mut diagnostic = Diagnostic::new(codes::CONSTANT_LIMIT, message)
        .with_primary(top.name_span, "this constant is computed at compile time")
        .with_note(note);
    let inner = abandoned
        .stack
        .last()
        .and_then(|slot| by_slot.get(&slot.addr()))
        .filter(|inner| inner.def != top.def);
    if let Some(inner) = inner {
        diagnostic = diagnostic.with_note(format!(
            "it was computing the constant `{}`, which it reads",
            inner.name
        ));
    }
    diagnostic.with_help(help)
}

/// A time limit as `[const-eval]` spells it: `10s`, `250ms`.
fn duration_text(limit: Duration) -> String {
    let millis = limit.as_millis();
    if !limit.subsec_nanos().is_multiple_of(1_000_000) {
        format!("{limit:?}")
    } else if millis.is_multiple_of(1000) {
        format!("{}s", limit.as_secs())
    } else {
        format!("{millis}ms")
    }
}

/// A memory limit as `[const-eval]` spells it: `1GiB`, `64KiB`, or bytes.
fn bytes_text(limit: u64) -> String {
    for (unit, shift) in [("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        let units = limit >> shift;
        if units > 0 && units << shift == limit {
            return format!("{units}{unit}");
        }
    }
    format!("{limit} bytes")
}

/// The limits of one top-level evaluation and its seed (D-536, D-538).
/// The runtime enforces the limits from W4-C on.
fn limits(info: &ConstInfo, config: &ConstEvalConfig) -> ConstEvalLimits {
    ConstEvalLimits {
        deadline: Instant::now().checked_add(config.time_limit),
        memory: Some(config.memory_limit),
        seed: info.seed,
    }
}

/// Makes every evaluated result static in place (D-539): out of the
/// build's accounting domain, never counted or freed.
fn freeze(jit: &Jit, slots: &[*mut u64]) {
    cheby_runtime::alloc::with_domain(jit.domain(), || {
        for &slot in slots {
            // SAFETY: each slot is a constant's slot in `jit`, 8-aligned and
            // readable; it holds 0 or the value this evaluation stored,
            // which is live and charged to the JIT's domain, now the
            // current one. No entry runs meanwhile.
            unsafe {
                let word = slot.read();
                cheby_runtime::alloc::freeze(word);
            }
        }
    });
}

/// The diagnostic for a top-level evaluation of `top` that panicked, or
/// `None` when the panic only ended a reader of a constant that had failed
/// already.
fn failure(
    report: &PanicReport,
    abandoned: &Abandoned,
    top: &ConstInfo,
    by_slot: &BTreeMap<usize, &ConstInfo>,
) -> Option<Diagnostic> {
    let find = |slot: *mut u64| by_slot.get(&slot.addr()).copied();
    if report.kind == PanicKind::ConstCycle {
        // A reader of a constant that already failed is not reported
        // again (D-592).
        if abandoned.failed.is_some() {
            return None;
        }
        let met = abandoned.cycle.and_then(find).unwrap_or(top);
        let label = format!("`{}` needs its own value while it is computed", met.name);
        return Some(
            Diagnostic::new(
                codes::CONSTANT_CYCLE,
                format!("the constant `{}` depends on itself", met.name),
            )
            .with_primary(met.name_span, label)
            .with_note("its initializer reaches its own value through a function call")
            .with_help("give it a value that does not depend on itself"),
        );
    }
    // The constant whose initializer panicked: the innermost one in
    // progress, which may be a constant the top-level one forced
    // (D-592).
    let culprit = abandoned
        .stack
        .last()
        .copied()
        .and_then(find)
        .unwrap_or(top);
    let name = &culprit.name;
    Some(if report.kind == PanicKind::ForbiddenEffect {
        let note = if report.message == UNRESOLVED_MESSAGE {
            "it called a native function that nothing in this build defines".to_owned()
        } else {
            format!(
                "it reached the external function `{}`; only the kernels of `std::list`, \
                 `std::map`, `std::set`, `std::string`, `std::bytes` and the numeric modules \
                 run at compile time",
                report.message
            )
        };
        Diagnostic::new(
            codes::CONSTANT_EFFECT,
            format!("evaluating the constant `{name}` reached an effect"),
        )
        .with_primary(
            culprit.name_span,
            "this constant is computed at compile time",
        )
        .with_note(note)
    } else {
        Diagnostic::new(
            codes::CONSTANT_PANICKED,
            format!("evaluating the constant `{name}` panicked"),
        )
        .with_primary(culprit.name_span, "its initializer panicked")
        .with_note(format!(
            "panicked at {}: {}",
            report.location, report.message
        ))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as `map_err(ice)`, which passes the error by value"
)]
fn ice(e: JitError) -> Ice {
    Ice::new(format!("constant evaluation: {e}"))
}
