//! The release profile's inliner and its clean-up (design T5 to T7): the
//! splice on hand-built MIR, at a call in a statement and at a tail call,
//! with constant arguments, several returns, call locations and source
//! positions; the selection on whole programs (recursion stays, a function
//! stays under D-780's bound, a `Float` helper inlines to no box, a
//! wrapper's shape stays a call); and the clean-up's folds. Every changed
//! unit is checked as lowered and then counted with the full ownership
//! check (D-774).

use std::fmt::Write as _;

use crate::abi::AbiSig;
use crate::builder::FunctionBuilder;
use crate::golden_tests::check_golden;
use crate::inline::{CallSite, CopiedPositions, SiteAt, cost, optimize, splice};
use crate::ir::{
    Abi, BlockId, Callee, CalleeDecl, CmpOp, Const, FnKey, Function, LayoutRef, Local, MirModule,
    Operand, PanicKind, Rvalue, Stmt, Terminator,
};
use crate::positions::Pos;
use crate::repr::{CType, ForeignSig, Repr};
use crate::test_support::{Lowered, lower_source, lower_source_release};
use crate::unwind_ir_tests::{builder, def, module};
use crate::validate::{Stage, validate};
use crate::{MirOptions, count_lowered_with, dump, dump_function};

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

/// A uniform signature of `n` values and `ints` integers.
fn sig(params: Vec<Repr>) -> AbiSig {
    AbiSig::owned(Abi::Cheby, params, Repr::Value)
}

/// `f1(a: value, n: int)`: returns `a` when `n < 0`, panics on `n == 0`
/// and otherwise tail-calls `f2(a, n - 1)`, an item of another unit. Two
/// returns, a tail call, a panic and a checked operation with their
/// locations.
fn branching_callee() -> Function {
    let mut b = builder(1);
    let a = b.add_param(Repr::Value, None, None).unwrap();
    let n = b.add_param(Repr::Int, None, None).unwrap();
    let neg = b.new_block().unwrap();
    let rest = b.new_block().unwrap();
    let zero = b.new_block().unwrap();
    let more = b.new_block().unwrap();
    let below = b
        .assign(
            Repr::Bool,
            Rvalue::Compare {
                op: CmpOp::Lt,
                lhs: local(n),
                rhs: Operand::Const(Const::Int(0)),
                repr: Repr::Int,
            },
        )
        .unwrap();
    b.terminate(Terminator::Branch {
        cond: local(below),
        then_: neg,
        else_: rest,
    })
    .unwrap();
    b.switch_to(neg).unwrap();
    b.terminate(Terminator::Return(local(a))).unwrap();
    b.switch_to(rest).unwrap();
    b.terminate(Terminator::Switch {
        on: local(n),
        cases: vec![(0, zero)],
        default: Some(more),
    })
    .unwrap();
    b.switch_to(zero).unwrap();
    let at = b.string("src/main.cheby:3:5").unwrap();
    b.terminate(Terminator::Panic {
        kind: PanicKind::Explicit,
        message: None,
        loc: at,
    })
    .unwrap();
    b.switch_to(more).unwrap();
    let sub_at = b.string("src/main.cheby:4:9").unwrap();
    let m = b
        .assign(
            Repr::Int,
            Rvalue::Checked {
                op: crate::ir::ArithOp::Sub,
                lhs: local(n),
                rhs: Operand::Const(Const::Int(1)),
                repr: Repr::Int,
                loc: sub_at,
            },
        )
        .unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(2)),
        args: vec![local(a), local(m)],
    })
    .unwrap();
    b.finish().unwrap()
}

/// The unit of `functions`, declaring `f1(value, int)` and `f2(value,
/// int)` as callees.
fn unit_of(functions: Vec<Function>) -> MirModule {
    let mut unit = module(functions);
    unit.callees = vec![
        CalleeDecl {
            def: def(1),
            sig: sig(vec![Repr::Value, Repr::Int]),
        },
        CalleeDecl {
            def: def(2),
            sig: sig(vec![Repr::Value, Repr::Int]),
        },
    ];
    unit
}

