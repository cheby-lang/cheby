//! The large-frame entry guard (D-679, refining D-124 and D-453): a
//! function whose frame exceeds the threshold is compiled as a small outer
//! function, whose entry check accounts for the whole frame, and an
//! unchecked inner body `cb$big$…` that the outer one tail-calls. A deep
//! recursion of such a function stops with a stack-overflow panic, never a
//! fault below the stack.

use super::*;

/// The module and item of the recursive function of these tests.
const ITEM: &str = "deep";

/// `a + b`, for local numbers.
fn add(a: u32, b: u32) -> u32 {
    a.checked_add(b).unwrap()
}

/// The locals after `n` and the body of `deep`'s recursive block (see
/// [`deep`]), and the local holding its result.
fn deep_body(me: DefId, width: u32) -> (Vec<Repr>, Vec<Stmt>, u32) {
    // Locals: 0 `n`, 1 `n == 0`, 2 `box 0`, then `width` values `v_i`,
    // then `m`, `r`, `t`, `width` running xors and the boxed result.
    let first_v = 3;
    let m = add(first_v, width);
    let (r, t, first_acc) = (add(m, 1), add(m, 2), add(m, 3));
    let result = add(first_acc, width);
    let wide = usize::try_from(width).unwrap();
    let mut locals = vec![Repr::Bool, Repr::Value];
    locals.extend(std::iter::repeat_n(Repr::Int, wide));
    locals.extend([Repr::Int, Repr::Value, Repr::Int]);
    locals.extend(std::iter::repeat_n(Repr::Int, wide));
    locals.push(Repr::Value);
    let xor = |lhs: Operand, rhs: Operand| Rvalue::Bitwise {
        op: BitOp::Xor,
        lhs,
        rhs,
        repr: Repr::Int,
    };
    let mut body: Vec<Stmt> = (0..width)
        .map(|i| {
            let k = i64::from(add(i, 1));
            assign(add(first_v, i), xor(op(0), Operand::Const(Const::Int(k))))
        })
        .collect();
    body.push(assign(
        m,
        Rvalue::Checked {
            op: ArithOp::Sub,
            lhs: op(0),
            rhs: Operand::Const(Const::Int(1)),
            repr: Repr::Int,
            loc: StrId::from_u32(1),
        },
    ));
    body.push(assign(
        r,
        Rvalue::Call {
            callee: Callee::Direct(me),
            args: vec![op(m)],
        },
    ));
    body.push(assign(
        t,
        Rvalue::Unbox {
            value: op(r),
            to: Repr::Int,
        },
    ));
    // `r` is owned: its count balances, as the unwind pass checks (D-698).
    body.push(Stmt::Drop(local(r)));
    let mut previous = t;
    for i in 0..width {
        let acc = add(first_acc, i);
        body.push(assign(acc, xor(op(previous), op(add(first_v, i)))));
        previous = acc;
    }
    body.push(assign(
        result,
        Rvalue::Box {
            value: op(previous),
            from: Repr::Int,
        },
    ));
    (locals, body, result)
}

/// `deep(n: Int)`: `n == 0` returns 0; otherwise it computes
/// `v_i = n ^ i` for `i` in `1..=width`, calls `deep(n - 1)` (through its
/// slot, so recursion re-enters the outer function, D-043), drops the
/// boxed result after unboxing it, and returns the result xor every
/// `v_i`. The `width` values are live across the call,
/// so each takes a spill slot: the frame is about `8 * width` bytes.
fn deep(index: u32, width: u32) -> Function {
    let me = def(1, index);
    let (locals, body, result) = deep_body(me, width);
    function(
        FnKey::Item(me),
        Abi::Cheby,
        &[Repr::Int],
        &locals,
        Repr::Value,
        vec![
            block(
                vec![assign(
                    1,
                    Rvalue::Compare {
                        op: CmpOp::Eq,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(0)),
                        repr: Repr::Int,
                    },
                )],
                Terminator::Branch {
                    cond: op(1),
                    then_: bb(1),
                    else_: bb(2),
                },
            ),
            block(
                vec![assign(
                    2,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(0)),
                        from: Repr::Int,
                    },
                )],
                ret(2),
            ),
            block(body, ret(result)),
        ],
        &[LOC],
    )
}

/// What `deep(n)` returns, computed in Rust.
fn expected(n: i64, width: u32) -> i128 {
    let mut acc = 0_i64;
    for k in 1..=n {
        for i in 1..=i64::from(width) {
            acc ^= k ^ i;
        }
    }
    i128::from(acc)
}

