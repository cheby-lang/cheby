//! The unwind pass (D-698) and the rules of `Stage::Unwound`: which
//! instructions get edges, what their pads release, how pads are shared
//! and chained, and for each rule of the validator an accepted and a
//! rejected function. Determinism (D-264): lowering twice gives identical
//! dumps.

use cheby_iface::Idx;

use crate::ir::{
    Abi, Block, BlockId, Callee, Const, EntryKind, FnKey, Function, LayoutRef, Local, Operand,
    PanicKind, RuntimeFn, Rvalue, Stmt, Terminator,
};
use crate::options::{MirOptions, count_and_check_with};
use crate::repr::{CType, ForeignSig, Repr};
use crate::test_support::lower_source;
use crate::unwind::{
    Direction, PadPlan, insert, insert_recording, plan_pads, rvalue_may_unwind, stmt_may_unwind,
    terminator_may_unwind,
};
use crate::unwind_ir_tests::{builder, module, scope_run};
use crate::validate::{Stage, validate, validate_recorded};
use crate::{FunctionBuilder, dump};

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

fn bb(n: u32) -> BlockId {
    BlockId::from_u32(n)
}

fn text(f: &Function) -> String {
    let m = module(vec![f.clone()]);
    dump::dump_function(&m, &m.functions[0]).unwrap()
}

fn errors(f: &Function) -> String {
    validate(&module(vec![f.clone()]), Stage::Unwound)
        .err()
        .unwrap_or_default()
        .join("\n")
}

fn accepted(f: &Function) {
    assert_eq!(errors(f), "", "{}", text(f));
}

fn rejected(f: &Function, needle: &str) {
    let found = errors(f);
    assert!(
        found.contains(needle),
        "expected {needle:?} in:\n{found}\n{}",
        text(f)
    );
}

/// `f` after the unwind pass.
fn unwound(mut f: Function) -> Function {
    insert(&mut f).unwrap();
    f
}

/// Appends a block to `f` and returns its id.
fn add_block(f: &mut Function, stmts: Vec<Stmt>, term: Terminator) -> BlockId {
    f.blocks
        .push(Block {
            params: Vec::new(),
            stmts,
            term,
            unwind: Vec::new(),
        })
        .unwrap()
}

/// Points the edge of instruction `at` of `block` to `pad`, adding it if
/// needed.
fn set_edge(f: &mut Function, block: u32, at: u32, pad: BlockId) {
    let b = f.blocks.get_mut(bb(block)).unwrap();
    b.unwind.retain(|&(i, _)| i != at);
    b.unwind.push((at, pad));
    b.unwind.sort_unstable();
}

/// A counted function: a Cheby call that consumes its arguments, and an
/// `alloc` that consumes nothing until it succeeded.
///
/// ```text
/// fn f0(_0: value, _1: value) -> value
///   dup _0
///   _2 = call _1(_0)        ; an unwinding leaves {_0}
///   _3 = alloc (_0, _2)     ; an unwinding leaves {_0, _2}
///   return _3
/// ```
fn calls() -> Function {
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let g = b.add_param(Repr::Value, None, None).unwrap();
    b.push(Stmt::Dup(x)).unwrap();
    let y = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(g),
                args: vec![local(x)],
            },
        )
        .unwrap();
    let pair = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(x), local(y)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(pair))).unwrap();
    b.finish().unwrap()
}

const CALLS_UNWOUND: &str = "\
fn app::f0(_0: value, _1: value) -> value check str#0 {
  entry unwind bb3
  str#0 = \"src/main.cheby:1:4\"
bb0:
  dup _0
  _2: value = call _1(_0) ; unwind bb2
  _3: value = alloc tuple/2(_0, _2) ; unwind bb4
  return _3
bb1:
  resume
bb2:
  drop _0
  goto bb1()
bb3:
  drop _1
  goto bb2()
bb4:
  drop _2
  goto bb2()
}
";

/// A counted function with a runtime helper that consumes its owned
/// arguments even when it raises, one that cannot unwind, a kernel that
/// borrows, and boxes with and without allocation.
///
/// ```text
/// fn f1(_0: value, _1: value) -> value
///   _2 = list_push(_0, _1)  ; consumes both: an unwinding leaves {}
///   _3 = list_length(_2)    ; cannot unwind
///   _4 = foreign k(_2)      ; borrows: an unwinding leaves {_2}
///   drop _2
///   _5 = box int 1          ; a tagged constant: cannot unwind
///   _6 = box int _4         ; may allocate: an unwinding leaves {}
///   return _6
/// ```
fn helpers() -> Function {
    let mut b = builder(1);
    let list = b.add_param(Repr::Value, None, None).unwrap();
    let elem = b.add_param(Repr::Value, None, None).unwrap();
    let pushed = b
        .assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListPush,
                args: vec![local(list), local(elem)],
            },
        )
        .unwrap();
    b.assign(
        Repr::Int,
        Rvalue::CallRuntime {
            func: RuntimeFn::ListLength,
            args: vec![local(pushed)],
        },
    )
    .unwrap();
    let raw = b
        .assign(
            Repr::Int,
            Rvalue::CallForeign {
                symbol: "cheby_kernel".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::I64,
                },
                args: vec![local(pushed)],
                blocking: false,
            },
        )
        .unwrap();
    b.push(Stmt::Drop(pushed)).unwrap();
    let one = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: Operand::Const(Const::Int(1)),
                from: Repr::Int,
            },
        )
        .unwrap();
    b.push(Stmt::Drop(one)).unwrap();
    let boxed = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(raw),
                from: Repr::Int,
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(boxed))).unwrap();
    b.finish().unwrap()
}

