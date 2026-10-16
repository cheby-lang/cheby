//! Tests of fusion (D-716, GAP(fusion-forms)): dup sinking, cancelling,
//! keep lists and drops specialized by constructor on programs as
//! `cheby run` counts them; static constants left uncounted; the forms'
//! validation and dump; reuse's decisions unchanged by fusion; and the
//! drop-position check (GAP(drop-position-check)) with its negatives.

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Idx, ModuleId};

use crate::abi::AbiSig;
use crate::borrow::BorrowMask;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, BlockId, Callee, CalleeDecl, Const, DefName, FnKey, Function, LayoutRef, Local, MirModule,
    Operand, Rvalue, Stmt, Terminator, UnitKind,
};
use crate::repr::Repr;
use crate::test_support::{lower_package_options, lower_source};
use crate::validate::{Stage, validate};
use crate::{MirOptions, RcMode, count_lowered_with, dump};

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

/// A code unit of `app` holding `functions` (`app::f0` to `app::f3`);
/// `app::f1` takes one value and is declared as a callee.
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

fn builder() -> FunctionBuilder {
    let mut b = FunctionBuilder::new(FnKey::Item(def(0)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn nil() -> Operand {
    Operand::Const(Const::Nil)
}

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

fn field(b: &mut FunctionBuilder, base: Local, layout: LayoutRef, i: u32) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::Field {
            base,
            layout,
            field: i,
        },
    )
    .unwrap()
}

fn alloc(b: &mut FunctionBuilder, layout: LayoutRef, fields: Vec<Operand>) -> Local {
    b.assign(Repr::Value, Rvalue::Alloc { layout, fields })
        .unwrap()
}

fn call(b: &mut FunctionBuilder, arg: Operand) -> Local {
    b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Direct(def(1)),
            args: vec![arg],
        },
    )
    .unwrap()
}

/// The errors `validate` gives the hand-counted `f` at `Stage::Counted`,
/// joined; empty when it is accepted.
fn counted_errors(f: Function) -> String {
    match validate(&module(vec![f]), Stage::Counted) {
        Ok(()) => String::new(),
        Err(errors) => errors.join("\n"),
    }
}

/// The dump of `app::<name>` in `source`'s `app` unit, as `cheby run`
/// counts it.
fn function_of(source: &str, name: &str) -> String {
    lower_source(&[("", source)]).function("app", &format!("app::{name}"))
}

/// The same with `options`.
fn function_with(source: &str, name: &str, options: MirOptions) -> String {
    lower_package_options("app", &[("", source)], options)
        .unwrap()
        .function("app", &format!("app::{name}"))
}

fn no_fusion() -> MirOptions {
    MirOptions {
        fuse: false,
        ..MirOptions::default()
    }
}

fn naive() -> MirOptions {
    MirOptions {
        rc: RcMode::Naive,
        ..MirOptions::default()
    }
}

const PROGRAM: &str = r#"//! Shapes fusion handles.

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

type Pair {
  Pair(String, String),
}

fn first(p: Pair) -> String {
  let Pair(a, _) = p
  a
}

fn deep(t: Tree) -> Tree {
  case t {
    Node(Node(a, _, _), _, _) => a
    _ => Leaf
  }
}

fn greet(name: String) -> String {
  let hello = "hello"
  "{hello}{name}"
}

fn swap(p: (String, Int)) -> (Int, String) {
  let (s, n) = p
  (n, s)
}

fn second(p: (String, Int)) -> String {
  let (s, _) = p
  s
}

fn pick(p: (Int, String)) -> String {
  let (n, s) = p
  case n {
    0 => s
    _ => "other"
  }
}

fn main() {
  let _ = insert(insert(Leaf, 2), 1)
  let _ = first(Pair("a", "b"))
  let _ = deep(Leaf)
  let _ = greet("x")
  let _ = swap(("a", 1))
  let _ = second(("a", 1))
  let _ = pick((0, "a"))
}
"#;

