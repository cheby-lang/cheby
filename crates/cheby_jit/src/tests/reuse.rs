//! Reuse under the JIT (D-716): hand-built counted MIR with `drop_reuse`
//! and `alloc_reuse`, given its unwind edges by the real pass and checked
//! at `Stage::Unwound`, compiled both with the inline paths and with the
//! checked runtime entries (`checked_rc`), which check every header and
//! size. A unique object's block is written over without an allocation;
//! a value of count 2, a static object and a shared one never reuse; a
//! panic between the two forms releases the token in the pad; `Some`
//! takes the block of an `Ok`. Each test's JIT has its own domain, which
//! ends with nothing alive: reuse keeps the domain's charges balanced. The
//! counters `reuse hits` and `reuse misses` (D-842) are read
//! from the domain. Run with `CHEBY_RT_POISON=0` and `=1` (D-458).

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::{FieldDesc, LayoutRef};
use cheby_runtime::abi::PanicKind as RtPanicKind;
use cheby_runtime::stats::{Event, count_events};

use super::*;

const T1: LayoutRef = LayoutRef::Tuple(1);
const T2: LayoutRef = LayoutRef::Tuple(2);

fn nil() -> Operand {
    Operand::Const(Const::Nil)
}

/// `alloc layout(fields)` into `dst`.
fn alloc(dst: u32, layout: LayoutRef, fields: Vec<Operand>) -> Stmt {
    assign(dst, Rvalue::Alloc { layout, fields })
}

/// `dst = drop_reuse local of layout`.
fn drop_reuse(dst: u32, local_: u32, layout: LayoutRef) -> Stmt {
    assign(
        dst,
        Rvalue::DropReuse {
            local: local(local_),
            layout,
            keep: Vec::new(),
        },
    )
}

/// `dst = alloc_reuse token, layout(fields)`.
fn alloc_reuse(dst: u32, token: u32, layout: LayoutRef, fields: Vec<Operand>) -> Stmt {
    assign(
        dst,
        Rvalue::AllocReuse {
            token: local(token),
            layout,
            fields,
        },
    )
}

/// The layout a unit lists for the builtin one-field constructor `ctor`
/// of `ty` (D-454), as lowering gives it.
fn builtin_one(ctor: DefId, item: &str, ty: DefId, type_name: &str, tag: u32) -> LayoutDesc {
    LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: item.into(),
        tag,
        only_ctor: false,
        fields: vec![FieldDesc {
            name: None,
            repr: Repr::Value,
            offset: 8,
        }],
        value_fields: 1,
        raw_fields: 0,
        type_def: ty,
        type_name: type_name.into(),
        ctor_name: item.into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key("", type_name),
    }
}

/// Lists `Ok`, `Err` and `Some` in the unit, sorted.
fn builtin_layouts(p: &mut Prog) {
    p.names.push(name(builtin::OK, "", "Ok"));
    p.names.push(name(builtin::ERR, "", "Err"));
    p.names.push(name(builtin::SOME, "", "Some"));
    p.foreign_layouts.extend([
        builtin_one(builtin::OK, "Ok", builtin::RESULT, "Result", 0),
        builtin_one(builtin::ERR, "Err", builtin::RESULT, "Result", 1),
        builtin_one(builtin::SOME, "Some", builtin::OPTION, "Option", 0),
    ]);
}

/// A new tuple of one field holding `field`, allocated by the runtime in
/// the test thread's domain, with count 1.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned and fields are at multiples of 8"
)]
fn one_field_tuple(field: u64) -> u64 {
    let obj =
        cheby_runtime::alloc::alloc_object(16, abi::header(abi::RC_UNIQUE, 1, abi::layout::TUPLE));
    // SAFETY: the one field of a fresh tuple of one field, 8-aligned.
    unsafe { obj.add(8).cast::<u64>().write(field) };
    u64::try_from(obj.expose_provenance()).unwrap()
}

/// `app::f()` with `locals` counted locals running `stmts` and returning
/// local `result`; returns the thunk's target.
fn program(p: &mut Prog, locals: usize, stmts: Vec<Stmt>, result: u32) -> DefId {
    let f = p.item(
        0,
        "f",
        &[],
        &vec![Repr::Value; locals],
        straight(stmts, result),
        &[],
    );
    p.call(f, Vec::new())
}

/// The program's units with their unwind edges, checked at
/// `Stage::Unwound` (`Prog::unwound_units`), compiled with the inline
/// count paths or, when `checked`, the checked runtime entries.
fn compile(p: Prog, checked: bool) -> Jit {
    count_events();
    let jit = JitOptions {
        checked_rc: checked,
        ..JitOptions::from_env()
    };
    p.compile_unwound_with(CodegenOptions::default(), jit)
}

