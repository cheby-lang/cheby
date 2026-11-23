//! Self tail calls as jumps inside the function: a top-level function
//! calling itself, or a closure's code calling its own `self`, jumps to a
//! loop header before the entry check instead of `return_call`ing
//! through its slot (D-015, D-453). A REPL session and the inner body of a
//! large frame keep the call (D-043, D-679).

use cheby_mir::{CType, ForeignSig};
use cheby_runtime::{ExecCtx, RuntimeConfig};

use super::*;

/// The blocks of one function's CLIF text, by label (`block3`), each with
/// its lines, the label's own first.
fn clif_blocks(text: &str) -> Vec<(String, Vec<String>)> {
    let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("block") && trimmed.ends_with(':') {
            let label: String = trimmed
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            blocks.push((label, vec![trimmed.to_owned()]));
        } else if let Some((_, lines)) = blocks.last_mut() {
            lines.push(trimmed.to_owned());
        }
    }
    blocks
}

/// The text of the function whose symbol is `symbol` in `clif`.
fn function_text<'c>(clif: &'c [String], symbol: &str) -> &'c str {
    let head = format!("; {symbol}\n");
    clif.iter()
        .find(|text| text.starts_with(&head))
        .unwrap_or_else(|| panic!("no {symbol} in:\n{}", clif.join("\n")))
}

/// The loop header of `text`: the block the entry block jumps to, which
/// makes the entry check (`get_stack_pointer`, the limit at `ctx+0`,
/// `icmp`), and that another block jumps back to. Panics if `text` has
/// none, or keeps a `return_call`.
fn assert_loops(text: &str) {
    let blocks = clif_blocks(text);
    let (_, entry) = blocks
        .first()
        .unwrap_or_else(|| panic!("no blocks:\n{text}"));
    let jump = entry
        .iter()
        .find_map(|line| line.strip_prefix("jump "))
        .unwrap_or_else(|| panic!("the entry block does not jump:\n{text}"));
    let header: String = jump
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    let (_, lines) = blocks
        .iter()
        .find(|(label, _)| *label == header)
        .unwrap_or_else(|| panic!("no {header}:\n{text}"));
    let body = lines.join("\n");
    for check in ["get_stack_pointer", "load.i64 notrap aligned", "icmp ult"] {
        assert!(body.contains(check), "{header} lacks `{check}`:\n{text}");
    }
    let back = format!("jump {header}(");
    let jumps = blocks
        .iter()
        .skip(1)
        .filter(|(_, lines)| lines.iter().any(|line| line.starts_with(&back)))
        .count();
    assert!(jumps >= 1, "nothing jumps back to {header}:\n{text}");
    assert!(!text.contains("return_call"), "{text}");
}

/// `count(n: Int, acc: Int) { if n == 0 { return box acc } m = n - 1;
/// s = acc + n; tail count(m, s) }`: the sum of `1..=n`. With `boxes`,
/// each iteration also boxes `s` and drops the box, whose out-of-line
/// paths are calls that `n` and `s` live across, so the function has a
/// frame of saved registers.
fn count_prog(boxes: bool) -> (Prog, DefId) {
    let mut p = Prog::default();
    let count = def(1, 0);
    let boxing = if boxes {
        vec![
            assign(
                6,
                Rvalue::Box {
                    value: op(5),
                    from: Repr::Int,
                },
            ),
            Stmt::Drop(local(6)),
        ]
    } else {
        Vec::new()
    };
    p.item(
        0,
        "count",
        &[Repr::Int, Repr::Int],
        &[Repr::Bool, Repr::Value, Repr::Int, Repr::Int, Repr::Value],
        vec![
            block(
                vec![assign(
                    2,
                    Rvalue::Compare {
                        op: CmpOp::Eq,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(0)),
                        repr: Repr::Int,
                    },
                )],
                Terminator::Branch {
                    cond: op(2),
                    then_: bb(1),
                    else_: bb(2),
                },
            ),
            block(
                vec![assign(
                    3,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Int,
                    },
                )],
                ret(3),
            ),
            block(
                vec![
                    assign(
                        4,
                        Rvalue::Checked {
                            op: ArithOp::Sub,
                            lhs: op(0),
                            rhs: Operand::Const(Const::Int(1)),
                            repr: Repr::Int,
                            loc: StrId::from_u32(1),
                        },
                    ),
                    assign(
                        5,
                        Rvalue::Checked {
                            op: ArithOp::Add,
                            lhs: op(1),
                            rhs: op(0),
                            repr: Repr::Int,
                            loc: StrId::from_u32(1),
                        },
                    ),
                ]
                .into_iter()
                .chain(boxing)
                .collect(),
                Terminator::TailCall {
                    callee: Callee::Direct(count),
                    args: vec![op(4), op(5)],
                },
            ),
        ],
        &[LOC],
    );
    (p, count)
}

/// `count_prog(false)` compiled with `codegen` and its CLIF captured,
/// with a thunk running `count(n, 0)`.
fn compile_count(codegen: CodegenOptions, n: i64) -> (Jit, Vec<String>, DefId) {
    compile_count_with(codegen, n, false)
}