/// The rebuild of a matched node (`tree_inserts`, survey-perceus §1.3):
/// the projections' `dup`s sink from the field reads into the arms; in the
/// arm that returns the node they cancel against its edge drops, and in
/// the rebuilding arms they become the `drop_reuse`'s keep list. The pad
/// of the comparison's call then drops the node alone.
#[test]
fn fuse_sinks_projection_dups_into_arms_and_keeps_them() {
    let text = function_of(PROGRAM, "insert");
    assert_eq!(
        text.matches("= drop_reuse _0 of ctor app::Node keep [_5 @0, _7 @2]")
            .count(),
        2,
        "{text}"
    );
    assert!(!text.contains("dup _5"), "{text}");
    assert!(!text.contains("dup _7"), "{text}");
    // The Equal arm returns the node, holding nothing else.
    assert!(text.contains("bb5:\n  goto bb7(_0)\n"), "{text}");
    // The comparison's pad drops the node only.
    assert!(
        text.contains("call std::int::compare(_1, _6) ; unwind bb9\n"),
        "{text}"
    );
    assert!(text.contains("bb9:\n  drop _0\n  goto bb8()\n"), "{text}");
    let unfused = function_with(PROGRAM, "insert", no_fusion());
    assert!(unfused.contains("dup _5"), "{unfused}");
    assert!(!unfused.contains("keep ["), "{unfused}");
}

/// A `drop` whose projections are kept, of the type's only constructor,
/// is specialized by constructor; a tuple's too.
#[test]
fn fuse_specializes_a_drop_of_an_only_constructor_or_a_tuple() {
    let text = function_of(PROGRAM, "first");
    assert!(
        text.contains("  drop _0 of ctor app::Pair keep [_1 @0]\n  return _1\n"),
        "{text}"
    );
    let text = function_of(PROGRAM, "second");
    assert!(
        text.contains("  drop _0 of tuple/2 keep [_1 @0]\n  return _1\n"),
        "{text}"
    );
    // A rebuild of the tuple keeps both fields in its `drop_reuse`.
    let text = function_of(PROGRAM, "swap");
    assert!(
        text.contains("= drop_reuse _0 of tuple/2 keep [_2 @1, _1 @0]"),
        "{text}"
    );
}

/// In an arm that a test of the tag enters, the read pins the
/// constructor, also for a projection of a projection.
#[test]
fn fuse_specializes_in_an_arm_that_tests_the_constructor() {
    let text = function_of(PROGRAM, "deep");
    assert!(
        text.contains("drop _0 of ctor app::Node keep [_2 @0]"),
        "{text}"
    );
    assert!(
        text.contains("drop _2 of ctor app::Node keep [_4 @0]"),
        "{text}"
    );
}

const SHARED: &str = r#"//! Field reads every variant shares (D-071) on variants of different
//! sizes, and reads after a test of the variant.

type Shape {
  Rect { name: String, width: Int, height: Int, depth: Int },
  Circle { name: String, radius: Int },
}

fn label(s: Shape) -> String {
  let n = s.name
  n
}

fn widen(s: Shape, k: Int) -> Shape {
  let n = s.name
  case k % 3 {
    0 => Rect { name: n, width: k, height: k + 1, depth: k + 2 }
    _ => Circle { name: n, radius: k }
  }
}

fn grow(s: Shape) -> Shape {
  case s {
    Circle { name, radius } => Circle { name: name, radius: radius + 1 }
    Rect { name, width, height, depth } => Rect { name: name, width: width + 1, height: height, depth: depth }
  }
}

fn main() {
  let _ = label(Circle { name: "c", radius: 1 })
  let _ = widen(Circle { name: "c", radius: 1 }, 2)
  let _ = grow(Circle { name: "c", radius: 1 })
}
"#;

/// A field every variant holds alike is read with the first variant's
/// layout and no test (D-071): the read pins nothing, so the drop is never
/// specialized as that variant (the fix review's TOK-1); after a test of
/// the tag, the arm's rebuild keeps its projection in the `drop_reuse`.
#[test]
fn fuse_never_specializes_by_a_shared_field_read() {
    for name in ["label", "widen"] {
        let text = function_of(SHARED, name);
        assert!(!text.contains(" keep ["), "{name}:\n{text}");
        assert!(!text.contains("drop_reuse"), "{name}:\n{text}");
    }
    let text = function_of(SHARED, "grow");
    assert_eq!(text.matches("drop_reuse").count(), 2, "{text}");
    assert_eq!(text.matches(" keep [").count(), 2, "{text}");
}

