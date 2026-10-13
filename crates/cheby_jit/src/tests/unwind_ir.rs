//! The IR forms of the slice under the JIT (D-692, D-698): `ctx` as a
//! foreign call's first argument, and unwind edges, which code generation
//! lowers with the return flag (D-733).

use cheby_mir::{Stage, validate};
use cheby_runtime::ExecCtx;

use super::*;

/// A `@std_context` kernel's shape (D-692): `ctx` first. Returns `n + 1`
/// when `ctx` is the context of the entry running on this thread, and
/// `i64::MIN` otherwise.
extern "C" fn cheby_test_ctx_kernel(ctx: *mut ExecCtx, n: i64) -> i64 {
    match cheby_runtime::entry::current_ctx() {
        Some(running) if !ctx.is_null() && running == ctx => n.wrapping_add(1),
        _ => i64::MIN,
    }
}

/// `f() { r = call_foreign cheby_test_ctx_kernel(ctx, 41); b = box r;
/// return b }`: the kernel finds the running entry's context.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_ctx_operand_is_the_running_context() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Int, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::CallForeign {
                        symbol: "cheby_test_ctx_kernel".into(),
                        sig: cheby_mir::ForeignSig {
                            params: vec![cheby_mir::CType::Ptr, cheby_mir::CType::I64],
                            ret: cheby_mir::CType::I64,
                        },
                        args: vec![Operand::Const(Const::Ctx), Operand::Const(Const::Int(41))],
                    },
                ),
                assign(
                    1,
                    Rvalue::Box {
                        value: op(0),
                        from: Repr::Int,
                    },
                ),
            ],
            1,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let units = p.units();
    validate(&units[0], Stage::Lowered).unwrap();
    let mut jit = Jit::new().unwrap();
    let kernel: extern "C" fn(*mut ExecCtx, i64) -> i64 = cheby_test_ctx_kernel;
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let address = kernel as *const u8;
    // SAFETY: `cheby_test_ctx_kernel` has the C signature the foreign call
    // above uses, and it lives as long as the process.
    unsafe { jit.define_symbol("cheby_test_ctx_kernel", address) };
    jit.compile(&units).unwrap();
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(42).unwrap());
}

/// `f()` with unwind edges on two helper calls and a foreign-free
/// `effect_check`, an entry pad, and a pad `bb1` chained into `resume`
/// in `bb2`. The edges are lowered with the return flag (D-733): each
/// helper call and the entry check's slow path load the flag and branch
/// to a cold pad. Nothing unwinds, so the function runs as if the pads
/// were not there.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lowered_edges_lead_to_untaken_pads() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value],
        vec![
            Block {
                params: Vec::new(),
                stmts: vec![
                    assign(
                        0,
                        Rvalue::CallRuntime {
                            func: RuntimeFn::StrWithCapacity,
                            args: vec![Operand::Const(Const::Int(4))],
                        },
                    ),
                    Stmt::Eval(Rvalue::CallRuntime {
                        func: RuntimeFn::EffectCheck,
                        args: vec![Operand::Const(Const::Str(StrId::from_u32(1)))],
                    }),
                    assign(
                        1,
                        Rvalue::CallRuntime {
                            func: RuntimeFn::StrAppend,
                            args: vec![op(0), Operand::Const(Const::Str(StrId::from_u32(1)))],
                        },
                    ),
                ],
                term: ret(1),
                unwind: vec![(0, bb(2)), (1, bb(1)), (2, bb(2))],
            },
            block(
                vec![Stmt::Drop(local(0))],
                Terminator::Goto {
                    target: bb(2),
                    args: Vec::new(),
                },
            ),
            block(Vec::new(), Terminator::Resume),
        ],
        &["ab"],
    );
    p.functions.last_mut().unwrap().entry_unwind = Some(bb(2));
    let target = p.call(f, Vec::new());
    let units = p.units();
    validate(&units[0], Stage::Unwound).unwrap();
    let mut jit = Jit::with_options(cheby_codegen_clif::CodegenOptions {
        capture_clif: true,
        ..cheby_codegen_clif::CodegenOptions::default()
    })
    .unwrap();
    jit.compile(&units).unwrap();
    let clif = jit.take_clif().join("\n");
    let (_, text) = clif.split_once("; cb$fn$app$f\n").unwrap();
    let (text, _) = text.split_once("\n}").unwrap();
    // The three helper calls; the entry check's releasing slow path tests
    // its own result (GAP(entry-unwind-helper)).
    let load_flag = format!("load.i32 notrap aligned v0+{}", abi::CTX_UNWIND);
    assert_eq!(text.matches(&load_flag).count(), 3, "{text}");
    assert!(text.contains("cheby_rt_stack_slow_unwind"), "{text}");
    assert!(text.contains(" cold:"), "{text}");
    assert!(!text.contains("trap user"), "{text}");
    let word = returned(run_const(&jit, target));
    assert_eq!(text_of(word), "ab");
    release(&jit, word);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}
