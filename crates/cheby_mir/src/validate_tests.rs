//! The validator, the RC pass and the dump on the forms of plan §4.1:
//! accepted and rejected cases of every new rvalue, layout, constant,
//! static and key, and the ownership of every runtime helper and of
//! `box`/`unbox` (D7).

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};

use crate::abi::AbiSig;
use crate::borrow::BorrowMask;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, ArithOp, BitOp, BuiltinDesc, Callee, CalleeDecl, CmpOp, Const, DefName, FnKey, Function,
    LayoutRef, Local, MirModule, Operand, RuntimeFn, Rvalue, StaticId, StaticKind, StaticObject,
    Stmt, StrId, TagType, Terminator, UnitKind, UseKind,
};
use crate::layout::{FieldDesc, LayoutDesc, type_key};
use crate::repr::Repr;
use crate::validate::{Stage, validate};
use crate::{dump, rc};

const MODULE: ModuleId = ModuleId::from_u32(1);
const OTHER: ModuleId = ModuleId::from_u32(2);

/// `app::f<n>`.
fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

/// `geo::f<n>`, a def of another module.
fn other(n: u32) -> DefId {
    DefId::new(OTHER, DefIndex::from_u32(n))
}

/// The type `app::f10` has two constructors, `app::f11` (tag 0, a value
/// and an `Int`) and `app::f12` (tag 1, no fields). The type `app::f13`
/// has one constructor `app::f14` (an `Int`). `geo::f1` is a constructor
/// of another module (one `Float`) of type `geo::f0`.
const PAIR_TY: u32 = 10;
const PAIR: u32 = 11;
const EMPTY: u32 = 12;
const SOLO_TY: u32 = 13;
const SOLO: u32 = 14;

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

fn desc(ctor: DefId, ty: DefId, tag: u32, reprs: &[Repr]) -> LayoutDesc {
    let values = reprs.iter().filter(|&&r| r == Repr::Value).count();
    let mut next_value = 8;
    let mut next_raw = i32::try_from(values)
        .unwrap()
        .checked_mul(8)
        .and_then(|n| n.checked_add(8))
        .unwrap();
    let fields = reprs
        .iter()
        .map(|&repr| {
            let slot = if repr == Repr::Value {
                &mut next_value
            } else {
                &mut next_raw
            };
            let offset = *slot;
            *slot = slot.checked_add(8).unwrap();
            FieldDesc {
                name: None,
                repr,
                offset,
            }
        })
        .collect();
    LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: format!("app::f{}", ctor.index.as_u32()).into(),
        tag,
        fields,
        value_fields: u32::try_from(values).unwrap(),
        raw_fields: u32::try_from(reprs.len().checked_sub(values).unwrap()).unwrap(),
        type_def: ty,
        type_name: "T".into(),
        ctor_name: "C".into(),
        record: false,
        hooks: None,
        type_key: type_key("app", "T"),
    }
}

/// A code unit of `app` with the layouts above and `functions`.
fn module(functions: Vec<Function>, statics: Vec<StaticObject>) -> MirModule {
    let mut names: Vec<DefName> = (0..16)
        .map(|n| DefName {
            def: def(n),
            module: "app".into(),
            item: format!("f{n}").into(),
        })
        .collect();
    names.extend((0..2).map(|n| DefName {
        def: other(n),
        module: "geo".into(),
        item: format!("f{n}").into(),
    }));
    MirModule {
        unit: UnitKind::Code(MODULE),
        path: "app".into(),
        functions,
        callees: vec![CalleeDecl {
            def: def(1),
            sig: AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value),
        }],
        layouts: vec![
            desc(def(PAIR), def(PAIR_TY), 0, &[Repr::Value, Repr::Int]),
            desc(def(EMPTY), def(PAIR_TY), 1, &[]),
            desc(def(SOLO), def(SOLO_TY), 0, &[Repr::Int]),
        ],
        foreign_layouts: vec![desc(other(1), other(0), 0, &[Repr::Float])],
        statics,
        names,
    }
}

fn builder() -> FunctionBuilder {
    builder_for(FnKey::Item(def(0)))
}

