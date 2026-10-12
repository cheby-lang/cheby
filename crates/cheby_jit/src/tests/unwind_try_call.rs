//! The `try_call` prototype under the JIT (D-696, D-698): hand-built MIR,
//! given its unwind edges and pads by the unwind pass, compiled with
//! `CodegenOptions { unwind: TryCall, checked_rc: true }`. The cases are
//! those of `unwind_flag`: an unwinding that starts in a Cheby frame, a C
//! call, an allocation, a checked operation, a `panic`, an entry check
//! (also the outer function of a large frame, D-679) or below a trampoline
//! resumes at each frame's handler, runs every pad on the way, reaches the
//! catch point, and leaves nothing of the entry's domain alive. Run with
//! `CHEBY_RT_POISON=1` for freed objects to be poisoned (D-458).
//!
//! Also: the handler tables exist only under the prototype, the exit stubs
//! are named and declared deterministically, and the registrations end
//! with the code.

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::{CType, ForeignSig, LayoutRef, Stage, validate};
use cheby_runtime::ConstEvalLimits;
use cheby_runtime::abi::{PanicKind as RtPanicKind, UnwindBackend};

use super::*;

/// The options of every test here: the `try_call` prototype, with the
/// checked count operations, which validate every header they touch.
fn try_call_options() -> CodegenOptions {
    CodegenOptions {
        unwind: UnwindBackend::TryCall,
        checked_rc: true,
        ..CodegenOptions::default()
    }
}

/// Gives `p`'s units their unwind edges and pads (D-698), checks them at
/// `Stage::Unwound`, and compiles them with `options`.
fn compile_with(p: Prog, options: CodegenOptions) -> Jit {
    let mut units = p.units();
    for unit in &mut units {
        cheby_mir::unwind::insert_module(unit).unwrap();
        validate(unit, Stage::Unwound).unwrap();
    }
    let mut jit = Jit::with_options(options).unwrap();
    jit.compile(&units).unwrap();
    jit
}

fn compile_try_call(p: Prog) -> Jit {
    compile_with(p, try_call_options())
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

/// `alloc (first, Nil)` into local `dst`, a 24-byte tuple owning `first`.
fn pair(dst: u32, first: Operand) -> Stmt {
    assign(
        dst,
        Rvalue::Alloc {
            layout: LayoutRef::Tuple(2),
            fields: vec![first, Operand::Const(Const::Nil)],
        },
    )
}

/// `alloc (Nil, Nil)` into local `dst`: an object a frame holds.
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
fn unwind_try_call_through_a_cheby_call() {
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
    let jit = compile_try_call(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("checked above")
    };
    assert_eq!(report.message, "boom");
}

/// Frames with nothing to drop keep plain calls, with no handler, and the
/// unwinder passes them: `g` panics, `mid` owns nothing across its call
/// of `g`, and `f` holds an object across its call of `mid`:
/// `mid() { r = call g(); return r }`,
/// `f() { s = alloc Some(Nil); r = call mid(); drop s; return r }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_a_frame_without_cleanup() {
    let mut p = Prog::default();
    let g = p.item(2, "g", &[], &[], vec![block(Vec::new(), boom())], &BOOM);
    let mid = p.item(
        1,
        "mid",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Call {
                    callee: Callee::Direct(g),
                    args: Vec::new(),
                },
            )],
            0,
        ),
        &[],
    );
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
                        callee: Callee::Direct(mid),
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
    let options = CodegenOptions {
        capture_clif: true,
        ..try_call_options()
    };
    let mut jit = compile_with(p, options);
    let clif = jit.take_clif();
    let text_of_fn = |symbol: &str| {
        clif.iter()
            .find(|text| text.starts_with(&format!("; {symbol}\n")))
            .unwrap()
            .clone()
    };
    // `mid` and `g` own nothing at their calls: no handler.
    assert!(!text_of_fn("cb$fn$app$mid").contains("try_call"));
    assert!(!text_of_fn("cb$fn$app$g").contains("try_call"));
    assert!(text_of_fn("cb$fn$app$f").contains("try_call"));
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
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
fn unwind_try_call_through_a_c_call() {
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
    let jit = compile_try_call(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::EqualityOnFunction);
}

/// An allocation that fails: constant evaluation's memory budget refuses
/// the second object (D-536), so `cheby_rt_alloc` unwinds:
/// `f() { a = alloc (Nil, Nil); b = alloc (a, Nil); return b }`, 24 bytes
/// each under a budget of 32.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_an_allocation_failure() {
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
    let jit = compile_try_call(p);
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    let config = EntryConfig {
        stack_size: cheby_runtime::entry::MIN_STACK_SIZE,
        ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
            memory: Some(32),
            ..ConstEvalLimits::default()
        }))
    };
    let outcome = entry.run(&config).unwrap();
    let EntryOutcome::Panicked(_) = &outcome else {
        panic!("the entry returned: {outcome:?}");
    };
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0, "objects leaked");
    }
}

/// A checked overflow: `f(n) { s = alloc Some(Nil); m = n + 1; drop s;
/// return box m }`. The panic block calls `cheby_rt_panic` through its
/// exit stub, which resumes at the block's handler.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_a_checked_overflow() {
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
    let jit = compile_try_call(p);
    assert_eq!(returned(run_const(&jit, fine)), abi::tag_int(42).unwrap());
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Overflow);
}

