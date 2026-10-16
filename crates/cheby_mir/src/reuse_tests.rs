//! Tests of reuse (D-716): the pass on programs as `cheby run` counts
//! them, on hand-built MIR, its decisions (the reuse report's input), the
//! parameters it keeps owned (D-806), and the validator's token rules
//! (GAP(reuse-token-form), GAP(reuse-pairing-scope),
//! GAP(reuse-and-large-tuples), GAP(reuse-owns-parameters)).

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Idx, ModuleId};

use crate::abi::AbiSig;
use crate::borrow::BorrowMask;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, BlockId, Callee, CalleeDecl, Const, DefName, FnKey, Function, LayoutRef, Local, MirModule,
    Operand, Rvalue, Stmt, TagType, Terminator, UnitKind,
};
use crate::repr::Repr;
use crate::reuse::{ReuseDecision, ReuseReason, decisions};
use crate::test_support::lower_source;
use crate::validate::{Stage, validate};
use crate::{FieldDesc, LayoutDesc, MirOptions, RcMode, count_lowered_with, dump, type_key};

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

/// A code unit of `app` holding `functions`, whose keys are `app::f0`
/// to `app::f3`; `app::f1` takes one value and is declared as a callee.
fn module(functions: Vec<Function>) -> MirModule {
    MirModule {
        unit: UnitKind::Code(MODULE),
        path: "app".into(),
        functions,
        callees: vec![CalleeDecl {
            def: def(1),
            sig: AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value),
        }],
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

fn builder(key: FnKey) -> FunctionBuilder {
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn nil() -> Operand {
    Operand::Const(Const::Nil)
}

fn nils(n: u32) -> Vec<Operand> {
    (0..n).map(|_| nil()).collect()
}

fn field(b: &mut FunctionBuilder, base: Local, layout: LayoutRef, field: u32) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::Field {
            base,
            layout,
            field,
        },
    )
    .unwrap()
}

/// `f0(x)`: reads field 0 of `x` as a tuple of `from`, then allocates a
/// tuple of `to` from it and `Nil`s and returns it; lowered, not counted.
fn rebuild(from: u32, to: u32) -> Function {
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let first = field(&mut b, x, LayoutRef::Tuple(from), 0);
    let mut fields = vec![Operand::Local(first)];
    fields.extend(nils(to.checked_sub(1).unwrap()));
    let made = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(to),
                fields,
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    b.finish().unwrap()
}

/// `functions` counted as `cheby run` counts a unit on its own, with
/// `options`; the dump.
fn counted(functions: Vec<Function>, options: MirOptions) -> (MirModule, String) {
    let mut unit = module(functions);
    validate(&unit, Stage::Lowered).unwrap();
    count_lowered_with(&mut unit, &options).unwrap();
    let text = dump(&unit).unwrap();
    (unit, text)
}

/// The dump of `app::<name>` in `source`'s `app` unit, as `cheby run`
/// counts it.
fn function_of(source: &str, name: &str) -> String {
    lower_source(&[("", source)]).function("app", &format!("app::{name}"))
}

const RECORD: &str = r"//! Record updates.

type Point {
  x: Int,
  y: Int,
}

const origin: Point = Point { x: 0, y: 0 }

fn moved(p: Point) -> Point {
  Point { ..p, x: 3 }
}

fn kept(p: Point) -> Int {
  let q = Point { ..p, y: 5 }
  p.x + q.y
}

fn from_constant() -> Point {
  Point { ..origin, x: 7 }
}

fn both(p: Point) -> (Point, Point) {
  let q = Point { ..p, y: 5 }
  (p, q)
}

fn main() {
  let p = moved(Point { x: 1, y: 2 })
  let _ = kept(p)
  let _ = from_constant()
  let _ = both(p)
}
";

const TREE: &str = r"//! A persistent tree's insert.

import std::int

type Tree {
  Leaf,
  Node(Tree, Int, Tree),
}

fn insert(tree: Tree, key: Int) -> Tree {
  case tree {
    Leaf => Node(Leaf, key, Leaf)
    Node(left, here, right) => case int::compare(key, here) {
      Less => Node(insert(left, key), here, right)
      Greater => Node(left, here, insert(right, key))
      Equal => tree
    }
  }
}

fn main() {
  let _ = insert(insert(Leaf, 2), 1)
}
";

/// A record update whose base dies there reuses the base's block, and the
/// base, which D-806 alone would borrow, stays owned (D-806 refined).
#[test]
fn reuse_record_update_reuses_its_base() {
    let text = function_of(RECORD, "moved");
    assert!(text.starts_with("fn app::moved(_0: value)"), "{text}");
    assert!(
        text.contains("= drop_reuse _0 of ctor app::Point"),
        "{text}"
    );
    assert!(text.contains("= alloc_reuse _"), "{text}");
    assert!(!text.contains("= alloc ctor"), "{text}");
}