/// Checks `unit` as lowered, then counts it with the full ownership check
/// (D-774), panicking with the dump on a finding.
fn check_and_count(mut unit: MirModule) {
    if let Err(errors) = validate(&unit, Stage::Lowered) {
        panic!("{}\n{}", errors.join("\n"), dump(&unit).unwrap());
    }
    let options = MirOptions {
        full_ownership_check: true,
        ..MirOptions::default()
    };
    if let Err(ice) = count_lowered_with(&mut unit, &options) {
        panic!("{ice}\n{}", dump(&unit).unwrap());
    }
}

/// The dump of the function of `unit` with key `key`.
fn dump_of(unit: &MirModule, key: FnKey) -> String {
    let f = unit.function(key).unwrap();
    dump_function(unit, f).unwrap()
}

/// D-015: at a call in a statement the block splits; each return of the
/// callee jumps to the continuation with its value, its tail call becomes
/// a call and that jump, its panic keeps its location text, and the
/// arguments stand for the parameters. The rest of the block, which uses
/// the result, moves to the continuation.
#[test]
fn a_call_in_a_statement_splits_its_block() {
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let r = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(1)),
                args: vec![local(x), Operand::Const(Const::Int(5))],
            },
        )
        .unwrap();
    let pair = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(r), local(x)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(pair))).unwrap();
    let mut host = b.finish().unwrap();
    let callee = branching_callee();
    let spliced = splice(
        &mut host,
        CallSite {
            block: BlockId::from_u32(0),
            at: SiteAt::Stmt(0),
        },
        &callee,
        CopiedPositions::Own,
    )
    .unwrap();
    assert_eq!(spliced.cont, Some(BlockId::from_u32(1)));
    assert_eq!(spliced.body.len(), callee.blocks.len());
    let unit = unit_of(vec![host, callee]);
    let text = dump_of(&unit, FnKey::Item(def(0)));
    check_golden("inline/call_in_a_statement", &text);
    assert!(text.contains("\"src/main.cheby:3:5\""), "{text}");
    check_and_count(unit);
}

/// D-015: at a tail call the callee's returns stay returns and its tail
/// call stays a tail call, so the stack does not grow.
#[test]
fn a_tail_call_keeps_returns_and_tail_calls() {
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![local(x), Operand::Const(Const::Int(-1))],
    })
    .unwrap();
    let mut host = b.finish().unwrap();
    let callee = branching_callee();
    let spliced = splice(
        &mut host,
        CallSite {
            block: BlockId::from_u32(0),
            at: SiteAt::Tail,
        },
        &callee,
        CopiedPositions::Own,
    )
    .unwrap();
    assert_eq!(spliced.cont, None);
    let unit = unit_of(vec![host, callee]);
    let text = dump_of(&unit, FnKey::Item(def(0)));
    check_golden("inline/tail_call", &text);
    assert!(text.contains("tail_call app::f2("), "{text}");
    assert_eq!(text.matches("return ").count(), 1, "{text}");
    check_and_count(unit);
}

/// A constant argument stands for its parameter as an operand, and gets a
/// local where only a local may stand: here an indirect callee.
#[test]
fn a_constant_argument_gets_a_local_only_where_one_must_stand() {
    // f1(g: value, n: int) = g(n boxed), twice: once called, once passed.
    let mut b = builder(1);
    let g = b.add_param(Repr::Value, None, None).unwrap();
    let n = b.add_param(Repr::Int, None, None).unwrap();
    let boxed = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(n),
                from: Repr::Int,
            },
        )
        .unwrap();
    let pair = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![local(g), local(boxed)],
            },
        )
        .unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Indirect(g),
        args: vec![local(pair)],
    })
    .unwrap();
    let callee = b.finish().unwrap();
    let mut b = builder(0);
    let r = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(1)),
                args: vec![
                    Operand::Const(Const::FnValue(def(3))),
                    Operand::Const(Const::Int(7)),
                ],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(r))).unwrap();
    let mut host = b.finish().unwrap();
    splice(
        &mut host,
        CallSite {
            block: BlockId::from_u32(0),
            at: SiteAt::Stmt(0),
        },
        &callee,
        CopiedPositions::Own,
    )
    .unwrap();
    let mut unit = unit_of(vec![host, callee]);
    unit.callees = vec![CalleeDecl {
        def: def(1),
        sig: sig(vec![Repr::Value, Repr::Int]),
    }];
    let text = dump_of(&unit, FnKey::Item(def(0)));
    check_golden("inline/constant_arguments", &text);
    assert!(text.contains("= const fn app::f3"), "{text}");
    assert!(text.contains("box int const int 7"), "{text}");
    check_and_count(unit);
}