/// [`compile_count`] of `count_prog(boxes)`.
fn compile_count_with(codegen: CodegenOptions, n: i64, boxes: bool) -> (Jit, Vec<String>, DefId) {
    let (mut p, count) = count_prog(boxes);
    let target = p.call(
        count,
        vec![Operand::Const(Const::Int(n)), Operand::Const(Const::Int(0))],
    );
    let mut jit = p.compile_unwound_with(
        CodegenOptions {
            capture_clif: true,
            ..codegen
        },
        JitOptions::from_env(),
    );
    let clif = jit.take_clif();
    (jit, clif, target)
}

/// A top-level function's self tail call is a jump to a loop header whose
/// entry check runs in every iteration, and ten million iterations run in
/// constant stack.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_self_tail_call_jumps_to_the_entry_check() {
    let n = 10_000_000;
    let (jit, clif, target) = compile_count(CodegenOptions::default(), n);
    assert_loops(function_text(&clif, "cb$fn$app$count"));
    let word = returned(run_with_stack(
        &jit,
        target,
        cheby_runtime::entry::MIN_STACK_SIZE,
    ));
    assert_eq!(int_of(word), i128::from(n) * i128::from(n + 1) / 2);
}

/// Release code calls its callees' symbols (`CallMode::Direct`), and a
/// self tail call is still a jump to the loop header, at Cranelift's
/// `speed` too.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_self_tail_call_jumps_in_release_code() {
    let n = 10_000_000;
    let (jit, clif, target) = compile_count(direct_calls::release(), n);
    assert_loops(function_text(&clif, "cb$fn$app$count"));
    let word = returned(run_with_stack(
        &jit,
        target,
        cheby_runtime::entry::MIN_STACK_SIZE,
    ));
    assert_eq!(int_of(word), i128::from(n) * i128::from(n + 1) / 2);
}

/// In a REPL session (`slot_self_calls`), the same self tail call goes
/// through the slot, so a redefinition reaches a running loop (D-043);
/// the loop gives the same result.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_repl_session_keeps_self_calls_on_the_slot() {
    let n = 1_000_000;
    let (jit, clif, target) = compile_count(
        CodegenOptions {
            slot_self_calls: true,
            ..CodegenOptions::default()
        },
        n,
    );
    let text = function_text(&clif, "cb$fn$app$count");
    assert!(text.contains("return_call_indirect"), "{text}");
    assert!(text.contains("= cb$slot$app$count"), "{text}");
    let word = returned(run_with_stack(
        &jit,
        target,
        cheby_runtime::entry::MIN_STACK_SIZE,
    ));
    assert_eq!(int_of(word), i128::from(n) * i128::from(n + 1) / 2);
}

/// An item with a session name keeps its self calls on its slot even
/// without `slot_self_calls`: a redefinition may take the slot over.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_session_item_keeps_self_calls_on_the_slot() {
    let (p, count) = count_prog(false);
    let mut jit = Jit::with_options(CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    })
    .unwrap();
    jit.set_session_names([(
        count,
        SessionName {
            module: "<repl:1>".into(),
            item: "count".into(),
            generation: 0,
            reuse: None,
        },
    )]);
    jit.compile(&p.units()).unwrap();
    let clif = jit.take_clif();
    let text = function_text(&clif, "cb$fn$<repl>$count");
    assert!(text.contains("return_call_indirect"), "{text}");
    assert!(!text.contains("jump block1("), "{text}");
}

/// A tail call of another function keeps `return_call_indirect` through
/// the callee's slot, and a function without a self call gets no loop
/// header: its entry block makes the check itself.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_tail_call_of_another_function_is_unchanged() {
    let (mut p, count) = count_prog(false);
    p.item(
        1,
        "start",
        &[Repr::Int],
        &[],
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(count),
                args: vec![op(0), Operand::Const(Const::Int(0))],
            },
        )],
        &[],
    );
    let mut jit = Jit::with_options(CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    })
    .unwrap();
    jit.compile(&p.units()).unwrap();
    let clif = jit.take_clif();
    let text = function_text(&clif, "cb$fn$app$start");
    assert!(text.contains("return_call_indirect"), "{text}");
    assert!(text.contains("= cb$slot$app$count"), "{text}");
    let blocks = clif_blocks(text);
    let (_, entry) = blocks.first().unwrap();
    assert!(
        entry.iter().any(|line| line.contains("get_stack_pointer")),
        "{text}"
    );
}

