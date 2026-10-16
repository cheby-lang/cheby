//! `@std_context` wrappers and the std intrinsic `scope_run` under the JIT
//! (D-692, D-693). The MIR here is a hand-built copy of what
//! `cheby_mir`'s lowering tests pin (`lower/intrinsic_tests.rs`).

use cheby_mir::{CType, ForeignSig, LayoutRef, Stage, validate};
use cheby_runtime::ExecCtx;

use super::*;

/// The kernel of the context wrapper below, with D-692's shape: `ctx`
/// first, then the Cheby parameters `count: Int` and `text`. Returns
/// `count + 1` when `ctx` is the context of the entry running on this
/// thread and `text` is the static `Nil` it was given, and `i64::MIN`
/// otherwise.
extern "C" fn cheby_fiber_test_context(ctx: *mut ExecCtx, count: i64, text: u64) -> i64 {
    match cheby_runtime::entry::current_ctx() {
        Some(running) if !ctx.is_null() && running == ctx && text == static_nil() => {
            count.wrapping_add(1)
        }
        _ => i64::MIN,
    }
}

/// `std::fiber::context(count: Int, text: String) -> Int` lowered with
/// `@std_context`:
///
/// ```text
/// bb0:
///   eval effect_check(const str#1 "cheby_fiber_test_context")
///   _2 = call_foreign cheby_fiber_test_context(ptr, i64, ptr) -> i64 (const ctx, _0, _1)
///   _3 = box int _2
///   return _3
/// ```
fn context_wrapper(p: &mut Prog) -> DefId {
    p.item(
        0,
        "context",
        &[Repr::Int, Repr::Value],
        &[Repr::Int, Repr::Value],
        straight(
            vec![
                Stmt::Eval(Rvalue::CallRuntime {
                    func: RuntimeFn::EffectCheck,
                    args: vec![Operand::Const(Const::Str(StrId::from_u32(1)))],
                }),
                assign(
                    2,
                    Rvalue::CallForeign {
                        symbol: "cheby_fiber_test_context".into(),
                        sig: ForeignSig {
                            params: vec![CType::Ptr, CType::I64, CType::Ptr],
                            ret: CType::I64,
                        },
                        args: vec![Operand::Const(Const::Ctx), op(0), op(1)],
                        blocking: false,
                    },
                ),
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
        &["cheby_fiber_test_context"],
    )
}

/// (a) The wrapper of a `@std_context` kernel passes the running entry's
/// context first, and the kernel finds it equal to
/// `entry::current_ctx()` (D-692, D-745).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_context_wrapper_passes_the_running_context() {
    let mut p = Prog::default();
    let f = context_wrapper(&mut p);
    let target = p.call(
        f,
        vec![Operand::Const(Const::Int(41)), Operand::Const(Const::Nil)],
    );
    let [mut code, entries] = p.units();
    validate(&code, Stage::Lowered).unwrap();
    // Reference counting adds the drop of the owned `text` after the call,
    // as in the lowering test's counted dump.
    cheby_mir::rc::insert_module(&mut code).unwrap();
    validate(&code, Stage::Counted).unwrap();
    let mut jit = Jit::new().unwrap();
    let kernel: extern "C" fn(*mut ExecCtx, i64, u64) -> i64 = cheby_fiber_test_context;
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let address = kernel as *const u8;
    // SAFETY: `cheby_fiber_test_context` has the C signature the foreign
    // call above uses, and it lives as long as the process.
    unsafe { jit.define_symbol("cheby_fiber_test_context", address) };
    jit.compile(&[code, entries]).unwrap();
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(42).unwrap());
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `std::fiber::scope_run(_0 timed: bool, _1 seconds: int, _2 nanoseconds:
/// int, _3 body: value) -> value`, generated (D-693, plan §2.4):
///
/// ```text
///   entry unwind bb5
///   precounted
///   bb0:
///     eval effect_check(const str#1 "cheby_rt_scope_run") ; unwind bb5
///     _4 = scope_open(_0, _1, _2) ; unwind bb5
///     dup _4
///     _5 = call indirect _3(_4) ; unwind bb3
///     _6 = scope_close(_4, const false) ; unwind bb4
///     branch _6 -> bb1, bb2
///   bb1:
///     _7 = alloc Ok(_5) ; unwind bb4
///     return _7
///   bb2:
///     drop _5
///     return const static Err(Nil)
///   bb3:
///     clear_unwind
///     _8 = scope_close(_4, const true) ; unwind bb6
///     return const static Err(Nil)
///   bb4:
///     drop _5
///     goto bb6()
///   bb5:
///     drop _3
///     goto bb6()
///   bb6:
///     resume
/// ```
fn scope_run() -> Function {
    let goto_resume = || Terminator::Goto {
        target: bb(6),
        args: Vec::new(),
    };
    let err_nil = || Terminator::Return(Operand::Const(Const::ErrNil));
    let mut f = function(
        FnKey::Item(def(1, 1)),
        Abi::Cheby,
        &[Repr::Bool, Repr::Int, Repr::Int, Repr::Value],
        &[
            Repr::Value,
            Repr::Value,
            Repr::Bool,
            Repr::Value,
            Repr::Bool,
        ],
        Repr::Value,
        vec![
            Block {
                params: Vec::new(),
                stmts: vec![
                    Stmt::Eval(Rvalue::CallRuntime {
                        func: RuntimeFn::EffectCheck,
                        args: vec![Operand::Const(Const::Str(StrId::from_u32(1)))],
                    }),
                    assign(
                        4,
                        Rvalue::CallRuntime {
                            func: RuntimeFn::ScopeOpen,
                            args: vec![op(0), op(1), op(2)],
                        },
                    ),
                    Stmt::Dup(local(4)),
                    assign(
                        5,
                        Rvalue::Call {
                            callee: Callee::Indirect(local(3)),
                            args: vec![op(4)],
                        },
                    ),
                    assign(
                        6,
                        Rvalue::CallRuntime {
                            func: RuntimeFn::ScopeClose,
                            args: vec![op(4), Operand::Const(Const::Bool(false))],
                        },
                    ),
                ],
                term: Terminator::Branch {
                    cond: op(6),
                    then_: bb(1),
                    else_: bb(2),
                },
                unwind: vec![(0, bb(5)), (1, bb(5)), (3, bb(3)), (4, bb(4))],
            },
            Block {
                params: Vec::new(),
                stmts: vec![assign(
                    7,
                    Rvalue::Alloc {
                        layout: LayoutRef::Ctor(builtin::OK),
                        fields: vec![op(5)],
                    },
                )],
                term: ret(7),
                unwind: vec![(0, bb(4))],
            },
            block(vec![Stmt::Drop(local(5))], err_nil()),
            Block {
                params: Vec::new(),
                stmts: vec![
                    Stmt::ClearUnwind,
                    assign(
                        8,
                        Rvalue::CallRuntime {
                            func: RuntimeFn::ScopeClose,
                            args: vec![op(4), Operand::Const(Const::Bool(true))],
                        },
                    ),
                ],
                term: err_nil(),
                unwind: vec![(1, bb(6))],
            },
            block(vec![Stmt::Drop(local(5))], goto_resume()),
            block(vec![Stmt::Drop(local(3))], goto_resume()),
            block(Vec::new(), Terminator::Resume),
        ],
        &["cheby_rt_scope_run"],
    );
    f.entry_unwind = Some(bb(5));
    f.precounted = true;
    f
}

/// (b) `scope_run`'s generated body, precounted with its own edges
/// (D-744), compiles with those edges lowered by the return flag (D-733).
/// It is not run here; the std tests of `std::fiber` run it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_context_scope_run_body_compiles() {
    let mut p = Prog::default();
    p.functions.push(scope_run());
    p.declare(
        def(1, 1),
        "scope_run",
        &[Repr::Bool, Repr::Int, Repr::Int, Repr::Value],
    );
    // What lowering gives the unit for the `Ok` it allocates.
    p.names.push(name(builtin::OK, "", "Ok"));
    p.foreign_layouts.push(LayoutDesc {
        layout: LayoutRef::Ctor(builtin::OK),
        name: "Ok".into(),
        tag: 0,
        only_ctor: false,
        fields: vec![FieldDesc {
            name: None,
            repr: Repr::Value,
            offset: 8,
        }],
        value_fields: 1,
        raw_fields: 0,
        type_def: builtin::RESULT,
        type_name: "Result".into(),
        ctor_name: "Ok".into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key("", "Result"),
    });
    let [code, entries] = p.units();
    validate(&code, Stage::Lowered).unwrap();
    validate(&code, Stage::Counted).unwrap();
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
}