const HELPERS_UNWOUND: &str = "\
fn app::f1(_0: value, _1: value) -> value check str#0 {
  entry unwind bb3
  str#0 = \"src/main.cheby:1:4\"
bb0:
  _2: value = call_runtime list_push(_0, _1) ; unwind bb1
  _3: int = call_runtime list_length(borrow _2)
  _4: int = call_foreign \"cheby_kernel\"(borrow _2) -> i64 ; unwind bb4
  drop _2
  _5: value = box int const int 1
  drop _5
  _6: value = box int _4 ; unwind bb1
  return _6
bb1:
  resume
bb2:
  drop _0
  goto bb1()
bb3:
  drop _1
  goto bb2()
bb4:
  drop _2
  goto bb1()
}
";

#[test]
fn unwind_pads_are_chained_and_shared_by_owned_set() {
    let f = unwound(calls());
    assert_eq!(text(&f), CALLS_UNWOUND);
    accepted(&f);
}

#[test]
fn unwind_helpers_consume_owned_arguments_and_kernels_borrow() {
    let f = unwound(helpers());
    assert_eq!(text(&f), HELPERS_UNWOUND);
    accepted(&f);
}

#[test]
fn unwind_pass_keeps_existing_edges_and_changes_nothing_twice() {
    // A precounted intrinsic body keeps its edges and entry pad (D-693).
    let body = scope_run();
    assert_eq!(unwound(body.clone()), body);
    let once = unwound(calls());
    assert_eq!(unwound(once.clone()), once);
    // And it goes through the pipeline unchanged.
    let mut m = module(vec![body.clone()]);
    count_and_check_with(&mut m, &MirOptions::default()).unwrap();
    assert_eq!(m.functions[0], body);
}

#[test]
fn unwind_entry_pad_of_a_function_without_counted_parameters_resumes() {
    let mut b = builder(2);
    b.add_param(Repr::Int, None, None).unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Nil)))
        .unwrap();
    let f = unwound(b.finish().unwrap());
    assert_eq!(f.entry_unwind, Some(bb(1)));
    assert_eq!(f.blocks.get(bb(1)).unwrap().term, Terminator::Resume);
    accepted(&f);
}

#[test]
fn unwind_classifies_what_may_unwind() {
    let x = Local::from_u32(0);
    let int = |n| Operand::Const(Const::Int(n));
    let may = [
        Rvalue::Call {
            callee: Callee::Indirect(x),
            args: Vec::new(),
        },
        Rvalue::CallForeign {
            symbol: "k".into(),
            sig: ForeignSig {
                params: Vec::new(),
                ret: CType::Void,
            },
            args: Vec::new(),
            blocking: false,
        },
        Rvalue::CallRuntime {
            func: RuntimeFn::EffectCheck,
            args: vec![local(x)],
        },
        Rvalue::Alloc {
            layout: LayoutRef::Tuple(1),
            fields: vec![local(x)],
        },
        Rvalue::LoadConst(crate::unwind_ir_tests::def(3)),
        Rvalue::Checked {
            op: crate::ArithOp::Add,
            lhs: int(1),
            rhs: int(2),
            repr: Repr::Int,
            loc: crate::StrId::from_u32(0),
        },
        Rvalue::Box {
            value: local(x),
            from: Repr::Int,
        },
        Rvalue::Box {
            value: int(1 << 62),
            from: Repr::Int,
        },
        Rvalue::Box {
            value: local(x),
            from: Repr::U64,
        },
        Rvalue::Box {
            value: Operand::Const(Const::Float(0)),
            from: Repr::Float,
        },
    ];
    for value in &may {
        assert!(rvalue_may_unwind(value), "{value:?}");
    }
}

#[test]
fn unwind_classifies_what_never_unwinds() {
    let x = Local::from_u32(0);
    let int = |n| Operand::Const(Const::Int(n));
    let never = [
        Rvalue::Use(local(x)),
        Rvalue::Field {
            base: x,
            layout: LayoutRef::Tuple(1),
            field: 0,
        },
        Rvalue::CallRuntime {
            func: RuntimeFn::ListGet,
            args: vec![local(x), int(0)],
        },
        Rvalue::Box {
            value: int(-(1 << 62)),
            from: Repr::Int,
        },
        Rvalue::Box {
            value: local(x),
            from: Repr::I32,
        },
        Rvalue::Box {
            value: local(x),
            from: Repr::Bool,
        },
        Rvalue::Compare {
            op: crate::CmpOp::Eq,
            lhs: int(1),
            rhs: int(2),
            repr: Repr::Int,
        },
        Rvalue::Unbox {
            value: local(x),
            to: Repr::Int,
        },
    ];
    for value in &never {
        assert!(!rvalue_may_unwind(value), "{value:?}");
    }
    assert!(!stmt_may_unwind(&Stmt::Dup(x)));
    assert!(!stmt_may_unwind(&Stmt::Drop(x)));
    assert!(!stmt_may_unwind(&Stmt::ClearUnwind));
    assert!(terminator_may_unwind(&Terminator::Panic {
        kind: PanicKind::Todo,
        message: None,
        loc: crate::StrId::from_u32(0),
    }));
    for term in [
        Terminator::TailCall {
            callee: Callee::Indirect(x),
            args: Vec::new(),
        },
        Terminator::Return(local(x)),
        Terminator::Unreachable,
        Terminator::Resume,
    ] {
        assert!(!terminator_may_unwind(&term), "{term:?}");
    }
}