/// D-856, D-938: a copied call keeps its own location and a tail call
/// that became a call takes the inlined call's, the locations stay sorted
/// by local, and the body's positions join the caller's table.
#[test]
#[expect(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    reason = "two hand-built functions and two splices"
)]
fn call_locations_and_positions_follow_the_body() {
    let at = |line, column| Pos { line, column };
    let mut b = builder(1);
    b.set_record_positions();
    b.set_record_call_locs(true);
    let a = b.add_param(Repr::Value, None, None).unwrap();
    let n = b.add_param(Repr::Int, None, None).unwrap();
    b.set_position(at(2, 3)).unwrap();
    let first = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(2)),
                args: vec![local(a), local(n)],
            },
        )
        .unwrap();
    b.record_call_loc(first, "src/main.cheby:2:3").unwrap();
    b.set_position(at(3, 3)).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(2)),
        args: vec![local(first), local(n)],
    })
    .unwrap();
    let callee = b.finish().unwrap();
    let mut b = builder(0);
    b.set_record_positions();
    b.set_record_call_locs(true);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    b.set_position(at(9, 5)).unwrap();
    let r = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(1)),
                args: vec![local(x), Operand::Const(Const::Int(1))],
            },
        )
        .unwrap();
    b.record_call_loc(r, "src/main.cheby:9:5").unwrap();
    b.set_position(at(10, 5)).unwrap();
    let s = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(2)),
                args: vec![local(r), Operand::Const(Const::Int(2))],
            },
        )
        .unwrap();
    b.record_call_loc(s, "src/main.cheby:10:5").unwrap();
    b.terminate(Terminator::Return(local(s))).unwrap();
    let mut host = b.finish().unwrap();
    splice(
        &mut host,
        CallSite {
            block: BlockId::from_u32(0),
            at: SiteAt::Stmt(0),
        },
        &callee,
        CopiedPositions::Own,
    )
    .unwrap();
    let locs: Vec<(u32, &str)> = host
        .call_locs
        .iter()
        .map(|&(l, s)| (l.as_u32(), &**host.call_loc_strings.get(s).unwrap()))
        .collect();
    assert!(locs.windows(2).all(|w| w[0].0 < w[1].0), "{locs:?}");
    let texts: Vec<&str> = locs.iter().map(|&(_, t)| t).collect();
    // The host's later call, the copied call, the tail call made a call.
    assert_eq!(
        texts,
        [
            "src/main.cheby:10:5",
            "src/main.cheby:2:3",
            "src/main.cheby:9:5"
        ]
    );
    assert!(locs.iter().all(|&(l, _)| l != r.as_u32()), "{locs:?}");
    for want in [at(2, 3), at(3, 3), at(9, 5), at(10, 5)] {
        assert!(host.positions.table.contains(&want), "{:?}", host.positions);
    }
    let body = host.blocks.get(BlockId::from_u32(2)).unwrap();
    let lines: Vec<u32> = body
        .pos
        .iter()
        .map(|&p| host.positions.get(p).unwrap().line)
        .collect();
    // The copied call, the tail call made a call, and the jump.
    assert_eq!(lines, [2, 3, 3]);
    // With the call's positions instead, every copied statement is at the
    // call.
    let mut again = builder(0);
    again.set_record_positions();
    let y = again.add_param(Repr::Value, None, None).unwrap();
    again.set_position(at(20, 1)).unwrap();
    again
        .terminate(Terminator::TailCall {
            callee: Callee::Direct(def(1)),
            args: vec![local(y), Operand::Const(Const::Int(1))],
        })
        .unwrap();
    let mut other = again.finish().unwrap();
    splice(
        &mut other,
        CallSite {
            block: BlockId::from_u32(0),
            at: SiteAt::Tail,
        },
        &callee,
        CopiedPositions::AtCall,
    )
    .unwrap();
    assert_eq!(other.positions.table, [at(20, 1)]);
    let unit = unit_of(vec![host, callee]);
    check_and_count(unit);
}

