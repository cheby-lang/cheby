//! Tail calls in constant stack (E2), kernels calling closures through a
//! trampoline (D-526), runtime helpers (D-520), and checked reference
//! counting (plan §4.3.4).

use super::objects::{CHILD, run_child};
use super::*;

const TEN_MILLION: i64 = 10_000_000;

/// `count(n: Int) { if n == 0 { return Nil } m = n - 1; tail next(m) }`.
fn countdown(index: u32, next: Callee) -> Function {
    let key = FnKey::Item(def(1, index));
    function(
        key,
        Abi::Cheby,
        &[Repr::Int],
        &[Repr::Bool, Repr::Int],
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
            block(Vec::new(), Terminator::Return(Operand::Const(Const::Nil))),
            block(
                vec![assign(
                    2,
                    Rvalue::Checked {
                        op: ArithOp::Sub,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                )],
                Terminator::TailCall {
                    callee: next,
                    args: vec![op(2)],
                },
            ),
        ],
        &[LOC],
    )
}

/// E2: ten million mutual tail calls between two top-level functions, each
/// through the other's slot (`return_call_indirect`), on an 8 MiB entry
/// stack: a stack that grew per call would overflow after about 100,000.
#[test]
fn ten_million_mutual_tail_calls_through_slots() {
    let mut p = Prog::default();
    let (even, odd) = (def(1, 0), def(1, 1));
    p.functions.push(countdown(0, Callee::Direct(odd)));
    p.declare(even, "even", &[Repr::Int]);
    p.functions.push(countdown(1, Callee::Direct(even)));
    p.declare(odd, "odd", &[Repr::Int]);
    let target = p.call(even, vec![Operand::Const(Const::Int(TEN_MILLION))]);
    let mut jit = Jit::with_options(CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    })
    .unwrap();
    jit.compile(&p.units()).unwrap();
    let clif = jit.take_clif().join("\n");
    assert!(clif.contains("return_call_indirect"), "{clif}");
    let outcome = run_with_stack(&jit, target, cheby_runtime::entry::DEFAULT_STACK_SIZE);
    assert_eq!(outcome, EntryOutcome::Returned(static_nil()));
}

/// The control of the test above: the same countdown with an ordinary call
/// and a return instead of the tail call grows the stack per call, and the
/// entry check stops it with a stack overflow long before ten million (E3).
#[test]
fn the_same_recursion_without_tail_calls_overflows() {
    let mut p = Prog::default();
    let (even, odd) = (def(1, 0), def(1, 1));
    for (index, next) in [(0, odd), (1, even)] {
        let mut f = countdown(index, Callee::Direct(next));
        f.locals
            .push(LocalDecl {
                repr: Repr::Value,
                name: None,
                ty: None,
            })
            .unwrap();
        let last = f.blocks.iter_mut().last().unwrap();
        last.stmts.push(assign(
            3,
            Rvalue::Call {
                callee: Callee::Direct(next),
                args: vec![op(2)],
            },
        ));
        last.term = ret(3);
        p.functions.push(f);
    }
    p.declare(even, "even", &[Repr::Int]);
    p.declare(odd, "odd", &[Repr::Int]);
    let target = p.call(even, vec![Operand::Const(Const::Int(TEN_MILLION))]);
    let jit = p.compile_unwound();
    let outcome = run_with_stack(&jit, target, cheby_runtime::entry::DEFAULT_STACK_SIZE);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("ten million nested calls returned: {outcome:?}");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::StackOverflow);
}