// Rule: each edge's instruction may unwind; no edge on an instruction
// that cannot.
#[test]
fn unwind_rule_edges_only_on_instructions_that_may_unwind() {
    let f = unwound(helpers());
    accepted(&f);
    let mut on_dup = unwound(calls());
    set_edge(&mut on_dup, 0, 0, bb(3));
    rejected(
        &on_dup,
        "bb0 statement 0 has an unwind edge, but this statement cannot unwind",
    );
    let mut on_quiet_helper = f.clone();
    set_edge(&mut on_quiet_helper, 0, 1, bb(4));
    rejected(
        &on_quiet_helper,
        "bb0 statement 1 has an unwind edge, but this statement cannot unwind",
    );
    let mut on_return = f;
    let len = u32::try_from(on_return.blocks.get(bb(0)).unwrap().stmts.len()).unwrap();
    set_edge(&mut on_return, 0, len, bb(1));
    rejected(
        &on_return,
        &format!("bb0 statement {len} has an unwind edge, but this terminator cannot unwind"),
    );
}

// Rule (the complement): every instruction that may unwind in reachable
// code has its edge, the terminator `panic` included.
#[test]
fn unwind_rule_every_instruction_that_may_unwind_has_an_edge() {
    let mut f = unwound(helpers());
    f.blocks
        .get_mut(bb(0))
        .unwrap()
        .unwind
        .retain(|&(at, _)| at != 2);
    rejected(&f, "bb0 statement 2 may unwind but has no unwind edge");

    let mut b = builder(2);
    let loc = b.string("src/main.cheby:2:3").unwrap();
    b.terminate(Terminator::Panic {
        kind: PanicKind::Explicit,
        message: None,
        loc,
    })
    .unwrap();
    let panics = unwound(b.finish().unwrap());
    assert_eq!(panics.blocks.get(bb(0)).unwrap().unwind, [(0, bb(1))]);
    accepted(&panics);
    let mut bare = panics;
    bare.blocks.get_mut(bb(0)).unwrap().unwind.clear();
    rejected(&bare, "bb0 terminator may unwind but has no unwind edge");
}

// Rule: each pad's owned set equals the ownership state at every source.
#[test]
fn unwind_rule_pad_sets_equal_the_ownership_at_the_source() {
    accepted(&unwound(calls()));
    // The alloc leaves {_0, _2}; bb2 releases only {_0}, which is what
    // the call leaves.
    let mut short = unwound(calls());
    set_edge(&mut short, 0, 2, bb(2));
    rejected(
        &short,
        "reference counts differ between the predecessors of bb2",
    );
    // A pad that releases something the source does not hold.
    let mut extra = unwound(helpers());
    let resume = bb(1);
    let pad = add_block(
        &mut extra,
        vec![
            Stmt::Drop(Local::from_u32(2)),
            Stmt::Drop(Local::from_u32(2)),
        ],
        Terminator::Goto {
            target: resume,
            args: Vec::new(),
        },
    );
    set_edge(&mut extra, 0, 2, pad);
    rejected(&extra, "_2 is used without a reference (holds 0, needs 1)");
    // The entry pad starts with the parameters.
    let mut entry = unwound(calls());
    entry.entry_unwind = Some(bb(2));
    rejected(
        &entry,
        "reference counts differ between the predecessors of bb2",
    );
}

// Rule: `resume`, like `return`, requires every count to be 0.
#[test]
fn unwind_rule_resume_and_return_leave_nothing_held() {
    let f = unwound(helpers());
    accepted(&f);
    let mut leaky = f.clone();
    let alone = add_block(&mut leaky, Vec::new(), Terminator::Resume);
    set_edge(&mut leaky, 0, 2, alone);
    rejected(
        &leaky,
        &format!(
            "bb{} terminator (resume): _2 still holds 1 reference(s)",
            alone.as_u32()
        ),
    );
    let mut returns = f;
    let block = returns.blocks.get_mut(bb(0)).unwrap();
    let at = block
        .stmts
        .iter()
        .position(|s| *s == Stmt::Drop(Local::from_u32(2)))
        .unwrap();
    block.stmts.remove(at);
    for (i, _) in &mut block.unwind {
        if usize::try_from(*i).unwrap() > at {
            *i -= 1;
        }
    }
    rejected(&returns, "bb0 terminator: _2 still holds 1 reference(s)");
}