/// The selection leaves a call whose callee has the shape of an
/// `@external` wrapper (a counted parameter passed to a foreign function,
/// D-525), and one that names a continuation (D-780).
#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "the locals of three hand-built functions"
)]
fn wrappers_and_owners_of_continuations_stay_calls() {
    let mut b = builder(1);
    let a = b.add_param(Repr::Value, None, None).unwrap();
    let n = b.add_param(Repr::Int, None, None).unwrap();
    let raw = b
        .assign(
            Repr::Int,
            Rvalue::CallForeign {
                symbol: "app_size".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr, CType::I64],
                    ret: CType::I64,
                },
                args: vec![local(a), local(n)],
                blocking: false,
            },
        )
        .unwrap();
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
    let wrapper = b.finish().unwrap();
    let mut b = builder(2);
    let a = b.add_param(Repr::Value, None, None).unwrap();
    let _n = b.add_param(Repr::Int, None, None).unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Code(FnKey::Continuation {
            owner: def(2),
            index: 0,
        }),
        args: vec![local(a)],
    })
    .unwrap();
    let owner = b.finish().unwrap();
    let mut b = builder(0);
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let r = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(1)),
                args: vec![local(x), Operand::Const(Const::Int(1))],
            },
        )
        .unwrap();
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(2)),
        args: vec![local(r), Operand::Const(Const::Int(2))],
    })
    .unwrap();
    let host = b.finish().unwrap();
    let mut unit = unit_of(vec![host, wrapper, owner]);
    optimize(&mut unit).unwrap();
    let text = dump_of(&unit, FnKey::Item(def(0)));
    assert!(text.contains("call app::f1("), "{text}");
    assert!(text.contains("tail_call app::f2("), "{text}");
}

/// A program for the selection: a `Float` helper in a loop, recursion and
/// mutual recursion, a small `Int` helper, an `Order` helper whose result
/// a `case` matches, and a sign function called with a constant.
const PROGRAM: &str = r#"//! Inlining in a release build.

import std::float
import std::io

/// A(i, j), a helper that returns a boxed `Float` (D-453).
fn a_of(i: Int, j: Int) -> Float {
  let ij = i + j
  1.0 / float::from_int(ij * (ij + 1) / 2 + i + 1)
}

/// The sum of A(i, j) for j below n.
fn row_sum(i: Int, j: Int, n: Int, sum: Float) -> Float {
  case j == n {
    True => sum
    False => row_sum(i, j + 1, n, sum + a_of(i, j))
  }
}

fn count_down(n: Int) -> Int {
  case n {
    0 => 0
    _ => count_down(n - 1)
  }
}

fn ping(n: Int) -> Int {
  case n {
    0 => 0
    _ => pong(n - 1)
  }
}

fn pong(n: Int) -> Int {
  case n {
    0 => 1
    _ => ping(n - 1)
  }
}

fn add1(n: Int) -> Int {
  n + 1
}

fn order_of(a: Int, b: Int) -> Order {
  case {
    a < b => Less
    a > b => Greater
    _ => Equal
  }
}

fn describe(a: Int, b: Int) -> String {
  case order_of(a, b) {
    Less => "less"
    Equal => "equal"
    Greater => "greater"
  }
}

fn sign(n: Int) -> Int {
  case {
    n < 0 => -1
    n == 0 => 0
    _ => 1
  }
}

