//! The return flag under the JIT (D-733, D-698): hand-built MIR, given
//! its unwind edges and pads by the unwind pass
//! ([`Prog::compile_unwound`]), compiled with
//! `CodegenOptions { checked_rc: true }`. An unwinding that
//! starts in a Cheby frame, a C call, an allocation, a checked operation,
//! a `panic`, an entry check (also the outer function of a large frame,
//! D-679) or below a trampoline runs every pad on the way, returns 0 to the
//! catch point, and leaves nothing of the entry's domain alive. Run with
//! `CHEBY_RT_POISON=1` for freed objects to be poisoned (D-458).

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::{CType, ForeignSig, LayoutRef};
use cheby_runtime::ConstEvalLimits;
use cheby_runtime::abi::PanicKind as RtPanicKind;

use super::*;

/// The options of every test here: the checked count operations, which
/// validate every header they touch.
fn flag_options() -> CodegenOptions {
    CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    }
}

/// Gives `p`'s units their unwind edges and pads (D-698), checks them at
/// `Stage::Unwound`, and compiles them with `options`.
fn compile_with(p: Prog, options: CodegenOptions) -> Jit {
    p.compile_unwound_with(options, JitOptions::from_env())
}

fn compile_flag(p: Prog) -> Jit {
    compile_with(p, flag_options())
}

/// The outcome is a panic of `kind`, and every object of the entry was
/// released by the pads.
fn assert_unwound(jit: &Jit, outcome: &EntryOutcome, kind: RtPanicKind) {
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the entry returned: {outcome:?}");
    };
    assert_eq!(report.kind, kind, "{report:?}");
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0, "objects leaked");
    }
}

/// `alloc (first, Nil)` into local `dst`, a 24-byte tuple owning
/// `first`.
fn pair(dst: u32, first: Operand) -> Stmt {
    assign(
        dst,
        Rvalue::Alloc {
            layout: LayoutRef::Tuple(2),
            fields: vec![first, Operand::Const(Const::Nil)],
        },
    )
}

/// `alloc Some(Nil)`, written here as `alloc (Nil, Nil)` into local
/// `dst`: an object a frame holds.
fn some_nil(dst: u32) -> Stmt {
    pair(dst, Operand::Const(Const::Nil))
}

/// `panic Explicit("boom")` at `str#1`'s location, the function's second
/// string.
fn boom() -> Terminator {
    Terminator::Panic {
        kind: PanicKind::Explicit,
        message: Some(Operand::Const(Const::Str(StrId::from_u32(2)))),
        loc: StrId::from_u32(1),
    }
}

/// The strings [`boom`] uses: a location, then the message.
const BOOM: [&str; 2] = [LOC, "boom"];

/// A Cheby call: `g` panics, and `f` holds an object across the call:
/// `f() { s = alloc Some(Nil); r = call g(); drop s; return r }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_cheby_call() {
    let mut p = Prog::default();
    let g = p.item(1, "g", &[], &[], vec![block(Vec::new(), boom())], &BOOM);
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                some_nil(0),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Direct(g),
                        args: Vec::new(),
                    },
                ),
                Stmt::Drop(local(0)),
            ],
            1,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = compile_flag(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("checked above")
    };
    assert_eq!(report.message, "boom");
}

/// A C call: the runtime helper `equal` panics on a closure (§11.6), and
/// the function holds the closure and another object:
/// `f() { s = alloc Some(Nil); k = alloc closure(code);
/// e = equal(k, k, LOC); drop k; drop s; return box e }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_c_call() {
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
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value, Repr::Bool, Repr::Value],
        straight(
            vec![
                some_nil(0),
                assign(
                    1,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(0),
                        fields: vec![Operand::Const(Const::Code(key))],
                    },
                ),
                assign(
                    2,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::Equal,
                        args: vec![op(1), op(1), Operand::Const(Const::Str(StrId::from_u32(1)))],
                    },
                ),
                Stmt::Drop(local(1)),
                Stmt::Drop(local(0)),
                assign(
                    3,
                    Rvalue::Box {
                        value: op(2),
                        from: Repr::Bool,
                    },
                ),
            ],
            3,
        ),
        &[LOC],
    );
    let target = p.call(f, Vec::new());
    let jit = compile_flag(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::EqualityOnFunction);
}

