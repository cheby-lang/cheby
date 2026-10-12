//! The unwind IR (D-698) and the IR forms of the slice (D-692, D-693):
//! the builder, the dump grammar, the shape checks of the validator, and
//! what the RC pass does with a `precounted` body.

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, ModuleId};

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Block, BlockId, Callee, Const, DefName, FnKey, Function, LayoutRef, Local, MirModule,
    Operand, RuntimeFn, Rvalue, Stmt, Terminator, UnitKind, UseKind,
};
use crate::repr::{CType, ForeignSig, Repr};
use crate::validate::{Stage, validate};
use crate::{dump, rc};

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

fn module(functions: Vec<Function>) -> MirModule {
    MirModule {
        unit: UnitKind::Code(MODULE),
        path: "app".into(),
        functions,
        callees: Vec::new(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: (0..4)
            .map(|n| DefName {
                def: def(n),
                module: "app".into(),
                item: format!("f{n}").into(),
            })
            .collect(),
    }
}

fn builder(n: u32) -> FunctionBuilder {
    let mut b = FunctionBuilder::new(FnKey::Item(def(n)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn bb(f: &Function, n: u32) -> &Block {
    f.blocks.get(BlockId::from_u32(n)).unwrap()
}

fn bb_mut(f: &mut Function, n: u32) -> &mut Block {
    f.blocks.get_mut(BlockId::from_u32(n)).unwrap()
}

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

fn runtime(func: RuntimeFn, args: Vec<Operand>) -> Rvalue {
    Rvalue::CallRuntime { func, args }
}

/// The shape of `scope_run`'s generated body (plan §2.4): every form of
/// the unwind IR, with a pad chained into a shared `resume`.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR function")]
fn scope_run() -> Function {
    let mut b = builder(0);
    let timed = b.add_param(Repr::Bool, None, None).unwrap();
    let seconds = b.add_param(Repr::Int, None, None).unwrap();
    let nanos = b.add_param(Repr::Int, None, None).unwrap();
    let body = b.add_param(Repr::Value, None, None).unwrap();
    let ok = b.new_block().unwrap();
    let failed = b.new_block().unwrap();
    let unwound = b.new_block().unwrap();
    let close_pad = b.new_block().unwrap();
    let entry_pad = b.new_block().unwrap();
    let resume = b.new_block().unwrap();
    b.set_precounted();
    b.set_entry_unwind(entry_pad);
    let symbol = b.string("cheby_rt_scope_run").unwrap();

    b.push(Stmt::Eval(runtime(
        RuntimeFn::EffectCheck,
        vec![Operand::Const(Const::Str(symbol))],
    )))
    .unwrap();
    b.unwind_last(entry_pad).unwrap();
    let scope = b
        .assign(
            Repr::Value,
            runtime(
                RuntimeFn::ScopeOpen,
                vec![local(timed), local(seconds), local(nanos)],
            ),
        )
        .unwrap();
    b.unwind_last(entry_pad).unwrap();
    b.push(Stmt::Dup(scope)).unwrap();
    let value = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(body),
                args: vec![local(scope)],
            },
        )
        .unwrap();
    b.unwind_last(unwound).unwrap();
    let decided = b
        .assign(
            Repr::Bool,
            runtime(
                RuntimeFn::ScopeClose,
                vec![local(scope), Operand::Const(Const::Bool(false))],
            ),
        )
        .unwrap();
    b.unwind_last(close_pad).unwrap();
    b.terminate(Terminator::Branch {
        cond: local(decided),
        then_: ok,
        else_: failed,
    })
    .unwrap();

    b.switch_to(ok).unwrap();
    let wrapped = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(1),
                fields: vec![local(value)],
            },
        )
        .unwrap();
    b.unwind_last(close_pad).unwrap();
    b.terminate(Terminator::Return(local(wrapped))).unwrap();

    b.switch_to(failed).unwrap();
    b.push(Stmt::Drop(value)).unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::ErrNil)))
        .unwrap();

    b.switch_to(unwound).unwrap();
    b.clear_unwind().unwrap();
    b.assign(
        Repr::Bool,
        runtime(
            RuntimeFn::ScopeClose,
            vec![local(scope), Operand::Const(Const::Bool(true))],
        ),
    )
    .unwrap();
    b.unwind_last(resume).unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::ErrNil)))
        .unwrap();

    b.switch_to(close_pad).unwrap();
    b.push(Stmt::Drop(value)).unwrap();
    b.terminate(Terminator::Goto {
        target: resume,
        args: Vec::new(),
    })
    .unwrap();

    b.switch_to(entry_pad).unwrap();
    b.push(Stmt::Drop(body)).unwrap();
    b.terminate(Terminator::Goto {
        target: resume,
        args: Vec::new(),
    })
    .unwrap();

    b.switch_to(resume).unwrap();
    b.terminate(Terminator::Resume).unwrap();
    b.finish().unwrap()
}