fn builder_for(key: FnKey) -> FunctionBuilder {
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn local(l: Local) -> Operand {
    Operand::Local(l)
}

fn nil() -> Operand {
    Operand::Const(Const::Nil)
}

fn int(n: i64) -> Operand {
    Operand::Const(Const::Int(n))
}

/// A function that defines `dst_repr` as `value` from parameters of
/// `params`, then returns `Nil`.
fn assigning(params: &[Repr], dst_repr: Repr, value: impl FnOnce(&[Local]) -> Rvalue) -> Function {
    let mut b = builder();
    let ps: Vec<Local> = params
        .iter()
        .map(|&r| b.add_param(r, None, None).unwrap())
        .collect();
    b.assign(dst_repr, value(&ps)).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    b.finish().unwrap()
}

fn ok(f: Function) {
    let m = module(vec![f], Vec::new());
    if let Err(errors) = validate(&m, Stage::Lowered) {
        panic!("{errors:#?}\n{}", dump(&m).unwrap());
    }
}

fn rejected(f: Function, needle: &str) {
    let m = module(vec![f], Vec::new());
    let errors = validate(&m, Stage::Lowered).unwrap_err().join("\n");
    assert!(errors.contains(needle), "expected {needle:?} in:\n{errors}");
}

fn rejected_module(m: &MirModule, needle: &str) {
    let errors = validate(m, Stage::Lowered).unwrap_err().join("\n");
    assert!(errors.contains(needle), "expected {needle:?} in:\n{errors}");
}

fn loc(b: &mut FunctionBuilder) -> StrId {
    b.string("src/main.cheby:2:3").unwrap()
}

// Operators on raw values.

#[test]
fn checked_is_for_integers_only() {
    for repr in [Repr::Int, Repr::I8, Repr::U8, Repr::U64] {
        let mut b = builder();
        let a = b.add_param(repr, None, None).unwrap();
        let at = loc(&mut b);
        b.assign(
            repr,
            Rvalue::Checked {
                op: ArithOp::Shl,
                lhs: local(a),
                rhs: local(a),
                repr,
                loc: at,
            },
        )
        .unwrap();
        b.terminate(Terminator::Return(nil())).unwrap();
        ok(b.finish().unwrap());
    }
    for repr in [Repr::Float, Repr::F32, Repr::Bool] {
        let mut b = builder();
        let a = b.add_param(repr, None, None).unwrap();
        let at = loc(&mut b);
        b.assign(
            repr,
            Rvalue::Checked {
                op: ArithOp::Add,
                lhs: local(a),
                rhs: local(a),
                repr,
                loc: at,
            },
        )
        .unwrap();
        b.terminate(Terminator::Return(nil())).unwrap();
        rejected(b.finish().unwrap(), &format!("checked add on {repr}"));
    }
}

#[test]
fn float_arithmetic_is_for_floats_only() {
    for repr in [Repr::Float, Repr::F32] {
        ok(assigning(&[repr, repr], repr, |p| Rvalue::FloatArith {
            op: ArithOp::Rem,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr,
        }));
        ok(assigning(&[repr], repr, |p| Rvalue::FloatNeg {
            value: local(p[0]),
            repr,
        }));
    }
    rejected(
        assigning(&[Repr::Int, Repr::Int], Repr::Int, |p| Rvalue::FloatArith {
            op: ArithOp::Add,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr: Repr::Int,
        }),
        "float_arith add on int",
    );
    rejected(
        assigning(&[Repr::Float, Repr::Float], Repr::Float, |p| {
            Rvalue::FloatArith {
                op: ArithOp::Shl,
                lhs: local(p[0]),
                rhs: local(p[1]),
                repr: Repr::Float,
            }
        }),
        "float_arith shl",
    );
    rejected(
        assigning(&[Repr::Int], Repr::Int, |p| Rvalue::FloatNeg {
            value: local(p[0]),
            repr: Repr::Int,
        }),
        "float_neg on int",
    );
}

#[test]
fn bitwise_is_for_integers_only() {
    for repr in [Repr::Int, Repr::I32, Repr::U16] {
        ok(assigning(&[repr, repr], repr, |p| Rvalue::Bitwise {
            op: BitOp::Xor,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr,
        }));
    }
    rejected(
        assigning(&[Repr::Float, Repr::Float], Repr::Float, |p| {
            Rvalue::Bitwise {
                op: BitOp::And,
                lhs: local(p[0]),
                rhs: local(p[1]),
                repr: Repr::Float,
            }
        }),
        "bitwise and on float",
    );
    rejected(
        assigning(&[Repr::Int, Repr::I8], Repr::Int, |p| Rvalue::Bitwise {
            op: BitOp::Or,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr: Repr::Int,
        }),
        "bitwise operand: expected int, found i8",
    );
}

#[test]
fn not_and_compare_check_their_reprs() {
    ok(assigning(&[Repr::Bool], Repr::Bool, |p| {
        Rvalue::Not(local(p[0]))
    }));
    rejected(
        assigning(&[Repr::Int], Repr::Bool, |p| Rvalue::Not(local(p[0]))),
        "not operand: expected bool, found int",
    );
    for op in [CmpOp::Eq, CmpOp::Ne] {
        ok(assigning(&[Repr::Bool, Repr::Bool], Repr::Bool, |p| {
            Rvalue::Compare {
                op,
                lhs: local(p[0]),
                rhs: local(p[1]),
                repr: Repr::Bool,
            }
        }));
    }
    rejected(
        assigning(&[Repr::Bool, Repr::Bool], Repr::Bool, |p| Rvalue::Compare {
            op: CmpOp::Lt,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr: Repr::Bool,
        }),
        "compare lt on bool",
    );
    ok(assigning(&[Repr::F32, Repr::F32], Repr::Bool, |p| {
        Rvalue::Compare {
            op: CmpOp::Ge,
            lhs: local(p[0]),
            rhs: local(p[1]),
            repr: Repr::F32,
        }
    }));
    rejected(
        assigning(&[Repr::Value, Repr::Value], Repr::Bool, |p| {
            Rvalue::Compare {
                op: CmpOp::Eq,
                lhs: local(p[0]),
                rhs: local(p[1]),
                repr: Repr::Value,
            }
        }),
        "compare on uniform values",
    );
}

// Tags.

#[test]
fn read_tag_needs_several_constructors() {
    for ty in [
        TagType::Option,
        TagType::Result,
        TagType::Order,
        TagType::Adt(def(PAIR_TY)),
    ] {
        ok(assigning(&[Repr::Value], Repr::Int, |p| Rvalue::ReadTag {
            base: local(p[0]),
            ty,
        }));
    }
    // A constant operand: a unit constructor's static object.
    ok(assigning(&[], Repr::Int, |_| Rvalue::ReadTag {
        base: Operand::Const(Const::StaticCtor(def(EMPTY))),
        ty: TagType::Adt(def(PAIR_TY)),
    }));
    rejected(
        assigning(&[Repr::Value], Repr::Int, |p| Rvalue::ReadTag {
            base: local(p[0]),
            ty: TagType::Adt(def(SOLO_TY)),
        }),
        "which has 1 known constructor(s)",
    );
    rejected(
        assigning(&[Repr::Value], Repr::U32, |p| Rvalue::ReadTag {
            base: local(p[0]),
            ty: TagType::Option,
        }),
        "has representation u32 but is assigned a int",
    );
    // The unit lacks the first constructor of the type.
    let mut m = module(
        vec![assigning(&[Repr::Value], Repr::Int, |p| Rvalue::ReadTag {
            base: local(p[0]),
            ty: TagType::Adt(def(PAIR_TY)),
        })],
        Vec::new(),
    );
    m.layouts.remove(0);
    rejected_module(&m, "has 1 known constructor(s)");
    m.layouts.push(desc(def(15), def(PAIR_TY), 2, &[]));
    rejected_module(&m, "without all its constructors");
}

// Layout-aware `alloc` and `field`.

#[test]
fn constructor_allocations_follow_their_layouts() {
    ok(assigning(&[Repr::Value, Repr::Int], Repr::Value, |p| {
        Rvalue::Alloc {
            layout: LayoutRef::Ctor(def(PAIR)),
            fields: vec![local(p[0]), local(p[1])],
        }
    }));
    ok(assigning(&[Repr::Float], Repr::Value, |p| Rvalue::Alloc {
        layout: LayoutRef::Ctor(other(1)),
        fields: vec![local(p[0])],
    }));
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Alloc {
            layout: LayoutRef::Ctor(def(PAIR)),
            fields: vec![local(p[0])],
        }),
        "with 1 fields, expected 2",
    );
    rejected(
        assigning(&[Repr::Value, Repr::Value], Repr::Value, |p| {
            Rvalue::Alloc {
                layout: LayoutRef::Ctor(def(PAIR)),
                fields: vec![local(p[0]), local(p[1])],
            }
        }),
        "constructor field: expected int, found value",
    );
    rejected(
        assigning(&[], Repr::Value, |_| Rvalue::Alloc {
            layout: LayoutRef::Ctor(def(EMPTY)),
            fields: Vec::new(),
        }),
        "which is a static object",
    );
    rejected(
        assigning(&[Repr::Int], Repr::Value, |p| Rvalue::Alloc {
            layout: LayoutRef::Ctor(def(9)),
            fields: vec![local(p[0])],
        }),
        "no layout for constructor",
    );
}