// Rule: a pad does not use the result of the instruction that unwinds to
// it; dominance follows the edges, with blocks split at them.
#[test]
fn unwind_rule_a_pad_does_not_use_its_instructions_result() {
    let f = unwound(calls());
    accepted(&f);
    // The call's pad drops the call's own result.
    let mut own_result = f.clone();
    let pad = add_block(
        &mut own_result,
        vec![Stmt::Drop(Local::from_u32(2))],
        Terminator::Goto {
            target: bb(2),
            args: Vec::new(),
        },
    );
    set_edge(&mut own_result, 0, 1, pad);
    rejected(
        &own_result,
        &format!(
            "bb{} uses _2, the result of bb0 statement 1, which an unwinding leaves undefined",
            pad.as_u32()
        ),
    );
    let found = recorded_errors(calls(), |f| {
        let pad = add_block(
            f,
            vec![Stmt::Drop(Local::from_u32(2))],
            Terminator::Goto {
                target: bb(2),
                args: Vec::new(),
            },
        );
        set_edge(f, 0, 1, pad);
    });
    assert!(
        found.contains(&format!(
            "bb{} uses _2, the result of bb0 statement 1",
            pad.as_u32()
        )),
        "{found}"
    );
    // A local defined after the edge in the same block is not defined in
    // its pad either. A canonical pad is left out of the dominance graph
    // (D-735), so the ownership states, taken before the instruction,
    // catch it: in both modes (D-774).
    let mut later = f;
    let add_later = |f: &mut Function| {
        let pad = add_block(
            f,
            vec![Stmt::Drop(Local::from_u32(3))],
            Terminator::Goto {
                target: bb(2),
                args: Vec::new(),
            },
        );
        set_edge(f, 0, 1, pad);
        pad
    };
    let pad = add_later(&mut later);
    rejected(
        &later,
        &format!(
            "bb{} statement 0: _3 is used without a reference (holds 0, needs 1)",
            pad.as_u32()
        ),
    );
    let found = recorded_errors(calls(), |f| {
        add_later(f);
    });
    assert!(
        found.contains(&format!(
            "bb0 statement 1 unwinds to bb{}, which drops {{_0, _3}}, but an unwinding there leaves {{_0}} held",
            pad.as_u32()
        )),
        "{found}"
    );
}

/// Runs the unwind pass on `f` keeping what its simulation recorded,
/// applies `change`, and validates the result as a release build does,
/// against the recorded states without the full simulation
/// (D-774).
fn recorded_errors(mut f: Function, change: impl FnOnce(&mut Function)) -> String {
    let recorded = insert_recording(&mut f).unwrap().unwrap();
    change(&mut f);
    validate_recorded(&module(vec![f]), &[recorded], false)
        .err()
        .unwrap_or_default()
        .join("\n")
}

// D-774: a pad chain that drops another multiset
// than the ownership at its edge fails `Stage::Unwound` in both modes:
// the full simulation, and the check against the pass's recorded states.
#[test]
fn unwind_rule_a_pad_chain_dropping_another_multiset_fails_in_both_modes() {
    assert_eq!(recorded_errors(calls(), |_| {}), "");
    assert_eq!(recorded_errors(helpers(), |_| {}), "");
    // bb4, the alloc's pad, drops _0 instead of _2.
    let wrong_local = |f: &mut Function| {
        f.blocks.get_mut(bb(4)).unwrap().stmts = vec![Stmt::Drop(Local::from_u32(0))];
    };
    let mut full = unwound(calls());
    wrong_local(&mut full);
    rejected(
        &full,
        "reference counts differ between the predecessors of bb2",
    );
    let found = recorded_errors(calls(), wrong_local);
    assert!(
        found.contains(
            "bb0 statement 2 unwinds to bb4, which drops {_0, _0}, but an unwinding there leaves {_0, _2} held (D-698)"
        ),
        "{found}"
    );
    // The alloc unwinds to the call's pad, which drops one value too few.
    let short = |f: &mut Function| set_edge(f, 0, 2, bb(2));
    let mut full = unwound(calls());
    short(&mut full);
    rejected(
        &full,
        "reference counts differ between the predecessors of bb2",
    );
    let found = recorded_errors(calls(), short);
    assert!(
        found.contains(
            "bb0 statement 2 unwinds to bb2, which drops {_0}, but an unwinding there leaves {_0, _2} held"
        ),
        "{found}"
    );
    // The entry pad drops only one of the two parameters.
    let entry = |f: &mut Function| f.entry_unwind = Some(bb(2));
    let mut full = unwound(calls());
    entry(&mut full);
    rejected(
        &full,
        "reference counts differ between the predecessors of bb2",
    );
    let found = recorded_errors(calls(), entry);
    assert!(
        found.contains(
            "the entry check unwinds to bb2, which drops {_0}, but an unwinding there leaves {_0, _1} held"
        ),
        "{found}"
    );
    // A pad that drops one value twice; and one that is not a chain of
    // drops at all, which only the pass's own pads must be.
    let twice = |f: &mut Function| {
        let pad = add_block(
            f,
            vec![Stmt::Drop(Local::from_u32(2))],
            Terminator::Goto {
                target: bb(4),
                args: Vec::new(),
            },
        );
        set_edge(f, 0, 2, pad);
    };
    let mut full = unwound(calls());
    twice(&mut full);
    rejected(&full, "_2 is used without a reference (holds 0, needs 1)");
    let found = recorded_errors(calls(), twice);
    assert!(
        found.contains("which drops {_0, _2, _2}, but an unwinding there leaves {_0, _2} held"),
        "{found}"
    );
    let not_a_chain = |f: &mut Function| {
        let pad = add_block(
            f,
            vec![
                Stmt::Drop(Local::from_u32(2)),
                Stmt::Drop(Local::from_u32(0)),
            ],
            Terminator::Goto {
                target: bb(1),
                args: Vec::new(),
            },
        );
        set_edge(f, 0, 2, pad);
    };
    let found = recorded_errors(calls(), not_a_chain);
    assert!(
        found.contains(
            "bb0 statement 2 unwinds to bb5, which is not a chain of drops ending in resume"
        ),
        "{found}"
    );
}