/// A JIT holding `deep` of `width` values compiled with `options` after
/// the unwind pass (an overflow unwinds by the return flag, D-733), two
/// thunks calling it with `shallow` and with `depth`, and its CLIF.
struct Built {
    jit: Jit,
    shallow: DefId,
    deep: DefId,
    clif: String,
}

fn compile_deep(width: u32, shallow: i64, depth: i64, options: CodegenOptions) -> Built {
    let mut p = Prog::default();
    let f = def(1, 0);
    p.functions.push(deep(0, width));
    p.declare(f, ITEM, &[Repr::Int]);
    let shallow = p.call(f, vec![Operand::Const(Const::Int(shallow))]);
    let deep = p.call(f, vec![Operand::Const(Const::Int(depth))]);
    let mut jit = Jit::with_options(CodegenOptions {
        capture_clif: true,
        ..options
    })
    .unwrap();
    jit.compile(&p.unwound_units()).unwrap();
    let clif = jit.take_clif().join("\n");
    Built {
        jit,
        shallow,
        deep,
        clif,
    }
}

/// The CLIF text of the function whose symbol is `symbol`.
fn clif_of<'a>(clif: &'a str, symbol: &str) -> &'a str {
    let header = format!("; {symbol}\n");
    let (_, rest) = clif
        .split_once(&header)
        .unwrap_or_else(|| panic!("no function {symbol} in\n{clif}"));
    rest.split_once("\n; cb$").map_or(rest, |(text, _)| text)
}

/// The frame size the outer function of `symbol` passes to its slow path:
/// the `iconst.i64` that is not the frame size plus the 64 KiB margin.
fn frame_size_of(clif: &str, symbol: &str) -> u64 {
    let outer = clif_of(clif, symbol);
    let constants: Vec<u64> = outer
        .lines()
        .filter_map(|line| line.split("iconst.i64 ").nth(1))
        .map(|n| {
            // Cranelift prints large immediates in hexadecimal with `_`
            // separators.
            let n = n.trim().replace('_', "");
            match n.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
                None => n.parse().unwrap(),
            }
        })
        .collect();
    let margin = cheby_runtime::panic::LARGE_FRAME_MARGIN;
    constants
        .iter()
        .copied()
        .find(|&n| {
            n.checked_add(margin)
                .is_some_and(|m| constants.contains(&m))
        })
        .unwrap_or_else(|| panic!("no frame size and margin in\n{outer}"))
}

/// Checks the shape of a split `deep`: the outer function keeps the
/// symbol, checks `sp - F - 64 KiB` against `[ctx+0]`, calls
/// `cheby_rt_stack_slow_frame` on its slow path and tail-calls the inner
/// body, which has no check of its own. Returns `F`.
fn assert_split(clif: &str) -> u64 {
    let outer_symbol = "cb$fn$app$deep";
    let inner_symbol = "cb$big$fn$app$deep";
    let outer = clif_of(clif, outer_symbol);
    assert!(outer.contains("= cheby_rt_stack_slow_frame"), "{outer}");
    assert!(outer.contains(&format!("= {inner_symbol}")), "{outer}");
    assert!(outer.contains("get_stack_pointer"), "{outer}");
    assert!(outer.contains("isub"), "{outer}");
    assert!(outer.contains("icmp ult"), "{outer}");
    assert!(outer.contains("return_call fn"), "{outer}");
    assert!(!outer.contains("cheby_rt_stack_slow\n"), "{outer}");
    let inner = clif_of(clif, inner_symbol);
    assert!(!inner.contains("get_stack_pointer"), "{inner}");
    assert!(!inner.contains("cheby_rt_stack_slow"), "{inner}");
    // The outer function comes first, then its body, in declaration order.
    assert!(
        clif.find(&format!("; {outer_symbol}\n")) < clif.find(&format!("; {inner_symbol}\n")),
        "{clif}"
    );
    frame_size_of(clif, outer_symbol)
}

fn assert_overflows(outcome: EntryOutcome) {
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the deep recursion returned: {outcome:?}");
    };
    assert_eq!(
        report.kind,
        cheby_runtime::abi::PanicKind::StackOverflow,
        "{report:?}"
    );
    assert_eq!(report.location, "test.cheby:1:1", "{report:?}");
}

