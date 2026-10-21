//! Tests of the hoist pass (D-911): field reads move up to the statement
//! that consumes a sibling projection, on lowered programs under both RC
//! modes, and the rules that keep a read where it is, on hand-built MIR.

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, ModuleId, Ty};

use crate::builder::FunctionBuilder;
use crate::hoist::{CROSSING_BUDGET, run};
use crate::ir::{
    Abi, BlockId, Callee, Const, FnKey, Function, LayoutRef, Local, Operand, RuntimeFn, Rvalue,
    Stmt, Terminator,
};
use crate::repr::Repr;
use crate::test_support::{lower_package_options, lower_source, lower_source_uncounted};
use crate::{MirOptions, RcMode};

/// The micro-benchmark shapes of D-911: a list field pushed while the
/// record's other fields are read after the push, written as field
/// accesses (`step`) and as a record update (`update`).
const STATE: &str = "import std::io
import std::list

type State { items: List<Int>, count: Int, sum: Int }

fn step(s: State, n: Int) -> State {
  State { items: list::push(s.items, n), count: s.count + 1, sum: s.sum + n }
}

fn update(s: State, n: Int) -> State {
  State { ..s, items: list::push(s.items, n) }
}

fn main() {
  let s = update(step(State { items: [], count: 0, sum: 0 }, 1), 2)
  io::println(\"{s.count}\")
}
";

/// The index of the first line of `dump` that holds `needle`.
fn line_of(dump: &str, needle: &str) -> usize {
    dump.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{dump}"))
}

#[test]
fn hoist_moves_field_reads_before_the_push() {
    let lowered = lower_source_uncounted(&[("", STATE)]);
    for name in ["step", "update"] {
        let f = lowered.function("app", &format!("app::{name}"));
        let push = line_of(&f, "call std::list::push(");
        let count = line_of(&f, "field _0.1 of ctor app::State");
        let sum = line_of(&f, "field _0.2 of ctor app::State");
        assert!(count < push && sum < push, "{f}");
    }
}

#[test]
fn hoist_lets_perceus_release_the_record_before_the_push() {
    let lowered = lower_source(&[("", STATE)]);
    for name in ["step", "update"] {
        let f = lowered.function("app", &format!("app::{name}"));
        let push = line_of(&f, "call std::list::push(");
        // The record is released, keeping its list, before the push, and
        // its block is reused for the result.
        let release = line_of(&f, "= drop_reuse _0 of ctor app::State keep [_2 @0]");
        assert!(release < push, "{f}");
        assert!(f.contains("alloc_reuse"), "{f}");
    }
}

#[test]
fn hoist_lets_naive_counting_release_the_record_before_the_push() {
    let options = MirOptions {
        rc: RcMode::Naive,
        ..MirOptions::default()
    };
    let lowered = lower_package_options("app", &[("", STATE)], options).unwrap();
    for name in ["step", "update"] {
        let f = lowered.function("app", &format!("app::{name}"));
        let push = line_of(&f, "call std::list::push(");
        let release = line_of(&f, "  drop _0");
        assert!(release < push, "{f}");
    }
}

#[test]
fn hoist_leaves_a_record_that_may_hold_a_handle() {
    // A `Sender` in a field nothing reads would close its channel before
    // the source's last use of the record (D-205).
    let source = "import std::channel
import std::io
import std::list

type Held { items: List<Int>, tx: channel::Sender<Int>, count: Int }

fn step(h: Held, n: Int) -> (List<Int>, Int) {
  (list::push(h.items, n), h.count + 1)
}

fn main() {
  let (tx, _rx) = channel::new::<Int>(1)
  let (items, count) = step(Held { items: [], tx, count: 0 }, 1)
  let n = list::length(items)
  io::println(\"{n} {count}\")
}
";
    let f = lower_source_uncounted(&[("", source)]).function("app", "app::step");
    let push = line_of(&f, "call std::list::push(");
    let count = line_of(&f, "field _0.2 of ctor app::Held");
    assert!(push < count, "{f}");
}

#[test]
fn hoist_leaves_a_record_of_a_type_variable() {
    let source = "import std::io
import std::list

type Tagged<T> { items: List<Int>, tag: T, count: Int }

fn step<T>(t: Tagged<T>, n: Int) -> (List<Int>, Int) {
  (list::push(t.items, n), t.count + 1)
}

fn main() {
  let (items, count) = step(Tagged { items: [], tag: \"x\", count: 0 }, 1)
  let n = list::length(items)
  io::println(\"{n} {count}\")
}
";
    let f = lower_source_uncounted(&[("", source)]).function("app", "app::step");
    let push = line_of(&f, "call std::list::push(");
    let count = line_of(&f, "field _0.2 of ctor app::Tagged");
    assert!(push < count, "{f}");
}

#[test]
fn hoist_leaves_a_record_used_whole_later() {
    let source = "import std::io
import std::list

type State { items: List<Int>, count: Int }

fn step(s: State, n: Int) -> (List<Int>, Int, State) {
  (list::push(s.items, n), s.count + 1, s)
}

fn main() {
  let (items, count, _) = step(State { items: [], count: 0 }, 1)
  let n = list::length(items)
  io::println(\"{n} {count}\")
}
";
    let f = lower_source_uncounted(&[("", source)]).function("app", "app::step");
    let push = line_of(&f, "call std::list::push(");
    let count = line_of(&f, "field _0.1 of ctor app::State");
    assert!(push < count, "{f}");
}

// Hand-built MIR: `_0` is the base, of a type the tests call handle-free.

const MODULE: ModuleId = ModuleId::from_u32(1);

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

fn builder() -> (FunctionBuilder, Local) {
    let def = DefId::new(MODULE, DefIndex::from_u32(0));
    let mut b = FunctionBuilder::new(FnKey::Item(def), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    let base = b.add_param(Repr::Value, None, Some(Ty::INT)).unwrap();
    (b, base)
}

const TRIPLE: LayoutRef = LayoutRef::Tuple(3);

fn field(b: &mut FunctionBuilder, base: Local, i: u32) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::Field {
            base,
            layout: TRIPLE,
            field: i,
        },
    )
    .unwrap()
}

fn call(b: &mut FunctionBuilder, callee: Callee, args: Vec<Operand>) -> Local {
    b.assign(Repr::Value, Rvalue::Call { callee, args })
        .unwrap()
}

fn direct() -> Callee {
    Callee::Direct(DefId::new(MODULE, DefIndex::from_u32(1)))
}

fn tuple(b: &mut FunctionBuilder, fields: &[Local]) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::Alloc {
            layout: TRIPLE,
            fields: fields.iter().copied().map(Operand::Local).collect(),
        },
    )
    .unwrap()
}

/// The locals the statements of block 0 define, in order.
fn defs(f: &Function) -> Vec<Local> {
    f.block(BlockId::from_u32(0))
        .unwrap()
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Assign { dst, .. } => Some(*dst),
            _ => None,
        })
        .collect()
}

fn hoisted(b: FunctionBuilder, handle_free: bool) -> Vec<Local> {
    let mut f = b.finish().unwrap();
    run(&mut f, &|_| handle_free).unwrap();
    defs(&f)
}

#[test]
fn hoist_moves_reads_before_a_direct_call() {
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 1);
    let read2 = field(&mut fb, base, 2);
    let out = tuple(&mut fb, &[res, read, read2]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, read, read2, res, out]);
}