/// A `@std_context` kernel's wrapper after the unwind pass: `ctx` first,
/// an edge on the foreign call and on the tail call.
fn context_wrapper() -> Function {
    let mut b = builder(1);
    let n = b.add_param(Repr::Int, None, None).unwrap();
    let pad = b.new_block().unwrap();
    b.set_entry_unwind(pad);
    let raw = b
        .assign(
            Repr::Int,
            Rvalue::CallForeign {
                symbol: "cheby_rt_kernel".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr, CType::I64],
                    ret: CType::I64,
                },
                args: vec![Operand::Const(Const::Ctx), local(n)],
            },
        )
        .unwrap();
    b.unwind_last(pad).unwrap();
    let boxed = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(raw),
                from: Repr::Int,
            },
        )
        .unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Indirect(boxed),
        args: Vec::new(),
    })
    .unwrap();
    let current = b.current();
    b.unwind_terminator(current, pad).unwrap();
    b.switch_to(pad).unwrap();
    b.terminate(Terminator::Resume).unwrap();
    b.finish().unwrap()
}

const SCOPE_RUN_DUMP: &str = "\
fn app::f0(_0: bool, _1: int, _2: int, _3: value) -> value check str#0 {
  entry unwind bb5
  precounted
  str#0 = \"src/main.cheby:1:4\"
  str#1 = \"cheby_rt_scope_run\"
bb0:
  call_runtime effect_check(borrow const str#1) ; unwind bb5
  _4: value = call_runtime scope_open(borrow _0, borrow _1, borrow _2) ; unwind bb5
  dup _4
  _5: value = call _3(_4) ; unwind bb3
  _6: bool = call_runtime scope_close(_4, borrow const bool false) ; unwind bb4
  branch _6, bb1, bb2
bb1:
  _7: value = alloc tuple/1(_5) ; unwind bb4
  return _7
bb2:
  drop _5
  return const static Err(Nil)
bb3:
  clear_unwind
  _8: bool = call_runtime scope_close(_4, borrow const bool true) ; unwind bb6
  return const static Err(Nil)
bb4:
  drop _5
  goto bb6()
bb5:
  drop _3
  goto bb6()
bb6:
  resume
}
";

const CONTEXT_WRAPPER_DUMP: &str = "\
fn app::f1(_0: int) -> value check str#0 {
  entry unwind bb1
  str#0 = \"src/main.cheby:1:4\"
bb0:
  _1: int = call_foreign \"cheby_rt_kernel\"(borrow const ctx, borrow _0) -> i64 ; unwind bb1
  _2: value = box int _1
  tail_call _2() ; unwind bb1
bb1:
  resume
}
";

fn errors(f: Function, stage: Stage) -> String {
    let m = module(vec![f]);
    validate(&m, stage).err().unwrap_or_default().join("\n")
}

fn rejected(f: Function, stage: Stage, needle: &str) {
    let found = errors(f, stage);
    assert!(found.contains(needle), "expected {needle:?} in:\n{found}");
}

#[test]
fn scope_run_dumps_as_the_grammar_says() {
    let m = module(vec![scope_run()]);
    assert_eq!(
        dump::dump_function(&m, &m.functions[0]).unwrap(),
        SCOPE_RUN_DUMP
    );
}

#[test]
fn ctx_and_terminator_edges_dump_as_the_grammar_says() {
    let m = module(vec![context_wrapper()]);
    assert_eq!(
        dump::dump_function(&m, &m.functions[0]).unwrap(),
        CONTEXT_WRAPPER_DUMP
    );
}

#[test]
fn a_function_without_edges_dumps_as_before() {
    let mut b = builder(2);
    b.terminate(Terminator::Return(Operand::Const(Const::Nil)))
        .unwrap();
    let m = module(vec![b.finish().unwrap()]);
    assert_eq!(
        dump::dump_function(&m, &m.functions[0]).unwrap(),
        "fn app::f2() -> value check str#0 {\n  str#0 = \"src/main.cheby:1:4\"\nbb0:\n  return const nil\n}\n"
    );
}

#[test]
fn the_shapes_pass_every_stage_that_allows_them() {
    for stage in [Stage::Lowered, Stage::Counted, Stage::Unwound] {
        assert_eq!(errors(scope_run(), stage), "", "{stage:?}");
    }
    assert_eq!(errors(context_wrapper(), Stage::Unwound), "");
}