#[test]
fn fields_follow_their_layouts() {
    ok(assigning(&[Repr::Value], Repr::Int, |p| Rvalue::Field {
        base: p[0],
        layout: LayoutRef::Ctor(def(PAIR)),
        field: 1,
    }));
    ok(assigning(&[Repr::Value], Repr::Float, |p| Rvalue::Field {
        base: p[0],
        layout: LayoutRef::Ctor(other(1)),
        field: 0,
    }));
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
            base: p[0],
            layout: LayoutRef::Ctor(def(PAIR)),
            field: 1,
        }),
        "has representation value but is assigned a int",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
            base: p[0],
            layout: LayoutRef::Ctor(def(PAIR)),
            field: 2,
        }),
        "field 2 of",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
            base: p[0],
            layout: LayoutRef::Tuple(2),
            field: 2,
        }),
        "field 2 of Tuple(2), which has 2",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
            base: p[0],
            layout: LayoutRef::Closure(1),
            field: 0,
        }),
        "is its code word",
    );
    ok(assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
        base: p[0],
        layout: LayoutRef::Closure(1),
        field: 1,
    }));
}

#[test]
fn dictionaries_dyn_values_and_descriptors_have_fixed_field_counts() {
    let allocs: [(LayoutRef, usize); 4] = [
        (
            LayoutRef::Dict {
                fns: 2,
                embedded: 1,
            },
            4,
        ),
        (LayoutRef::Dyn, 2),
        (LayoutRef::Desc { args: 2 }, 3),
        (LayoutRef::Tuple(3), 3),
    ];
    for (layout, n) in allocs {
        ok(assigning(&vec![Repr::Value; n], Repr::Value, |p| {
            Rvalue::Alloc {
                layout,
                fields: p.iter().map(|&l| local(l)).collect(),
            }
        }));
        rejected(
            assigning(&vec![Repr::Value; n - 1], Repr::Value, |p| Rvalue::Alloc {
                layout,
                fields: p.iter().map(|&l| local(l)).collect(),
            }),
            &format!("with {} fields, expected {n}", n - 1),
        );
        ok(assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
            base: p[0],
            layout,
            field: u32::try_from(n - 1).unwrap(),
        }));
        rejected(
            assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Field {
                base: p[0],
                layout,
                field: u32::try_from(n).unwrap(),
            }),
            &format!("which has {n}"),
        );
    }
    rejected(
        assigning(&[Repr::Value, Repr::Int], Repr::Value, |p| Rvalue::Alloc {
            layout: LayoutRef::Dyn,
            fields: vec![local(p[0]), local(p[1])],
        }),
        "field: expected value, found int",
    );
    rejected(
        assigning(&[Repr::Value, Repr::Value], Repr::Value, |p| {
            Rvalue::Alloc {
                layout: LayoutRef::DescNominal,
                fields: vec![local(p[0]), local(p[1])],
            }
        }),
        "only a static object",
    );
}

#[test]
fn closures_hold_uniform_code() {
    ok(assigning(&[Repr::Value], Repr::Value, |p| Rvalue::Alloc {
        layout: LayoutRef::Closure(1),
        fields: vec![
            Operand::Const(Const::Code(FnKey::Closure {
                owner: def(0),
                index: 0,
            })),
            local(p[0]),
        ],
    }));
    rejected(
        assigning(&[], Repr::Value, |_| Rvalue::Alloc {
            layout: LayoutRef::Closure(0),
            fields: vec![Operand::Const(Const::Code(FnKey::Item(def(1))))],
        }),
        "is not uniform",
    );
    rejected(
        assigning(&[], Repr::Value, |_| Rvalue::Alloc {
            layout: LayoutRef::Tuple(1),
            fields: vec![Operand::Const(Const::Code(FnKey::ValueShim(def(1))))],
        }),
        "a code word appears only as a closure's first field",
    );
}