/// The pipeline gives the same MIR with and without the full simulation,
/// and accepts it both ways (D-774).
#[test]
fn unwind_release_validation_accepts_what_the_full_one_does() {
    // Before counting: `_2 = alloc (_0, _0); _3 = call _1(_2);
    // foreign cheby_io_println(_3); return _3`.
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let callee = b.add_param(Repr::Value, None, None).unwrap();
    let pair = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(x), local(x)],
            },
        )
        .unwrap();
    let result = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(callee),
                args: vec![local(pair)],
            },
        )
        .unwrap();
    b.push(Stmt::Eval(println(local(result)))).unwrap();
    b.terminate(Terminator::Return(local(result))).unwrap();
    let function = b.finish().unwrap();
    let mut dumps = Vec::new();
    for full_ownership_check in [false, true] {
        let options = MirOptions {
            full_ownership_check,
            ..MirOptions::default()
        };
        let mut m = module(vec![function.clone()]);
        count_and_check_with(&mut m, &options).unwrap();
        dumps.push(dump::dump(&m).unwrap());
    }
    assert_eq!(dumps[0], dumps[1]);
    assert!(dumps[0].contains("; unwind bb"), "{}", dumps[0]);
}

/// The pipeline reports what its unwind pass's simulation finds as invalid
/// MIR after reference counting (D-774).
#[test]
fn unwind_pipeline_reports_unbalanced_counts_after_reference_counting() {
    let mut f = calls();
    // `_0` is used once more than it is held.
    let block = f.blocks.get_mut(bb(0)).unwrap();
    block.stmts.retain(|s| *s != Stmt::Dup(Local::from_u32(0)));
    let mut m = module(vec![f]);
    let err = crate::options::unwind_recording(&mut m)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("invalid MIR after reference counting"),
        "{err}"
    );
    assert!(
        err.contains("bb0 statement 1: _0 is used without a reference (holds 0, needs 1)"),
        "{err}"
    );
    // The pass left the function as it was.
    assert!(!dump::dump(&m).unwrap().contains("unwind"));
}

/// A void foreign call of `cheby_io_println`, which never raises
/// (D-767).
fn println(text: Operand) -> Rvalue {
    Rvalue::CallForeign {
        symbol: "cheby_io_println".into(),
        sig: ForeignSig {
            params: vec![CType::Ptr],
            ret: CType::Void,
        },
        args: vec![text],
        blocking: false,
    }
}

// D-767: a foreign call of a kernel on the never-raise table has no unwind
// edge, and the validator refuses one; any other kernel's call has one.
#[test]
fn unwind_never_raising_kernels_get_no_edge() {
    for &symbol in crate::never_raise::NEVER_RAISE_KERNELS {
        let call = Rvalue::CallForeign {
            symbol: symbol.into(),
            sig: ForeignSig {
                params: Vec::new(),
                ret: CType::Void,
            },
            args: Vec::new(),
            blocking: false,
        };
        assert!(!rvalue_may_unwind(&call), "{symbol}");
        // D-818: a blocking call raises a cancellation after
        // it returns, whatever its symbol.
        let blocking = Rvalue::CallForeign {
            symbol: symbol.into(),
            sig: ForeignSig {
                params: Vec::new(),
                ret: CType::Void,
            },
            args: Vec::new(),
            blocking: true,
        };
        assert!(rvalue_may_unwind(&blocking));
    }
    // `_0` is printed, then passed to a kernel that may raise.
    let mut b = builder(0);
    let message = b.add_param(Repr::Value, None, None).unwrap();
    b.push(Stmt::Eval(println(local(message)))).unwrap();
    let raw = b
        .assign(
            Repr::Int,
            Rvalue::CallForeign {
                symbol: "cheby_kernel".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::I64,
                },
                args: vec![local(message)],
                blocking: false,
            },
        )
        .unwrap();
    b.push(Stmt::Drop(message)).unwrap();
    let boxed = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(raw),
                from: Repr::Int,
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(boxed))).unwrap();
    let before = b.finish().unwrap();
    let f = unwound(before.clone());
    let edges: Vec<u32> = f
        .blocks
        .get(bb(0))
        .unwrap()
        .unwind
        .iter()
        .map(|&(at, _)| at)
        .collect();
    // The kernel (statement 1) and the box (statement 3), not the print.
    assert_eq!(edges, [1, 3], "{}", text(&f));
    accepted(&f);
    assert_eq!(recorded_errors(before, |_| {}), "");
    let mut with_edge = f;
    set_edge(&mut with_edge, 0, 0, bb(2));
    rejected(
        &with_edge,
        "bb0 statement 0 has an unwind edge, but this statement cannot unwind (D-698)",
    );
}

// Rule: every Cheby-ABI function has its entry pad after the pass; a
// C-ABI function, which has no entry check, has none
// (D-752).
#[test]
fn unwind_rule_entry_pads() {
    let f = unwound(calls());
    accepted(&f);
    let mut none = f;
    none.entry_unwind = None;
    rejected(&none, "a Cheby-ABI function needs its entry pad");

    let mut b = FunctionBuilder::new(
        FnKey::Trampoline(1),
        Abi::C,
        Repr::Value,
        cheby_diag::Span::new(cheby_diag::FileId::new(0), cheby_diag::TextRange::empty(0)),
    )
    .unwrap();
    let closure = b.add_param(Repr::Value, None, None).unwrap();
    let arg = b.add_param(Repr::Value, None, None).unwrap();
    let out = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(closure),
                args: vec![local(arg)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(out))).unwrap();
    let trampoline = unwound(b.finish().unwrap());
    assert_eq!(trampoline.entry_unwind, None);
    assert_eq!(trampoline.blocks.get(bb(0)).unwrap().unwind, [(0, bb(1))]);
    accepted(&trampoline);
    let mut with_pad = trampoline;
    with_pad.entry_unwind = Some(bb(1));
    rejected(
        &with_pad,
        "a C-ABI function has no entry check, so no entry pad",
    );
}