/// The validator rejects a specialized drop that no read pins: the shared
/// field read of `label`, made into one by hand (D-071, the fix review's
/// TOK-1).
#[test]
fn fuse_validator_rejects_a_specialized_drop_by_a_shared_field_read() {
    let mut lowered = lower_package_options("app", &[("", SHARED)], no_fusion()).unwrap();
    let unit = lowered
        .units
        .iter_mut()
        .find(|u| u.path.as_ref() == "app")
        .unwrap();
    let mut done = false;
    for f in &mut unit.functions {
        for block in &mut f.blocks {
            let read = block.stmts.iter().find_map(|s| match s {
                Stmt::Assign {
                    dst,
                    value:
                        Rvalue::Field {
                            base,
                            layout,
                            field,
                        },
                } => Some((*dst, *base, *layout, *field)),
                _ => None,
            });
            let Some((p, x, layout, i)) = read else {
                continue;
            };
            let Some(at) = block.stmts.iter().position(|s| *s == Stmt::Drop(x)) else {
                continue;
            };
            let Some(dup) = block.stmts.iter().position(|s| *s == Stmt::Dup(p)) else {
                continue;
            };
            if dup.checked_add(1) != Some(at) {
                continue;
            }
            block.stmts[at] = Stmt::DropKeep {
                local: x,
                layout,
                keep: vec![(i, p)],
            };
            block.stmts.remove(dup);
            done = true;
            break;
        }
        if done {
            break;
        }
    }
    assert!(done, "no shared read followed by its drop");
    let errors = validate(unit, Stage::Unwound).unwrap_err().join("\n");
    assert!(errors.contains("which no read pins"), "{errors}");
}

/// A tuple's boxed field read only to unbox it: its `dup` meets the
/// `drop` after the `unbox` and both go.
#[test]
fn fuse_cancels_a_dup_against_the_next_drop() {
    let fused = function_of(PROGRAM, "pick");
    let unfused = function_with(PROGRAM, "pick", no_fusion());
    assert!(
        unfused.contains("  dup _1\n  _2: int = unbox int _1\n  drop _1\n"),
        "{unfused}"
    );
    assert!(
        fused.contains("  _2: int = unbox int _1\n  _3: value = field _0.1 of tuple/2\n"),
        "{fused}"
    );
    assert!(
        !fused.contains("  dup _1\n") && !fused.contains("  drop _1\n"),
        "{fused}"
    );
    let rc = |t: &str| t.matches("  dup ").count() + t.matches("  drop ").count();
    assert!(rc(&fused) < rc(&unfused), "{fused}\n{unfused}");
}

/// String literals and static constructors are operands, never counted
/// locals, under Perceus; naive counting keeps the locals (D-808).
#[test]
fn fuse_propagates_static_constants_before_counting() {
    let text = function_of(PROGRAM, "greet");
    assert!(!text.contains("= const str#1"), "{text}");
    assert!(text.contains("borrow const str#1)"), "{text}");
    let text = function_with(PROGRAM, "greet", naive());
    assert!(text.contains("= const str#1"), "{text}");
    let text = function_with(PROGRAM, "greet", no_fusion());
    assert!(text.contains("= const str#1"), "{text}");
}

/// Naive counting and Perceus with every pass off have no keep list, no
/// specialized drop and no propagated constant: the oracle (D-718,
/// D-808).
#[test]
fn fuse_is_off_under_naive_counting_and_with_the_passes_off() {
    for options in [naive(), MirOptions::default().perceus_passes_off()] {
        for name in ["insert", "first", "deep", "swap"] {
            let text = function_with(PROGRAM, name, options);
            assert!(!text.contains(" keep ["), "{name}:\n{text}");
        }
    }
}