/// The rebuild of the matched constructor in one arm (`tree_inserts`)
/// reuses the matched node; the token is held across the recursive call,
/// whose pad releases it with a plain `drop` (D-698).
#[test]
fn reuse_pattern_and_rebuild_in_one_arm_reuses_the_node() {
    let text = function_of(TREE, "insert");
    assert!(
        text.starts_with("fn app::insert(_0: value, _1: int)"),
        "{text}"
    );
    let reuses = text.matches("drop_reuse _0 of ctor app::Node").count();
    assert_eq!(reuses, 2, "both rebuilding arms reuse:\n{text}");
    assert_eq!(text.matches("alloc_reuse").count(), 2, "{text}");
    // `Leaf` is static and has no fields: never a source; its arm
    // allocates.
    assert!(text.contains("= alloc ctor app::Node(const"), "{text}");
    // Each token is dropped by the pad of the call it is held across.
    for line in text.lines().filter(|l| l.contains("drop_reuse")) {
        let token = line.trim().split(':').next().unwrap();
        assert!(
            text.contains(&format!("  drop {token}\n")),
            "no pad drops {token}:\n{text}"
        );
    }
}

const NESTED: &str = r"//! Rebuilds in nested arms where no arm uses the matched tree, the shape
//! of `std::sorted_map::insert`.

import std::int

type Tree {
  Leaf,
  Node(Tree, Int, Tree),
}

fn size(tree: Tree) -> Int {
  case tree {
    Leaf => 0
    Node(left, _, right) => size(left) + size(right) + 1
  }
}

fn insert(tree: Tree, key: Int) -> Tree {
  case tree {
    Leaf => Node(Leaf, key, Leaf)
    Node(left, here, right) => case int::compare(key, here) {
      Less => Node(insert(left, key), here, right)
      Greater => Node(left, here, insert(right, key))
      Equal => Node(left, key, right)
    }
  }
}

fn trim(tree: Tree, key: Int) -> Tree {
  case tree {
    Leaf => Leaf
    Node(left, here, right) => case int::compare(key, here) {
      Less => {
        let smaller = trim(left, key)
        case size(smaller) > size(right) {
          True => Node(smaller, here, right)
          False => smaller
        }
      }
      Greater => Node(left, here, trim(right, key))
      Equal => left
    }
  }
}

fn main() {
  let tree = insert(insert(insert(Leaf, 2), 1), 3)
  let _ = trim(tree, 1)
}
";

/// The matched node rebuilt in each arm of a nested `case` whose arms do
/// not use the matched tree (gate 6's ROB6-1): counting drops the tree in
/// the block that reads its fields, before the inner switch, and its one
/// token goes to the rebuild of each arm, held across the recursive call.
#[test]
fn reuse_rebuild_in_nested_arms_takes_the_token_across_blocks() {
    let text = function_of(NESTED, "insert");
    assert!(
        text.starts_with("fn app::insert(_0: value, _1: int)"),
        "the tree stays owned (D-806 refined):\n{text}"
    );
    assert_eq!(
        text.matches("drop_reuse _0 of ctor app::Node").count(),
        1,
        "one drop gives the token:\n{text}"
    );
    assert_eq!(
        text.matches("alloc_reuse").count(),
        3,
        "every rebuilding arm reuses:\n{text}"
    );
    assert!(
        !text.contains("alloc ctor app::Node(_"),
        "no arm allocates a node of its own:\n{text}"
    );
    // The `Leaf` arm has no node to reuse.
    assert!(text.contains("= alloc ctor app::Node(const"), "{text}");
}

/// A path that reaches no rebuild drops the token where it leaves the
/// rebuilding blocks: the `Equal` arm and the `False` arm of a `case`
/// nested two levels below the drop each start with a `drop` of it.
#[test]
fn reuse_token_is_dropped_on_paths_without_a_rebuild() {
    let text = function_of(NESTED, "trim");
    assert_eq!(
        text.matches("drop_reuse _0 of ctor app::Node").count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("alloc_reuse").count(), 2, "{text}");
    // Outside the cleanup pads (the blocks reached through normal
    // successors from bb0), the token is dropped twice: at the start of the
    // `Equal` arm and of the `False` arm.
    let lowered = lower_source(&[("", NESTED)]);
    let unit = lowered.unit("app");
    let f = unit
        .functions
        .iter()
        .find(|f| crate::dump_function(unit, f).is_ok_and(|d| d.starts_with("fn app::trim(")))
        .unwrap();
    let token = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .find_map(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::DropReuse { .. },
            } => Some(*dst),
            _ => None,
        })
        .unwrap();
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![0_usize];
    let mut starts = 0;
    while let Some(b) = work.pop() {
        if std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = f.blocks.get(BlockId::from_usize(b).unwrap()).unwrap();
        let drops = block
            .stmts
            .iter()
            .filter(|s| **s == Stmt::Drop(token))
            .count();
        if drops > 0 {
            assert_eq!(drops, 1, "bb{b} drops the token twice:\n{text}");
            assert!(
                block
                    .stmts
                    .iter()
                    .take_while(|s| matches!(s, Stmt::Drop(_)))
                    .any(|s| *s == Stmt::Drop(token)),
                "bb{b} drops the token after its start:\n{text}"
            );
            starts += 1;
        }
        work.extend(block.term.successors().into_iter().map(Idx::index));
    }
    assert_eq!(
        starts, 2,
        "the Equal and False arms drop the token:\n{text}"
    );
}