fn main() {
  let s = row_sum(1, 0, 10, 0.0)
  let c = count_down(3) + ping(3) + add1(2) + sign(5)
  let d = describe(1, 2)
  io::println("{s} {c} {d}")
}
"#;

/// The release units of [`PROGRAM`], counted when `counted`.
fn program(counted: bool) -> Lowered {
    lower_source_release(&[("", PROGRAM)], counted)
}

/// The item named `item` of `unit`.
fn named<'u>(unit: &'u MirModule, item: &str) -> &'u Function {
    unit.functions
        .iter()
        .find(|f| matches!(f.key, FnKey::Item(d) if unit.name(d).is_ok_and(|n| &*n.item == item)))
        .unwrap_or_else(|| panic!("no {item}"))
}

/// The helper of cheby-compare's `spectral_norm`: inlined into the loop, its
/// result's box and the loop's unbox cancel, so the loop boxes only what
/// it returns. Its `float::from_int` is a conversion, with no call and no
/// unwind edge (`crate::kernels_instr`).
#[test]
fn a_float_helper_inlines_to_no_box() {
    let counted = program(true);
    let row = counted.function("app", "app::row_sum");
    check_golden("inline/float_helper", &row);
    assert!(!row.contains("app::a_of"), "{row}");
    assert_eq!(row.matches("box float").count(), 1, "{row}");
    assert!(!row.contains("unbox"), "{row}");
    assert!(row.contains("= convert int float "), "{row}");
    assert!(!row.contains("call_foreign"), "{row}");
}

/// A function in a cycle of the call graph is never inlined: every loop
/// keeps its function entry, and so its stack check and yield (D-453).
#[test]
fn recursion_is_not_inlined() {
    let lowered = program(false);
    let unit = lowered.unit("app");
    let main = dump_function(unit, named(unit, "main")).unwrap();
    assert!(main.contains("call app::count_down("), "{main}");
    assert!(main.contains("call app::ping("), "{main}");
    assert!(!main.contains("app::add1"), "{main}");
    assert!(!main.contains("app::sign"), "{main}");
    let ping = dump_function(unit, named(unit, "ping")).unwrap();
    assert!(ping.contains("tail_call app::pong("), "{ping}");
    let count_down = dump_function(unit, named(unit, "count_down")).unwrap();
    assert!(
        count_down.contains("tail_call app::count_down("),
        "{count_down}"
    );
}

/// The clean-up folds what inlining exposes: `sign(5)` is `1` with no
/// comparison left, and the `Order` that `order_of` returns goes straight
/// to its arm, with no tag read.
#[test]
fn constants_and_static_constructors_fold() {
    let lowered = program(false);
    let unit = lowered.unit("app");
    let main = dump_function(unit, named(unit, "main")).unwrap();
    assert!(!main.contains("compare"), "{main}");
    let describe = dump_function(unit, named(unit, "describe")).unwrap();
    check_golden("inline/order_threaded", &describe);
    assert!(!describe.contains("read_tag"), "{describe}");
    assert!(!describe.contains("app::order_of"), "{describe}");
}

/// D-780: inlining never grows a function past the bound, however many
/// tiny calls it makes.
#[test]
fn a_function_never_passes_the_bound() {
    let mut lets = String::new();
    for n in 0..200 {
        writeln!(lets, "  let x{} = twice(x{n})", n + 1).unwrap();
    }
    // The chain starts from a parameter, so the clean-up cannot fold it.
    let source = format!(
        "//! Many calls of a tiny helper.\n\nimport std::int\nimport std::io\n\n\
         fn twice(x: Int) -> Int {{\n  x * 2 % 1000\n}}\n\n\
         fn chain(x0: Int) -> Int {{\n{lets}  x200\n}}\n\n\
         fn main() {{\n  io::println(int::show(chain(1)))\n}}\n"
    );
    let lowered = lower_source_release(&[("", source.as_str())], false);
    let unit = lowered.unit("app");
    let main = named(unit, "chain");
    assert!(cost(main) <= crate::lower::SPLIT_SIZE, "{}", cost(main));
    let text = dump_function(unit, main).unwrap();
    assert!(text.contains("call app::twice("), "nothing left to inline");
    assert!(
        text.matches("checked mul").count() > 10,
        "too few inlined:\n{text}"
    );
    // The counted program is valid too.
    let counted = lower_source_release(&[("", source.as_str())], true);
    assert!(
        counted
            .function("app", "app::chain")
            .contains("checked rem")
    );
}