/// Under a large frame (D-679, a threshold of 0 splits every function
/// whose frame is not empty, so `count` boxes and drops its sum to have
/// one on aarch64 too, where it has none without; on x86-64 it has one
/// either way), the unchecked inner body keeps its self tail call on the
/// slot, which leads through the outer function's check; the loop still
/// runs in constant stack.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_large_frame_keeps_self_calls_on_the_outer_check() {
    let n = 1_000_000;
    let (jit, clif, target) = compile_count_with(
        CodegenOptions {
            large_frame_threshold: 0,
            ..CodegenOptions::default()
        },
        n,
        true,
    );
    let inner = function_text(&clif, "cb$big$fn$app$count");
    assert!(inner.contains("return_call_indirect"), "{inner}");
    assert!(!inner.contains("get_stack_pointer"), "{inner}");
    let outer = function_text(&clif, "cb$fn$app$count");
    assert!(outer.contains("get_stack_pointer"), "{outer}");
    assert!(outer.contains("return_call fn"), "{outer}");
    let word = returned(run_with_stack(
        &jit,
        target,
        cheby_runtime::entry::MIN_STACK_SIZE,
    ));
    assert_eq!(int_of(word), i128::from(n) * i128::from(n + 1) / 2);
}

/// The state of `permute` after `k` iterations from `START`: the Ints
/// rotate, and the Floats, Bools and uniform words swap.
#[derive(Clone, Copy, Debug, PartialEq)]
struct State {
    ints: [i64; 3],
    floats: [f64; 2],
    bools: [bool; 2],
    words: [i64; 2],
}

/// `[r, l]` for `[l, r]`.
const fn swapped<T: Copy>([left, right]: [T; 2]) -> [T; 2] {
    [right, left]
}

const START: State = State {
    ints: [1, 2, 3],
    floats: [0.5, -1.25],
    bools: [true, false],
    words: [7, 8],
};

impl State {
    fn step(self) -> Self {
        let [first, second, third] = self.ints;
        Self {
            ints: [second, third, first],
            floats: swapped(self.floats),
            bools: swapped(self.bools),
            words: swapped(self.words),
        }
    }

    fn after(k: u32) -> Self {
        (0..k).fold(START, |s, _| s.step())
    }
}

/// `permute(k, a, b, c, x, y, p, q, u, v)`: while `k > 0`, tail-calls
/// itself as `permute(k - 1, b, c, a, y, x, q, p, v, u)`; at `k == 0`
/// returns `u` if every parameter equals `expected` and `Nil`
/// otherwise. `u` and `v` are tagged integers, unboxed to compare.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR function")]
fn permute_prog(expected: State) -> (Prog, DefId) {
    let mut p = Prog::default();
    let permute = def(1, 0);
    let params = [
        Repr::Int,
        Repr::Int,
        Repr::Int,
        Repr::Int,
        Repr::Float,
        Repr::Float,
        Repr::Bool,
        Repr::Bool,
        Repr::Value,
        Repr::Value,
    ];
    // Locals 10..: the test of k, k - 1, each check, the unboxed words.
    let locals = [
        Repr::Bool,
        Repr::Int,
        Repr::Bool,
        Repr::Bool,
        Repr::Bool,
        Repr::Bool,
        Repr::Bool,
        Repr::Bool,
        Repr::Bool,
        Repr::Int,
        Repr::Int,
        Repr::Bool,
        Repr::Bool,
    ];
    let eq = |dst: u32, lhs: u32, rhs: Const, repr: Repr| {
        assign(
            dst,
            Rvalue::Compare {
                op: CmpOp::Eq,
                lhs: op(lhs),
                rhs: Operand::Const(rhs),
                repr,
            },
        )
    };
    // bb10 and bb13 return `Nil` on a mismatch, before and after `v` is
    // released.
    let check_to = |stmts: Vec<Stmt>, cond: u32, next: u32, fail: u32| {
        block(
            stmts,
            Terminator::Branch {
                cond: op(cond),
                then_: bb(next),
                else_: bb(fail),
            },
        )
    };
    let check = |stmts: Vec<Stmt>, cond: u32, next: u32| check_to(stmts, cond, next, 10);
    let late = |stmts: Vec<Stmt>, cond: u32, next: u32| check_to(stmts, cond, next, 13);
    let [ea, eb, ec] = expected.ints;
    let [ex, ey] = expected.floats;
    let [ep, eq_] = expected.bools;
    let [eu, ev] = expected.words;
    let blocks = vec![
        // bb0: k == 0?
        block(
            vec![eq(10, 0, Const::Int(0), Repr::Int)],
            Terminator::Branch {
                cond: op(10),
                then_: bb(2),
                else_: bb(1),
            },
        ),
        // bb1: the permuted self tail call.
        block(
            vec![assign(
                11,
                Rvalue::Checked {
                    op: ArithOp::Sub,
                    lhs: op(0),
                    rhs: Operand::Const(Const::Int(1)),
                    repr: Repr::Int,
                    loc: StrId::from_u32(1),
                },
            )],
            Terminator::TailCall {
                callee: Callee::Direct(permute),
                args: vec![
                    op(11),
                    op(2),
                    op(3),
                    op(1),
                    op(5),
                    op(4),
                    op(7),
                    op(6),
                    op(9),
                    op(8),
                ],
            },
        ),
        // bb2..bb11: the checks, then bb12 returns `u`.
        check(vec![eq(12, 1, Const::Int(ea), Repr::Int)], 12, 3),
        check(vec![eq(13, 2, Const::Int(eb), Repr::Int)], 13, 4),
        check(vec![eq(14, 3, Const::Int(ec), Repr::Int)], 14, 5),
        check(
            vec![
                eq(15, 4, Const::Float(ex.to_bits()), Repr::Float),
                eq(16, 5, Const::Float(ey.to_bits()), Repr::Float),
                eq(17, 6, Const::Bool(ep), Repr::Bool),
            ],
            15,
            6,
        ),
        check(Vec::new(), 16, 7),
        check_to(
            vec![
                eq(18, 7, Const::Bool(eq_), Repr::Bool),
                assign(
                    19,
                    Rvalue::Unbox {
                        value: op(8),
                        to: Repr::Int,
                    },
                ),
                assign(
                    20,
                    Rvalue::Unbox {
                        value: op(9),
                        to: Repr::Int,
                    },
                ),
                Stmt::Drop(local(9)),
                eq(21, 19, Const::Int(eu), Repr::Int),
                eq(22, 20, Const::Int(ev), Repr::Int),
            ],
            17,
            8,
            13,
        ),
        late(Vec::new(), 18, 9),
        late(Vec::new(), 21, 11),
        // bb10: a mismatch while `u` and `v` are held.
        block(
            vec![Stmt::Drop(local(8)), Stmt::Drop(local(9))],
            Terminator::Return(Operand::Const(Const::Nil)),
        ),
        late(Vec::new(), 22, 12),
        // bb12: every parameter matched.
        block(Vec::new(), ret(8)),
        // bb13: a mismatch once `v` is released.
        block(
            vec![Stmt::Drop(local(8))],
            Terminator::Return(Operand::Const(Const::Nil)),
        ),
    ];
    p.item(0, "permute", &params, &locals, blocks, &[LOC]);
    (p, permute)
}