/// An allocation that fails: constant evaluation's memory budget refuses
/// the second object (D-536), so `cheby_rt_alloc` unwinds and returns 0:
/// `f() { a = alloc (Nil, Nil); b = alloc (a, Nil); return b }`, 24 bytes
/// each under a budget of 32.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_an_allocation_failure() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value],
        straight(vec![some_nil(0), pair(1, op(0))], 1),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = compile_flag(p);
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    let config = EntryConfig {
        stack_size: cheby_runtime::entry::MIN_STACK_SIZE,
        ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
            memory: Some(32),
            ..ConstEvalLimits::default()
        }))
    };
    let outcome = entry.run(&config).unwrap();
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
}

/// A checked overflow: `f() { s = alloc Some(Nil); n = MAX + 1; drop s;
/// return box n }`. The panic block calls `cheby_rt_panic`, which returns
/// under the flag, and jumps to the pad.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_checked_overflow() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[Repr::Int],
        &[Repr::Value, Repr::Int, Repr::Value],
        straight(
            vec![
                some_nil(1),
                assign(
                    2,
                    Rvalue::Checked {
                        op: ArithOp::Add,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                ),
                Stmt::Drop(local(1)),
                assign(
                    3,
                    Rvalue::Box {
                        value: op(2),
                        from: Repr::Int,
                    },
                ),
            ],
            3,
        ),
        &[LOC],
    );
    let target = p.call(f, vec![Operand::Const(Const::Int(i64::MAX))]);
    let fine = p.call(f, vec![Operand::Const(Const::Int(41))]);
    let jit = compile_flag(p);
    assert_eq!(returned(run_const(&jit, fine)), abi::tag_int(42).unwrap());
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Overflow);
}

/// An explicit panic: `f(x) { s = alloc (x, Nil); panic "boom" }`, whose
/// pad drops `s` (and with it the argument).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_an_explicit_panic() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[Repr::Value],
        &[Repr::Value],
        vec![block(vec![pair(1, op(0))], boom())],
        &BOOM,
    );
    // `start() { a = alloc Some(Nil); r = call f(a); return r }`.
    let start = p.item(
        1,
        "start",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                some_nil(0),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Direct(f),
                        args: vec![op(0)],
                    },
                ),
            ],
            1,
        ),
        &[],
    );
    let target = p.call(start, Vec::new());
    let jit = compile_flag(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
}

/// `deep(x) { s = alloc Some(Nil); dup x; r = call deep(x); drop s;
/// drop x; return r }` and `start() { a = alloc Some(Nil); r = call
/// deep(a); return r }`: an endless recursion that overflows at an entry
/// check, whose entry pad drops `x`; every caller's pad drops its `s` and
/// `x`.
fn deep_program() -> (Prog, DefId) {
    let mut p = Prog::default();
    let me = def(1, 0);
    p.item(
        0,
        "deep",
        &[Repr::Value],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                some_nil(1),
                Stmt::Dup(local(0)),
                assign(
                    2,
                    Rvalue::Call {
                        callee: Callee::Direct(me),
                        args: vec![op(0)],
                    },
                ),
                Stmt::Drop(local(1)),
                Stmt::Drop(local(0)),
            ],
            2,
        ),
        &[],
    );
    let start = p.item(
        1,
        "start",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                some_nil(0),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Direct(me),
                        args: vec![op(0)],
                    },
                ),
            ],
            1,
        ),
        &[],
    );
    let target = p.call(start, Vec::new());
    (p, target)
}

/// A stack overflow at the entry check (D-124, D-700).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_stack_overflow() {
    let (p, target) = deep_program();
    let jit = compile_flag(p);
    let outcome = run_with_stack(&jit, target, cheby_runtime::entry::MIN_STACK_SIZE);
    assert_unwound(&jit, &outcome, RtPanicKind::StackOverflow);
}

/// The same overflow when every function is split into an outer function
/// and its large-frame body (D-679, threshold 0): the outer function's
/// own pad drops the parameter.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_stack_overflow_of_a_large_frame() {
    let (p, target) = deep_program();
    let options = CodegenOptions {
        large_frame_threshold: 0,
        capture_clif: true,
        ..flag_options()
    };
    let mut jit = compile_with(p, options);
    let clif = jit.take_clif().join("\n");
    assert!(clif.contains("cb$big$fn$app$deep"), "not split:\n{clif}");
    assert!(clif.contains("cheby_rt_stack_slow_frame"), "{clif}");
    let outcome = run_with_stack(&jit, target, cheby_runtime::entry::MIN_STACK_SIZE);
    assert_unwound(&jit, &outcome, RtPanicKind::StackOverflow);
}