#[test]
fn edges_wait_for_the_unwind_pass_unless_precounted() {
    rejected(
        context_wrapper(),
        Stage::Counted,
        "bb0 has unwind edges before the unwind pass",
    );
    rejected(
        context_wrapper(),
        Stage::Lowered,
        "an entry pad before the unwind pass",
    );
    rejected(
        context_wrapper(),
        Stage::Counted,
        "bb1 resumes before the unwind pass",
    );
}

#[test]
fn unsorted_or_duplicate_edges_are_rejected() {
    let needle = "the unwind edges of bb0 are not sorted by statement without duplicates";
    let mut f = scope_run();
    bb_mut(&mut f, 0).unwind.swap(0, 1);
    rejected(f, Stage::Unwound, needle);
    let mut f = scope_run();
    let pad = BlockId::from_u32(5);
    bb_mut(&mut f, 0).unwind[1] = (0, pad);
    rejected(f, Stage::Unwound, needle);
}

#[test]
fn an_edge_past_the_terminator_is_rejected() {
    let mut f = scope_run();
    let block = bb_mut(&mut f, 1);
    let past = u32::try_from(block.stmts.len()).unwrap() + 1;
    block.unwind.push((past, BlockId::from_u32(4)));
    rejected(
        f,
        Stage::Unwound,
        "bb1 has an unwind edge for statement 2, past its terminator",
    );
}

#[test]
fn a_pad_takes_no_parameters_and_exists() {
    let mut b = builder(1);
    let pad = b.new_block().unwrap();
    b.add_block_param(pad, Repr::Int).unwrap();
    b.push(Stmt::Eval(runtime(
        RuntimeFn::EffectCheck,
        vec![Operand::Const(Const::Nil)],
    )))
    .unwrap();
    b.unwind_last(pad).unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Nil)))
        .unwrap();
    b.switch_to(pad).unwrap();
    b.terminate(Terminator::Resume).unwrap();
    rejected(
        b.finish().unwrap(),
        Stage::Unwound,
        "bb0 statement 0 unwinds to bb1, which takes parameters",
    );

    let mut f = scope_run();
    f.entry_unwind = Some(BlockId::from_u32(40));
    rejected(f, Stage::Unwound, "the entry check unwinds to unknown bb40");
    let mut f = scope_run();
    bb_mut(&mut f, 0).unwind[0].1 = BlockId::from_u32(0);
    rejected(
        f,
        Stage::Unwound,
        "bb0 statement 0 unwinds to the entry block",
    );
}

#[test]
fn ctx_is_only_the_first_foreign_argument() {
    let needle = "ctx appears only as the first argument of a foreign call";
    let mut f = context_wrapper();
    let Stmt::Assign {
        value: Rvalue::CallForeign { sig, args, .. },
        ..
    } = &mut bb_mut(&mut f, 0).stmts[0]
    else {
        panic!("the foreign call moved");
    };
    sig.params.swap(0, 1);
    args.swap(0, 1);
    rejected(f, Stage::Unwound, needle);

    let mut b = builder(1);
    b.assign(Repr::U64, Rvalue::Use(Operand::Const(Const::Ctx)))
        .unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Nil)))
        .unwrap();
    rejected(b.finish().unwrap(), Stage::Lowered, needle);

    let mut f = context_wrapper();
    let Stmt::Assign {
        value: Rvalue::CallForeign { sig, .. },
        ..
    } = &mut bb_mut(&mut f, 0).stmts[0]
    else {
        panic!("the foreign call moved");
    };
    sig.params[0] = CType::U64;
    rejected(f, Stage::Unwound, "ctx passed as u64");
}

#[test]
fn resume_and_clear_unwind_are_unwinding_code() {
    let mut b = builder(1);
    b.terminate(Terminator::Resume).unwrap();
    rejected(
        b.finish().unwrap(),
        Stage::Unwound,
        "bb0 resumes, but normal control flow reaches it",
    );

    let mut f = scope_run();
    bb_mut(&mut f, 2).stmts.insert(0, Stmt::ClearUnwind);
    rejected(
        f,
        Stage::Unwound,
        "bb2 clears the unwind flag, but normal control flow reaches it",
    );

    let mut f = scope_run();
    f.precounted = false;
    rejected(
        f,
        Stage::Unwound,
        "clear_unwind outside a generated intrinsic body",
    );
}