/// A token held across a branch whose other edge enters a block with
/// another predecessor is dropped on a new block that splits that edge:
/// `bb0` drops `x` and branches to `bb1`, which rebuilds, and to the join
/// `bb2`, which `bb1` also enters.
#[test]
fn reuse_token_edge_into_a_join_is_split() {
    let layout = LayoutRef::Tuple(2);
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let cond = b.add_param(Repr::Bool, None, None).unwrap();
    let first = field(&mut b, x, layout, 0);
    let rebuild = b.new_block().unwrap();
    let join = b.new_block().unwrap();
    b.terminate(Terminator::Branch {
        cond: Operand::Local(cond),
        then_: rebuild,
        else_: join,
    })
    .unwrap();
    b.switch_to(rebuild).unwrap();
    b.assign(
        Repr::Value,
        Rvalue::Alloc {
            layout,
            fields: nils(2),
        },
    )
    .unwrap();
    b.terminate(Terminator::Goto {
        target: join,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(join).unwrap();
    b.terminate(Terminator::Return(Operand::Local(first)))
        .unwrap();
    let (unit, text) = counted(vec![b.finish().unwrap()], MirOptions::default());
    assert!(text.contains("= drop_reuse _0 of tuple/2"), "{text}");
    assert!(text.contains("= alloc_reuse _"), "{text}");
    let function = &unit.functions[0];
    let token = function
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .find_map(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::DropReuse { .. },
            } => Some(*dst),
            _ => None,
        })
        .unwrap();
    // bb0's `else` edge now goes to a new block that drops the token and
    // jumps to the join.
    let Terminator::Branch { else_, .. } = function.blocks.iter().next().unwrap().term else {
        panic!("bb0 does not branch:\n{text}");
    };
    assert_ne!(else_, join, "{text}");
    let split = function.blocks.get(else_).unwrap();
    assert_eq!(split.stmts, vec![Stmt::Drop(token)], "{text}");
    assert_eq!(
        split.term,
        Terminator::Goto {
            target: join,
            args: Vec::new()
        },
        "{text}"
    );
    let decisions = decisions(&unit, function).unwrap();
    assert_eq!(
        decisions,
        [ReuseDecision {
            sink: decisions[0].sink,
            source: Some(x),
            reason: ReuseReason::Reused,
        }]
    );
}

const SHARED: &str = r#"//! Field reads every variant shares (D-071) on variants of different
//! sizes, and reads after a test of the variant.

type Shape {
  Rect { name: String, width: Int, height: Int, depth: Int },
  Circle { name: String, radius: Int },
}

fn widen(s: Shape, k: Int) -> Shape {
  let n = s.name
  case k % 3 {
    0 => Rect { name: n, width: k, height: k + 1, depth: k + 2 }
    1 => Rect { name: n, width: 0, height: 0, depth: k }
    _ => Circle { name: n, radius: k }
  }
}

fn rename(s: Shape) -> Shape {
  let n = s.name
  Rect { name: n, width: 1, height: 1, depth: 1 }
}

fn grow(s: Shape) -> Shape {
  case s {
    Circle { name, radius } => Circle { name: name, radius: radius + 1 }
    Rect { name, width, height, depth } => Rect { name: name, width: width + 1, height: height, depth: depth }
  }
}

fn renamed(s: Shape) -> Shape {
  Shape { ..s, name: "x" }
}

fn main() {
  let _ = widen(Circle { name: "c", radius: 1 }, 2)
  let _ = rename(Circle { name: "c", radius: 1 })
  let _ = grow(Circle { name: "c", radius: 1 })
  let _ = renamed(Rect { name: "r", width: 1, height: 2, depth: 3 })
}
"#;