/// Fusion never touches a cleanup pad: every pad is a chain of plain
/// drops (D-735), and pads shrink where `dup`s sank.
#[test]
fn fuse_leaves_pads_plain_and_smaller() {
    let pads = |text: &str| -> (usize, usize) {
        let mut drops = 0;
        let mut keeps = 0;
        let mut in_pad = false;
        for line in text.lines() {
            if line.starts_with("bb") {
                in_pad = false;
            }
            if line.trim_start().starts_with("drop ") && line.contains(" keep [") {
                keeps += 1;
            }
            if line.ends_with(':') && line.starts_with("bb") {
                in_pad = true;
            }
            if in_pad && line.trim() == "resume" {
                in_pad = false;
            }
            if line.trim_start().starts_with("drop _") && !line.contains(" of ") {
                drops += 1;
            }
        }
        (drops, keeps)
    };
    let fused = function_of(PROGRAM, "insert");
    let unfused = function_with(PROGRAM, "insert", no_fusion());
    let (fused_drops, _) = pads(&fused);
    let (unfused_drops, _) = pads(&unfused);
    assert!(fused_drops < unfused_drops, "{fused}\n{unfused}");
    let lowered = lower_source(&[("", PROGRAM)]);
    for unit in &lowered.units {
        for f in &unit.functions {
            let normal = normal_blocks(f);
            for (n, block) in f.blocks.iter().enumerate() {
                if normal.get(n).copied().unwrap_or(true) {
                    continue;
                }
                for stmt in &block.stmts {
                    assert!(
                        matches!(stmt, Stmt::Drop(_) | Stmt::ClearUnwind),
                        "{stmt:?} in a pad of {:?}",
                        f.key
                    );
                }
            }
        }
    }
}

/// The blocks normal control flow reaches, by index.
fn normal_blocks(f: &Function) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![BlockId::from_u32(0)];
    while let Some(id) = work.pop() {
        let Some(slot) = seen.get_mut(id.index()) else {
            continue;
        };
        if *slot {
            continue;
        }
        *slot = true;
        work.extend(f.block(id).unwrap().term.successors());
    }
    seen
}

/// `bench-compile`'s RC column counts a specialized drop and a
/// `drop_reuse` with a keep list as one operation each
/// (GAP(rc-column-forms)).
#[test]
fn fuse_rc_ops_count_a_specialized_drop_once() {
    let ops = |options: MirOptions| {
        let lowered = lower_package_options("app", &[("", PROGRAM)], options).unwrap();
        let unit = lowered.unit("app");
        let f = unit
            .functions
            .iter()
            .find(|f| {
                dump::dump_function(unit, f)
                    .unwrap()
                    .starts_with("fn app::first(")
            })
            .unwrap();
        crate::stats::rc_ops(f)
    };
    assert_eq!(ops(MirOptions::default()), 1);
    assert_eq!(ops(no_fusion()), 2);
}

/// Reuse's decisions, the reuse report's input (D-717), are the same with
/// and without fusion and constant propagation for every example.
#[test]
fn fuse_keeps_reuse_decisions_of_every_example() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").exists())
        .collect();
    dirs.sort();
    let mut compared = 0;
    for dir in dirs {
        let (name, files) = crate::golden_tests::example_sources(&dir);
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let Ok(fused) = lower_package_options(&name, &refs, MirOptions::default()) else {
            continue;
        };
        let unfused = lower_package_options(&name, &refs, no_fusion()).unwrap();
        assert_eq!(fused.units.len(), unfused.units.len());
        for (a, b) in fused.units.iter().zip(&unfused.units) {
            assert_eq!(a.functions.len(), b.functions.len());
            for (fa, fb) in a.functions.iter().zip(&b.functions) {
                assert_eq!(fa.key, fb.key);
                assert_eq!(
                    crate::reuse::decisions(a, fa).unwrap(),
                    crate::reuse::decisions(b, fb).unwrap(),
                    "{}: {:?}",
                    dir.display(),
                    fa.key
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 1000, "{compared}");
}

/// Fusion is deterministic (D-264): the same unit twice gives the same
/// dump.
#[test]
fn fuse_is_deterministic() {
    assert_eq!(
        lower_source(&[("", PROGRAM)]).dump(),
        lower_source(&[("", PROGRAM)]).dump()
    );
}

/// `f0(x)` reading field 0 of `x` as `layout` into `_1`, then `body`
/// with `x` and `_1`, then returning what `body` returns: hand-counted.
fn with_field(
    layout: LayoutRef,
    body: impl FnOnce(&mut FunctionBuilder, Local, Local) -> Local,
) -> Function {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let p = field(&mut b, x, layout, 0);
    let out = body(&mut b, x, p);
    b.terminate(Terminator::Return(local(out))).unwrap();
    b.finish().unwrap()
}

/// The dump of both forms.
#[test]
fn fuse_dump_syntax() {
    let t1 = LayoutRef::Tuple(1);
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: t1,
            keep: vec![(0, p)],
        })
        .unwrap();
        p
    });
    let text = dump(&module(vec![f])).unwrap();
    assert!(
        text.contains("  drop _0 of tuple/1 keep [_1 @0]\n"),
        "{text}"
    );
    let f = with_field(t1, |b, x, p| {
        let t = b
            .assign(
                Repr::Value,
                Rvalue::DropReuse {
                    local: x,
                    layout: t1,
                    keep: vec![(0, p)],
                },
            )
            .unwrap();
        b.assign(
            Repr::Value,
            Rvalue::AllocReuse {
                token: t,
                layout: t1,
                fields: vec![local(p)],
            },
        )
        .unwrap()
    });
    let text = dump(&module(vec![f])).unwrap();
    assert!(
        text.contains("  _2: value = drop_reuse _0 of tuple/1 keep [_1 @0]\n"),
        "{text}"
    );
}