#[test]
fn counts_are_checked_in_a_precounted_body_from_lowering_on() {
    let mut f = scope_run();
    let block = bb_mut(&mut f, 0);
    let dup = block
        .stmts
        .iter()
        .position(|s| matches!(s, Stmt::Dup(_)))
        .unwrap();
    block.stmts.remove(dup);
    for (at, _) in &mut block.unwind {
        if usize::try_from(*at).unwrap() > dup {
            *at -= 1;
        }
    }
    rejected(f, Stage::Lowered, "is used without a reference");

    let mut f = scope_run();
    f.precounted = false;
    rejected(
        f,
        Stage::Lowered,
        "dup or drop before reference-count insertion",
    );
}

#[test]
fn rc_leaves_a_precounted_body_alone_and_refuses_edges() {
    let mut f = scope_run();
    let before = f.clone();
    rc::insert(&mut f).unwrap();
    assert_eq!(f, before);

    let mut f = context_wrapper();
    let err = rc::insert(&mut f).unwrap_err();
    assert!(
        err.to_string()
            .contains("unwind edges before reference counting")
    );
}

#[test]
fn successors_and_unwind_targets() {
    let f = scope_run();
    assert_eq!(
        f.successors(BlockId::from_u32(0)).unwrap(),
        [1, 2, 5, 5, 3, 4].map(BlockId::from_u32)
    );
    assert_eq!(
        f.successors(BlockId::from_u32(1)).unwrap(),
        [BlockId::from_u32(4)]
    );
    assert!(f.successors(BlockId::from_u32(60)).is_err());
    let bb0 = bb(&f, 0);
    assert_eq!(bb0.unwind_target(0), Some(BlockId::from_u32(5)));
    assert_eq!(bb0.unwind_target(2), None);
    assert_eq!(bb0.unwind_target(3), Some(BlockId::from_u32(3)));
    assert_eq!(bb0.unwind_target(bb0.stmts.len()), None);
    let wrapper = context_wrapper();
    let bb0 = bb(&wrapper, 0);
    assert_eq!(
        bb0.unwind_target(bb0.stmts.len()),
        Some(BlockId::from_u32(1))
    );
}

#[test]
fn the_builder_refuses_misplaced_edges() {
    let mut b = builder(1);
    let pad = b.new_block().unwrap();
    assert!(b.unwind_last(pad).is_err(), "no statement yet");
    b.clear_unwind().unwrap();
    b.unwind_last(pad).unwrap();
    assert!(b.unwind_last(pad).is_err(), "an edge twice");
    let current = b.current();
    assert!(b.unwind_terminator(current, pad).is_err(), "no terminator");
    b.terminate(Terminator::Unreachable).unwrap();
    assert!(b.unwind_last(pad).is_err(), "after the terminator");
    b.unwind_terminator(current, pad).unwrap();
    assert!(b.unwind_terminator(current, pad).is_err(), "twice");
    b.switch_to(pad).unwrap();
    b.terminate(Terminator::Resume).unwrap();
    let f = b.finish().unwrap();
    assert_eq!(bb(&f, 0).unwind, [(0, pad), (1, pad)]);
}

#[test]
fn the_scope_helpers() {
    assert_eq!(RuntimeFn::ALL.len(), 14);
    assert_eq!(
        RuntimeFn::ScopeOpen.params(),
        [
            (Repr::Bool, UseKind::Borrowed),
            (Repr::Int, UseKind::Borrowed),
            (Repr::Int, UseKind::Borrowed),
        ]
    );
    assert_eq!(RuntimeFn::ScopeOpen.ret(), Some(Repr::Value));
    assert_eq!(
        RuntimeFn::ScopeClose.params(),
        [
            (Repr::Value, UseKind::Owned),
            (Repr::Bool, UseKind::Borrowed)
        ]
    );
    assert_eq!(RuntimeFn::ScopeClose.ret(), Some(Repr::Bool));
    assert_eq!(RuntimeFn::ScopeOpen.name(), "scope_open");
    assert_eq!(RuntimeFn::ScopeClose.name(), "scope_close");
    let quiet: Vec<RuntimeFn> = RuntimeFn::ALL
        .into_iter()
        .filter(|f| !f.may_unwind())
        .collect();
    assert_eq!(
        quiet,
        [RuntimeFn::StrEq, RuntimeFn::ListLength, RuntimeFn::ListGet]
    );
    assert_eq!(Const::Ctx.repr(), Repr::U64);
    assert!(!Repr::U64.is_counted());
    assert_eq!(Const::ErrNil.repr(), Repr::Value);
    assert!(CType::Ptr.accepts_ctx());
    assert!(!CType::U64.accepts_ctx());
}