/// A field every variant holds at the same place is read once, with the
/// layout of the first variant and no test of the variant (D-071), so the
/// read proves no layout: the value may be a variant of another size
/// (the fix review's TOK-1). Neither the rebuild in later arms nor the one
/// in the read's block reuses it.
#[test]
fn reuse_shared_field_read_proves_no_layout() {
    for name in ["widen", "rename"] {
        let text = function_of(SHARED, name);
        assert!(!text.contains("drop_reuse"), "{text}");
        assert!(!text.contains("alloc_reuse"), "{text}");
    }
}

/// A read after a switch on the value's tag proves the variant the arm
/// selects, by its own case or as the only tag left to the default; a
/// record update on several variants rebuilds in such arms: each reuses.
#[test]
fn reuse_read_after_a_tag_test_proves_its_variant() {
    for name in ["grow", "renamed"] {
        let text = function_of(SHARED, name);
        for ctor in ["Rect", "Circle"] {
            assert_eq!(
                text.matches(&format!("= drop_reuse _0 of ctor app::{ctor}"))
                    .count(),
                1,
                "{ctor}:\n{text}"
            );
        }
        assert_eq!(text.matches("= alloc_reuse _").count(), 2, "{text}");
    }
}

/// The constructors `app::f4` (tag 0, two value fields), `app::f5` (tag
/// 1, three) and `app::f6` (tag 2, two) of the type `app::f7`.
fn three_variants() -> Vec<LayoutDesc> {
    [(4, 0, 2_i32), (5, 1, 3), (6, 2, 2)]
        .into_iter()
        .map(|(n, tag, count)| LayoutDesc {
            layout: LayoutRef::Ctor(def(n)),
            name: format!("app::f{n}").into(),
            tag,
            only_ctor: false,
            fields: (1..=count)
                .map(|k| FieldDesc {
                    name: None,
                    repr: Repr::Value,
                    offset: k.checked_mul(8).unwrap(),
                })
                .collect(),
            value_fields: u32::try_from(count).unwrap(),
            raw_fields: 0,
            type_def: def(7),
            type_name: "f7".into(),
            ctor_name: format!("f{n}").into(),
            record: false,
            hooks: None,
            type_key: type_key("app", "f7"),
        })
        .collect()
}

/// `f0(x)` of [`three_variants`]: switches on `x`'s tag with `cases`
/// (the default returns `Nil`); the block that tags 0 and 1 enter reads
/// field 0 of `x` as `app::f4` and builds an `app::f4` from it. Counted.
fn rebuild_in_arm(arm_tags: &[u64]) -> (MirModule, String) {
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let tag = b
        .assign(
            Repr::Int,
            Rvalue::ReadTag {
                base: Operand::Local(x),
                ty: TagType::Adt(def(7)),
            },
        )
        .unwrap();
    let arm = b.new_block().unwrap();
    let other = b.new_block().unwrap();
    b.terminate(Terminator::Switch {
        on: Operand::Local(tag),
        cases: arm_tags.iter().map(|&t| (t, arm)).collect(),
        default: Some(other),
    })
    .unwrap();
    b.switch_to(arm).unwrap();
    let first = field(&mut b, x, LayoutRef::Ctor(def(4)), 0);
    let made = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Ctor(def(4)),
                fields: vec![Operand::Local(first), nil()],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    b.switch_to(other).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let mut unit = module(vec![b.finish().unwrap()]);
    unit.layouts = three_variants();
    unit.names.extend((4..8).map(|n| DefName {
        def: def(n),
        module: "app".into(),
        item: format!("f{n}").into(),
    }));
    validate(&unit, Stage::Lowered).unwrap();
    count_lowered_with(&mut unit, &MirOptions::default()).unwrap();
    let text = dump(&unit).unwrap();
    (unit, text)
}

/// An arm of a tag's switch proves a layout only when one tag enters it:
/// constructors that hold a field alike share one arm (D-071's grouped
/// read), and a read there may see a constructor of another size.
#[test]
fn reuse_arm_of_several_tags_proves_no_layout() {
    let (_, text) = rebuild_in_arm(&[0]);
    assert!(text.contains("= drop_reuse _0 of ctor app::f4"), "{text}");
    assert!(text.contains("= alloc_reuse _"), "{text}");
    let (unit, text) = rebuild_in_arm(&[0, 1]);
    assert!(!text.contains("drop_reuse"), "{text}");
    let decisions = decisions(&unit, &unit.functions[0]).unwrap();
    assert_eq!(
        decisions.iter().map(|d| d.reason).collect::<Vec<_>>(),
        [ReuseReason::NoCandidate],
        "{text}"
    );
}

