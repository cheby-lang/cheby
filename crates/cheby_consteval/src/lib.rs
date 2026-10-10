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
//!    without a report of their own.
//! 4. Every result is frozen in place (count 0, out of the build's
//!    accounting domain, D-539, D-564), so code reads constants without
//!    counting and they never show up as leaks.
//!
//! W4-C adds, at the hooks marked below: the time and memory limits
//! (E0076, D-536), the rebuild of maps and sets with the per-process seed
//! before freezing when code runs afterwards (D-196, D-539), and the JS
//! range check (E0078, D-540).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, Ice};
use cheby_jit::{Jit, JitError};
use cheby_mir::EntryKind;
use cheby_package::Targets;
use cheby_runtime::abi::PanicKind;
use cheby_runtime::consteval::{Abandoned, ConstTable, UNRESOLVED_MESSAGE};
use cheby_runtime::{ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome, PanicReport};

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
/// result in its slot; returns the diagnostics (E0069, E0075-E0078). Every
/// constant of `order` must have its slot and its constant entry thunk in
/// `jit`. `Check` skips the D-196 rebuild (plan §7.6).
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
        let entry_config = EntryConfig {
            consts: Some(Arc::clone(&table)),
            ..EntryConfig::new(EntryMode::ConstEval(limits(info, config)))
        };
        // Hook for W4-C: the watchdog of the time limit and the memory
        // budget wrap this run (D-536), and their outcomes become E0076.
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
                if let Some(d) = failure(&report, &abandoned, info, &by_slot) {
                    diagnostics.push(d);
                }
            }
        }
    }
    // Hook for W4-C: when `config.mode` is `Run` and nothing failed, the
    // D-196 rebuild of every `Map` and `Set` in the results goes here,
    // before freezing; the E0078 range walk of the constants that exist on
    // `js` too (D-539, D-540).
    freeze(jit, &slots);
    Ok(diagnostics)
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
        // again (GAP(const-failure-site)).
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
    // (GAP(const-failure-site)).
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