/// E2 through closures: a closure that tail-calls itself through `self`
/// (the uniform convention, `return_call_indirect`) ten million times in
/// constant stack, unboxing and boxing its counter on the way; nothing
/// leaks.
#[test]
#[expect(clippy::too_many_lines, reason = "one hand-built MIR program")]
fn ten_million_tail_calls_through_a_closure() {
    let mut p = Prog::default();
    let owner = def(1, 0);
    let key = FnKey::Closure { owner, index: 0 };
    // `loop(self, n) { k = unbox n; drop n; if k == 0 { drop self;
    // return Nil } m = k - 1; w = box m; tail self(w) }`.
    p.functions.push(function(
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
                    callee: Callee::Indirect(local(0)),
                    args: vec![op(5)],
                },
            ),
        ],
        &[LOC],
    ));
    // `start() { k = alloc closure(code); n = box 10_000_000; tail k(n) }`.
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
                        fields: vec![Operand::Const(Const::Code(key))],
                    },
                ),
                assign(
                    1,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(TEN_MILLION)),
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
    let jit = p.compile();
    let outcome = run_with_stack(&jit, target, cheby_runtime::entry::DEFAULT_STACK_SIZE);
    assert_eq!(outcome, EntryOutcome::Returned(static_nil()));
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// A test kernel: applies the borrowed closure to the borrowed argument
/// through the runtime's re-entry (D-526), which takes the argument owned.
unsafe extern "C" fn cheby_test_apply(closure: u64, argument: u64) -> u64 {
    // SAFETY: the JIT calls this kernel from an entry with a borrowed
    // closure of one parameter and a borrowed uniform word; the argument is
    // dup'd because `call` consumes it.
    unsafe {
        cheby_runtime::rc::dup(argument);
        cheby_runtime::callback::call(closure, &[argument])
    }
}

/// `cb$tramp$1` as `closure::trampolines` lowers it (plan §4.3.4): C ABI,
/// `ctx`, the closure and one owned word; the uniform call consumes the
/// closure, and returns the owned result. The runtime lends the closure
/// (D-526), so code generation adds the reference the call consumes at
/// the trampoline's entry; the MIR has no `dup`.
fn trampoline_1() -> Function {
    function(
        FnKey::Trampoline(1),
        Abi::C,
        &[Repr::Value, Repr::Value],
        &[Repr::Value],
        Repr::Value,
        straight(
            vec![assign(
                2,
                Rvalue::Call {
                    callee: Callee::Indirect(local(0)),
                    args: vec![op(1)],
                },
            )],
            2,
        ),
        &[],
    )
}

/// Re-entry (D-526): Cheby code calls a Rust kernel registered as a
/// symbol, the kernel calls `cheby_runtime::callback::call`, which reaches
/// a Cheby closure through `cb$tramp$1`, passed by `Entry::run` in
/// `EntryConfig.callbacks`. The closure adds 1 to its argument, so the
/// result proves it ran.
#[test]
fn kernels_call_closures_through_trampolines() {
    let mut p = Prog::default();
    let owner = def(1, 0);
    let key = FnKey::Closure { owner, index: 0 };
    // `inc(self, a) { drop self; n = unbox a; drop a; m = n + 1;
    // return box m }`.
    p.functions.push(function(
        key,
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Int, Repr::Int, Repr::Value],
        Repr::Value,
        straight(
            vec![
                Stmt::Drop(local(0)),
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
                    Rvalue::Checked {
                        op: ArithOp::Add,
                        lhs: op(2),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                ),
                assign(
                    4,
                    Rvalue::Box {
                        value: op(3),
                        from: Repr::Int,
                    },
                ),
            ],
            4,
        ),
        &[LOC],
    ));
    p.functions.push(trampoline_1());
    // `f() { k = alloc closure(code); a = box 41;
    // r = call_foreign cheby_test_apply(k, a); drop k; drop a; return r }`.
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(0),
                        fields: vec![Operand::Const(Const::Code(key))],
                    },
                ),
                assign(
                    1,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(41)),
                        from: Repr::Int,
                    },
                ),
                assign(
                    2,
                    Rvalue::CallForeign {
                        symbol: "cheby_test_apply".into(),
                        sig: cheby_mir::ForeignSig {
                            params: vec![cheby_mir::CType::Ptr, cheby_mir::CType::Ptr],
                            ret: cheby_mir::CType::Ptr,
                        },
                        args: vec![op(0), op(1)],
                        blocking: false,
                    },
                ),
                Stmt::Drop(local(0)),
                Stmt::Drop(local(1)),
            ],
            2,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let mut jit = Jit::new().unwrap();
    let apply: unsafe extern "C" fn(u64, u64) -> u64 = cheby_test_apply;
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let address = apply as *const u8;
    // SAFETY: `cheby_test_apply` is a function with the C signature the
    // `@external` call above uses, and it lives as long as the process.
    unsafe { jit.define_symbol("cheby_test_apply", address) };
    jit.compile(&p.units()).unwrap();
    assert!(jit.callbacks().t1.is_some());
    assert!(jit.callbacks().t0.is_none());
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(42).unwrap());
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