/// Arguments that permute the parameters of a self tail call, Ints in a
/// cycle and Floats, Bools and uniform words in swaps, arrive in place
/// after any number of jumps, as they do through the slot.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_self_tail_call_permutes_its_parameters() {
    let word = |n: i64| Operand::Const(Const::Int(n));
    for slot_self_calls in [false, true] {
        for k in [0, 1, 2, 3, 5, 6, 1_001] {
            let expected = State::after(k);
            for (asked, matches) in [(expected, true), (State::after(k + 1), false)] {
                let (mut prog, permute) = permute_prog(asked);
                let float = |n: f64| Operand::Const(Const::Float(n.to_bits()));
                let boolean = |b: bool| Operand::Const(Const::Bool(b));
                let boxed = |dst: u32, n: i64| {
                    assign(
                        dst,
                        Rvalue::Box {
                            value: word(n),
                            from: Repr::Int,
                        },
                    )
                };
                // `start() { u = box 7; v = box 8; tail permute(k, …, u, v) }`.
                let start = prog.item(
                    1,
                    "start",
                    &[],
                    &[Repr::Value, Repr::Value],
                    vec![block(
                        vec![boxed(0, START.words[0]), boxed(1, START.words[1])],
                        Terminator::TailCall {
                            callee: Callee::Direct(permute),
                            args: vec![
                                word(i64::from(k)),
                                word(START.ints[0]),
                                word(START.ints[1]),
                                word(START.ints[2]),
                                float(START.floats[0]),
                                float(START.floats[1]),
                                boolean(START.bools[0]),
                                boolean(START.bools[1]),
                                op(0),
                                op(1),
                            ],
                        },
                    )],
                    &[],
                );
                let target = prog.call(start, Vec::new());
                let mut jit = prog.compile_unwound_with(
                    CodegenOptions {
                        capture_clif: true,
                        slot_self_calls,
                        ..CodegenOptions::default()
                    },
                    JitOptions::from_env(),
                );
                if !slot_self_calls {
                    assert_loops(function_text(&jit.take_clif(), "cb$fn$app$permute"));
                }
                let got = run_with_stack(&jit, target, cheby_runtime::entry::MIN_STACK_SIZE);
                let want = if matches {
                    EntryOutcome::Returned(abi::tag_int(expected.words[0]).unwrap())
                } else {
                    EntryOutcome::Returned(static_nil())
                };
                assert_eq!(got, want, "k = {k}, slots {slot_self_calls}, {asked:?}");
            }
        }
    }
}

