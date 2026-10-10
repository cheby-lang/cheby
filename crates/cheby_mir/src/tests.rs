//! The builder, the RC pass, the validator and the dump on hand-built MIR.

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, ModuleId};

use crate::abi::AbiSig;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, BlockId, Callee, CalleeDecl, Const, DefName, EntryKind, FnKey, Function, LayoutRef, Local,
    MirModule, Operand, PanicKind, Rvalue, Stmt, Terminator, UnitKind,
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

/// `app::f0` is the function under test; `app::f1` takes one value.
fn unit(unit: UnitKind, functions: Vec<Function>) -> MirModule {
    MirModule {
        unit,
        path: "app".into(),
        functions,
        callees: vec![CalleeDecl {
            def: def(1),
            sig: AbiSig {
                abi: Abi::Cheby,
                params: vec![Repr::Value],
                ret: Repr::Value,
            },
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: (0..3)
            .map(|n| DefName {
                def: def(n),
                module: "app".into(),
                item: format!("f{n}").into(),
            })
            .collect(),
    }
}

fn code(functions: Vec<Function>) -> MirModule {
    unit(UnitKind::Code(MODULE), functions)
}

fn builder() -> FunctionBuilder {
    let mut b = FunctionBuilder::new(FnKey::Item(def(0)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

fn nil() -> Operand {
    Operand::Const(Const::Nil)
}

fn string(b: &mut FunctionBuilder, text: &str) -> Local {
    let s = b.string(text).unwrap();
    b.assign(Repr::Value, Rvalue::Use(Operand::Const(Const::Str(s))))
        .unwrap()
}

fn call_f1(arg: Operand) -> Rvalue {
    Rvalue::Call {
        callee: Callee::Direct(def(1)),
        args: vec![arg],
    }
}

fn print(arg: Local) -> Stmt {
    Stmt::Eval(Rvalue::CallForeign {
        symbol: "cheby_io_print".into(),
        sig: ForeignSig {
            params: vec![CType::Ptr],
            ret: CType::Void,
        },
        args: vec![local(arg)],
    })
}

/// Runs the pass on a single-function module, validates before and after,
/// and returns the function's statements per block as text.
fn counted(f: Function) -> Vec<Vec<String>> {
    let mut m = code(vec![f]);
    validate(&m, Stage::Lowered).unwrap();
    rc::insert_module(&mut m).unwrap();
    if let Err(errors) = validate(&m, Stage::Counted) {
        panic!("{errors:#?}\n{}", dump(&m).unwrap());
    }
    blocks(&m)
}

fn blocks(m: &MirModule) -> Vec<Vec<String>> {
    let text = dump(m).unwrap();
    let mut out: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        if line.starts_with("bb") {
            out.push(Vec::new());
        } else if let (Some(last), Some(stmt)) = (out.last_mut(), line.strip_prefix("  "))
            && !stmt.starts_with("str#")
        {
            last.push(stmt.to_owned());
        }
    }
    out
}

fn errors(m: &MirModule, stage: Stage) -> String {
    validate(m, stage).unwrap_err().join("\n")
}

#[test]
fn owned_last_use_needs_no_dup() {
    let mut b = builder();
    let s = string(&mut b, "a");
    b.terminate(Terminator::Return(local(s))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [["_0: value = const str#1", "return _0"]]
    );
}

#[test]
fn second_owned_use_dups_once() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    b.assign(Repr::Value, call_f1(local(x))).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![local(x)],
    })
    .unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [[
            "dup _0",
            "_1: value = call app::f1(_0)",
            "drop _1",
            "tail_call app::f1(_0)"
        ]]
    );
}

#[test]
fn two_owned_uses_in_one_instruction() {
    let mut b = builder();
    let x = string(&mut b, "a");
    let t = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(x), local(x)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(t))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [[
            "_0: value = const str#1",
            "dup _0",
            "_1: value = alloc tuple/2(_0, _0)",
            "return _1"
        ]]
    );
}