const SOURCE: &str = "\
//! Calls, allocation, a panic and a list walk.

import std::io

type Pair {
  Pair(String, String),
}

fn swap(p: Pair) -> Pair {
  let Pair(left, right) = p
  Pair(right, left)
}

fn total(xs: List<Int>, acc: Int) -> Int {
  case xs {
    [] => acc
    [x, ..rest] => total(rest, acc + x)
  }
}

fn main() {
  let Pair(left, right) = swap(Pair(\"a\", \"b\"))
  case total([1, 2, 3], 0) {
    6 => io::println(\"{left}{right}\")
    _ => panic as \"unexpected\"
  }
}
";

#[test]
fn unwind_lowering_twice_gives_identical_dumps() {
    let first = lower_source(&[("", SOURCE)]).dump();
    let second = lower_source(&[("", SOURCE)]).dump();
    assert_eq!(first, second);
    assert!(first.contains("; unwind bb"), "{first}");
    assert!(first.contains("entry unwind bb"), "{first}");
}

#[test]
fn unwind_entry_thunks_get_edges_but_no_entry_pad() {
    let lowered = lower_source(&[("", SOURCE)]);
    let entries = lowered.units.iter().find(|u| u.module().is_none()).unwrap();
    let thunk = entries
        .functions
        .iter()
        .find(|f| matches!(f.key, FnKey::Entry(EntryKind::Main(_))))
        .unwrap();
    assert_eq!(thunk.entry_unwind, None);
    assert!(
        thunk.blocks.iter().any(|b| !b.unwind.is_empty()),
        "{}",
        lowered.dump()
    );
    validate(entries, Stage::Unwound).unwrap();
}

/// What the pad chain from `pad` releases before it resumes, ascending.
fn released(f: &Function, mut pad: BlockId) -> Vec<Local> {
    let mut out = Vec::new();
    loop {
        let block = f.blocks.get(pad).unwrap();
        for stmt in &block.stmts {
            let Stmt::Drop(l) = stmt else {
                panic!("a pad statement {stmt:?}");
            };
            out.push(*l);
        }
        match &block.term {
            Terminator::Goto { target, .. } => pad = *target,
            Terminator::Resume => break,
            other => panic!("a pad ends with {other:?}"),
        }
    }
    out.sort_unstable();
    out
}

#[test]
fn unwind_edges_off_validates_counted_without_edges() {
    // Before counting: `_2 = alloc (_0, _0); _3 = call _1(_2)`.
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let callee = b.add_param(Repr::Value, None, None).unwrap();
    let pair = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(x), local(x)],
            },
        )
        .unwrap();
    let result = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(callee),
                args: vec![local(pair)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(result))).unwrap();
    let function = b.finish().unwrap();
    let off = MirOptions {
        unwind_edges: false,
        ..MirOptions::default()
    };
    let mut without = module(vec![function.clone()]);
    count_and_check_with(&mut without, &off).unwrap();
    let text = dump::dump(&without).unwrap();
    assert!(!text.contains("unwind"), "{text}");
    validate(&without, Stage::Counted).unwrap();
    let mut with = module(vec![function]);
    count_and_check_with(&mut with, &MirOptions::default()).unwrap();
    let text = dump::dump(&with).unwrap();
    assert!(text.contains("; unwind bb"), "{text}");
    assert!(text.contains("entry unwind bb"), "{text}");
    // `dup _0; _2 = alloc (_0, _0); _3 = call _1(_2)`: the alloc leaves
    // both references of _0 and _1; the call consumes its callee and its
    // argument; the entry check leaves the parameters.
    let counted = &with.functions[0];
    let bb0 = counted.blocks.get(bb(0)).unwrap();
    assert_eq!(bb0.stmts[0], Stmt::Dup(x));
    let pad = |at| released(counted, bb0.unwind_target(at).unwrap());
    assert_eq!(pad(1), [x, x, callee]);
    assert_eq!(pad(2), []);
    assert_eq!(
        released(counted, counted.entry_unwind.unwrap()),
        [x, callee]
    );
}

/// One block of `n` calls `_k = call _0(_0)` whose results are unused
/// (counting drops them), before counting: the shape of a long `main` of
/// independent prints.
fn long_block(n: usize) -> Function {
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    for _ in 0..n {
        b.assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(x),
                args: vec![local(x)],
            },
        )
        .unwrap();
    }
    b.terminate(Terminator::Return(local(x))).unwrap();
    b.finish().unwrap()
}

#[test]
fn unwind_a_long_block_compiles_in_linear_time() {
    // Review finding R2-ROB-1: the ownership simulation kept a zero count
    // for every local the block had consumed and copied them all at each
    // call, and the validator's dominance walked from each call's segment
    // up the whole block to the shared pad: both quadratic in the block's
    // length, the first even with the pass off. 20_000 calls took minutes
    // in a debug build; the budget is generous for a loaded machine.
    let n = 20_000;
    for unwind_edges in [true, false] {
        let options = MirOptions {
            unwind_edges,
            ..MirOptions::default()
        };
        let mut m = module(vec![long_block(n)]);
        let start = std::time::Instant::now();
        count_and_check_with(&mut m, &options).unwrap();
        let took = start.elapsed();
        assert!(
            took < std::time::Duration::from_secs(10),
            "edges {unwind_edges}: {took:?}"
        );
        if unwind_edges {
            // Every call unwinds to the one pad that releases _0.
            let f = &m.functions[0];
            let bb0 = f.blocks.get(bb(0)).unwrap();
            assert_eq!(bb0.unwind.len(), n);
            let pad = bb0.unwind[0].1;
            assert!(bb0.unwind.iter().all(|&(_, p)| p == pad));
            assert_eq!(released(f, pad), [Local::from_u32(0)]);
        }
    }
}