#[test]
fn boxed_numbers_hold_their_raw_value() {
    ok(assigning(&[Repr::U64], Repr::Value, |p| Rvalue::Alloc {
        layout: LayoutRef::Builtin(crate::BuiltinLayout::BoxedU64),
        fields: vec![local(p[0])],
    }));
    rejected(
        assigning(&[Repr::Int], Repr::Value, |p| Rvalue::Alloc {
            layout: LayoutRef::Builtin(crate::BuiltinLayout::BoxedFloat),
            fields: vec![local(p[0])],
        }),
        "boxed number: expected float, found int",
    );
    rejected(
        assigning(&[], Repr::Value, |_| Rvalue::Alloc {
            layout: LayoutRef::Builtin(crate::BuiltinLayout::String),
            fields: Vec::new(),
        }),
        "which the runtime builds",
    );
}

// Uniform signatures.

#[test]
fn uniform_keys_have_the_uniform_signature() {
    let keys = [
        FnKey::ValueShim(def(1)),
        FnKey::Closure {
            owner: def(0),
            index: 3,
        },
        FnKey::TupleDictFn {
            iface_fn: other(0),
            arity: 2,
        },
        FnKey::DynForward {
            iface_fn: other(0),
            dyn_iface: other(1),
        },
    ];
    for key in keys {
        // `self` and two words.
        let mut b = builder_for(key);
        for _ in 0..3 {
            b.add_param(Repr::Value, None, None).unwrap();
        }
        b.terminate(Terminator::Return(nil())).unwrap();
        ok(b.finish().unwrap());
        // No `self`.
        let mut b = builder_for(key);
        b.terminate(Terminator::Return(nil())).unwrap();
        rejected(b.finish().unwrap(), "expected the uniform convention");
        // An unboxed parameter.
        let mut b = builder_for(key);
        b.add_param(Repr::Value, None, None).unwrap();
        b.add_param(Repr::Int, None, None).unwrap();
        b.terminate(Terminator::Return(nil())).unwrap();
        rejected(b.finish().unwrap(), "expected the uniform convention");
    }
}

// Runtime helpers.

/// A function calling `func` with parameters of its parameter
/// representations, using the result if it has one.
fn runtime_call(func: RuntimeFn) -> Function {
    let mut b = builder();
    let mut args: Vec<Operand> = func
        .params()
        .iter()
        .map(|&(r, _)| local(b.add_param(r, None, None).unwrap()))
        .collect();
    if func == RuntimeFn::Equal {
        // The location is a static string.
        args[2] = Operand::Const(Const::Str(b.string("src/main.cheby:1:1").unwrap()));
    }
    let result = b.call_runtime(func, args).unwrap();
    match (result, func.ret()) {
        (Some(r), Some(Repr::Value)) => {
            b.terminate(Terminator::Return(local(r))).unwrap();
        }
        (Some(r), Some(repr)) => {
            let boxed = b
                .assign(
                    Repr::Value,
                    Rvalue::Box {
                        value: local(r),
                        from: repr,
                    },
                )
                .unwrap();
            b.terminate(Terminator::Return(local(boxed))).unwrap();
        }
        _ => b.terminate(Terminator::Return(nil())).unwrap(),
    }
    b.finish().unwrap()
}

#[test]
fn runtime_calls_check_arity_reprs_and_results() {
    for func in RuntimeFn::ALL {
        ok(runtime_call(func));
    }
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::CallRuntime {
            func: RuntimeFn::StrWithCapacity,
            args: vec![local(p[0])],
        }),
        "runtime argument: expected int, found value",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Bool, |p| Rvalue::CallRuntime {
            func: RuntimeFn::Equal,
            args: vec![local(p[0])],
        }),
        "call_runtime equal with 1 arguments, expected 3",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Bool, |p| Rvalue::CallRuntime {
            func: RuntimeFn::Equal,
            args: vec![local(p[0]), local(p[0]), local(p[0])],
        }),
        "call_runtime equal: the location is not a string constant",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::CallRuntime {
            func: RuntimeFn::ListLength,
            args: vec![local(p[0])],
        }),
        "has representation value but is assigned a int",
    );
    rejected(
        assigning(&[Repr::Value], Repr::Value, |p| Rvalue::CallRuntime {
            func: RuntimeFn::EffectCheck,
            args: vec![local(p[0])],
        }),
        "a void call cannot define a local",
    );
    let mut b = builder();
    let a = b.add_param(Repr::Value, None, None).unwrap();
    b.push(Stmt::Eval(Rvalue::CallRuntime {
        func: RuntimeFn::Debug,
        args: vec![local(a)],
    }))
    .unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    rejected(
        b.finish().unwrap(),
        "only a void call is evaluated for effect",
    );
}

/// Runs the RC pass on one function and returns its statements, without
/// the string table, one line each; validates before and after.
fn counted(f: Function) -> Vec<String> {
    let mut m = module(vec![f], Vec::new());
    validate(&m, Stage::Lowered).unwrap();
    rc::insert_module(&mut m).unwrap();
    if let Err(errors) = validate(&m, Stage::Counted) {
        panic!("{errors:#?}\n{}", dump(&m).unwrap());
    }
    let f = m.functions.first().unwrap();
    crate::dump_function(&m, f)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("  ") && !l.trim_start().starts_with("str#"))
        .map(|l| l.trim().to_owned())
        .collect()
}