/// `(allocations, reuse hits, reuse misses)` of the JIT's domain.
fn counts(jit: &Jit) -> (u64, u64, u64) {
    let domain = jit.domain();
    let stats = domain.stats();
    (
        stats.count(Event::Alloc),
        stats.count(Event::ReuseHit),
        stats.count(Event::ReuseMiss),
    )
}

fn live(jit: &Jit) -> i64 {
    jit.domain().live_objects()
}

/// Releases `word`, then checks that nothing of the domain is alive.
fn release_all(jit: &Jit, word: u64) {
    release(jit, word);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(live(jit), 0, "objects leaked");
    }
}

/// A hit: `t = drop_reuse (inner, Nil)` releases `inner` and keeps the
/// pair's block, which `alloc_reuse` writes over: two allocations, not
/// three, one hit, and the domain holds only the result.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_a_hit_writes_over_the_block_and_allocates_nothing() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            4,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T2, vec![op(0), nil()]),
                drop_reuse(2, 1, T2),
                alloc_reuse(3, 2, T2, vec![nil(), nil()]),
            ],
            3,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (2, 1, 0), "checked: {checked}");
        assert_eq!(rc_of(word), 1);
        assert_eq!(layout_of(word), abi::layout::TUPLE);
        assert_eq!(read_word(word, 8), static_nil());
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 1, "only the result is alive");
        }
        release_all(&jit, word);
    }
}

/// A miss on a value of count 2: the `drop_reuse` gives one reference up
/// and the `alloc_reuse` allocates.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_misses_on_a_value_of_count_2() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            3,
            vec![
                alloc(0, T1, vec![nil()]),
                Stmt::Dup(local(0)),
                drop_reuse(1, 0, T1),
                alloc_reuse(2, 1, T1, vec![op(0)]),
            ],
            2,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (2, 0, 1), "checked: {checked}");
        let inner = read_word(word, 8);
        assert_eq!(rc_of(inner), 1, "the drop gave one reference up");
        release_all(&jit, word);
    }
}

/// A miss on a static object (count 0): the runtime's static `Err(Nil)`
/// is never unique.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_misses_on_a_static_constant() {
    for checked in [false, true] {
        let mut p = Prog::default();
        builtin_layouts(&mut p);
        let err = LayoutRef::Ctor(builtin::ERR);
        let target = program(
            &mut p,
            3,
            vec![
                assign(0, Rvalue::Use(Operand::Const(Const::ErrNil))),
                drop_reuse(1, 0, err),
                alloc_reuse(2, 1, err, vec![nil()]),
            ],
            2,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (1, 0, 1), "checked: {checked}");
        assert_eq!(rc_of(word), 1, "a new object, not the static one");
        assert_eq!(layout_of(word), abi::layout::ERR);
        release_all(&jit, word);
    }
}

/// A miss on a value marked shared (negative count, D-072), as a REPL
/// session's values are (D-712): read from a constant's slot, `dup`'d,
/// then given up by `drop_reuse`, which decrements the shared count.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_misses_on_a_value_marked_shared() {
    for checked in [false, true] {
        let constant = def(1, 70);
        let mut p = Prog::default();
        p.names.push(name(constant, "app", "SHARED"));
        let target = program(
            &mut p,
            3,
            vec![
                assign(0, Rvalue::LoadConst(constant)),
                Stmt::Dup(local(0)),
                drop_reuse(1, 0, T1),
                alloc_reuse(2, 1, T1, vec![nil()]),
            ],
            2,
        );
        let mut jit = compile(p, checked);
        let shared = one_field_tuple(abi::tag_int(5).unwrap());
        // SAFETY: a live object this test owns, not yet seen by any other
        // fiber.
        unsafe { cheby_runtime::rc::mark_shared(shared) };
        assert!(rc_of(shared) < 0, "marked shared");
        jit.set_const(constant, shared).unwrap();
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (1, 0, 1), "checked: {checked}");
        assert_eq!(rc_of(shared), -1, "the shared value keeps one reference");
        release_all(&jit, word);
        // SAFETY: the test's reference, the last one.
        unsafe { cheby_runtime::rc::cheby_rt_drop(std::ptr::null_mut(), shared) };
    }
}

/// `Some` takes the block of an `Ok`: both are builtin one-field
/// constructors of one size (D-454).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_some_takes_the_block_of_an_ok() {
    for checked in [false, true] {
        let mut p = Prog::default();
        builtin_layouts(&mut p);
        let target = program(
            &mut p,
            3,
            vec![
                alloc(0, LayoutRef::Ctor(builtin::OK), vec![nil()]),
                drop_reuse(1, 0, LayoutRef::Ctor(builtin::OK)),
                alloc_reuse(2, 1, LayoutRef::Ctor(builtin::SOME), vec![nil()]),
            ],
            2,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (1, 1, 0), "checked: {checked}");
        assert_eq!(layout_of(word), abi::layout::SOME);
        assert_eq!(rc_of(word), 1);
        release_all(&jit, word);
    }
}