#[test]
fn hoist_needs_a_handle_free_base() {
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 1);
    let out = tuple(&mut fb, &[res, read, read]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, false), [proj, res, read, out]);
}

#[test]
fn hoist_moves_reads_before_a_slice_at_the_lists_last_use() {
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let slice = fb
        .call_runtime(
            RuntimeFn::ListSlice,
            vec![
                Operand::Local(proj),
                Operand::Const(Const::Int(1)),
                Operand::Const(Const::Int(2)),
            ],
        )
        .unwrap()
        .unwrap();
    let read = field(&mut fb, base, 1);
    let out = tuple(&mut fb, &[slice, read, read]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, read, slice, out]);
}

#[test]
fn hoist_ignores_a_called_closure() {
    // A closure called as a function is not updated in place.
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, Callee::Indirect(proj), Vec::new());
    let read = field(&mut fb, base, 1);
    let out = tuple(&mut fb, &[res, read, read]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, res, read, out]);
}

#[test]
fn hoist_needs_the_projections_last_use() {
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 1);
    let out = tuple(&mut fb, &[res, read, proj]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, res, read, out]);
}

#[test]
fn hoist_needs_the_base_dead_at_the_blocks_end() {
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 1);
    let out = tuple(&mut fb, &[res, read, base]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, res, read, out]);

    let (mut fb, base) = builder();
    let next = fb.new_block().unwrap();
    let proj = field(&mut fb, base, 0);
    let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 1);
    fb.terminate(Terminator::Goto {
        target: next,
        args: Vec::new(),
    })
    .unwrap();
    fb.switch_to(next).unwrap();
    let out = tuple(&mut fb, &[res, read, base]);
    fb.terminate(Terminator::Return(Operand::Local(out)))
        .unwrap();
    assert_eq!(hoisted(fb, true), [proj, res, read]);
}

#[test]
fn hoist_moves_to_the_earliest_consumer_after_the_bases_other_uses() {
    // `_0` is passed whole to the first call, so only the reads after the
    // second, which consumes `proj2`, may move, and they move before it.
    let (mut fb, base) = builder();
    let proj = field(&mut fb, base, 0);
    let proj2 = field(&mut fb, base, 1);
    let res = call(&mut fb, direct(), vec![Operand::Local(base)]);
    let mid = call(&mut fb, direct(), vec![Operand::Local(proj2)]);
    let res2 = call(&mut fb, direct(), vec![Operand::Local(proj)]);
    let read = field(&mut fb, base, 2);
    let out = tuple(&mut fb, &[res, mid, res2]);
    let top = tuple(&mut fb, &[out, read, read]);
    fb.terminate(Terminator::Return(Operand::Local(top)))
        .unwrap();
    assert_eq!(
        hoisted(fb, true),
        [proj, proj2, res, read, mid, res2, out, top]
    );
}

#[test]
fn hoist_keeps_reads_that_would_cross_too_many_calls() {
    // Two reads after `calls` calls that may unwind: moved, each would
    // cross the consumer and every call.
    let shape = |calls: usize| {
        let (mut fb, base) = builder();
        let proj = field(&mut fb, base, 0);
        let res = call(&mut fb, direct(), vec![Operand::Local(proj)]);
        for _ in 0..calls {
            call(&mut fb, direct(), Vec::new());
        }
        let read = field(&mut fb, base, 1);
        let read2 = field(&mut fb, base, 2);
        let out = tuple(&mut fb, &[res, read, read2]);
        fb.terminate(Terminator::Return(Operand::Local(out)))
            .unwrap();
        let moved = hoisted(fb, true);
        (moved.get(1) == Some(&read), moved.get(2) == Some(&read2))
    };
    // 2 * (1 + calls) crossings.
    let most = CROSSING_BUDGET / 2 - 1;
    assert_eq!(shape(most), (true, true));
    assert_eq!(shape(most + 1), (false, false));
}