// The pad chain's direction, chosen per function (D-777). Finding
// ROB-W3-1: with every chain built oldest local first, a function that
// releases its locals in the order it made them built about n²/2 pads.

/// What the chain from `pad` drops, in the order it drops them.
fn drop_order(f: &Function, mut pad: BlockId) -> Vec<Local> {
    let mut out = Vec::new();
    loop {
        let block = f.blocks.get(pad).unwrap();
        match (block.stmts.as_slice(), &block.term) {
            ([Stmt::Drop(l)], Terminator::Goto { target, .. }) => {
                out.push(*l);
                pad = *target;
            }
            ([], Terminator::Resume) => return out,
            other => panic!("a pad {other:?}"),
        }
    }
}

/// What the chain from `pad` of `plan` (numbered from 0) drops, in the
/// order it drops them.
fn plan_drop_order(plan: &PadPlan, mut pad: BlockId) -> Vec<Local> {
    let mut out = Vec::new();
    loop {
        let block = &plan.blocks[pad.index()];
        match (block.stmts.as_slice(), &block.term) {
            ([Stmt::Drop(l)], Terminator::Goto { target, .. }) => {
                out.push(*l);
                pad = *target;
            }
            ([], Terminator::Resume) => return out,
            other => panic!("a pad {other:?}"),
        }
    }
}

fn locals(range: impl Iterator<Item = usize>) -> Vec<Local> {
    range.map(|i| Local::from_usize(i).unwrap()).collect()
}

/// The pads [`plan_pads`] builds for `sets` in a function of `count`
/// locals, after checking that each set's chain drops exactly that set.
fn planned(count: usize, sets: &[Vec<Local>]) -> PadPlan {
    let slices: Vec<&[Local]> = sets.iter().map(Vec::as_slice).collect();
    let plan = plan_pads(0, count, &slices).unwrap();
    assert_eq!(plan.pads.len(), sets.len());
    for (set, &pad) in sets.iter().zip(&plan.pads) {
        let mut dropped = plan_drop_order(&plan, pad);
        dropped.sort_unstable();
        assert_eq!(&dropped, set);
    }
    // Planning twice gives the same pads (D-264).
    let again = plan_pads(0, count, &slices).unwrap();
    assert_eq!(again.pads, plan.pads);
    assert_eq!(again.blocks, plan.blocks);
    plan
}

#[test]
fn unwind_direction_newest_first_release_stays_ascending() {
    // Locals 0..n made in order and released newest first: every set is a
    // prefix, which D-698's ascending chains share.
    let n = 200;
    let mut sets: Vec<Vec<Local>> = (0..=n).map(|k| locals(0..k)).collect();
    sets.extend((0..n).rev().map(|k| locals(0..k)));
    let plan = planned(n, &sets);
    assert_eq!(plan.direction, Direction::Ascending);
    assert_eq!(plan.blocks.len(), n + 1);
    let all = *plan.pads.get(n).unwrap();
    assert_eq!(plan_drop_order(&plan, all), locals((0..n).rev()));
}

#[test]
fn unwind_direction_oldest_first_release_is_descending() {
    // Locals 0..n released oldest first with no unwind site while they
    // are made: every set is a suffix, which descending chains share.
    let n = 200;
    let sets: Vec<Vec<Local>> = (0..=n).map(|k| locals(k..n)).collect();
    let plan = planned(n, &sets);
    assert_eq!(plan.direction, Direction::Descending);
    assert_eq!(plan.blocks.len(), n + 1);
    let all = *plan.pads.first().unwrap();
    assert_eq!(plan_drop_order(&plan, all), locals(0..n));
}

#[test]
fn unwind_direction_made_and_released_in_order_is_nested() {
    // D-788: local 0 held throughout, locals 1..=n made
    // in order (each site holds a prefix) and released in the same order
    // (each site holds a suffix). Each direction shares only one of the
    // two halves and builds about n²/2 pads for the other.
    let n = 200;
    let with_base = |rest: Vec<Local>| {
        let mut set = vec![Local::from_u32(0)];
        set.extend(rest);
        set
    };
    let mut sets: Vec<Vec<Local>> = (1..=n + 1).map(|k| with_base(locals(1..k))).collect();
    sets.extend((2..=n + 1).map(|k| with_base(locals(k..=n))));
    let plan = planned(n + 1, &sets);
    assert_eq!(plan.direction, Direction::Nested);
    assert!(plan.blocks.len() <= 2 * n + 2, "{} pads", plan.blocks.len());
}

#[test]
fn unwind_direction_released_one_at_a_time_in_any_order_is_nested() {
    // D-788: locals 0..n made in order, then released one
    // at a time in a scrambled order (37k mod n), each from the middle of
    // what is held: each set holds one local fewer than the one before.
    let n = 200;
    let mut sets: Vec<Vec<Local>> = (0..=n).map(|k| locals(0..k)).collect();
    let mut held = locals(0..n);
    for k in 0..n {
        let gone = Local::from_usize(37 * k % n).unwrap();
        held.retain(|&l| l != gone);
        sets.push(held.clone());
    }
    let plan = planned(n, &sets);
    assert_eq!(plan.direction, Direction::Nested);
    assert!(plan.blocks.len() <= 2 * n + 1, "{} pads", plan.blocks.len());
}