/// The forms check (GAP(fusion-forms)): a hand-counted specialized drop
/// and a keep list are accepted, and modelled as their `dup`s, then the
/// release.
#[test]
fn fuse_forms_are_accepted_as_their_dups_then_the_release() {
    let t1 = LayoutRef::Tuple(1);
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: t1,
            keep: vec![(0, p)],
        })
        .unwrap();
        p
    });
    assert_eq!(counted_errors(f), "");
    // Without the keep list, the projection would be returned holding
    // nothing.
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::Drop(x)).unwrap();
        p
    });
    assert!(counted_errors(f).contains("is used without a reference"));
}

/// The validator's keep-list rules: a kept local is that field of the
/// released local in its layout, indices are distinct, nothing kept is
/// borrowed, a specialized drop keeps something, and never before
/// counting.
#[test]
fn fuse_validator_rejects_bad_keep_lists() {
    let t2 = LayoutRef::Tuple(2);
    // Not a projection: a call's result.
    let f = with_field(t2, |b, x, p| {
        let other = call(b, nil());
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(0, other)],
        })
        .unwrap();
        alloc(b, t2, vec![local(p), local(other)])
    });
    assert!(
        counted_errors(f).contains("but it is not that field"),
        "not a projection"
    );
    // The wrong index.
    let f = with_field(t2, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(1, p)],
        })
        .unwrap();
        p
    });
    assert!(counted_errors(f).contains("but it is not that field"));
    // Another layout.
    let f = with_field(t2, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: LayoutRef::Tuple(3),
            keep: vec![(0, p)],
        })
        .unwrap();
        p
    });
    assert!(counted_errors(f).contains("but it is not that field"));
    // The same field twice.
    let f = with_field(t2, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(0, p), (0, p)],
        })
        .unwrap();
        alloc(b, t2, vec![local(p), local(p)])
    });
    assert!(counted_errors(f).contains("keeps field 0 of _0 twice"));
    // Nothing kept.
    let f = with_field(t2, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: Vec::new(),
        })
        .unwrap();
        p
    });
    assert!(counted_errors(f).contains("keeps nothing"));
    // A borrowed projection of a borrowed parameter.
    let mut f = with_field(t2, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(0, p)],
        })
        .unwrap();
        p
    });
    f.borrow = BorrowMask::OWNED.with(0);
    let errors = validate(
        &MirModule {
            callees: Vec::new(),
            ..module(vec![f])
        },
        Stage::Counted,
    )
    .unwrap_err()
    .join("\n");
    assert!(errors.contains("keeps _1, which is borrowed"), "{errors}");
    // Before counting.
    let f = with_field(t2, |b, x, p| {
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(0, p)],
        })
        .unwrap();
        p
    });
    let errors = validate(&module(vec![f]), Stage::Lowered)
        .unwrap_err()
        .join("\n");
    assert!(
        errors.contains("before reference-count insertion"),
        "{errors}"
    );
}