/// D-679: `deep` with a frame of about 300 KiB, above the 64 KiB default,
/// is split. A shallow call returns the right value through both halves;
/// a recursion a million calls deep on an 8 MiB entry stops at about 27
/// frames with a stack-overflow panic, raised by the outer function's
/// check before the frame is allocated.
///
/// Without the split, the check runs after the prologue has allocated
/// 300 KiB, beyond the headroom of 256 KiB, so where the last frame lands
/// decides whether the slow path runs below the stack. The deep call is
/// therefore repeated on stacks of 8 MiB plus 0 to 11 times 32 KiB, which
/// moves the last frame through a whole frame's length: compiled without
/// the split, one of these runs (the seventh, when this test was written)
/// aborts the process on the thread's guard page.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn large_frame_of_300_kib_overflows_with_a_panic() {
    let width = 38_400;
    let d = compile_deep(width, 3, 1_000_000, CodegenOptions::default());
    let frame = assert_split(&d.clif);
    assert!(
        (290 << 10..400 << 10).contains(&frame),
        "a frame of {frame} bytes"
    );
    assert_eq!(
        int_of(returned(run_with_stack(
            &d.jit,
            d.shallow,
            cheby_runtime::entry::DEFAULT_STACK_SIZE,
        ))),
        expected(3, width)
    );
    for step in 0..12 {
        let size = cheby_runtime::entry::DEFAULT_STACK_SIZE + step * (32 << 10);
        assert_overflows(run_with_stack(&d.jit, d.deep, size));
    }
}

/// The threshold is an option: lowered to 1 KiB, `deep` of 200 values
/// (a frame of about 1.6 KiB) is split, returns the right value, and its
/// deep recursion stops with a stack-overflow panic.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn large_frame_threshold_lowered_to_1_kib() {
    let width = 200;
    let d = compile_deep(
        width,
        50,
        10_000_000,
        CodegenOptions {
            large_frame_threshold: 1 << 10,
            ..CodegenOptions::default()
        },
    );
    let frame = assert_split(&d.clif);
    assert!(
        (1 << 10..4 << 10).contains(&frame),
        "a frame of {frame} bytes"
    );
    assert_eq!(
        int_of(returned(run_with_stack(
            &d.jit,
            d.shallow,
            cheby_runtime::entry::DEFAULT_STACK_SIZE,
        ))),
        expected(50, width)
    );
    assert_overflows(run_with_stack(
        &d.jit,
        d.deep,
        cheby_runtime::entry::DEFAULT_STACK_SIZE,
    ));
}

/// A function under the threshold is compiled exactly as before: the same
/// CLIF and the same machine code size as with the split turned off, one
/// function, and no `cb$big$` body. Its deep recursion still stops at the
/// ordinary entry check (D-124).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn large_frame_split_leaves_small_functions_unchanged() {
    let width = 200;
    let build = |threshold: u32| {
        compile_deep(
            width,
            50,
            10_000_000,
            CodegenOptions {
                large_frame_threshold: threshold,
                ..CodegenOptions::default()
            },
        )
    };
    let default = build(cheby_codegen_clif::LARGE_FRAME_THRESHOLD);
    let never = build(u32::MAX);
    assert_eq!(default.clif, never.clif);
    assert_eq!(default.jit.code_stats(), never.jit.code_stats());
    assert!(!default.clif.contains("cb$big$"), "{}", default.clif);
    assert!(
        !default.clif.contains("stack_slow_frame"),
        "{}",
        default.clif
    );
    assert!(
        default.clif.contains("cheby_rt_stack_slow"),
        "{}",
        default.clif
    );
    assert_overflows(run_with_stack(
        &default.jit,
        default.deep,
        cheby_runtime::entry::DEFAULT_STACK_SIZE,
    ));
}

/// The split is deterministic (D-264): two builds give the same CLIF, with
/// the same names and function ids, and the same code size.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn large_frame_split_is_deterministic() {
    let options = CodegenOptions {
        large_frame_threshold: 1 << 10,
        ..CodegenOptions::default()
    };
    let first = compile_deep(200, 1, 2, options);
    let second = compile_deep(200, 1, 2, options);
    assert_split(&first.clif);
    assert_eq!(first.clif, second.clif);
    assert_eq!(first.jit.code_stats(), second.jit.code_stats());
    // The thunks, the outer function and its inner body.
    assert_eq!(first.jit.code_stats().functions, 4);
}

/// Without the entry check (`stack_check: false`) nothing is split.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn large_frame_needs_the_entry_check() {
    let d = compile_deep(
        200,
        5,
        6,
        CodegenOptions {
            large_frame_threshold: 1 << 10,
            stack_check: false,
            ..CodegenOptions::default()
        },
    );
    assert!(!d.clif.contains("cb$big$"), "{}", d.clif);
    assert_eq!(
        int_of(returned(run_with_stack(
            &d.jit,
            d.shallow,
            cheby_runtime::entry::DEFAULT_STACK_SIZE,
        ))),
        expected(5, 200)
    );
}