/// A panic between `drop_reuse` and `alloc_reuse`: the token is held
/// across the call of `g`, which panics, and the call's pad releases it
/// with a plain `drop`; the inner object went with the drop's release, so
/// nothing of the domain stays alive.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_a_panic_between_the_forms_frees_the_token_in_the_pad() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let boom = p.item(
            1,
            "g",
            &[],
            &[],
            vec![block(
                Vec::new(),
                Terminator::Panic {
                    kind: PanicKind::Explicit,
                    message: Some(Operand::Const(Const::Str(StrId::from_u32(2)))),
                    loc: StrId::from_u32(1),
                },
            )],
            &[LOC, "boom"],
        );
        let target = program(
            &mut p,
            5,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T2, vec![op(0), nil()]),
                drop_reuse(2, 1, T2),
                assign(
                    3,
                    Rvalue::Call {
                        callee: Callee::Direct(boom),
                        args: Vec::new(),
                    },
                ),
                alloc_reuse(4, 2, T2, vec![op(3), nil()]),
            ],
            4,
        );
        let [code] = p.units_dump();
        assert!(
            code.contains("_2: value = drop_reuse _1 of tuple/2"),
            "{code}"
        );
        let jit = compile(p, checked);
        let outcome = run_const(&jit, target);
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("the entry returned: {outcome:?}");
        };
        assert_eq!(report.kind, RtPanicKind::Explicit, "{report:?}");
        assert_eq!(report.message, "boom");
        let (allocations, hits, misses) = counts(&jit);
        assert_eq!((allocations, hits, misses), (2, 0, 0), "checked: {checked}");
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 0, "the pad released the token");
        }
    }
}

/// `dst = field base.i of layout`.
fn field_of(dst: u32, base: u32, layout: LayoutRef, i: u32) -> Stmt {
    assign(
        dst,
        Rvalue::Field {
            base: local(base),
            layout,
            field: i,
        },
    )
}

/// `dst = drop_reuse local of layout keep [...]` (D-850).
fn drop_reuse_keep(dst: u32, local_: u32, layout: LayoutRef, keep: &[(u32, u32)]) -> Stmt {
    assign(
        dst,
        Rvalue::DropReuse {
            local: local(local_),
            layout,
            keep: keep.iter().map(|&(i, p)| (i, local(p))).collect(),
        },
    )
}

/// `drop local of layout keep [...]`, a drop specialized by constructor
/// (D-850).
fn drop_keep(local_: u32, layout: LayoutRef, keep: &[(u32, u32)]) -> Stmt {
    Stmt::DropKeep {
        local: local(local_),
        layout,
        keep: keep.iter().map(|&(i, p)| (i, local(p))).collect(),
    }
}

/// A hit with a keep list: the kept field's reference moves to its
/// projection instead of being released, and the block is written over:
/// two allocations, one hit, and the inner object, now the result's
/// field, keeps count 1.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_a_hit_moves_the_kept_field_to_its_projection() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            5,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T2, vec![op(0), nil()]),
                field_of(2, 1, T2, 0),
                drop_reuse_keep(3, 1, T2, &[(0, 2)]),
                alloc_reuse(4, 3, T2, vec![op(2), nil()]),
            ],
            4,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (2, 1, 0), "checked: {checked}");
        let inner = read_word(word, 8);
        assert_eq!(rc_of(inner), 1, "checked: {checked}");
        assert_eq!(layout_of(inner), abi::layout::TUPLE);
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 2, "the result and its field");
        }
        release_all(&jit, word);
    }
}

/// A miss with a keep list on a value of count 2: the kept projection is
/// `dup`'d and the base gives up one reference; the base's last
/// reference then releases its own copy of the field.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_a_miss_dups_the_kept_projection() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            5,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T2, vec![op(0), nil()]),
                Stmt::Dup(local(1)),
                field_of(2, 1, T2, 0),
                drop_reuse_keep(3, 1, T2, &[(0, 2)]),
                Stmt::Drop(local(1)),
                alloc_reuse(4, 3, T2, vec![op(2), nil()]),
            ],
            4,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit), (3, 0, 1), "checked: {checked}");
        let inner = read_word(word, 8);
        assert_eq!(rc_of(inner), 1, "checked: {checked}");
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 2, "the result and its field");
        }
        release_all(&jit, word);
    }
}