/// `cb$tramp$1` as `closure::trampolines` lowers it: the call consumes
/// the closure, which code generation dups at the entry (D-526).
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

/// A trampoline: `map::upsert` calls the closure through `cb$tramp$1`
/// (D-526), and the closure panics. The trampoline returns 0 with the flag
/// set, the kernel's guard passes the unwinding on (D-697), and `f`'s pad
/// drops the map and the closure:
/// `f() { m = map::new(); k = alloc closure(code); one = box 1;
/// r = map::upsert(m, one, k); drop k; drop m; return r }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_through_a_trampoline() {
    let mut p = Prog::default();
    let owner = def(1, 0);
    let key = FnKey::Closure { owner, index: 0 };
    // `update(self, old) { panic "boom" }`.
    p.functions.push(function(
        key,
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[],
        Repr::Value,
        vec![block(Vec::new(), boom())],
        &BOOM,
    ));
    p.functions.push(trampoline_1());
    let foreign = |symbol: &str, params: usize, args: Vec<Operand>| Rvalue::CallForeign {
        symbol: symbol.into(),
        sig: ForeignSig {
            params: vec![CType::Ptr; params],
            ret: CType::Ptr,
        },
        args,
    };
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value, Repr::Value, Repr::Value],
        straight(
            vec![
                assign(0, foreign("cheby_map_new", 0, Vec::new())),
                assign(
                    1,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(0),
                        fields: vec![Operand::Const(Const::Code(key))],
                    },
                ),
                assign(
                    2,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(1)),
                        from: Repr::Int,
                    },
                ),
                assign(3, foreign("cheby_map_upsert", 3, vec![op(0), op(2), op(1)])),
                Stmt::Drop(local(2)),
                Stmt::Drop(local(1)),
                Stmt::Drop(local(0)),
            ],
            3,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = compile_flag(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
}

/// A `main` entry thunk returns its exit status, and 0 is success, so
/// under the flag the host never decides from the result that the entry
/// unwound (plan §2.5): a `main` that returns 0 normally is `Returned(0)`
/// with no report, and one whose `main` panics is `Panicked`, though its
/// thunk returns 0 as well. `thunk() { r = call main(); drop r;
/// return 0 }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_flag_main_returning_zero_is_not_an_unwinding() {
    for panics in [false, true] {
        let mut p = Prog::default();
        let main = if panics {
            p.item(0, "main", &[], &[], vec![block(Vec::new(), boom())], &BOOM)
        } else {
            p.item(
                0,
                "main",
                &[],
                &[],
                vec![block(
                    Vec::new(),
                    Terminator::Return(Operand::Const(Const::Nil)),
                )],
                &[],
            )
        };
        p.thunks.push(function(
            FnKey::Entry(EntryKind::Main(main)),
            Abi::C,
            &[],
            &[Repr::Value],
            Repr::I64,
            vec![block(
                vec![
                    assign(
                        0,
                        Rvalue::Call {
                            callee: Callee::Direct(main),
                            args: Vec::new(),
                        },
                    ),
                    Stmt::Drop(local(0)),
                ],
                Terminator::Return(Operand::Const(Const::Sized {
                    repr: Repr::I64,
                    bits: 0,
                })),
            )],
            &[],
        ));
        let decl = p.callees.iter().find(|c| c.def == main).unwrap().clone();
        p.thunk_callees.push(decl);
        p.thunk_names.push(name(main, "app", "main"));
        let jit = compile_flag(p);
        let entry = jit.entry(EntryKind::Main(main)).unwrap();
        let outcome = entry.run(&EntryConfig::new(EntryMode::Test)).unwrap();
        if panics {
            assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
        } else {
            assert_eq!(outcome, EntryOutcome::Returned(0));
            if cheby_runtime::LEAK_CHECK_ENABLED {
                assert_eq!(jit.domain().live_objects(), 0);
            }
        }
    }
}
