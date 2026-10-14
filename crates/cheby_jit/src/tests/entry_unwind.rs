//! The entry check's releasing slow path under the JIT
//! (D-775, refining D-698 and D-743): a function with at
//! most four counted parameters calls `cheby_rt_stack_slow_unwind` (the
//! outer function of a large frame `cheby_rt_stack_slow_frame_unwind`),
//! which releases them itself when it starts an unwinding; a function with
//! more keeps the translated entry pad chain. A recursion that holds its
//! counted parameters across the call overflows the stack, and every
//! object of the entry's domain is released on the way out. Run with
//! `CHEBY_RT_POISON=1` for freed objects to be poisoned (D-458).

use cheby_codegen_clif::{CodegenOptions, LARGE_FRAME_THRESHOLD};
use cheby_runtime::abi::PanicKind as RtPanicKind;

use super::*;

/// The counts of counted parameters the tests use: the helper takes up to
/// four, and five keep the chain.
const COUNTS: [u32; 4] = [0, 1, 4, 5];

/// `rec(x_0 … x_(k-1))` holds its parameters across a call of itself:
/// `dup x_i…; r = call rec(x_0 … x_(k-1)); drop x_i…; return r`; and
/// `start()` allocates `k` pairs and calls `rec` with them. Returns the
/// program and the target of the thunk that calls `start`.
fn recursion_with(k: u32) -> (Prog, DefId) {
    let mut p = Prog::default();
    let rec = def(1, 0);
    let params: Vec<Repr> = (0..k).map(|_| Repr::Value).collect();
    let args: Vec<Operand> = (0..k).map(op).collect();
    let mut body: Vec<Stmt> = (0..k).map(|i| Stmt::Dup(local(i))).collect();
    body.push(assign(
        k,
        Rvalue::Call {
            callee: Callee::Direct(rec),
            args: args.clone(),
        },
    ));
    body.extend((0..k).map(|i| Stmt::Drop(local(i))));
    p.item(0, "rec", &params, &[Repr::Value], straight(body, k), &[]);
    let mut start: Vec<Stmt> = (0..k)
        .map(|i| {
            assign(
                i,
                Rvalue::Alloc {
                    layout: LayoutRef::Tuple(2),
                    fields: vec![Operand::Const(Const::Nil), Operand::Const(Const::Nil)],
                },
            )
        })
        .collect();
    start.push(assign(
        k,
        Rvalue::Call {
            callee: Callee::Direct(rec),
            args,
        },
    ));
    let locals: Vec<Repr> = (0..=k).map(|_| Repr::Value).collect();
    let start = p.item(1, "start", &[], &locals, straight(start, k), &[]);
    let target = p.call(start, Vec::new());
    (p, target)
}

/// The CLIF text of the function `symbol` among `clif`'s.
fn text_of_function<'a>(clif: &'a [String], symbol: &str) -> &'a str {
    let header = format!("; {symbol}\n");
    clif.iter()
        .find(|text| text.starts_with(&header))
        .unwrap_or_else(|| panic!("no {symbol} in:\n{}", clif.join("\n")))
}

/// Whether `text` names the runtime function `symbol` in its legend.
fn imports(text: &str, symbol: &str) -> bool {
    text.lines()
        .any(|line| line.ends_with(&format!("= {symbol}")))
}

/// Compiles `recursion_with(k)` with its unwind edges and pads, the checked
/// count operations and `large_frame_threshold`, runs it on a 1 MiB stack,
/// and checks that it overflowed with nothing of its domain left alive.
/// Returns the CLIF of the function that makes `rec`'s entry check: `rec`,
/// or its outer function when it is split.
fn overflow_releases_everything(k: u32, large_frame_threshold: u32) -> String {
    let (p, target) = recursion_with(k);
    let options = CodegenOptions {
        checked_rc: true,
        capture_clif: true,
        large_frame_threshold,
        ..CodegenOptions::default()
    };
    let mut jit = p.compile_unwound_with(options, JitOptions::from_env());
    let clif = jit.take_clif();
    let rec = text_of_function(&clif, "cb$fn$app$rec").to_owned();
    let outcome = run_with_stack(&jit, target, 1 << 20);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("{k} parameters: the recursion returned: {outcome:?}");
    };
    assert_eq!(report.kind, RtPanicKind::StackOverflow, "{k} parameters");
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(
            jit.domain().live_objects(),
            0,
            "{k} parameters: objects leaked"
        );
    }
    rec
}

/// Functions with 0, 1, 4 and 5 counted parameters overflow the stack:
/// up to four go through `cheby_rt_stack_slow_unwind`, which released
/// them; five keep `cheby_rt_stack_slow` and the translated chain.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn entry_unwind_releases_the_counted_parameters_on_overflow() {
    for k in COUNTS {
        let rec = overflow_releases_everything(k, LARGE_FRAME_THRESHOLD);
        let helper = imports(&rec, "cheby_rt_stack_slow_unwind");
        let plain = imports(&rec, "cheby_rt_stack_slow");
        if k <= 4 {
            assert!(helper && !plain, "{k} parameters:\n{rec}");
        } else {
            assert!(plain && !helper, "{k} parameters:\n{rec}");
        }
    }
}

/// The same with every function split into an outer function and its
/// large-frame body (D-679, threshold 0): the outer function of `rec`
/// goes through `cheby_rt_stack_slow_frame_unwind` with up to four
/// counted parameters, and keeps `cheby_rt_stack_slow_frame` and its own
/// pad with five (D-743).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn entry_unwind_releases_the_counted_parameters_of_a_large_frame() {
    for k in COUNTS {
        let outer = overflow_releases_everything(k, 0);
        assert!(outer.contains("return_call fn"), "not split:\n{outer}");
        let helper = imports(&outer, "cheby_rt_stack_slow_frame_unwind");
        let plain = imports(&outer, "cheby_rt_stack_slow_frame");
        if k <= 4 {
            assert!(helper && !plain, "{k} parameters:\n{outer}");
        } else {
            assert!(plain && !helper, "{k} parameters:\n{outer}");
        }
    }
}