/// Each helper consumes its owned arguments and borrows the others
/// (D-520): a borrowed argument is dropped after the call, an owned one
/// is not, and the result is owned.
#[test]
fn runtime_helpers_follow_their_ownership() {
    for func in RuntimeFn::ALL {
        let lines = counted(runtime_call(func));
        // A slice of a list used for the last time consumes the list
        // instead of borrowing and dropping it (rc.rs).
        let func = match func {
            RuntimeFn::ListSlice => RuntimeFn::ListSliceOwned,
            other => other,
        };
        let call = lines
            .iter()
            .position(|l| l.contains(&format!("call_runtime {}(", func.name())))
            .unwrap_or_else(|| panic!("{}: {lines:#?}", func.name()));
        for (n, &(repr, kind)) in func.params().iter().enumerate() {
            // `equal`'s location is a string constant, not parameter `_2`.
            if repr != Repr::Value || (func == RuntimeFn::Equal && n == 2) {
                continue;
            }
            let drop = format!("drop _{n}");
            let dropped_after = lines.iter().skip(call).any(|l| *l == drop);
            assert_eq!(
                dropped_after,
                kind == UseKind::Borrowed,
                "{}: parameter {n}\n{lines:#?}",
                func.name()
            );
            assert!(
                !lines.iter().any(|l| *l == format!("dup _{n}")),
                "{}: {lines:#?}",
                func.name()
            );
        }
    }
    // An owned argument that is used again is dup'd first; a borrowed one
    // is not.
    let mut b = builder();
    let acc = b.add_param(Repr::Value, None, None).unwrap();
    let piece = b.add_param(Repr::Value, None, None).unwrap();
    let joined = b
        .assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::StrAppend,
                args: vec![local(acc), local(piece)],
            },
        )
        .unwrap();
    let again = b
        .assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::StrAppend,
                args: vec![local(joined), local(acc)],
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(local(again))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        [
            "dup _0",
            "_2: value = call_runtime str_append(_0, borrow _1)",
            "drop _1",
            "_3: value = call_runtime str_append(_2, borrow _0)",
            "drop _0",
            "return _3",
        ]
    );
}

/// A `list_slice` consumes its list when that is the list's last use, and
/// borrows it when the list is used again (rc.rs).
#[test]
fn list_slices_consume_a_list_used_for_the_last_time() {
    let slice_then = |use_again: bool| {
        let mut b = builder();
        let list = b.add_param(Repr::Value, None, None).unwrap();
        let rest = b
            .assign(
                Repr::Value,
                Rvalue::CallRuntime {
                    func: RuntimeFn::ListSlice,
                    args: vec![local(list), int(1), int(2)],
                },
            )
            .unwrap();
        if use_again {
            let _len = b
                .assign(
                    Repr::Int,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::ListLength,
                        args: vec![local(list)],
                    },
                )
                .unwrap();
        }
        b.terminate(Terminator::Return(local(rest))).unwrap();
        counted(b.finish().unwrap())
    };
    assert_eq!(
        slice_then(false),
        [
            "_1: value = call_runtime list_slice_owned(_0, borrow const int 1, borrow const int 2)",
            "return _1",
        ]
    );
    assert_eq!(
        slice_then(true),
        [
            "_1: value = call_runtime list_slice(borrow _0, borrow const int 1, borrow const int 2)",
            "_2: int = call_runtime list_length(borrow _0)",
            "drop _0",
            "return _1",
        ]
    );
}

/// `box` consumes nothing (its operand is raw) and gives an owned word;
/// `unbox` borrows its operand (D7).
#[test]
fn box_owns_its_result_and_unbox_borrows() {
    let mut b = builder();
    let n = b.add_param(Repr::Int, None, None).unwrap();
    let word = b.add_param(Repr::Value, None, None).unwrap();
    let raw = b
        .assign(
            Repr::Int,
            Rvalue::Unbox {
                value: local(word),
                to: Repr::Int,
            },
        )
        .unwrap();
    let unused = b
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: local(n),
                from: Repr::Int,
            },
        )
        .unwrap();
    let _ = unused;
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
    assert_eq!(
        counted(b.finish().unwrap()),
        [
            "_2: int = unbox int _1",
            "drop _1",
            "_3: value = box int _0",
            "drop _3",
            "_4: value = box int _2",
            "return _4",
        ]
    );
    // A unboxed word still used later is not dropped after `unbox`.
    let mut b = builder();
    let word = b.add_param(Repr::Value, None, None).unwrap();
    let raw = b
        .assign(
            Repr::Bool,
            Rvalue::Unbox {
                value: local(word),
                to: Repr::Bool,
            },
        )
        .unwrap();
    let _ = raw;
    b.terminate(Terminator::Return(local(word))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        ["_1: bool = unbox bool _0", "return _0"]
    );
}

/// The new rvalues borrow their operands; `read_tag` on a word keeps it.
#[test]
fn new_rvalues_borrow_their_operands() {
    let mut b = builder();
    let x = b.add_param(Repr::Value, None, None).unwrap();
    let tag = b
        .assign(
            Repr::Int,
            Rvalue::ReadTag {
                base: local(x),
                ty: TagType::Option,
            },
        )
        .unwrap();
    let _ = tag;
    b.terminate(Terminator::Return(local(x))).unwrap();
    assert_eq!(
        counted(b.finish().unwrap()),
        ["_1: int = read_tag _0 of option", "return _0"]
    );
}

// Statics.

fn dict_static(fields: Vec<Const>) -> StaticObject {
    StaticObject {
        id: StaticId::from_u32(0),
        kind: StaticKind::Dict,
        module: "app".into(),
        name: "Show$Point".into(),
        layout: LayoutRef::Dict {
            fns: 1,
            embedded: 0,
        },
        fields: fields.into(),
        strings: IndexVec::new(),
    }
}