/// An owned last-use argument of an in-place kernel (D-525) stays owned
/// and is passed without a `dup`, so the kernel updates in place: reuse
/// and its refinement of D-806 change nothing there.
#[test]
fn reuse_owned_last_use_kernel_argument_stays_in_place() {
    let text = function_of(
        r"//! A set grown in place.

import std::set

fn add(s: set::Set<Int>, x: Int) -> set::Set<Int> {
  set::insert(s, x)
}

fn main() {
  let _ = add(set::new(), 1)
}
",
        "add",
    );
    assert!(
        text.starts_with("fn app::add(_0: value, _1: int)"),
        "{text}"
    );
    assert!(text.contains("call std::set::insert(_0, "), "{text}");
    assert!(!text.contains("dup _0"), "{text}");
}

/// A base used after the update is not reused: its drop comes after the
/// allocation.
#[test]
fn reuse_base_used_after_the_alloc_is_not_reused() {
    let text = function_of(RECORD, "kept");
    assert!(!text.contains("drop_reuse"), "{text}");
    assert!(text.contains("= alloc ctor app::Point"), "{text}");
}

/// A constant's value is static and never unique, so it is never a
/// source (019's `..save_policy`).
#[test]
fn reuse_constant_base_is_never_a_source() {
    let text = function_of(RECORD, "from_constant");
    assert!(text.contains("load_const app::origin"), "{text}");
    assert!(!text.contains("drop_reuse"), "{text}");
}

/// A drop pairs only with an allocation of the same object size: a tuple
/// of 2 fields with one of 2, or of 3 with a constructor... of 3, never
/// one of another size.
#[test]
fn reuse_pairs_only_equal_sizes() {
    let (_, text) = counted(vec![rebuild(2, 2)], MirOptions::default());
    assert!(text.contains("drop_reuse _0 of tuple/2"), "{text}");
    assert!(text.contains("alloc_reuse _"), "{text}");
    let (_, text) = counted(vec![rebuild(2, 3)], MirOptions::default());
    assert!(!text.contains("drop_reuse"), "{text}");
    let (_, text) = counted(vec![rebuild(3, 2)], MirOptions::default());
    assert!(!text.contains("drop_reuse"), "{text}");
}

/// Escaped tuples (more than 254 fields, D-279) carry one more word,
/// which the size counts: no pairing with a tuple one field longer.
#[test]
fn reuse_object_size_counts_the_escaped_count() {
    let unit = module(Vec::new());
    let size = |n| crate::layout::object_size(&unit, LayoutRef::Tuple(n)).unwrap();
    assert_eq!(size(1), 16);
    assert_eq!(size(254), 8 + 8 * 254);
    assert_eq!(size(255), 16 + 8 * 255);
    assert_eq!(size(256), size(255) + 8);
    assert_eq!(
        crate::layout::object_size(&unit, LayoutRef::Ctor(cheby_iface::builtin::SOME)),
        Some(16)
    );
    assert_eq!(
        crate::layout::object_size(&unit, LayoutRef::Ctor(def(3))),
        None
    );
    let (_, text) = counted(vec![rebuild(255, 255)], MirOptions::default());
    assert!(text.contains("drop_reuse _0 of tuple/255"), "{text}");
}

/// A tuple literal above the threshold starts as a placeholder tuple
/// (D-810), which never takes a token, even of the right size.
#[test]
fn reuse_placeholder_tuple_is_never_a_sink() {
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let layout = LayoutRef::Tuple(40);
    let first = field(&mut b, x, layout, 0);
    let obj = b
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout })
        .unwrap();
    for k in 0..40 {
        let value = if k == 0 { Operand::Local(first) } else { nil() };
        b.push(Stmt::Eval(Rvalue::InitField {
            obj,
            layout,
            field: k,
            value,
        }))
        .unwrap();
    }
    b.terminate(Terminator::Return(Operand::Local(obj)))
        .unwrap();
    let (_, text) = counted(vec![b.finish().unwrap()], MirOptions::default());
    assert!(text.contains("drop _0"), "{text}");
    assert!(!text.contains("drop_reuse"), "{text}");
}

/// A tuple being filled field by field (D-810) is never a source, even
/// when a field read proves its layout and it is dropped before an
/// allocation of its size.
#[test]
fn reuse_tuple_being_filled_is_never_a_source() {
    let layout = LayoutRef::Tuple(33);
    let mut b = builder(FnKey::Item(def(0)));
    let obj = b
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout })
        .unwrap();
    for k in 0..33 {
        b.push(Stmt::Eval(Rvalue::InitField {
            obj,
            layout,
            field: k,
            value: nil(),
        }))
        .unwrap();
    }
    let read = field(&mut b, obj, layout, 0);
    b.push(Stmt::Dup(read)).unwrap();
    b.push(Stmt::Drop(obj)).unwrap();
    let mut fields = vec![Operand::Local(read)];
    fields.extend(nils(32));
    let made = b
        .assign(Repr::Value, Rvalue::Alloc { layout, fields })
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    let unit = module(Vec::new());
    let mut f = b.finish().unwrap();
    crate::reuse::insert(&unit, &mut f).unwrap();
    let unit = module(vec![f]);
    let text = dump(&unit).unwrap();
    assert!(!text.contains("drop_reuse"), "{text}");
    validate(&unit, Stage::Counted).unwrap();
}