#[test]
fn unused_definitions_and_parameters_are_dropped() {
    let mut b = builder();
    b.add_param(Repr::Value, None, None).unwrap();
    b.add_param(Repr::Int, None, None).unwrap();
    string(&mut b, "a");
    b.terminate(Terminator::Return(nil())).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [[
            "drop _0",
            "_2: value = const str#1",
            "drop _2",
            "return const nil"
        ]]
    );
}

#[test]
fn borrowed_last_use_drops_after() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let y = string(&mut b, "a");
    b.push(print(x)).unwrap();
    b.push(print(y)).unwrap();
    // Every local still holding a reference is dropped before the tail
    // call, so the callee's frame replaces ours with nothing pending.
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![local(y)],
    })
    .unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [[
            "_1: value = const str#1",
            "call_foreign \"cheby_io_print\"(borrow _0) -> void",
            "drop _0",
            "call_foreign \"cheby_io_print\"(borrow _1) -> void",
            "tail_call app::f1(_1)"
        ]]
    );
}

#[test]
fn branch_rule_drops_at_the_start_of_the_successor() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let c = b.add_param(Repr::Bool, None, None).unwrap();
    let then_ = b.new_block().unwrap();
    let else_ = b.new_block().unwrap();
    b.terminate(Terminator::Branch {
        cond: local(c),
        then_,
        else_,
    })
    .unwrap();
    b.switch_to(then_).unwrap();
    b.terminate(Terminator::Return(local(x))).unwrap();
    b.switch_to(else_).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [
            vec!["branch _1, bb1, bb2"],
            vec!["return _0"],
            vec!["drop _0", "return const nil"],
        ]
    );
}

#[test]
fn branch_rule_splits_edges_into_joins() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let c = b.add_param(Repr::Bool, None, None).unwrap();
    let use_it = b.new_block().unwrap();
    let join = b.new_block().unwrap();
    b.terminate(Terminator::Branch {
        cond: local(c),
        then_: use_it,
        else_: join,
    })
    .unwrap();
    b.switch_to(use_it).unwrap();
    b.push(print(x)).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [
            vec!["branch _1, bb1, bb3"],
            vec![
                "call_foreign \"cheby_io_print\"(borrow _0) -> void",
                "drop _0",
                "goto bb2()"
            ],
            vec!["return const nil"],
            vec!["drop _0", "goto bb2()"],
        ]
    );
}

#[test]
fn block_parameters_are_owned() {
    let mut b = builder();
    let x = string(&mut b, "a");
    let join = b.new_block().unwrap();
    let p = b.add_block_param(join, Repr::Value).unwrap();
    let unused = b.add_block_param(join, Repr::Value).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: vec![local(x), local(x)],
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(local(p))).unwrap();
    let _ = unused;
    assert_eq!(
        counted(b.finish().unwrap()),
        [
            vec!["_0: value = const str#1", "dup _0", "goto bb1(_0, _0)"],
            vec!["drop _2", "return _1"],
        ]
    );
}

#[test]
fn field_result_is_dupped_before_its_base_is_dropped() {
    let mut b = builder();
    let base = b.add_param(Repr::Value, None, None).unwrap();
    let field = b
        .assign(
            Repr::Value,
            Rvalue::Field {
                base,
                layout: LayoutRef::Tuple(2),
                field: 0,
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(field))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [[
            "_1: value = field _0.0 of tuple/2",
            "dup _1",
            "drop _0",
            "return _1"
        ]]
    );
}

#[test]
fn panics_leak_live_locals() {
    let mut b = builder();
    let x = string(&mut b, "boom");
    let loc = b.string("src/main.cheby:2:3").unwrap();
    b.terminate(Terminator::Panic {
        kind: PanicKind::Explicit,
        message: Some(local(x)),
        loc,
    })
    .unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [["_0: value = const str#1", "panic explicit _0 at str#2"]]
    );
}

#[test]
fn rc_insertion_runs_once() {
    let mut b = builder();
    string(&mut b, "a");
    b.terminate(Terminator::Return(nil())).unwrap();
    let mut m = code(vec![b.finish().unwrap()]);
    rc::insert_module(&mut m).unwrap();
    assert!(rc::insert_module(&mut m).is_err());
}

// Validator: structure.

