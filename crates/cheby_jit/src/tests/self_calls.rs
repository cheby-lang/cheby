//! Self tail calls as jumps inside the function: a top-level function
//! calling itself, or a closure's code calling its own `self`, jumps to a
//! loop header before the entry check instead of `return_call`ing
//! through its slot (D-015, D-453). A REPL session and the inner body of a
//! large frame keep the call (D-043, D-679).

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
/// s = acc + n; tail count(m, s) }`: the sum of `1..=n`.
fn count_prog() -> (Prog, DefId) {
    let mut p = Prog::default();
    let count = def(1, 0);
    p.item(
        0,
        "count",
        &[Repr::Int, Repr::Int],
        &[Repr::Bool, Repr::Value, Repr::Int, Repr::Int],
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
                ],
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

/// `count_prog` compiled with `codegen` and its CLIF captured, with a
/// thunk running `count(n, 0)`.
fn compile_count(codegen: CodegenOptions, n: i64) -> (Jit, Vec<String>, DefId) {
    let (mut p, count) = count_prog();
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
    let (p, count) = count_prog();
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
    let (mut p, count) = count_prog();
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

/// Under a large frame (D-679, a threshold of 0 splits every function),
/// the unchecked inner body keeps its self tail call on the slot, which
/// leads through the outer function's check; the loop still runs in
/// constant stack.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_large_frame_keeps_self_calls_on_the_outer_check() {
    let n = 1_000_000;
    let (jit, clif, target) = compile_count(
        CodegenOptions {
            large_frame_threshold: 0,
            ..CodegenOptions::default()
        },
        n,
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