/// `loop(self, n)`, a local function's code: while `n > 0`, tail-calls
/// `callee` with `n - 1` boxed; at 0 drops `self` and returns `Nil`.
fn closure_loop(key: FnKey, callee: Callee) -> Function {
    function(
        key,
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Int, Repr::Bool, Repr::Int, Repr::Value],
        Repr::Value,
        vec![
            block(
                vec![
                    assign(
                        2,
                        Rvalue::Unbox {
                            value: op(1),
                            to: Repr::Int,
                        },
                    ),
                    Stmt::Drop(local(1)),
                    assign(
                        3,
                        Rvalue::Compare {
                            op: CmpOp::Eq,
                            lhs: op(2),
                            rhs: Operand::Const(Const::Int(0)),
                            repr: Repr::Int,
                        },
                    ),
                ],
                Terminator::Branch {
                    cond: op(3),
                    then_: bb(1),
                    else_: bb(2),
                },
            ),
            block(
                vec![Stmt::Drop(local(0))],
                Terminator::Return(Operand::Const(Const::Nil)),
            ),
            block(
                vec![
                    assign(
                        4,
                        Rvalue::Checked {
                            op: ArithOp::Sub,
                            lhs: op(2),
                            rhs: Operand::Const(Const::Int(1)),
                            repr: Repr::Int,
                            loc: StrId::from_u32(1),
                        },
                    ),
                    assign(
                        5,
                        Rvalue::Box {
                            value: op(4),
                            from: Repr::Int,
                        },
                    ),
                ],
                Terminator::TailCall {
                    callee,
                    args: vec![op(5)],
                },
            ),
        ],
        &[LOC],
    )
}

/// A local function's tail call of its own `self` jumps to its loop
/// header, in a REPL session too, since no slot is involved; a closure's
/// tail call of another closure value keeps `return_call_indirect`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_local_function_jumps_on_its_own_self_only() {
    for slot_self_calls in [false, true] {
        let mut p = Prog::default();
        let owner = def(1, 0);
        let own = FnKey::Closure { owner, index: 0 };
        let other = FnKey::Closure { owner, index: 1 };
        p.functions
            .push(closure_loop(own, Callee::Indirect(local(0))));
        // Its argument is called, not its `self`: not a self call.
        p.functions.push(function(
            other,
            Abi::Cheby,
            &[Repr::Value, Repr::Value],
            &[],
            Repr::Value,
            vec![block(
                vec![Stmt::Drop(local(0))],
                Terminator::TailCall {
                    callee: Callee::Indirect(local(1)),
                    args: vec![Operand::Const(Const::Nil)],
                },
            )],
            &[],
        ));
        // `start() { k = alloc closure(own); n = box 1_000_000; tail k(n) }`.
        let start = p.item(
            0,
            "start",
            &[],
            &[Repr::Value, Repr::Value],
            vec![block(
                vec![
                    assign(
                        0,
                        Rvalue::Alloc {
                            layout: LayoutRef::Closure(0),
                            fields: vec![Operand::Const(Const::Code(own))],
                        },
                    ),
                    assign(
                        1,
                        Rvalue::Box {
                            value: Operand::Const(Const::Int(1_000_000)),
                            from: Repr::Int,
                        },
                    ),
                ],
                Terminator::TailCall {
                    callee: Callee::Indirect(local(0)),
                    args: vec![op(1)],
                },
            )],
            &[],
        );
        let target = p.call(start, Vec::new());
        let mut jit = p.compile_unwound_with(
            CodegenOptions {
                capture_clif: true,
                slot_self_calls,
                ..CodegenOptions::default()
            },
            JitOptions::from_env(),
        );
        let clif = jit.take_clif();
        assert_loops(function_text(&clif, "cb$clo$app$start$0"));
        let calls_other = function_text(&clif, "cb$clo$app$start$1");
        assert!(
            calls_other.contains("return_call_indirect"),
            "{calls_other}"
        );
        let outcome = run_with_stack(&jit, target, cheby_runtime::entry::MIN_STACK_SIZE);
        assert_eq!(outcome, EntryOutcome::Returned(static_nil()));
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(jit.domain().live_objects(), 0);
        }
    }
}

/// The rotation step of `float_spin`: `(cos, sin)` of a small angle,
/// not exactly a rotation, so every iteration's Floats are new.
const SPIN_C: f64 = 0.999_999_5;
const SPIN_S: f64 = 0.000_999_999_8;

/// The Floats of `float_spin`: `x`, `y`, `u`, `v` and the sum `acc`.
type SpinState = [f64; 5];

/// The start of root `root` of `float_spin`, distinct for each root.
fn spin_start(root: u32) -> SpinState {
    let r = f64::from(root);
    [1.0 + r * 0.25, 0.5 - r * 0.125, r, -r, 0.0]
}

/// `float_spin`'s state after `k` iterations from `state`, with the
/// same operations in the same order as its MIR, which Cranelift neither
/// reorders nor fuses.
fn spin_after(k: u32, state: SpinState) -> SpinState {
    (0..k).fold(state, |[x, y, u, v, acc], _| {
        let x1 = x * SPIN_C - y * SPIN_S;
        let y1 = x * SPIN_S + y * SPIN_C;
        let u1 = u * 0.5 + x1;
        let v1 = v * 0.75 + y1;
        [x1, y1, u1, v1, acc + (x1 * u1 - y1 * v1)]
    })
}

/// The most rounds of `spin` a root of `float_spin` runs while it waits
/// for its first interrupt. A round takes milliseconds and the host asks
/// every 200 µs, so reaching the bound means the slow path never ran.
const SPIN_ROUNDS: u32 = 1_000;