#[test]
fn builder_rejects_unterminated_blocks() {
    let mut b = builder();
    b.new_block().unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    assert!(b.finish().is_err());
    let mut b = builder();
    b.terminate(Terminator::Return(nil())).unwrap();
    assert!(b.terminate(Terminator::Unreachable).is_err());
    assert!(b.push(Stmt::Dup(Local::from_u32(0))).is_err());
}

#[test]
fn rejects_double_definitions() {
    let mut b = builder();
    let x = string(&mut b, "a");
    b.push(Stmt::Assign {
        dst: x,
        value: Rvalue::Use(nil()),
    })
    .unwrap();
    b.terminate(Terminator::Return(local(x))).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(errors(&m, Stage::Lowered).contains("_0 is defined more than once"));
}

#[test]
fn rejects_uses_not_dominated_by_their_definition() {
    let mut b = builder();
    let c = b.add_param(Repr::Bool, None, None).unwrap();
    let left = b.new_block().unwrap();
    let right = b.new_block().unwrap();
    let join = b.new_block().unwrap();
    b.terminate(Terminator::Branch {
        cond: local(c),
        then_: left,
        else_: right,
    })
    .unwrap();
    b.switch_to(left).unwrap();
    let x = string(&mut b, "a");
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(right).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(local(x))).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(
        errors(&m, Stage::Lowered)
            .contains("_1 is used in bb3 where its definition does not dominate")
    );
}

#[test]
fn rejects_use_before_definition_in_a_block() {
    let mut b = builder();
    let later = b.new_local(Repr::Value, None, None).unwrap();
    b.push(print(later)).unwrap();
    b.push(Stmt::Assign {
        dst: later,
        value: Rvalue::Use(nil()),
    })
    .unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(errors(&m, Stage::Lowered).contains("does not dominate"));
}

#[test]
fn rejects_goto_argument_mismatches() {
    let mut b = builder();
    let join = b.new_block().unwrap();
    b.add_block_param(join, Repr::Int).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: vec![nil(), nil()],
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("bb0 passes 2 arguments to bb1, which takes 1"));
    assert!(text.contains("goto argument: expected int, found value"));
}

#[test]
fn rejects_call_mismatches() {
    let mut b = builder();
    b.assign(Repr::Value, call_f1(Operand::Const(Const::Int(1))))
        .unwrap();
    b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Direct(def(2)),
            args: Vec::new(),
        },
    )
    .unwrap();
    let f = b.add_param(Repr::Value, None, None).unwrap();
    b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Indirect(f),
            args: vec![Operand::Const(Const::Bool(true))],
        },
    )
    .unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Int(0))))
        .unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("call argument: expected value, found int"));
    assert!(text.contains("has no declaration"));
    assert!(text.contains("indirect call argument: expected value, found bool"));
    assert!(text.contains("returned value: expected value, found int"));
}

#[test]
fn rejects_misplaced_c_abi_and_tail_calls() {
    // A C-ABI item.
    let mut b = FunctionBuilder::new(FnKey::Item(def(0)), Abi::C, Repr::Value, span()).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("only entry thunks and trampolines use the C ABI"));

    // An entry thunk outside an entries unit, with a tail call.
    let key = FnKey::Entry(EntryKind::Main(def(0)));
    let mut b = FunctionBuilder::new(key, Abi::C, Repr::I64, span()).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![nil()],
    })
    .unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("entry thunks belong in an entries unit"));
    assert!(text.contains("a tail call in a C-ABI function"));

    // A Cheby function in an entries unit, without its entry check.
    let mut b = FunctionBuilder::new(FnKey::Item(def(0)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let text = errors(
        &unit(UnitKind::Entries, vec![b.finish().unwrap()]),
        Stage::Lowered,
    );
    assert!(text.contains(
        "an entries unit holds only entry thunks, trampolines, value shims, tuple functions and `dyn` forwarders"
    ));
    assert!(text.contains("needs its entry-check location"));
}

#[test]
fn accepts_a_well_formed_entry_unit() {
    let key = FnKey::Entry(EntryKind::Main(def(0)));
    let mut b = FunctionBuilder::new(key, Abi::C, Repr::I64, span()).unwrap();
    let r = b.assign(Repr::Value, call_f1(nil())).unwrap();
    b.push(Stmt::Drop(r)).unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Sized {
        repr: Repr::I64,
        bits: 0,
    })))
    .unwrap();
    let m = unit(UnitKind::Entries, vec![b.finish().unwrap()]);
    validate(&m, Stage::Counted).unwrap();
    assert!(errors(&m, Stage::Lowered).contains("dup or drop before reference-count insertion"));
}