fn nominal() -> StaticObject {
    StaticObject {
        id: StaticId::from_u32(1),
        kind: StaticKind::Desc,
        module: "geo".into(),
        name: "Point".into(),
        layout: LayoutRef::DescNominal,
        fields: Box::new([
            Const::Str(StrId::from_u32(0)),
            Const::Str(StrId::from_u32(1)),
        ]),
        strings: IndexVec::from_raw(vec!["Point".into(), "geo".into()]).unwrap(),
    }
}

fn fn_value() -> StaticObject {
    StaticObject {
        id: StaticId::from_u32(2),
        kind: StaticKind::FnValue,
        module: "app".into(),
        name: "show$tuple".into(),
        layout: LayoutRef::Closure(0),
        fields: Box::new([Const::Code(FnKey::TupleDictFn {
            iface_fn: other(0),
            arity: 2,
        })]),
        strings: IndexVec::new(),
    }
}

/// A function using `static#0`.
fn uses_static() -> Function {
    let mut b = builder();
    let d = b
        .assign(
            Repr::Value,
            Rvalue::Use(Operand::Const(Const::Static(StaticId::from_u32(0)))),
        )
        .unwrap();
    b.terminate(Terminator::Return(local(d))).unwrap();
    b.finish().unwrap()
}

fn statics_module(statics: Vec<StaticObject>) -> MirModule {
    module(vec![uses_static()], statics)
}

#[test]
fn statics_are_checked() {
    let good = vec![
        dict_static(vec![
            Const::Static(StaticId::from_u32(1)),
            Const::Static(StaticId::from_u32(2)),
        ]),
        nominal(),
        fn_value(),
    ];
    let m = statics_module(good.clone());
    validate(&m, Stage::Lowered).unwrap();
    let text = dump(&m).unwrap();
    assert!(
        text.ends_with(
            "static#0 dict app$Show$Point = dict/1+0(static#1, static#2)\n\
             static#1 desc geo$Point = desc_nominal(str#0, str#1) \
             str#0 = \"Point\" str#1 = \"geo\"\n\
             static#2 fnval app$show$tuple = closure/0(code <tuple_dict_fn geo::f0/2>)\n"
        ),
        "{text}"
    );

    let cases: Vec<(Vec<StaticObject>, &str)> = vec![
        // A code word outside a function value.
        (
            vec![dict_static(vec![Const::Code(FnKey::ValueShim(def(1)))])],
            "as field 0",
        ),
        // A raw constant.
        (vec![dict_static(vec![Const::Int(1)])], "is a raw int"),
        // A reference to itself, and to a static that does not exist.
        (
            vec![dict_static(vec![Const::Static(StaticId::from_u32(0))])],
            "field 0 is static#0",
        ),
        (
            vec![dict_static(vec![Const::Static(StaticId::from_u32(7))])],
            "field 0 is static#7",
        ),
        // A string outside the static's table.
        (
            vec![dict_static(vec![Const::Str(StrId::from_u32(0))])],
            "unknown string str#0",
        ),
    ];
    for (statics, needle) in cases {
        rejected_module(&statics_module(statics), needle);
    }
    // The dict layout has 2 fields (descriptor + 1 closure); one field is
    // too few.
    let mut short = dict_static(vec![Const::Nil]);
    short.layout = LayoutRef::Dict {
        fns: 2,
        embedded: 0,
    };
    rejected_module(&statics_module(vec![short]), "1 fields, its layout has 3");
    // A kind with the wrong layout, a wrong id, a function value without
    // its code, and a symbol defined twice.
    let mut wrong = nominal();
    wrong.kind = StaticKind::Dict;
    wrong.id = StaticId::from_u32(0);
    rejected_module(
        &statics_module(vec![wrong]),
        "a dict static with layout DescNominal",
    );
    let mut misnumbered = dict_static(vec![Const::Nil]);
    misnumbered.id = StaticId::from_u32(3);
    rejected_module(&statics_module(vec![misnumbered]), "has id 3");
    let mut codeless = fn_value();
    codeless.id = StaticId::from_u32(0);
    codeless.fields = Box::new([Const::Nil]);
    rejected_module(
        &statics_module(vec![codeless]),
        "a function value without its code",
    );
    let mut twice = good;
    let mut copy = twice[0].clone();
    copy.id = StaticId::from_u32(3);
    twice.push(copy);
    rejected_module(&statics_module(twice), "app$Show$Point is defined twice");
    // A function naming a static the unit does not have.
    rejected_module(&module(vec![uses_static()], Vec::new()), "unknown static#0");
}

/// Constants of the runtime: the empty list and builtin descriptors.
#[test]
fn runtime_constants_are_uniform() {
    ok(assigning(&[], Repr::Value, |_| {
        Rvalue::Use(Operand::Const(Const::EmptyList))
    }));
    let f = assigning(&[], Repr::Value, |_| {
        Rvalue::Use(Operand::Const(Const::BuiltinDesc(BuiltinDesc::Option)))
    });
    let m = module(vec![f], Vec::new());
    validate(&m, Stage::Lowered).unwrap();
    assert!(
        dump(&m)
            .unwrap()
            .contains("_0: value = const builtin_desc option"),
    );
}