/// A borrowed parameter is never dropped, so never a source; its
/// projections are borrowed too (D-715, D-807).
#[test]
fn reuse_borrowed_parameter_is_not_reused() {
    let mut f = rebuild(2, 2);
    f.borrow = BorrowMask::OWNED.with(0);
    let mut unit = module(vec![f]);
    count_lowered_with(&mut unit, &MirOptions::default()).unwrap();
    let text = dump(&unit).unwrap();
    assert!(text.contains("fn app::f0(borrow _0: value)"), "{text}");
    assert!(!text.contains("drop_reuse"), "{text}");
    let decisions = decisions(&unit, &unit.functions[0]).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].reason, ReuseReason::Borrowed);
    assert_eq!(decisions[0].source, Some(Local::from_u32(0)));
}

/// Naive counting is the MIR before borrowing and reuse (D-808,
/// GAP(reuse-token-form)), and so is Perceus with its passes off.
#[test]
fn reuse_naive_mode_and_passes_off_have_no_reuse() {
    let naive = MirOptions {
        rc: RcMode::Naive,
        ..MirOptions::default()
    };
    let (_, text) = counted(vec![rebuild(2, 2)], naive);
    assert!(!text.contains("drop_reuse"), "{text}");
    let off = MirOptions::default().perceus_passes_off();
    let (_, off_text) = counted(vec![rebuild(2, 2)], off);
    assert_eq!(text, off_text);
}

/// A precounted body (D-693) keeps its counts as generated.
#[test]
fn reuse_precounted_bodies_are_left_alone() {
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let layout = LayoutRef::Tuple(1);
    let read = field(&mut b, x, layout, 0);
    b.push(Stmt::Dup(read)).unwrap();
    b.push(Stmt::Drop(x)).unwrap();
    let made = b
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout,
                fields: vec![Operand::Local(read)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    b.set_precounted();
    let mut f = b.finish().unwrap();
    let before = f.clone();
    crate::reuse::insert(&module(Vec::new()), &mut f).unwrap();
    assert_eq!(f, before);
    // Without the flag the same counted body is paired.
    f.precounted = false;
    crate::reuse::insert(&module(Vec::new()), &mut f).unwrap();
    assert_ne!(f, before);
}

/// Each sink takes the nearest unpaired source of its size before it, in
/// statement order (D-264): two drops, then two allocations.
#[test]
fn reuse_each_sink_takes_the_nearest_unpaired_source() {
    let layout = LayoutRef::Tuple(1);
    let mut body = builder(FnKey::Item(def(0)));
    let first = body.add_param(Repr::Value, None, None).unwrap();
    let second = body.add_param(Repr::Value, None, None).unwrap();
    let from_first = field(&mut body, first, layout, 0);
    let from_second = field(&mut body, second, layout, 0);
    let one = body
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout,
                fields: vec![Operand::Local(from_first)],
            },
        )
        .unwrap();
    let two = body
        .assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![Operand::Local(one), Operand::Local(from_second)],
            },
        )
        .unwrap();
    body.terminate(Terminator::Return(Operand::Local(two)))
        .unwrap();
    let (unit, text) = counted(vec![body.finish().unwrap()], MirOptions::default());
    // `y` dies last before the first allocation, so it is the nearest; `x`
    // is left, and the pair's size differs from it.
    assert!(text.contains("drop_reuse _1 of tuple/1"), "{text}");
    assert!(!text.contains("drop_reuse _0"), "{text}");
    let decisions = decisions(&unit, &unit.functions[0]).unwrap();
    assert_eq!(
        decisions.iter().map(|d| d.reason).collect::<Vec<_>>(),
        [ReuseReason::Reused, ReuseReason::NoCandidate]
    );
}

/// The decisions of the reuse report (D-717): a reused update, a base
/// used later, a constant base, and an allocation with nothing to reuse.
#[test]
fn reuse_decisions_give_the_reasons() {
    let lowered = lower_source(&[("", RECORD)]);
    let unit = lowered.unit("app");
    let reasons = |name: &str| -> Vec<(ReuseReason, bool)> {
        let f = unit
            .functions
            .iter()
            .find(|f| {
                crate::dump_function(unit, f)
                    .unwrap()
                    .starts_with(&format!("fn app::{name}("))
            })
            .unwrap();
        decisions(unit, f)
            .unwrap()
            .into_iter()
            .map(|ReuseDecision { source, reason, .. }| (reason, source.is_some()))
            .collect()
    };
    assert_eq!(reasons("moved"), [(ReuseReason::Reused, true)]);
    // `kept` reads its base after the update, so nothing consumes it and
    // it is borrowed (D-806); `both` owns its base, which it returns.
    assert_eq!(reasons("kept"), [(ReuseReason::Borrowed, true)]);
    assert_eq!(
        reasons("both"),
        [
            (ReuseReason::UsedLater, true),
            (ReuseReason::UsedLater, true)
        ]
    );
    assert_eq!(reasons("from_constant"), [(ReuseReason::Constant, true)]);
    assert_eq!(reasons("main"), [(ReuseReason::NoCandidate, false)]);
}