/// An explicit panic two frames down: `f(x) { s = alloc (x, Nil); panic
/// "boom" }` and `start() { a = alloc Some(Nil); r = call f(a); return r
/// }`. `f`'s pad drops `s` and resumes into `start`, whose pad has nothing
/// left to drop (the call consumed `a`).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_an_explicit_panic() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[Repr::Value],
        &[Repr::Value],
        vec![block(vec![pair(1, op(0))], boom())],
        &BOOM,
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
    let jit = compile_try_call(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
}

/// `deep(x) { s = alloc Some(Nil); dup x; r = call deep(x); drop s;
/// drop x; return r }` and `start() { a = alloc Some(Nil); r = call
/// deep(a); return r }`: an endless recursion that overflows at an entry
/// check, whose entry pad drops `x`; every caller's pad drops its `s` and
/// `x`, each resuming one frame up.
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

/// A stack overflow at the entry check (D-124, D-700): thousands of frames
/// each resume one frame up.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_a_stack_overflow() {
    let (p, target) = deep_program();
    let jit = compile_try_call(p);
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
fn unwind_try_call_through_a_stack_overflow_of_a_large_frame() {
    let (p, target) = deep_program();
    let options = CodegenOptions {
        large_frame_threshold: 0,
        capture_clif: true,
        ..try_call_options()
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
/// (D-526), and the closure panics. The trampoline catches and returns 0,
/// `callback::call` restores the kernel's exit record and passes the
/// unwinding on, the kernel's guard throws again, and `f`'s pad drops the
/// map and the closure:
/// `f() { m = map::new(); k = alloc closure(code); one = box 1;
/// r = map::upsert(m, one, k); drop k; drop m; return r }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_through_a_trampoline() {
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
    let jit = compile_try_call(p);
    let outcome = run_const(&jit, target);
    assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
}

/// A `main` entry thunk returns its exit status, and 0 is success, so the
/// host never decides from the result that the entry unwound (plan §2.5):
/// a `main` that returns 0 normally is `Returned(0)` with no report, and
/// one whose `main` panics is `Panicked`, though its thunk returns 0 as
/// well. `thunk() { r = call main(); drop r; return 0 }`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_main_returning_zero_is_not_an_unwinding() {
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
        let jit = compile_try_call(p);
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

/// The same entry many times in a row, each unwinding through several
/// frames: the exit record and the registry stay consistent, and nothing
/// leaks across runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_repeatedly() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[Repr::Value],
        &[Repr::Value],
        vec![block(vec![pair(1, op(0))], boom())],
        &BOOM,
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
    let jit = compile_try_call(p);
    for _ in 0..200 {
        let outcome = run_const(&jit, target);
        assert_unwound(&jit, &outcome, RtPanicKind::Explicit);
    }
}

/// The registrations end with the code: they outlive the `Jit` while a
/// code token keeps the memory (D-708), and the last token drops them
/// before the memory is freed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_registrations_end_with_the_code() {
    let (p, target) = deep_program();
    let jit = compile_try_call(p);
    let thunk = jit.entry(EntryKind::Const(target)).unwrap().thunk();
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let address = (thunk as *const u8).addr();
    assert!(cheby_runtime::unwind::try_call::is_registered(address));
    let token = jit.code_token();
    drop(jit);
    assert!(cheby_runtime::unwind::try_call::is_registered(address));
    drop(token);
    assert!(!cheby_runtime::unwind::try_call::is_registered(address));
}

/// Handler tables and exit stubs exist only under the prototype: compiled
/// with `Off` or `Flag` the batch keeps no table and declares no stub, so
/// neither baseline pays for them (plan §4.4). Under `TryCall` the stubs
/// are named by machine signature and come out the same in every build
/// (D-264).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_try_call_tables_and_stubs_only_under_the_prototype() {
    let build = |unwind: UnwindBackend| {
        let (p, _) = deep_program();
        let mut jit = compile_with(
            p,
            CodegenOptions {
                unwind,
                capture_clif: true,
                ..try_call_options()
            },
        );
        let clif = jit.take_clif().join("\n");
        let registered = jit.keep_alive.registrations.lock().unwrap().len();
        (clif, registered, jit.code_stats())
    };
    for unwind in [UnwindBackend::Off, UnwindBackend::Flag] {
        let (clif, registered, _) = build(unwind);
        assert_eq!(registered, 0, "{unwind:?}");
        assert!(!clif.contains("try_call"), "{unwind:?}");
        assert!(!clif.contains("cb$exit$"), "{unwind:?}");
    }
    let (first, registered, stats) = build(UnwindBackend::TryCall);
    // `deep`, `start` and the thunk each have handlers; the stubs none.
    assert_eq!(registered, 3);
    assert!(first.contains("try_call"), "{first}");
    // The entry check's slow path `(ctx, location) -> void`.
    assert!(first.contains("= cb$exit$i64_i64$v"), "{first}");
    assert!(first.contains("cheby_rt_unwind_resume"), "{first}");
    let (second, _, again) = build(UnwindBackend::TryCall);
    assert_eq!(first, second);
    assert_eq!(stats, again);
}