/// `FnKey::Trampoline(0)` as lowering builds it: `_1 = call indirect
/// _0(); return _1`, C ABI.
fn trampoline_zero() -> Function {
    function(
        FnKey::Trampoline(0),
        Abi::C,
        &[Repr::Value],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![assign(
                1,
                Rvalue::Call {
                    callee: Callee::Indirect(local(0)),
                    args: Vec::new(),
                },
            )],
            ret(1),
        )],
        &[],
    )
}

/// A code unit with a `@std_context` wrapper carries `cb$tramp$0` (D-561):
/// with an entries unit that carries it too, the JIT defines it once and
/// the wrapper still runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_context_unit_trampoline_is_defined_once() {
    let mut p = Prog::default();
    let f = context_wrapper(&mut p);
    p.functions.push(trampoline_zero());
    p.thunks.push(trampoline_zero());
    let target = p.call(
        f,
        vec![Operand::Const(Const::Int(1)), Operand::Const(Const::Nil)],
    );
    let [mut code, mut entries] = p.units();
    cheby_mir::rc::insert_module(&mut code).unwrap();
    cheby_mir::rc::insert_module(&mut entries).unwrap();
    validate(&code, Stage::Counted).unwrap();
    validate(&entries, Stage::Counted).unwrap();
    let mut jit = Jit::new().unwrap();
    let kernel: extern "C" fn(*mut ExecCtx, i64, u64) -> i64 = cheby_fiber_test_context;
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let address = kernel as *const u8;
    // SAFETY: as in `std_context_wrapper_passes_the_running_context`.
    unsafe { jit.define_symbol("cheby_fiber_test_context", address) };
    jit.compile(&[code, entries]).unwrap();
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(2).unwrap());
}

/// (c) The compiler's table of std intrinsics is the runtime's list of
/// names it never registers (D-693).
#[test]
fn std_context_intrinsics_match_the_runtime() {
    assert_eq!(cheby_mir::lower::intrinsic_symbols(), sym::INTRINSICS);
}