/// The parameters reuse keeps owned (D-806 refined): the record update's
/// base and the tree, each of which D-806 alone would borrow.
#[test]
fn reuse_candidate_parameters_stay_owned() {
    let lowered = lower_source(&[("", TREE)]);
    assert!(
        !lowered.masks.contains_key("app::insert"),
        "{:?}",
        lowered.masks
    );
    let lowered = lower_source(&[("", RECORD)]);
    assert!(
        !lowered.masks.contains_key("app::moved"),
        "{:?}",
        lowered.masks
    );
    let f = rebuild(2, 2);
    let unit = module(Vec::new());
    assert_eq!(
        crate::reuse::candidate_params(&unit, &f).unwrap(),
        [Local::from_u32(0)]
    );
    assert_eq!(
        crate::reuse::candidate_params(&unit, &rebuild(2, 3)).unwrap(),
        Vec::<Local>::new()
    );
}

// Validator negatives ------------------------------------------------------

/// A counted `f0(x)` of tuples of one field: `field x.0`, `dup`, then
/// `t = drop_reuse x`, then what `rest` adds, which must consume `t`;
/// returns the unit's validation errors at `Stage::Counted`.
fn token_errors(rest: impl FnOnce(&mut FunctionBuilder, Local, Local) -> Operand) -> String {
    let layout = LayoutRef::Tuple(1);
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let read = field(&mut b, x, layout, 0);
    b.push(Stmt::Dup(read)).unwrap();
    let token = b
        .assign(Repr::Value, Rvalue::DropReuse { local: x, layout })
        .unwrap();
    let result = rest(&mut b, token, read);
    b.terminate(Terminator::Return(result)).unwrap();
    match validate(&module(vec![b.finish().unwrap()]), Stage::Counted) {
        Ok(()) => String::new(),
        Err(errors) => errors.join("\n"),
    }
}

fn alloc_reuse(
    b: &mut FunctionBuilder,
    token: Local,
    layout: LayoutRef,
    fields: Vec<Operand>,
) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::AllocReuse {
            token,
            layout,
            fields,
        },
    )
    .unwrap()
}

/// The well-formed shape the negatives below break.
#[test]
fn reuse_validator_accepts_a_token_consumed_once() {
    let errors = token_errors(|b, token, read| {
        let made = alloc_reuse(b, token, LayoutRef::Tuple(1), vec![Operand::Local(read)]);
        Operand::Local(made)
    });
    assert_eq!(errors, "");
}

#[test]
fn reuse_validator_rejects_a_token_dupd_or_returned() {
    let errors = token_errors(|b, token, read| {
        b.push(Stmt::Dup(token)).unwrap();
        b.push(Stmt::Drop(token)).unwrap();
        b.push(Stmt::Drop(read)).unwrap();
        Operand::Local(token)
    });
    assert!(
        errors.contains("uses the reuse token _2 other than by alloc_reuse or drop"),
        "{errors}"
    );
    assert!(errors.contains("passes on the reuse token _2"), "{errors}");
}

#[test]
fn reuse_validator_rejects_a_token_passed_to_a_call_or_stored() {
    let errors = token_errors(|b, token, read| {
        let called = b
            .assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Direct(def(1)),
                    args: vec![Operand::Local(token)],
                },
            )
            .unwrap();
        let made = b
            .assign(
                Repr::Value,
                Rvalue::Alloc {
                    layout: LayoutRef::Tuple(2),
                    fields: vec![Operand::Local(called), Operand::Local(read)],
                },
            )
            .unwrap();
        Operand::Local(made)
    });
    assert!(
        errors.contains("uses the reuse token _2 other than by alloc_reuse or drop"),
        "{errors}"
    );
}

#[test]
fn reuse_validator_rejects_a_token_dropped_twice_or_left_unconsumed() {
    let errors = token_errors(|b, token, read| {
        b.push(Stmt::Drop(token)).unwrap();
        b.push(Stmt::Drop(token)).unwrap();
        Operand::Local(read)
    });
    assert!(
        errors.contains("_2 is used without a reference"),
        "{errors}"
    );
    let errors = token_errors(|_, _, read| Operand::Local(read));
    assert!(errors.contains("_2 still holds 1 reference(s)"), "{errors}");
}