#[test]
fn unwind_direction_tie_stays_ascending() {
    // Sliding windows of 10 of 110 locals (released from the middle): no
    // two windows share their oldest or their newest local, and no window
    // contains its neighbour, so every way builds 10 pads per window. The
    // count is above the linear bound, the others are counted, and
    // ascending wins the tie. This shape stays quadratic in the window.
    let (windows, width) = (100, 10);
    let sets: Vec<Vec<Local>> = (0..windows).map(|i| locals(i..i + width)).collect();
    let plan = planned(windows + width, &sets);
    assert_eq!(plan.direction, Direction::Ascending);
    assert_eq!(plan.blocks.len(), windows * width + 1);
    let first = *plan.pads.first().unwrap();
    assert_eq!(plan_drop_order(&plan, first), locals((0..width).rev()));
}

/// `n` results `_k = call _0(_0)` held, then each passed to `call _0(_k)`
/// in the order they were made (`oldest_first`) or the reverse, before
/// counting: every call may unwind, so the sites hold growing prefixes
/// of the results and then what the releases left.
fn made_then_released(n: usize, oldest_first: bool) -> Function {
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let mut made = Vec::new();
    for _ in 0..n {
        made.push(
            b.assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Indirect(x),
                    args: vec![local(x)],
                },
            )
            .unwrap(),
        );
    }
    if !oldest_first {
        made.reverse();
    }
    for l in made {
        b.assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(x),
                args: vec![local(l)],
            },
        )
        .unwrap();
    }
    b.terminate(Terminator::Return(local(x))).unwrap();
    b.finish().unwrap()
}

/// Counts `f` and runs the unwind pass, within the time budget of
/// `unwind_a_long_block_compiles_in_linear_time`, and returns the result
/// and its number of pads.
fn unwound_in_budget(f: Function) -> (Function, usize) {
    let mut m = module(vec![f]);
    let before = m.functions[0].blocks.len();
    let start = std::time::Instant::now();
    count_and_check_with(&mut m, &MirOptions::default()).unwrap();
    let took = start.elapsed();
    assert!(took < std::time::Duration::from_secs(10), "{took:?}");
    let f = m.functions.pop().unwrap();
    let pads = f.blocks.len().checked_sub(before).unwrap();
    (f, pads)
}

#[test]
fn unwind_oldest_first_release_is_linear() {
    let n = 300;
    let (f, pads) = unwound_in_budget(made_then_released(n, true));
    // 2n + c: one pad per local while they are made, one per release,
    // the parameter's and `resume`.
    assert!(pads <= 2 * n + 4, "{pads} pads:\n{}", text(&f));
    // The first release leaves the parameter and the newer results.
    let bb0 = f.blocks.get(bb(0)).unwrap();
    let (_, pad) = *bb0.unwind.get(n).unwrap();
    let mut held = vec![Local::from_u32(0)];
    held.extend(locals(2..=n));
    assert_eq!(released(&f, pad), held);
}

#[test]
fn unwind_newest_first_release_still_shares() {
    let n = 300;
    let (f, pads) = unwound_in_budget(made_then_released(n, false));
    assert!(pads <= n + 4, "{pads} pads:\n{}", text(&f));
    // Ascending (D-698's shape): the newest local is dropped first.
    let bb0 = f.blocks.get(bb(0)).unwrap();
    let (_, pad) = *bb0.unwind.get(n).unwrap();
    let order = drop_order(&f, pad);
    assert_eq!(order.len(), n);
    assert!(order.is_sorted_by(|a, b| a > b), "{order:?}");
}

/// ROB-W3-1's generator (`$S3/tools/u6_gen.py`): `f` holds `n` lists and
/// sums their lengths in the order it made them.
fn oldest_first_source(n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("import std::io\nimport std::list\n\nfn f(n: Int) -> Int {\n");
    for i in 0..n {
        writeln!(s, "  let l{i} = [n, {i}]").unwrap();
    }
    s.push_str("  let t0 = list::length(l0)\n");
    for (before, i) in (0..n).zip(1..n) {
        writeln!(s, "  let t{i} = t{before} + list::length(l{i})").unwrap();
    }
    writeln!(s, "  t{}\n}}\n", n.checked_sub(1).unwrap()).unwrap();
    s.push_str("fn main() {\n  let r = f(1)\n  io::println(\"{r}\")\n}\n");
    s
}

#[test]
fn unwind_oldest_first_release_from_source_is_linear() {
    let n = 300;
    let source = oldest_first_source(n);
    let start = std::time::Instant::now();
    let lowered = lower_source(&[("", &source)]);
    let took = start.elapsed();
    // Before D-777 this function had about n²/2 = 45,000 pads. The budget
    // covers the whole front end and std in a debug build.
    assert!(took < std::time::Duration::from_secs(60), "{took:?}");
    let unit = lowered.unit("app");
    validate(unit, Stage::Unwound).unwrap();
    let f = lowered.function("app", "app::f");
    let blocks = f.lines().filter(|l| l.starts_with("bb")).count();
    println!("u6 source N={n}: {blocks} blocks in app::f, {took:?}");
    assert!(blocks <= 2 * n + 8, "{blocks} blocks:\n{f}");
}