// Dump.

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one function exercising every new dump form"
)]
fn dump_prints_new_forms_layouts_and_callees() {
    let mut b = builder();
    let x = b.add_param(Repr::Float, None, None).unwrap();
    let flag = b.add_param(Repr::Bool, None, None).unwrap();
    let list = b.add_param(Repr::Value, None, None).unwrap();
    let neg = b
        .assign(
            Repr::Float,
            Rvalue::FloatNeg {
                value: local(x),
                repr: Repr::Float,
            },
        )
        .unwrap();
    let _ = b
        .assign(
            Repr::Float,
            Rvalue::FloatArith {
                op: ArithOp::Div,
                lhs: local(neg),
                rhs: Operand::Const(Const::Float(2.0_f64.to_bits())),
                repr: Repr::Float,
            },
        )
        .unwrap();
    let _ = b
        .assign(
            Repr::Int,
            Rvalue::Bitwise {
                op: BitOp::And,
                lhs: int(6),
                rhs: int(3),
                repr: Repr::Int,
            },
        )
        .unwrap();
    let _ = b.assign(Repr::Bool, Rvalue::Not(local(flag))).unwrap();
    let len = b
        .assign(
            Repr::Int,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListLength,
                args: vec![local(list)],
            },
        )
        .unwrap();
    let _ = len;
    let pushed = b
        .assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListPush,
                args: vec![
                    Operand::Const(Const::EmptyList),
                    Operand::Const(Const::StaticCtor(def(EMPTY))),
                ],
            },
        )
        .unwrap();
    let tag = b
        .assign(
            Repr::Int,
            Rvalue::ReadTag {
                base: local(pushed),
                ty: TagType::Adt(def(PAIR_TY)),
            },
        )
        .unwrap();
    let _ = tag;
    let field = b
        .assign(
            Repr::Float,
            Rvalue::Field {
                base: pushed,
                layout: LayoutRef::Ctor(other(1)),
                field: 0,
            },
        )
        .unwrap();
    let _ = field;
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def(1)),
        args: vec![local(pushed)],
    })
    .unwrap();
    let m = module(vec![b.finish().unwrap()], Vec::new());
    validate(&m, Stage::Lowered).unwrap();
    let key = type_key("app", "T");
    let expected = format!(
        r#"module app

fn app::f0(_0: float, _1: bool, _2: value) -> value check str#0 {{
  str#0 = "src/main.cheby:1:4"
bb0:
  _3: float = float_neg float _0
  _4: float = float_arith div float _3, const float 2.0
  _5: int = bitwise and int const int 6, const int 3
  _6: bool = not _1
  _7: int = call_runtime list_length(borrow _2)
  _8: value = call_runtime list_push(const empty_list, const ctor app::f12)
  _9: int = read_tag _8 of app::f10
  _10: float = field _8.0 of ctor geo::f1
  tail_call app::f1(_8)
}}

layout ctor app::f11: T.C tag 0 key {key:016x} [value @8, int @16]
layout ctor app::f12: T.C tag 1 key {key:016x} []
layout ctor app::f14: T.C tag 0 key {key:016x} [int @8]
foreign_layout ctor geo::f1: T.C tag 0 key {key:016x} [float @8]
callee app::f1(value) -> value
"#
    );
    assert_eq!(dump(&m).unwrap(), expected);
}

// Borrowing (D-715): the validator's cross-checks of a borrowed parameter
// against counting, the callee declarations, tail calls and the entry pad.

/// The mask that borrows parameter 0.
fn first_borrowed() -> BorrowMask {
    BorrowMask::OWNED.with(0)
}

/// [`module`] whose callee `app::f1` borrows its one parameter.
fn borrowing_module(functions: Vec<Function>) -> MirModule {
    let mut m = module(functions, Vec::new());
    for decl in &mut m.callees {
        decl.sig.borrow = first_borrowed();
    }
    m
}

/// `app::f1(borrow _0: value)` with the body `body` writes.
fn borrower(body: impl FnOnce(&mut FunctionBuilder, Local)) -> Function {
    let mut b = builder_for(FnKey::Item(def(1)));
    let p = b.add_param(Repr::Value, None, None).unwrap();
    body(&mut b, p);
    let mut f = b.finish().unwrap();
    f.borrow = first_borrowed();
    f
}

fn errors_at(m: &MirModule, stage: Stage) -> String {
    validate(m, stage).err().unwrap_or_default().join("\n")
}

#[test]
fn borrow_validate_borrowed_parameter_consumed_without_a_dup() {
    // `return _0` gives away a reference the function does not hold.
    let returns = borrower(|b, p| b.terminate(Terminator::Return(local(p))).unwrap());
    let m = borrowing_module(vec![returns.clone()]);
    validate(&m, Stage::Lowered).unwrap();
    let errors = errors_at(&m, Stage::Counted);
    assert!(
        errors.contains("_0 is used without a reference (holds 0, needs 1)"),
        "{errors}"
    );
    // A drop of it is the same mistake.
    let drops = borrower(|b, p| {
        b.push(Stmt::Drop(p)).unwrap();
        b.terminate(Terminator::Return(nil())).unwrap();
    });
    let errors = errors_at(&borrowing_module(vec![drops]), Stage::Counted);
    assert!(
        errors.contains("_0 is used without a reference"),
        "{errors}"
    );
    // The RC pass gives the return its `dup` and drops nothing.
    let mut counted = borrowing_module(vec![returns]);
    rc::insert_module(&mut counted).unwrap();
    validate(&counted, Stage::Counted).unwrap();
    let text = dump(&counted).unwrap();
    assert!(
        text.contains("fn app::f1(borrow _0: value)") && text.contains("  dup _0\n  return _0"),
        "{text}"
    );
    assert!(!text.contains("drop _0"), "{text}");
}