/// What a root of `float_spin` shares with the host.
#[derive(Clone, Copy)]
struct SpinSlot {
    /// The context of the running fiber, as an address, while its rounds
    /// run, and 0 otherwise.
    ctx: usize,
    /// The interrupts the context had handled when the rounds began.
    before: u64,
    /// The rounds of `spin` run so far.
    rounds: u32,
    /// The interrupts handled during the rounds, recorded at their end.
    handled: u64,
}

impl SpinSlot {
    const IDLE: Self = Self {
        ctx: 0,
        before: 0,
        rounds: 0,
        handled: 0,
    };
}

/// The slots of the two roots of `float_spin`. The host requests
/// interrupts only under this lock and only of a published context, which
/// its fiber cannot retire before `cheby_test_spin_end` takes the lock.
static SPIN_ROOTS: Mutex<[SpinSlot; 2]> = Mutex::new([SpinSlot::IDLE; 2]);

/// The slot of root `root` in [`SPIN_ROOTS`].
fn spin_slot(roots: &mut [SpinSlot; 2], root: i64) -> &mut SpinSlot {
    usize::try_from(root)
        .ok()
        .and_then(|n| roots.get_mut(n))
        .expect("a root of float_spin")
}

/// `cheby_test_spin_begin(ctx, root)`: publishes the running fiber's
/// context and the interrupts it handled before its rounds.
extern "C" fn cheby_test_spin_begin(ctx: *mut ExecCtx, root: i64) {
    // SAFETY: the context of the fiber that runs this call.
    let before = unsafe { &*ctx }.interrupts();
    let mut roots = SPIN_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    *spin_slot(&mut roots, root) = SpinSlot {
        ctx: ctx.expose_provenance(),
        before,
        ..SpinSlot::IDLE
    };
}

/// `cheby_test_spin_again(ctx, root, result)`: counts a round that
/// returned `result`, and says whether to run another: only if the round
/// matched the reference, no interrupt was handled since the rounds began,
/// and fewer than [`SPIN_ROUNDS`] ran. Only `spin`'s entry checks run
/// between the rounds' begin and end, so a handled interrupt is one that
/// stopped the loop with its Floats in registers.
extern "C" fn cheby_test_spin_again(ctx: *mut ExecCtx, root: i64, result: u64) -> bool {
    // SAFETY: the context of the fiber that runs this call.
    let interrupts = unsafe { &*ctx }.interrupts();
    let mut roots = SPIN_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = spin_slot(&mut roots, root);
    slot.rounds = slot.rounds.checked_add(1).unwrap();
    Some(result) == abi::tag_int(1) && interrupts == slot.before && slot.rounds < SPIN_ROUNDS
}

/// `cheby_test_spin_end(ctx, root)`: withdraws the context and records how
/// many interrupts its slow path handled during the rounds.
extern "C" fn cheby_test_spin_end(ctx: *mut ExecCtx, root: i64) {
    // SAFETY: the context of the fiber that runs this call.
    let interrupts = unsafe { &*ctx }.interrupts();
    let mut roots = SPIN_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = spin_slot(&mut roots, root);
    slot.ctx = 0;
    slot.handled = interrupts.checked_sub(slot.before).unwrap();
}