#[test]
fn rejects_signature_disagreements() {
    // `f1` is declared with one value parameter but defined with none.
    let mut b = FunctionBuilder::new(FnKey::Item(def(1)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("x:1:1").unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("differs from its callee declaration"));
}

#[test]
fn rejects_branches_to_blocks_with_parameters() {
    let mut b = builder();
    let c = b.add_param(Repr::Bool, None, None).unwrap();
    let target = b.new_block().unwrap();
    b.add_block_param(target, Repr::Value).unwrap();
    b.terminate(Terminator::Branch {
        cond: local(c),
        then_: target,
        else_: target,
    })
    .unwrap();
    b.switch_to(target).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let text = errors(&code(vec![b.finish().unwrap()]), Stage::Lowered);
    assert!(text.contains("branches to bb1, which takes parameters"));
}

// Validator: ownership.

#[test]
fn rejects_leaks() {
    let mut b = builder();
    string(&mut b, "a");
    b.terminate(Terminator::Return(nil())).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    validate(&m, Stage::Lowered).unwrap();
    assert!(errors(&m, Stage::Counted).contains("_0 still holds 1 reference(s)"));
}

#[test]
fn rejects_double_drops() {
    let mut b = builder();
    let x = string(&mut b, "a");
    b.push(Stmt::Drop(x)).unwrap();
    b.push(Stmt::Drop(x)).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(errors(&m, Stage::Counted).contains("_0 is used without a reference"));
}

#[test]
fn rejects_use_after_move() {
    let mut b = builder();
    let x = string(&mut b, "a");
    b.assign(Repr::Value, call_f1(local(x))).unwrap();
    b.terminate(Terminator::Return(local(x))).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(errors(&m, Stage::Counted).contains("_0 is used without a reference"));
}

#[test]
fn rejects_field_dup_after_the_base_is_dropped() {
    let mut b = builder();
    let base = b.add_param(Repr::Value, None, None).unwrap();
    let field = b
        .assign(
            Repr::Value,
            Rvalue::Field {
                base,
                layout: LayoutRef::Tuple(1),
                field: 0,
            },
        )
        .unwrap();
    b.push(Stmt::Drop(base)).unwrap();
    b.push(Stmt::Dup(field)).unwrap();
    b.terminate(Terminator::Return(local(field))).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(errors(&m, Stage::Counted).contains("dup of _1 which holds no reference"));
}

#[test]
fn rejects_unbalanced_joins() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let c = b.add_param(Repr::Bool, None, None).unwrap();
    let left = b.new_block().unwrap();
    let right = b.new_block().unwrap();
    let join = b.new_block().unwrap();
    b.terminate(Terminator::Branch {
        cond: local(c),
        then_: left,
        else_: right,
    })
    .unwrap();
    b.switch_to(left).unwrap();
    b.push(Stmt::Drop(x)).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(right).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    assert!(
        errors(&m, Stage::Counted)
            .contains("reference counts differ between the predecessors of bb3")
    );
}

