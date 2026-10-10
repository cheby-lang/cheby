//! The runtime side of constant evaluation (E6, plan §7). W3-D owns this
//! file from wave 3 on and completes it (lazy forcing, cycles, freezing);
//! W4-C adds limits and the rebuild of maps and sets. Wave 1 lands the
//! entry points generated code calls, as stubs:
//!
//! - `cheby_rt_effect_check`: panics `ForbiddenEffect` when the running
//!   entry evaluates a constant, and does nothing otherwise (plan §7.4).
//! - `cheby_rt_const_missing`: an empty constant slot outside evaluation is
//!   a broken invariant; lazy forcing comes with W3-D.
//! - `cheby_rt_unresolved`: bound to every unresolved native symbol of a
//!   `cheby check` build (D-532); panics `ForbiddenEffect`.

use crate::abi::{self, PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::panic::{cheby_rt_panic, fatal, panic_current};

/// The kernels this module registers: none.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose kernels this module registers completely: none.
pub(crate) const COMPLETE: &[&str] = &[];

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
/// the constant's slot empty. Stub until W3-D: always a fatal error.
///
/// # Safety
///
/// Callable with any arguments; it never returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_const_missing(ctx: *mut ExecCtx, slot: u64) -> Word {
    let _ = ctx;
    fatal(format_args!(
        "not implemented yet: `cheby_rt_const_missing` (the constant slot at {slot:#x} is empty)"
    ))
}

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
    panic_current(
        PanicKind::ForbiddenEffect,
        Some("constant evaluation called a native function this build cannot resolve"),
    )
}

#[cfg(test)]
mod tests {
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
}