/// `spin(k, x, y, u, v, acc)`, item `index`: while `k != 0`, tail-calls
/// itself with [`spin_after`]'s step; at 0 returns the tagged 1 if every
/// Float equals `expected`, and `Nil` otherwise. Its loop carries five
/// Floats and an Int across the entry check.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR function")]
fn spin_item(p: &mut Prog, index: u32, expected: SpinState) -> DefId {
    let spin = def(1, index);
    let float = |n: f64| Operand::Const(Const::Float(n.to_bits()));
    let arith = |dst: u32, op_: ArithOp, lhs: Operand, rhs: Operand| {
        assign(
            dst,
            Rvalue::FloatArith {
                op: op_,
                lhs,
                rhs,
                repr: Repr::Float,
            },
        )
    };
    let compare = |dst: u32, lhs: u32, rhs: Const, repr: Repr| {
        assign(
            dst,
            Rvalue::Compare {
                op: CmpOp::Eq,
                lhs: op(lhs),
                rhs: Operand::Const(rhs),
                repr,
            },
        )
    };
    // bb2..bb6 check one Float each, on to the next block or to bb8.
    let check = |dst: u32, param: u32, want: f64, next: u32| {
        block(
            vec![compare(
                dst,
                param,
                Const::Float(want.to_bits()),
                Repr::Float,
            )],
            Terminator::Branch {
                cond: op(dst),
                then_: bb(next),
                else_: bb(8),
            },
        )
    };
    let [ex, ey, eu, ev, eacc] = expected;
    let params = [
        Repr::Int,
        Repr::Float,
        Repr::Float,
        Repr::Float,
        Repr::Float,
        Repr::Float,
    ];
    // Locals 6..: the test of `k`, the fourteen steps, `k - 1`, the five
    // checks and the result.
    let mut locals = vec![Repr::Bool];
    locals.extend([Repr::Float; 14]);
    locals.push(Repr::Int);
    locals.extend([Repr::Bool; 5]);
    locals.push(Repr::Value);
    let blocks = vec![
        // bb0: k == 0?
        block(
            vec![compare(6, 0, Const::Int(0), Repr::Int)],
            Terminator::Branch {
                cond: op(6),
                then_: bb(2),
                else_: bb(1),
            },
        ),
        // bb1: one step, then the self tail call.
        block(
            vec![
                arith(7, ArithOp::Mul, op(1), float(SPIN_C)),
                arith(8, ArithOp::Mul, op(2), float(SPIN_S)),
                arith(9, ArithOp::Sub, op(7), op(8)),
                arith(10, ArithOp::Mul, op(1), float(SPIN_S)),
                arith(11, ArithOp::Mul, op(2), float(SPIN_C)),
                arith(12, ArithOp::Add, op(10), op(11)),
                arith(13, ArithOp::Mul, op(3), float(0.5)),
                arith(14, ArithOp::Add, op(13), op(9)),
                arith(15, ArithOp::Mul, op(4), float(0.75)),
                arith(16, ArithOp::Add, op(15), op(12)),
                arith(17, ArithOp::Mul, op(9), op(14)),
                arith(18, ArithOp::Mul, op(12), op(16)),
                arith(19, ArithOp::Sub, op(17), op(18)),
                arith(20, ArithOp::Add, op(5), op(19)),
                assign(
                    21,
                    Rvalue::Checked {
                        op: ArithOp::Sub,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                ),
            ],
            Terminator::TailCall {
                callee: Callee::Direct(spin),
                args: vec![op(21), op(9), op(12), op(14), op(16), op(20)],
            },
        ),
        check(22, 1, ex, 3),
        check(23, 2, ey, 4),
        check(24, 3, eu, 5),
        check(25, 4, ev, 6),
        check(26, 5, eacc, 7),
        // bb7: every Float matched.
        block(
            vec![assign(
                27,
                Rvalue::Box {
                    value: Operand::Const(Const::Int(1)),
                    from: Repr::Int,
                },
            )],
            ret(27),
        ),
        // bb8: a mismatch.
        block(Vec::new(), Terminator::Return(Operand::Const(Const::Nil))),
    ];
    p.item(
        index,
        &format!("spin{index}"),
        &params,
        &locals,
        blocks,
        &[LOC],
    )
}

/// `start(root, k)`, item `index`: publishes its context, then runs
/// rounds of `spin`, each for `k` iterations from root `root`'s start,
/// until `cheby_test_spin_again` says to stop, withdraws the context and
/// returns what the last round returned.
fn spin_root(p: &mut Prog, index: u32, root: u32, spin: DefId) -> DefId {
    let float = |n: f64| Operand::Const(Const::Float(n.to_bits()));
    let foreign = |symbol: &str| {
        Stmt::Eval(Rvalue::CallForeign {
            symbol: symbol.into(),
            sig: ForeignSig {
                params: vec![CType::Ptr, CType::I64],
                ret: CType::Void,
            },
            args: vec![Operand::Const(Const::Ctx), op(0)],
            blocking: false,
        })
    };
    let [x0, y0, u0, v0, acc0] = spin_start(root);
    p.item(
        index,
        &format!("start{index}"),
        &[Repr::Int, Repr::Int],
        // 2 the round's result, 3 another round?
        &[Repr::Value, Repr::Bool],
        vec![
            // bb0: publish the context.
            block(
                vec![foreign("cheby_test_spin_begin")],
                Terminator::Goto {
                    target: bb(1),
                    args: Vec::new(),
                },
            ),
            // bb1: one round, then another or the end.
            block(
                vec![
                    assign(
                        2,
                        Rvalue::Call {
                            callee: Callee::Direct(spin),
                            args: vec![
                                op(1),
                                float(x0),
                                float(y0),
                                float(u0),
                                float(v0),
                                float(acc0),
                            ],
                        },
                    ),
                    assign(
                        3,
                        Rvalue::CallForeign {
                            symbol: "cheby_test_spin_again".into(),
                            sig: ForeignSig {
                                params: vec![CType::Ptr, CType::I64, CType::Ptr],
                                ret: CType::Bool,
                            },
                            args: vec![Operand::Const(Const::Ctx), op(0), op(2)],
                            blocking: false,
                        },
                    ),
                ],
                Terminator::Branch {
                    cond: op(3),
                    then_: bb(3),
                    else_: bb(2),
                },
            ),
            // bb2: withdraw the context.
            block(vec![foreign("cheby_test_spin_end")], ret(2)),
            // bb3: release the round's result for the next.
            block(
                vec![Stmt::Drop(local(2))],
                Terminator::Goto {
                    target: bb(1),
                    args: Vec::new(),
                },
            ),
        ],
        &[],
    )
}

/// A self tail call's loop whose Floats live in registers across the entry
/// check (the slow path is called with `preserve_all` and saves every
/// register) gets them back intact when the slow path switches its fiber
/// out and in again. Two roots with distinct Floats run the loop at once,
/// at one worker, where each runs while the other is switched out, and at
/// two, where a root may resume on the other worker. The host requests an
/// interrupt of each running loop every 200 µs, as the monitor does every
/// 10 ms. Each root repeats its loop from the start until its slow path
/// has handled an interrupt, so a loaded machine that runs the host late
/// cannot let a root end unpreempted, and every round ends with the state
/// the same steps give in Rust.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_float_loop_survives_preemption() {
    const K: u32 = 3_000_000;
    let mut p = Prog::default();
    let mut targets = Vec::new();
    for (root, start_index, spin_index) in [(0, 0, 1), (1, 2, 3)] {
        let expected = spin_after(K, spin_start(root));
        let spin = spin_item(&mut p, spin_index, expected);
        let start = spin_root(&mut p, start_index, root, spin);
        targets.push(p.call(
            start,
            vec![
                Operand::Const(Const::Int(i64::from(root))),
                Operand::Const(Const::Int(i64::from(K))),
            ],
        ));
    }
    let units = p.unwound_units();
    let mut jit = Jit::configured(
        CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        },
        JitOptions::from_env(),
    )
    .unwrap();
    let begin: extern "C" fn(*mut ExecCtx, i64) = cheby_test_spin_begin;
    let again: extern "C" fn(*mut ExecCtx, i64, u64) -> bool = cheby_test_spin_again;
    let end: extern "C" fn(*mut ExecCtx, i64) = cheby_test_spin_end;
    #[expect(clippy::as_conversions, reason = "functions' addresses")]
    let (begin, again, end) = (begin as *const u8, again as *const u8, end as *const u8);
    // SAFETY: all three have the C signature their foreign calls use, and
    // live as long as the process.
    unsafe {
        jit.define_symbol("cheby_test_spin_begin", begin);
        jit.define_symbol("cheby_test_spin_again", again);
        jit.define_symbol("cheby_test_spin_end", end);
    }
    jit.compile(&units).unwrap();
    let clif = jit.take_clif();
    for index in [1, 3] {
        let text = function_text(&clif, &format!("cb$fn$app$spin{index}"));
        assert_loops(text);
        assert!(text.contains("preserve_all"), "{text}");
    }
    for threads in [1, 2] {
        run_spin_roots(&jit, &targets, threads);
    }
}