/// The drop-position check (GAP(drop-position-check)): a `drop` placed
/// after a later call is rejected.
#[test]
fn drop_position_rejects_a_drop_moved_past_a_call() {
    let t1 = LayoutRef::Tuple(1);
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        let r = call(b, local(p));
        b.push(Stmt::Drop(x)).unwrap();
        r
    });
    let errors = counted_errors(f);
    assert!(
        errors.contains("bb0 statement 3 releases _0 after bb0 statement 2 runs past its last use"),
        "{errors}"
    );
    // Right after the last use, it is accepted.
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        b.push(Stmt::Drop(x)).unwrap();
        call(b, local(p))
    });
    assert_eq!(counted_errors(f), "");
}

/// … and after a later allocation.
#[test]
fn drop_position_rejects_a_drop_moved_past_an_alloc() {
    let t1 = LayoutRef::Tuple(1);
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        let made = alloc(b, t1, vec![local(p)]);
        b.push(Stmt::Drop(x)).unwrap();
        made
    });
    assert!(counted_errors(f).contains("releases _0 after bb0 statement 2 runs"));
}

/// … and a specialized drop placed after a later instruction.
#[test]
fn drop_position_rejects_a_specialized_drop_after_a_later_instruction() {
    let t2 = LayoutRef::Tuple(2);
    let f = with_field(t2, |b, x, p| {
        let other = alloc(b, LayoutRef::Tuple(1), vec![nil()]);
        b.push(Stmt::DropKeep {
            local: x,
            layout: t2,
            keep: vec![(0, p)],
        })
        .unwrap();
        alloc(b, t2, vec![local(p), local(other)])
    });
    assert!(counted_errors(f).contains("bb0 statement 2 releases _0 after bb0 statement 1 runs"));
}

/// … and a drop moved into a successor past a call, across a block's end.
#[test]
fn drop_position_rejects_a_drop_moved_across_blocks() {
    let t1 = LayoutRef::Tuple(1);
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let p = field(&mut b, x, t1, 0);
    b.push(Stmt::Dup(p)).unwrap();
    let r = call(&mut b, local(p));
    let next = b.new_block().unwrap();
    b.terminate(Terminator::Goto {
        target: next,
        args: Vec::new(),
    })
    .unwrap();
    b.switch_to(next).unwrap();
    b.push(Stmt::Drop(x)).unwrap();
    b.terminate(Terminator::Return(local(r))).unwrap();
    let errors = counted_errors(b.finish().unwrap());
    assert!(
        errors.contains("bb1 statement 0 releases _0, but on the path from bb0 statement 2 runs"),
        "{errors}"
    );
}

/// A reuse token's own `drop` is exempt (it holds no reference and no
/// handle; GAP(reuse-pairing-scope)): reuse holds it across calls into
/// the arms that rebuild.
#[test]
fn drop_position_exempts_a_reuse_tokens_drop() {
    let t1 = LayoutRef::Tuple(1);
    let f = with_field(t1, |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        let t = b
            .assign(
                Repr::Value,
                Rvalue::DropReuse {
                    local: x,
                    layout: t1,
                    keep: Vec::new(),
                },
            )
            .unwrap();
        let r = call(b, local(p));
        b.push(Stmt::Drop(t)).unwrap();
        r
    });
    assert_eq!(counted_errors(f), "");
}

/// Every unit counting gives passes the check: the examples' and std's
/// (they lower through `count_lowered_with` with the full check in debug
/// builds); here, the fused program above at `Stage::Counted` and with
/// its unwind edges.
#[test]
fn fuse_output_passes_every_check() {
    let lowered = lower_package_options("app", &[("", PROGRAM)], MirOptions::default()).unwrap();
    for unit in &lowered.units {
        validate(unit, Stage::Unwound).unwrap();
    }
    let mut unit = module(vec![with_field(LayoutRef::Tuple(1), |b, x, p| {
        b.push(Stmt::Dup(p)).unwrap();
        b.push(Stmt::Drop(x)).unwrap();
        p
    })]);
    validate(&unit, Stage::Lowered).unwrap_err();
    // A lowered copy of the same, counted by the pipeline.
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let p = field(&mut b, x, LayoutRef::Tuple(1), 0);
    b.terminate(Terminator::Return(local(p))).unwrap();
    unit = module(vec![b.finish().unwrap()]);
    count_lowered_with(&mut unit, &MirOptions::default()).unwrap();
    let text = dump(&unit).unwrap();
    assert!(
        text.contains("  drop _0 of tuple/1 keep [_1 @0]\n  return _1\n"),
        "{text}"
    );
}