// Dump.

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one function exercising every dump form"
)]
fn dump_covers_the_grammar() {
    let mut b = builder();
    let num = b.add_param(Repr::Int, None, None).unwrap();
    let obj = b.add_param(Repr::Value, None, None).unwrap();
    let loc = b.string("src/main.cheby:3:7").unwrap();
    let sum = b
        .assign(
            Repr::Int,
            Rvalue::Checked {
                op: crate::ArithOp::Add,
                lhs: local(num),
                rhs: Operand::Const(Const::Int(-2)),
                repr: Repr::Int,
                loc,
            },
        )
        .unwrap();
    let small = b
        .assign(
            Repr::Bool,
            Rvalue::Compare {
                op: crate::CmpOp::Lt,
                lhs: local(sum),
                rhs: Operand::Const(Const::Int(10)),
                repr: Repr::Int,
            },
        )
        .unwrap();
    let yes = b.new_block().unwrap();
    let no = b.new_block().unwrap();
    let join = b.new_block().unwrap();
    let param = b.add_block_param(join, Repr::Value).unwrap();
    b.terminate(Terminator::Branch {
        cond: local(small),
        then_: yes,
        else_: no,
    })
    .unwrap();
    b.switch_to(yes).unwrap();
    let boxed = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(sum),
                from: Repr::Int,
            },
        )
        .unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: vec![local(boxed)],
    })
    .unwrap();
    b.switch_to(no).unwrap();
    let tag = b
        .assign(Repr::U32, Rvalue::ReadLayout { base: obj })
        .unwrap();
    let other = b.new_block().unwrap();
    let nil_case = join_arg_block(&mut b, join);
    b.terminate(Terminator::Switch {
        on: local(tag),
        cases: vec![(16, nil_case)],
        default: Some(other),
    })
    .unwrap();
    b.switch_to(other).unwrap();
    b.terminate(Terminator::Panic {
        kind: PanicKind::Todo,
        message: None,
        loc,
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![local(param)],
    })
    .unwrap();
    let mut m = code(vec![b.finish().unwrap()]);
    validate(&m, Stage::Lowered).unwrap();
    rc::insert_module(&mut m).unwrap();
    validate(&m, Stage::Counted).unwrap();
    let expected = r#"module app

fn app::f0(_0: int, _1: value) -> value check str#0 {
  str#0 = "src/main.cheby:1:4"
  str#1 = "src/main.cheby:3:7"
bb0:
  _2: int = checked add int _0, const int -2 at str#1
  _3: bool = compare lt int _2, const int 10
  branch _3, bb1, bb2
bb1:
  drop _1
  _5: value = box int _2
  goto bb3(_5)
bb2:
  _6: u32 = read_layout _1
  drop _1
  switch _6 [16 => bb5, _ => bb4]
bb3(_4: value):
  tail_call app::f1(_4)
bb4:
  panic todo at str#1
bb5:
  goto bb3(const nil)
}

callee app::f1(value) -> value
"#;
    assert_eq!(dump(&m).unwrap(), expected);
}

/// A block that jumps to `join` with `Nil`.
fn join_arg_block(b: &mut FunctionBuilder, join: BlockId) -> BlockId {
    let here = b.current();
    let block = b.new_block().unwrap();
    b.switch_to(block).unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: vec![nil()],
    })
    .unwrap();
    b.switch_to(here).unwrap();
    block
}

#[test]
fn dump_escapes_strings_and_names_keys() {
    let mut b = FunctionBuilder::new(
        FnKey::Closure {
            owner: def(0),
            index: 2,
        },
        Abi::Cheby,
        Repr::Value,
        span(),
    )
    .unwrap();
    b.set_entry_loc("a \"quoted\"\npath").unwrap();
    let this = b.add_param(Repr::Value, None, None).unwrap();
    let s = b.string("tab\there").unwrap();
    let t = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Closure(1),
                fields: vec![
                    Operand::Const(Const::Code(FnKey::ValueShim(def(1)))),
                    Operand::Const(Const::Str(s)),
                ],
            },
        )
        .unwrap();
    let _ = this;
    b.terminate(Terminator::Return(local(t))).unwrap();
    let m = code(vec![b.finish().unwrap()]);
    validate(&m, Stage::Lowered).unwrap();
    let text = dump(&m).unwrap();
    assert!(
        text.contains("fn closure app::f0#2(_0: value) -> value check str#0 {"),
        "{text}"
    );
    assert!(text.contains(r#"str#0 = "a \"quoted\"\npath""#), "{text}");
    assert!(text.contains(r#"str#1 = "tab\there""#), "{text}");
    assert!(
        text.contains("_1: value = alloc closure/1(const code <shim app::f1>, const str#1)"),
        "{text}"
    );
}

#[test]
fn panic_kinds_follow_the_runtime_numbering() {
    let codes: Vec<u32> = PanicKind::ALL.iter().map(|k| k.code()).collect();
    assert_eq!(codes, (0..11).collect::<Vec<u32>>());
}