/// Starts the roots `targets` of `a_float_loop_survives_preemption` on a
/// runtime of `threads` workers, requests an interrupt of each running
/// loop every 200 µs until both ended, and checks what they returned and
/// that each handled an interrupt within [`SPIN_ROUNDS`] rounds.
fn run_spin_roots(jit: &Jit, targets: &[DefId], threads: usize) {
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(threads),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let domain: Arc<Domain> = Domain::new();
    let config = EntryConfig {
        stack_size: 1 << 20,
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let group = DetachedGroup::new();
    let mut running: Vec<_> = targets
        .iter()
        .map(|&target| {
            Some(
                jit.entry(EntryKind::Const(target))
                    .unwrap()
                    .start_root_in(&runtime, &config, &group)
                    .unwrap(),
            )
        })
        .collect();
    let mut outcomes: Vec<Option<EntryOutcome>> = running.iter().map(|_| None).collect();
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(600))
        .unwrap();
    while running.iter().any(Option::is_some) {
        assert!(std::time::Instant::now() < deadline, "the roots hung");
        {
            let roots = SPIN_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
            for &SpinSlot { ctx, .. } in roots.iter() {
                if ctx != 0 {
                    // SAFETY: a published context: its fiber is inside
                    // its loop and cannot withdraw it, and so cannot
                    // end, while this lock is held.
                    unsafe { &*std::ptr::with_exposed_provenance::<ExecCtx>(ctx) }
                        .request_interrupt();
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
        for (handle, outcome) in running.iter_mut().zip(&mut outcomes) {
            if let Some(root) = handle.take() {
                match root.wait_timeout(std::time::Duration::ZERO) {
                    Ok(done) => *outcome = Some(done.unwrap()),
                    Err(root) => *handle = Some(root),
                }
            }
        }
    }
    let slots = *SPIN_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    for (root, outcome) in outcomes.into_iter().enumerate() {
        assert_eq!(
            outcome,
            Some(EntryOutcome::Returned(abi::tag_int(1).unwrap())),
            "root {root} at {threads} threads"
        );
        let SpinSlot {
            ctx,
            rounds,
            handled,
            ..
        } = slots.get(root).copied().unwrap();
        assert_eq!(ctx, 0, "root {root} at {threads} threads");
        assert!(
            handled > 0,
            "root {root} at {threads} threads: no interrupt in {rounds} rounds"
        );
    }
    runtime.shutdown();
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(domain.live_objects(), 0, "at {threads} threads");
    }
}