#[test]
fn reuse_validator_rejects_an_alloc_reuse_of_another_size() {
    let errors = token_errors(|b, token, read| {
        let made = alloc_reuse(
            b,
            token,
            LayoutRef::Tuple(2),
            vec![Operand::Local(read), nil()],
        );
        Operand::Local(made)
    });
    assert!(
        errors.contains("reuses a Tuple(1) of Some(16) bytes for a Tuple(2) of Some(24) bytes"),
        "{errors}"
    );
}

#[test]
fn reuse_validator_rejects_a_closure_layout_and_a_non_token() {
    let errors = token_errors(|b, token, read| {
        b.push(Stmt::Drop(token)).unwrap();
        let other = b.assign(Repr::Value, Rvalue::Use(nil())).unwrap();
        let made = alloc_reuse(b, other, LayoutRef::Closure(0), vec![nil()]);
        b.push(Stmt::Drop(made)).unwrap();
        Operand::Local(read)
    });
    assert!(
        errors.contains(
            "alloc_reuse of Closure(0), which is not a constructor with fields or a tuple"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("allocates with _3, which is not a reuse token"),
        "{errors}"
    );
}

#[test]
fn reuse_validator_rejects_a_borrowed_or_filled_source() {
    let layout = LayoutRef::Tuple(1);
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let token = b
        .assign(Repr::Value, Rvalue::DropReuse { local: x, layout })
        .unwrap();
    let made = alloc_reuse(&mut b, token, layout, vec![nil()]);
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    let mut f = b.finish().unwrap();
    f.borrow = BorrowMask::OWNED.with(0);
    let errors = validate(&module(vec![f]), Stage::Counted)
        .unwrap_err()
        .join("\n");
    assert!(errors.contains("reuses _0, which is borrowed"), "{errors}");

    let layout = LayoutRef::Tuple(33);
    let mut b = builder(FnKey::Item(def(0)));
    let obj = b
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout })
        .unwrap();
    for k in 0..33 {
        b.push(Stmt::Eval(Rvalue::InitField {
            obj,
            layout,
            field: k,
            value: nil(),
        }))
        .unwrap();
    }
    let token = b
        .assign(Repr::Value, Rvalue::DropReuse { local: obj, layout })
        .unwrap();
    let made = alloc_reuse(&mut b, token, layout, nils(33));
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    let errors = validate(&module(vec![b.finish().unwrap()]), Stage::Counted)
        .unwrap_err()
        .join("\n");
    assert!(
        errors.contains("reuses _0, a tuple being filled field by field"),
        "{errors}"
    );
}

#[test]
fn reuse_validator_rejects_the_forms_before_counting() {
    let layout = LayoutRef::Tuple(1);
    let mut b = builder(FnKey::Item(def(0)));
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let token = b
        .assign(Repr::Value, Rvalue::DropReuse { local: x, layout })
        .unwrap();
    let made = alloc_reuse(&mut b, token, layout, vec![nil()]);
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    let errors = validate(&module(vec![b.finish().unwrap()]), Stage::Lowered)
        .unwrap_err()
        .join("\n");
    assert!(
        errors.contains("reuses before reference-count insertion"),
        "{errors}"
    );
}

/// The dump of both forms, which the reuse report's tests and the
/// differential oracle read (plan §2.3).
#[test]
fn reuse_dump_syntax() {
    let (_, text) = counted(vec![rebuild(1, 1)], MirOptions::default());
    assert!(
        text.contains("  dup _1\n  _3: value = drop_reuse _0 of tuple/1\n"),
        "{text}"
    );
    assert!(
        text.contains("  _2: value = alloc_reuse _3, tuple/1(_1) ; unwind bb4\n"),
        "{text}"
    );
    // The pad of the allocation releases the token, then what else is
    // held.
    assert!(text.contains("bb4:\n  drop _3\n  goto bb3()\n"), "{text}");
}

/// `Builder::record_site` records only when asked, and `finish` moves the
/// sites into `Function::sites` (D-717's report).
#[test]
fn reuse_sites_are_recorded_only_when_asked() {
    let mut b = builder(FnKey::Item(def(0)));
    let made = b.assign(Repr::Value, Rvalue::Use(nil())).unwrap();
    b.record_site(made, span());
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    assert_eq!(b.finish().unwrap().sites, Vec::new());
    let mut b = builder(FnKey::Item(def(0)));
    b.set_record_sites(true);
    assert!(b.records_sites());
    let made = b.assign(Repr::Value, Rvalue::Use(nil())).unwrap();
    b.record_site(made, span());
    b.terminate(Terminator::Return(Operand::Local(made)))
        .unwrap();
    assert_eq!(b.finish().unwrap().sites, [(made, span())]);
}