fn helper(func: RuntimeFn, args: Vec<Operand>) -> Rvalue {
    Rvalue::CallRuntime { func, args }
}

fn s(n: u32) -> Operand {
    Operand::Const(Const::Str(StrId::from_u32(n)))
}

fn int(n: i64) -> Operand {
    Operand::Const(Const::Int(n))
}

/// The runtime helpers W1-B provides (D-520), called with `ctx` where they
/// take one: building and comparing strings, structural equality, debug
/// text, and the effect check, which does nothing outside constant
/// evaluation.
#[test]
#[expect(clippy::too_many_lines, reason = "one hand-built MIR program")]
fn runtime_helpers() {
    let mut p = Prog::default();
    let strings = ["ab", "cd", "x", "cheby_io_print", LOC];
    // `build() { s = with_capacity(1); t = append(s, "ab");
    // u = append(t, "cd"); return u }`.
    let build = p.item(
        0,
        "build",
        &[],
        &[Repr::Value; 3],
        straight(
            vec![
                assign(0, helper(RuntimeFn::StrWithCapacity, vec![int(1)])),
                assign(1, helper(RuntimeFn::StrAppend, vec![op(0), s(1)])),
                assign(2, helper(RuntimeFn::StrAppend, vec![op(1), s(2)])),
            ],
            2,
        ),
        &strings,
    );
    let boolean = |p: &mut Prog, index: u32, func: RuntimeFn, a: Operand, b: Operand| {
        p.item(
            index,
            &format!("b{index}"),
            &[],
            &[Repr::Bool, Repr::Value],
            straight(
                vec![
                    assign(0, helper(func, two_and_location(func, a, b))),
                    assign(
                        1,
                        Rvalue::Box {
                            value: op(0),
                            from: Repr::Bool,
                        },
                    ),
                ],
                1,
            ),
            &strings,
        )
    };
    let str_eq = boolean(&mut p, 1, RuntimeFn::StrEq, s(1), s(1));
    let str_ne = boolean(&mut p, 2, RuntimeFn::StrEq, s(1), s(2));
    let equal = boolean(&mut p, 3, RuntimeFn::Equal, s(3), s(3));
    let unequal = boolean(
        &mut p,
        4,
        RuntimeFn::Equal,
        Operand::Const(Const::Nil),
        s(3),
    );
    // `debug() { w = box 42; d = debug(w); return d }`.
    let debug = p.item(
        5,
        "debug",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Box {
                        value: int(42),
                        from: Repr::Int,
                    },
                ),
                assign(1, helper(RuntimeFn::Debug, vec![op(0)])),
            ],
            1,
        ),
        &[],
    );
    let effect = p.item(
        6,
        "effect",
        &[],
        &[],
        vec![block(
            vec![Stmt::Eval(helper(RuntimeFn::EffectCheck, vec![s(4)]))],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &strings,
    );
    let targets = [
        p.call(build, Vec::new()),
        p.call(str_eq, Vec::new()),
        p.call(str_ne, Vec::new()),
        p.call(equal, Vec::new()),
        p.call(unequal, Vec::new()),
        p.call(debug, Vec::new()),
        p.call(effect, Vec::new()),
    ];
    let jit = p.compile();
    let appended = returned(run_const(&jit, targets[0]));
    assert_eq!(text_of(appended), "abcd");
    release(&jit, appended);
    assert_eq!(returned(run_const(&jit, targets[1])), truth(true));
    assert_eq!(returned(run_const(&jit, targets[2])), truth(false));
    assert_eq!(returned(run_const(&jit, targets[3])), truth(true));
    assert_eq!(returned(run_const(&jit, targets[4])), truth(false));
    let text = returned(run_const(&jit, targets[5]));
    assert_eq!(text_of(text), "42");
    release(&jit, text);
    assert_eq!(returned(run_const(&jit, targets[6])), static_nil());
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// The arguments of a comparison helper: `equal` also takes its
/// location, `str#5` in `runtime_helpers` (D-556).
fn two_and_location(func: RuntimeFn, a: Operand, b: Operand) -> Vec<Operand> {
    if func == RuntimeFn::Equal {
        vec![a, b, s(5)]
    } else {
        vec![a, b]
    }
}

/// A structural `==` that reaches a function value panics at the `==`'s
/// location (§11.6, D-449, D-556), not at `<unknown location>`.
#[test]
fn equality_on_functions_panics_at_its_location() {
    let mut p = Prog::default();
    let owner = def(1, 0);
    let key = FnKey::Closure { owner, index: 0 };
    p.functions.push(function(
        key,
        Abi::Cheby,
        &[Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            vec![Stmt::Drop(local(0))],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    ));
    // `f() { k = alloc closure(code); e = equal(k, k, LOC); drop k;
    // return box e }`.
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Bool, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(0),
                        fields: vec![Operand::Const(Const::Code(key))],
                    },
                ),
                assign(1, helper(RuntimeFn::Equal, vec![op(0), op(0), s(1)])),
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Bool,
                    },
                ),
            ],
            2,
        ),
        &[LOC],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile_unwound();
    let EntryOutcome::Panicked(report) = run_const(&jit, target) else {
        panic!("comparing a closure did not panic");
    };
    assert_eq!(
        report.kind,
        cheby_runtime::abi::PanicKind::EqualityOnFunction
    );
    assert_eq!(report.location, LOC);
}