/// A drop specialized by constructor on a unique object: the kept
/// field's reference moves to its projection, the other value fields
/// (heap objects) are released inline, and the block is freed through
/// `cheby_rt_free_unique`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_a_specialized_drop_frees_the_block_and_keeps_the_field() {
    let t3 = LayoutRef::Tuple(3);
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            5,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T1, vec![nil()]),
                alloc(2, t3, vec![op(0), op(1), nil()]),
                field_of(3, 2, t3, 1),
                drop_keep(2, t3, &[(1, 3)]),
            ],
            3,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(counts(&jit).0, 3, "checked: {checked}");
        assert_eq!(rc_of(word), 1, "checked: {checked}");
        assert_eq!(layout_of(word), abi::layout::TUPLE);
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 1, "only the kept field");
        }
        release_all(&jit, word);
    }
}

/// A specialized drop of a value of count 2: the kept projection is
/// `dup`'d and the count decremented; the last reference frees it later.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_a_specialized_drop_of_a_count_of_2_gives_up_a_reference() {
    for checked in [false, true] {
        let mut p = Prog::default();
        let target = program(
            &mut p,
            3,
            vec![
                alloc(0, T1, vec![nil()]),
                alloc(1, T2, vec![op(0), nil()]),
                Stmt::Dup(local(1)),
                field_of(2, 1, T2, 0),
                drop_keep(1, T2, &[(0, 2)]),
                Stmt::Drop(local(1)),
            ],
            2,
        );
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(rc_of(word), 1, "checked: {checked}");
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 1, "only the kept field");
        }
        release_all(&jit, word);
    }
}

/// More than 8 value fields not kept: the hit path zeroes the kept one and
/// releases the rest through `cheby_rt_release_fields`, then frees the
/// block (the inline cap, D-850).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_releases_more_than_eight_fields_out_of_line() {
    let t10 = LayoutRef::Tuple(10);
    for checked in [false, true] {
        let mut p = Prog::default();
        let mut stmts: Vec<Stmt> = (0..10).map(|n| alloc(n, T1, vec![nil()])).collect();
        stmts.push(alloc(10, t10, (0..10).map(op).collect()));
        stmts.push(field_of(11, 10, t10, 4));
        stmts.push(drop_keep(10, t10, &[(4, 11)]));
        let target = program(&mut p, 12, stmts, 11);
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(rc_of(word), 1, "checked: {checked}");
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 1, "only the kept field");
        }
        release_all(&jit, word);
    }
}

/// A specialized drop of a static object (count 0): nothing is released
/// or freed, and the kept field, static too, needs no count. The arm of a
/// switch on the tag pins `Err` (the validator requires it, D-071).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_keep_a_specialized_drop_of_a_static_object_skips_it() {
    for checked in [false, true] {
        let mut p = Prog::default();
        builtin_layouts(&mut p);
        let err = LayoutRef::Ctor(builtin::ERR);
        let f = p.item(
            0,
            "f",
            &[],
            &[Repr::Value, Repr::Value, Repr::Int],
            vec![
                block(
                    vec![
                        assign(0, Rvalue::Use(Operand::Const(Const::ErrNil))),
                        assign(
                            2,
                            Rvalue::ReadTag {
                                base: op(0),
                                ty: TagType::Result,
                            },
                        ),
                    ],
                    Terminator::Switch {
                        on: op(2),
                        cases: vec![(1, BlockId::from_u32(1))],
                        default: Some(BlockId::from_u32(2)),
                    },
                ),
                block(
                    vec![field_of(1, 0, err, 0), drop_keep(0, err, &[(0, 1)])],
                    Terminator::Return(op(1)),
                ),
                block(vec![Stmt::Drop(local(0))], Terminator::Return(nil())),
            ],
            &[],
        );
        let target = p.call(f, Vec::new());
        let jit = compile(p, checked);
        let word = returned(run_const(&jit, target));
        assert_eq!(word, static_nil(), "checked: {checked}");
        assert_eq!(counts(&jit).0, 0, "checked: {checked}");
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(live(&jit), 0);
        }
    }
}

impl Prog {
    /// The dump of the code unit after the unwind pass, without consuming
    /// the program.
    fn units_dump(&self) -> [String; 1] {
        let mut code = code_unit(
            1,
            "app",
            self.functions.clone(),
            {
                let mut callees = self.callees.clone();
                callees.sort_by_key(|c| c.def);
                callees
            },
            self.names.clone(),
        );
        code.layouts.clone_from(&self.layouts);
        code.foreign_layouts.clone_from(&self.foreign_layouts);
        cheby_mir::unwind::insert_module(&mut code).unwrap();
        cheby_mir::validate(&code, cheby_mir::Stage::Unwound).unwrap();
        [cheby_mir::dump(&code).unwrap()]
    }
}