#[test]
fn borrow_validate_call_mask_differs_from_its_callee_declaration() {
    let call = |callee: Callee| {
        let mut b = builder();
        let p = b.add_param(Repr::Value, None, None).unwrap();
        b.assign(
            Repr::Value,
            Rvalue::Call {
                callee,
                args: vec![local(p)],
            },
        )
        .unwrap();
        b.terminate(Terminator::Return(nil())).unwrap();
        b.finish().unwrap()
    };
    // A borrowing call of an all-owned callee.
    let m = module(
        vec![call(Callee::Borrowing(def(1), first_borrowed()))],
        Vec::new(),
    );
    let errors = errors_at(&m, Stage::Lowered);
    assert!(
        errors.contains("borrows BorrowMask(1), but its declaration borrows BorrowMask(0) (D-715)"),
        "{errors}"
    );
    // An all-owned call of a borrowing callee.
    let m = borrowing_module(vec![call(Callee::Direct(def(1)))]);
    let errors = errors_at(&m, Stage::Lowered);
    assert!(
        errors.contains("borrows BorrowMask(0), but its declaration borrows BorrowMask(1) (D-715)"),
        "{errors}"
    );
    // A borrowing call that borrows nothing.
    let mut m = module(
        vec![call(Callee::Borrowing(def(1), BorrowMask::OWNED))],
        Vec::new(),
    );
    let errors = errors_at(&m, Stage::Lowered);
    assert!(errors.contains("borrows nothing (D-715)"), "{errors}");
    // And a body whose mask differs from its own declaration.
    m.functions = vec![borrower(|b, _| {
        b.terminate(Terminator::Return(nil())).unwrap();
    })];
    let errors = errors_at(&m, Stage::Lowered);
    assert!(
        errors.contains("differs from its callee declaration"),
        "{errors}"
    );
    // The matching call is fine, and the argument stays the caller's.
    let mut m = borrowing_module(vec![call(Callee::Borrowing(def(1), first_borrowed()))]);
    validate(&m, Stage::Lowered).unwrap();
    rc::insert_module(&mut m).unwrap();
    validate(&m, Stage::Counted).unwrap();
    let text = dump(&m).unwrap();
    assert!(
        text.contains("call app::f1(borrow _0)\n  drop _0"),
        "{text}"
    );
}

#[test]
fn borrow_validate_owned_local_in_a_borrowed_tail_position() {
    // `f0` owns `_0` and jumps to `f1`, which borrows it: nothing would
    // release it after the jump (D-015).
    let jump = || {
        let mut b = builder();
        let p = b.add_param(Repr::Value, None, None).unwrap();
        b.terminate(Terminator::TailCall {
            callee: Callee::Borrowing(def(1), first_borrowed()),
            args: vec![local(p)],
        })
        .unwrap();
        b.finish().unwrap()
    };
    let m = borrowing_module(vec![jump()]);
    validate(&m, Stage::Lowered).unwrap();
    let errors = errors_at(&m, Stage::Counted);
    assert!(
        errors.contains("bb0 terminator: _0 still holds 1 reference(s)"),
        "{errors}"
    );
    // The RC pass refuses to place it.
    let mut m = borrowing_module(vec![jump()]);
    let ice = rc::insert_module(&mut m).unwrap_err();
    assert!(
        ice.to_string()
            .contains("a borrowed use in a moving terminator"),
        "{ice}"
    );
    // A borrowed parameter or a constant fits the position.
    let mut passes_on = borrower(|b, p| {
        b.terminate(Terminator::TailCall {
            callee: Callee::Borrowing(def(1), first_borrowed()),
            args: vec![local(p)],
        })
        .unwrap();
    });
    passes_on.key = FnKey::Item(def(2));
    let mut m = borrowing_module(vec![passes_on]);
    m.callees.push(CalleeDecl {
        def: def(2),
        sig: AbiSig {
            borrow: first_borrowed(),
            ..AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value)
        },
    });
    validate(&m, Stage::Lowered).unwrap();
    rc::insert_module(&mut m).unwrap();
    validate(&m, Stage::Counted).unwrap();
}

#[test]
fn borrow_validate_entry_pad_drops_a_borrowed_parameter() {
    let mut f = borrower(|b, _| b.terminate(Terminator::Return(nil())).unwrap());
    let recorded = crate::unwind::insert_recording(&mut f).unwrap().unwrap();
    // The pass's entry pad releases nothing: `_0` is the caller's.
    let resume = f.entry_unwind.unwrap();
    assert_eq!(f.block(resume).unwrap().term, Terminator::Resume);
    let m = borrowing_module(vec![f.clone()]);
    crate::validate::validate_recorded(&m, std::slice::from_ref(&recorded), true).unwrap();
    // An entry pad that drops it.
    let pad = f
        .blocks
        .push(crate::ir::Block {
            params: Vec::new(),
            stmts: vec![Stmt::Drop(Local::from_u32(0))],
            term: Terminator::Goto {
                target: resume,
                args: Vec::new(),
            },
            unwind: Vec::new(),
        })
        .unwrap();
    f.entry_unwind = Some(pad);
    let m = borrowing_module(vec![f]);
    let errors = crate::validate::validate_recorded(&m, std::slice::from_ref(&recorded), false)
        .unwrap_err()
        .join("\n");
    assert!(
        errors.contains("which drops {_0}, but an unwinding there leaves {} held"),
        "{errors}"
    );
    let errors = errors_at(&m, Stage::Unwound);
    assert!(
        errors.contains("_0 is used without a reference (holds 0, needs 1)"),
        "{errors}"
    );
}

#[test]
fn borrow_validate_mask_only_on_counted_parameters_of_items() {
    // A raw parameter holds no reference to borrow.
    let mut b = builder_for(FnKey::Item(def(3)));
    b.add_param(Repr::Int, None, None).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let mut f = b.finish().unwrap();
    f.borrow = first_borrowed();
    rejected(f.clone(), "which is not counted (D-715)");
    // A parameter the function does not have.
    f.borrow = BorrowMask::OWNED.with(5);
    rejected(f, "borrows parameter 5, but has 1 (D-715)");
    // Only a top-level function borrows.
    let mut b = builder_for(FnKey::ValueShim(def(3)));
    b.add_param(Repr::Value, None, None).unwrap();
    b.terminate(Terminator::Return(nil())).unwrap();
    let mut shim = b.finish().unwrap();
    shim.borrow = first_borrowed();
    rejected(
        shim,
        "only a top-level function that is not precounted may borrow",
    );
}