fn truth(b: bool) -> u64 {
    static_word(if b {
        abi::layout::TRUE
    } else {
        abi::layout::FALSE
    })
}

/// The list helpers of W1-E (D-520): a push onto the static empty list,
/// the length, an element, a slice and an append.
#[test]
fn list_helpers() {
    let mut p = Prog::default();
    // `list() { a = push([], box 7); b = push(a, box 8); n = length(b);
    // x = get(b, 1); s = slice(b, 1, 2); c = append(b, s); m = length(c);
    // … return (n, x, m) }`.
    let boxed = |n| Rvalue::Box {
        value: int(n),
        from: Repr::Int,
    };
    let f = p.item(
        0,
        "list",
        &[],
        &[
            Repr::Value,
            Repr::Value,
            Repr::Value,
            Repr::Value,
            Repr::Int,
            Repr::Value,
            Repr::Value,
            Repr::Value,
            Repr::Int,
            Repr::Value,
            Repr::Value,
            Repr::Value,
        ],
        straight(
            vec![
                assign(0, boxed(7)),
                assign(
                    1,
                    helper(
                        RuntimeFn::ListPush,
                        vec![Operand::Const(Const::EmptyList), op(0)],
                    ),
                ),
                assign(2, boxed(8)),
                assign(3, helper(RuntimeFn::ListPush, vec![op(1), op(2)])),
                assign(4, helper(RuntimeFn::ListLength, vec![op(3)])),
                assign(5, helper(RuntimeFn::ListGet, vec![op(3), int(1)])),
                assign(6, helper(RuntimeFn::ListSlice, vec![op(3), int(1), int(2)])),
                assign(7, helper(RuntimeFn::ListAppend, vec![op(3), op(6)])),
                assign(8, helper(RuntimeFn::ListLength, vec![op(7)])),
                Stmt::Drop(local(6)),
                Stmt::Drop(local(7)),
                assign(
                    9,
                    Rvalue::Box {
                        value: op(4),
                        from: Repr::Int,
                    },
                ),
                assign(
                    10,
                    Rvalue::Box {
                        value: op(8),
                        from: Repr::Int,
                    },
                ),
                assign(
                    11,
                    Rvalue::Alloc {
                        layout: LayoutRef::Tuple(3),
                        fields: vec![op(9), op(5), op(10)],
                    },
                ),
            ],
            11,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile();
    let tuple = returned(run_const(&jit, target));
    assert_eq!(int_of(read_word(tuple, 8)), 2, "length");
    assert_eq!(int_of(read_word(tuple, 16)), 8, "element 1");
    assert_eq!(int_of(read_word(tuple, 24)), 3, "length after append");
    release(&jit, tuple);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `list_slice_owned` (W5-D's `cheby_rt_list_slice_owned`), which the RC
/// pass emits for a slice of a list used for the last time: a unique list
/// becomes its own slice, and a shared one stays as it was.
#[test]
fn owned_list_slices() {
    let mut p = Prog::default();
    // `list() { a = [7, 8, 9]; s = slice_owned(a, 1, 3); n = length(s);
    // x = get(s, 0); b = [1]; dup b; e = slice_owned(b, 1, 1);
    // m = length(b); k = length(e); … return (n, x, m, k) }`.
    let boxed = |n| Rvalue::Box {
        value: int(n),
        from: Repr::Int,
    };
    let boxed_local = |n| Rvalue::Box {
        value: op(n),
        from: Repr::Int,
    };
    let mut reprs = vec![Repr::Value; 18];
    for n in [7, 12, 13] {
        reprs[n] = Repr::Int;
    }
    let f = p.item(
        0,
        "list",
        &[],
        &reprs,
        straight(
            vec![
                assign(0, boxed(7)),
                assign(
                    1,
                    helper(
                        RuntimeFn::ListPush,
                        vec![Operand::Const(Const::EmptyList), op(0)],
                    ),
                ),
                assign(2, boxed(8)),
                assign(3, helper(RuntimeFn::ListPush, vec![op(1), op(2)])),
                assign(4, boxed(9)),
                assign(5, helper(RuntimeFn::ListPush, vec![op(3), op(4)])),
                assign(
                    6,
                    helper(RuntimeFn::ListSliceOwned, vec![op(5), int(1), int(3)]),
                ),
                assign(7, helper(RuntimeFn::ListLength, vec![op(6)])),
                assign(8, helper(RuntimeFn::ListGet, vec![op(6), int(0)])),
                assign(9, boxed(1)),
                assign(
                    10,
                    helper(
                        RuntimeFn::ListPush,
                        vec![Operand::Const(Const::EmptyList), op(9)],
                    ),
                ),
                Stmt::Dup(local(10)),
                assign(
                    11,
                    helper(RuntimeFn::ListSliceOwned, vec![op(10), int(1), int(1)]),
                ),
                assign(12, helper(RuntimeFn::ListLength, vec![op(10)])),
                assign(13, helper(RuntimeFn::ListLength, vec![op(11)])),
                Stmt::Drop(local(6)),
                Stmt::Drop(local(10)),
                Stmt::Drop(local(11)),
                assign(14, boxed_local(7)),
                assign(15, boxed_local(12)),
                assign(16, boxed_local(13)),
                assign(
                    17,
                    Rvalue::Alloc {
                        layout: LayoutRef::Tuple(4),
                        fields: vec![op(14), op(8), op(15), op(16)],
                    },
                ),
            ],
            17,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile();
    let tuple = returned(run_const(&jit, target));
    assert_eq!(int_of(read_word(tuple, 8)), 2, "length of the slice");
    assert_eq!(int_of(read_word(tuple, 16)), 8, "its first element");
    assert_eq!(int_of(read_word(tuple, 24)), 1, "the shared list is intact");
    assert_eq!(int_of(read_word(tuple, 32)), 0, "an empty slice");
    release(&jit, tuple);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `JitOptions::checked_rc` (on under `CHEBY_RT_POISON=1`) makes every
/// count operation call the runtime's checked `cheby_rt_dup`: a tuple
/// freed and then dup'd ends the process with the use-after-free message.
/// The poisoning mode is read once per process, so this runs in a child.
#[test]
fn checked_rc_catches_a_use_after_free() {
    if std::env::var_os(CHILD).is_some() {
        assert!(JitOptions::from_env().checked_rc);
        let mut p = Prog::default();
        let f = p.item(
            0,
            "f",
            &[],
            &[Repr::Value],
            vec![block(
                vec![
                    assign(
                        0,
                        Rvalue::Alloc {
                            layout: LayoutRef::Tuple(1),
                            fields: vec![Operand::Const(Const::Nil)],
                        },
                    ),
                    Stmt::Drop(local(0)),
                    Stmt::Dup(local(0)),
                ],
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        let target = p.call(f, Vec::new());
        let jit = p.compile();
        let _ = run_const(&jit, target);
        return;
    }
    if !cfg!(debug_assertions) {
        // Poisoning exists only in builds with debug assertions.
        return;
    }
    let stderr = run_child(
        "tests::special::checked_rc_catches_a_use_after_free",
        &[("CHEBY_RT_POISON", "1")],
    );
    assert!(stderr.contains("use after free"), "{stderr}");
}

/// Without `checked_rc`, count operations are inline: no call on the fast
/// path, which tests the tag bit and the count; with it, a plain call.
#[test]
fn count_operations_are_inline_unless_checked() {
    let clif = |checked_rc: bool| {
        let mut p = Prog::default();
        p.item(
            0,
            "f",
            &[Repr::Value],
            &[],
            vec![block(
                vec![Stmt::Dup(local(0)), Stmt::Drop(local(0))],
                ret(0),
            )],
            &[],
        );
        let mut jit = Jit::configured(
            CodegenOptions {
                capture_clif: true,
                ..CodegenOptions::default()
            },
            JitOptions {
                checked_rc,
                stub_unresolved: false,
            },
        )
        .unwrap();
        jit.compile(&p.units()).unwrap();
        jit.take_clif().join("\n")
    };
    // The tag test prints as `band` or, when its operand is defined in
    // another block, with an explicit type: `band.i64`.
    let inline = clif(false);
    assert!(inline.contains(" = band"), "{inline}");
    assert!(inline.contains("store notrap aligned"), "{inline}");
    let checked = clif(true);
    assert!(!checked.contains(" = band"), "{checked}");
    assert!(!checked.contains("store notrap aligned"), "{checked}");
    assert!(checked.contains("= cheby_rt_dup"), "{checked}");
    assert!(checked.contains("= cheby_rt_drop"), "{checked}");
}

/// `f(self) { drop self; return nil }`: a closure body of `owner`.
fn nil_closure(owner: DefId) -> Function {
    function(
        FnKey::Closure { owner, index: 0 },
        Abi::Cheby,
        &[Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            vec![Stmt::Drop(local(0))],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    )
}

/// `k = alloc closure/0(code); r = call k(); return r`.
fn calls_closure(key: FnKey, owner: DefId) -> Function {
    function(
        key,
        Abi::Cheby,
        &[],
        &[Repr::Value, Repr::Value],
        Repr::Value,
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(0),
                        fields: vec![Operand::Const(Const::Code(FnKey::Closure {
                            owner,
                            index: 0,
                        }))],
                    },
                ),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Indirect(local(0)),
                        args: Vec::new(),
                    },
                ),
            ],
            1,
        ),
        &[],
    )
}

/// A test's name is free text and may equal a function's: the closures of
/// a test named `spin` and of a function `spin` of the same module get
/// different symbols (the test's by its ordinal, D-456), so one batch
/// compiles both, and the function's closure runs.
#[test]
fn closures_of_a_test_and_a_function_of_the_same_name() {
    let mut p = Prog::default();
    let spin = def(1, 0);
    let test = def(1, 1);
    p.functions.push(calls_closure(FnKey::Item(spin), spin));
    p.functions.push(nil_closure(spin));
    p.callees.push(CalleeDecl {
        def: spin,
        sig: cheby_sig(&[]),
    });
    p.names.push(name(spin, "app", "spin"));
    let target = p.call(spin, Vec::new());
    let [code, entries] = p.units();
    let tests = MirModule {
        unit: UnitKind::Tests(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![calls_closure(FnKey::Test(test), test), nil_closure(test)],
        callees: Vec::new(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![name(test, "app", "spin")],
    };
    let mut jit = Jit::configured(
        CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        },
        JitOptions::default(),
    )
    .unwrap();
    jit.compile(&[code, tests, entries]).unwrap();
    assert_eq!(returned(run_const(&jit, target)), static_nil());
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}