/// The inliner and its clean-up are deterministic (D-264): the same
/// program gives the same text twice.
#[test]
fn release_lowering_is_deterministic() {
    assert_eq!(program(true).dump(), program(true).dump());
}

/// A function the builder makes with a `bb0` and nothing else is its own
/// size: one block, no statements. Cost is statements plus blocks.
#[test]
fn cost_counts_statements_and_blocks() {
    let mut b = FunctionBuilder::new(
        FnKey::Item(def(0)),
        Abi::Cheby,
        Repr::Value,
        cheby_diag::Span::new(cheby_diag::FileId::new(0), cheby_diag::TextRange::empty(0)),
    )
    .unwrap();
    let one = b.new_local(Repr::Int, None, None).unwrap();
    b.push(Stmt::Assign {
        dst: one,
        value: Rvalue::Use(Operand::Const(Const::Int(1))),
    })
    .unwrap();
    b.terminate(Terminator::Return(Operand::Const(Const::Nil)))
        .unwrap();
    assert_eq!(cost(&b.finish().unwrap()), 2);
}

/// The clean-up folds a switch on a negative sized constant to its case:
/// a case holds the value's bits within its width, while the constant's
/// bits are sign-extended (std's `i16::checked_div(min_value, -1)` takes
/// its `-1` arm).
#[test]
fn a_switch_on_a_negative_sized_constant_takes_its_case() {
    let source = "//! Classifies an `I16`.

import std::int
import std::io

fn classify(b: I16) -> Int {
  case b {
    0 => 10
    -1 => 11
    _ => 12
  }
}

fn main() {
  io::println(int::show(classify(-1)))
}
";
    let lowered = lower_source_release(&[("", source)], false);
    let main = lowered.function("app", "app::main");
    assert!(main.contains("const int 11"), "{main}");
    assert!(!main.contains("const int 12"), "{main}");
}

/// D-839: a helper with a join of its own (`size`'s `case`) is not
/// inlined at a call in a statement where the join would stop the matched
/// node's block from reaching the node rebuilt after the call, the shape of
/// `std::sorted_map::insert`: the release build reuses as many nodes as the
/// debug build.
#[test]
fn a_join_that_would_lose_reuse_is_not_inlined() {
    let source = "//! A tree that rebuilds its nodes.

import std::int
import std::io

type Tree {
  Leaf,
  Node(Int, Int, Tree, Tree),
}

fn size(t: Tree) -> Int {
  case t {
    Leaf => 0
    Node(n, _, _, _) => n
  }
}

fn insert(t: Tree, k: Int) -> Tree {
  case t {
    Leaf => Node(1, k, Leaf, Leaf)
    Node(n, x, l, r) => case k < x {
      True => {
        let grown = insert(l, k)
        let s = size(grown) + size(r) + 1
        Node(s, x, grown, r)
      }
      False => {
        let grown = insert(r, k)
        let s = size(l) + size(grown) + 1
        Node(s, x, l, grown)
      }
    }
  }
}

fn main() {
  let t = insert(insert(insert(Leaf, 2), 1), 3)
  io::println(int::show(size(t)))
}
";
    let debug = lower_source(&[("", source)]);
    let release = lower_source_release(&[("", source)], true);
    let reuses = |l: &Lowered| {
        l.function("app", "app::insert")
            .matches("alloc_reuse")
            .count()
    };
    assert_eq!(
        reuses(&debug),
        2,
        "{}",
        debug.function("app", "app::insert")
    );
    assert_eq!(
        reuses(&release),
        2,
        "{}",
        release.function("app", "app::insert")
    );
}
