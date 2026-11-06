//! Objects on hand-built MIR: allocation and fields of user constructors
//! (through `cb$layout$`), tuples, closures, dictionaries, `dyn` values
//! and descriptors (D-454, D-524); tags (D-522); constant slots (E6);
//! static objects and function values (D-523).

use super::*;

/// The type `app::T` with three constructors, in tag order:
/// `A(v0: Value, x: Int, f: Float, b: Bool, s: I8, u: U16, h: F32,
/// v1: Value)`, the nullary `B`, and `C(Value)`.
const T: (u32, u32) = (1, 900);
const A: (u32, u32) = (1, 901);
const B: (u32, u32) = (1, 902);
const C: (u32, u32) = (1, 903);

fn at(d: (u32, u32)) -> DefId {
    def(d.0, d.1)
}

fn field(name: Option<&str>, repr: Repr, offset: i32) -> FieldDesc {
    FieldDesc {
        name: name.map(Box::from),
        repr,
        offset,
    }
}

fn ctor_desc(ctor: DefId, ctor_name: &str, tag: u32, fields: Vec<FieldDesc>) -> LayoutDesc {
    let value_fields = fields.iter().filter(|f| f.repr == Repr::Value).count();
    let raw_fields = fields.len().checked_sub(value_fields).unwrap();
    LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: format!("app::{ctor_name}").into(),
        tag,
        only_ctor: false,
        fields,
        value_fields: u32::try_from(value_fields).unwrap(),
        raw_fields: u32::try_from(raw_fields).unwrap(),
        type_def: at(T),
        type_name: "T".into(),
        ctor_name: ctor_name.into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key("app", "T"),
    }
}

/// The layouts of `T`'s constructors, as MIR computes them: value fields
/// first, then raw fields, each in declaration order (plan §5.2).
fn t_layouts() -> Vec<LayoutDesc> {
    vec![
        ctor_desc(
            at(A),
            "A",
            0,
            vec![
                field(None, Repr::Value, 8),
                field(None, Repr::Int, 24),
                field(None, Repr::Float, 32),
                field(None, Repr::Bool, 40),
                field(None, Repr::I8, 48),
                field(None, Repr::U16, 56),
                field(None, Repr::F32, 64),
                field(None, Repr::Value, 16),
            ],
        ),
        ctor_desc(at(B), "B", 1, Vec::new()),
        ctor_desc(at(C), "C", 2, vec![field(None, Repr::Value, 8)]),
    ]
}

/// The layout ids registration gave `A`, `B` and `C`, read from their
/// layout words through the batch's module record (D-919, D-920).
fn t_ids(jit: &Jit) -> [u32; 3] {
    let records = jit.module_records();
    let words = super::records::ctor_words(&records[0]);
    let [a, b, c] = words[0].as_slice() else {
        panic!("{words:?}");
    };
    [a, b, c].map(|&(word, _)| super::records::id_at(word))
}

/// A `Prog` that declares `T`.
fn with_t() -> Prog {
    let mut p = Prog {
        layouts: t_layouts(),
        ..Prog::default()
    };
    for (d, item) in [(T, "T"), (A, "A"), (B, "B"), (C, "C")] {
        p.names.push(name(at(d), "app", item));
    }
    p
}

/// The fields `A` is built with: a string, -5, 2.5, `True`, -3, 65535,
/// 1.5, `Nil`.
fn a_fields() -> Vec<Operand> {
    vec![
        Operand::Const(Const::Str(StrId::from_u32(1))),
        Operand::Const(Const::Int(-5)),
        Operand::Const(Const::Float(2.5_f64.to_bits())),
        Operand::Const(Const::Bool(true)),
        Operand::Const(Const::Sized {
            repr: Repr::I8,
            bits: (-3_i64).cast_unsigned(),
        }),
        Operand::Const(Const::Sized {
            repr: Repr::U16,
            bits: 65535,
        }),
        Operand::Const(Const::Sized {
            repr: Repr::F32,
            bits: u64::from(1.5_f32.to_bits()),
        }),
        Operand::Const(Const::Nil),
    ]
}

/// `get_k() { o = alloc A(…); x = field o k; return x }`, a value field
/// dup'd before `o` is dropped, a raw one boxed.
fn get_field(p: &mut Prog, index: u32, k: u32, repr: Repr) -> DefId {
    let read = assign(
        1,
        Rvalue::Field {
            base: local(0),
            layout: LayoutRef::Ctor(at(A)),
            field: k,
        },
    );
    let alloc = assign(
        0,
        Rvalue::Alloc {
            layout: LayoutRef::Ctor(at(A)),
            fields: a_fields(),
        },
    );
    let (stmts, result, locals) = if repr == Repr::Value {
        (
            vec![alloc, read, Stmt::Dup(local(1)), Stmt::Drop(local(0))],
            1,
            vec![Repr::Value, Repr::Value],
        )
    } else {
        (
            vec![
                alloc,
                read,
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: repr,
                    },
                ),
                Stmt::Drop(local(0)),
            ],
            2,
            vec![Repr::Value, repr, Repr::Value],
        )
    };
    p.item(
        index,
        &format!("get{k}"),
        &[],
        &locals,
        straight(stmts, result),
        &["a string field"],
    )
}

/// `A` is allocated with the header from its `cb$layout$` word (count 1,
/// the registered id), and each value and raw field reads back.
#[test]
fn user_constructors_allocate_through_their_layout_words() {
    let mut p = with_t();
    let reprs = [
        Repr::Value,
        Repr::Int,
        Repr::Float,
        Repr::Bool,
        Repr::I8,
        Repr::U16,
        Repr::F32,
        Repr::Value,
    ];
    let mut runs = Vec::new();
    for (k, repr) in (0_u32..).zip(reprs) {
        let f = get_field(&mut p, k, k, repr);
        runs.push((k, p.call(f, Vec::new())));
    }
    let whole = p.item(
        50,
        "whole",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Alloc {
                    layout: LayoutRef::Ctor(at(A)),
                    fields: a_fields(),
                },
            )],
            0,
        ),
        &["a string field"],
    );
    let whole = p.call(whole, Vec::new());
    let jit = p.compile();
    let [a_id, b_id, c_id] = t_ids(&jit);
    assert_eq!(b_id, a_id.checked_add(1).unwrap());
    assert_eq!(c_id, a_id.checked_add(2).unwrap());
    let object = returned(run_const(&jit, whole));
    let header = read_word(object, 0);
    assert_eq!(abi::header_layout(header), a_id);
    assert_eq!(abi::header_rc(header), 1);
    assert_eq!(abi::header_scan(header), 2);
    let info = cheby_runtime::layout::get(a_id).unwrap();
    assert_eq!(&*info.ctor_name, "A");
    assert_eq!(&*info.type_name, "T");
    release(&jit, object);
    for (k, target) in runs {
        let word = returned(run_const(&jit, target));
        match k {
            0 => assert_eq!(text_of(word), "a string field"),
            1 => assert_eq!(int_of(word), -5),
            2 => assert_eq!(read_word(word, 8), 2.5_f64.to_bits()),
            3 => assert_eq!(word, static_word(abi::layout::TRUE)),
            4 => assert_eq!(int_of(word), -3),
            5 => assert_eq!(int_of(word), 65535),
            6 => assert_eq!(
                read_word(word, 8) & 0xffff_ffff,
                u64::from(1.5_f32.to_bits())
            ),
            _ => assert_eq!(word, static_nil()),
        }
        release(&jit, word);
    }
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `tag(x) { t = read_tag x ty; drop x; return box t }`, where `make`
/// computes `x`.
fn tag_of(p: &mut Prog, index: u32, make: Rvalue, ty: TagType) -> DefId {
    p.item(
        index,
        &format!("tag{index}"),
        &[],
        &[Repr::Value, Repr::Int, Repr::Value],
        straight(
            vec![
                assign(0, make),
                assign(1, Rvalue::ReadTag { base: op(0), ty }),
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Int,
                    },
                ),
            ],
            2,
        ),
        &["a string field"],
    )
}

fn constant(c: Const) -> Rvalue {
    Rvalue::Use(Operand::Const(c))
}

/// Tags read from layout ids (D-522): `Option`, `Result` and `Order` by
/// the fixed ids, the user type `T` by the id in `A`'s `cb$layout$` word,
/// including its nullary `B`, a `cb$ctor$` static with count 0.
#[test]
fn tags_come_from_layout_ids() {
    let mut p = with_t();
    let one = |ctor| Rvalue::Alloc {
        layout: LayoutRef::Ctor(ctor),
        fields: vec![Operand::Const(Const::Nil)],
    };
    let cases = [
        (one(builtin::SOME), TagType::Option, 0),
        (
            constant(Const::StaticCtor(builtin::NONE)),
            TagType::Option,
            1,
        ),
        (one(builtin::OK), TagType::Result, 0),
        (one(builtin::ERR), TagType::Result, 1),
        (
            constant(Const::StaticCtor(builtin::LESS)),
            TagType::Order,
            0,
        ),
        (
            constant(Const::StaticCtor(builtin::EQUAL)),
            TagType::Order,
            1,
        ),
        (
            constant(Const::StaticCtor(builtin::GREATER)),
            TagType::Order,
            2,
        ),
        (
            Rvalue::Alloc {
                layout: LayoutRef::Ctor(at(A)),
                fields: a_fields(),
            },
            TagType::Adt(at(T)),
            0,
        ),
        (constant(Const::StaticCtor(at(B))), TagType::Adt(at(T)), 1),
        (one(at(C)), TagType::Adt(at(T)), 2),
    ];
    let mut runs = Vec::new();
    for (index, (make, ty, tag)) in (0_u32..).zip(cases) {
        let f = tag_of(&mut p, index, make, ty);
        runs.push((p.call(f, Vec::new()), tag));
    }
    let b = p.item(
        60,
        "b",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::StaticCtor(at(B)))),
        )],
        &[],
    );
    let b = p.call(b, Vec::new());
    for d in [builtin::SOME, builtin::NONE, builtin::OK, builtin::ERR] {
        p.names.push(name(d, "", "builtin"));
    }
    for d in [builtin::LESS, builtin::EQUAL, builtin::GREATER] {
        p.names.push(name(d, "", "builtin"));
    }
    let jit = p.compile();
    for (target, tag) in runs {
        assert_eq!(
            returned(run_const(&jit, target)),
            abi::tag_int(tag).unwrap()
        );
    }
    let word = returned(run_const(&jit, b));
    assert_eq!(rc_of(word), abi::RC_STATIC);
    assert_eq!(layout_of(word), t_ids(&jit)[1]);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `get() { o = alloc layout fields; x = field o k; dup x; drop o;
/// return x }`.
fn alloc_and_read(
    p: &mut Prog,
    index: u32,
    layout: LayoutRef,
    fields: Vec<Operand>,
    k: u32,
) -> DefId {
    p.item(
        index,
        &format!("get{index}"),
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                assign(0, Rvalue::Alloc { layout, fields }),
                assign(
                    1,
                    Rvalue::Field {
                        base: local(0),
                        layout,
                        field: k,
                    },
                ),
                Stmt::Dup(local(1)),
                Stmt::Drop(local(0)),
            ],
            1,
        ),
        &["the field"],
    )
}

fn nils(n: usize) -> Vec<Operand> {
    vec![Operand::Const(Const::Nil); n]
}

fn the_field() -> Operand {
    Operand::Const(Const::Str(StrId::from_u32(1)))
}

/// Tuples (also one with more fields than a scan count holds),
/// dictionaries, `dyn` values and descriptors are allocated with their
/// fixed ids and read back by index (plan §5.4).
#[test]
fn runtime_objects_allocate_and_read_back() {
    let mut p = Prog::default();
    let with_last = |n: usize| {
        let mut fields = nils(n.checked_sub(1).unwrap());
        fields.push(the_field());
        fields
    };
    let cases = [
        (LayoutRef::Tuple(3), with_last(3), 2),
        (LayoutRef::Tuple(300), with_last(300), 299),
        (
            LayoutRef::Dict {
                fns: 2,
                embedded: 1,
            },
            with_last(4),
            3,
        ),
        (LayoutRef::Dyn, with_last(2), 1),
        (LayoutRef::Desc { args: 2 }, with_last(3), 2),
    ];
    let mut runs = Vec::new();
    let mut whole = Vec::new();
    for (index, (layout, fields, k)) in (0_u32..).zip(cases) {
        let f = alloc_and_read(&mut p, index, layout, fields.clone(), k);
        runs.push(p.call(f, Vec::new()));
        let w = p.item(
            index.checked_add(100).unwrap(),
            &format!("whole{index}"),
            &[],
            &[Repr::Value],
            straight(vec![assign(0, Rvalue::Alloc { layout, fields })], 0),
            &["the field"],
        );
        whole.push((p.call(w, Vec::new()), builtin_layout_id(&layout).unwrap()));
    }
    let jit = p.compile();
    for target in runs {
        assert_eq!(text_of(returned(run_const(&jit, target))), "the field");
    }
    for (target, id) in whole {
        let word = returned(run_const(&jit, target));
        assert_eq!(layout_of(word), id);
        release(&jit, word);
    }
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// A closure: its code word `addr | 1` from `Const::Code`, a capture, and
/// an indirect call that passes it as `self` (D-453).
#[test]
fn closures_call_their_code_with_self() {
    let mut p = Prog::default();
    let owner = def(1, 0);
    let key = FnKey::Closure { owner, index: 0 };
    // `clo(self, x) { c = field self 1; dup c; drop self; drop x; return c }`.
    p.functions.push(function(
        key,
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Value],
        Repr::Value,
        straight(
            vec![
                assign(
                    2,
                    Rvalue::Field {
                        base: local(0),
                        layout: LayoutRef::Closure(1),
                        field: 1,
                    },
                ),
                Stmt::Dup(local(2)),
                Stmt::Drop(local(0)),
                Stmt::Drop(local(1)),
            ],
            2,
        ),
        &[],
    ));
    // `f() { k = alloc closure(code, "captured"); r = call k(nil); return r }`.
    let f = p.item(
        0,
        "f",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(1),
                        fields: vec![
                            Operand::Const(Const::Code(key)),
                            Operand::Const(Const::Str(StrId::from_u32(1))),
                        ],
                    },
                ),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Indirect(local(0)),
                        args: vec![Operand::Const(Const::Nil)],
                    },
                ),
            ],
            1,
        ),
        &["captured"],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile();
    assert_eq!(text_of(returned(run_const(&jit, target))), "captured");
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `const_slot` and `set_const`: a `LoadConst` reads the slot
/// `cb$const$app$LIMIT` once it is filled; an empty slot calls
/// `cheby_rt_const_missing`, which outside constant evaluation ends the
/// process, so that path runs in a child process.
#[test]
fn load_const_reads_the_slot() {
    let constant = def(1, 70);
    let build = || {
        let mut p = Prog::default();
        p.names.push(name(constant, "app", "LIMIT"));
        let f = p.item(
            0,
            "f",
            &[],
            &[Repr::Value],
            straight(
                vec![assign(0, Rvalue::LoadConst(constant)), Stmt::Dup(local(0))],
                0,
            ),
            &[],
        );
        let target = p.call(f, Vec::new());
        let mut jit = Jit::with_options(CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        })
        .unwrap();
        jit.compile(&p.units()).unwrap();
        (jit, target)
    };
    if std::env::var_os(CHILD).is_some() {
        let (jit, target) = build();
        let _ = run_const(&jit, target);
        return;
    }
    let (mut jit, target) = build();
    let clif = jit.take_clif().join("\n");
    assert!(clif.contains("= cb$const$app$LIMIT"), "{clif}");
    assert!(clif.contains("= cb$imp$cheby_rt_const_missing"), "{clif}");
    let slot = jit.const_slot(constant).unwrap();
    // SAFETY: the slot is a live, aligned word of the JIT.
    assert_eq!(unsafe { slot.read() }, 0);
    let seven = abi::tag_int(7).unwrap();
    jit.set_const(constant, seven).unwrap();
    assert_eq!(returned(run_const(&jit, target)), seven);
    let stderr = run_child("tests::objects::load_const_reads_the_slot", &[]);
    assert!(
        stderr.contains("is empty outside constant evaluation"),
        "{stderr}"
    );
}

/// The environment variable that makes a test run its child half.
pub(super) const CHILD: &str = "CHEBY_JIT_TEST_CHILD";

/// Runs the test `test` of this binary in a child process with `env` set
/// and returns its standard error; the child must fail.
pub(super) fn run_child(test: &str, env: &[(&str, &str)]) -> String {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(!output.status.success(), "the child of {test} succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `g(x: Int) { y = checked add x 1; return box y }`, its shim
/// `shim(self, x) { drop self; n = unbox x; drop x; tail g(n) }`, and the
/// declarations they need.
fn g_and_shim(p: &mut Prog) -> DefId {
    let g = p.item(
        20,
        "g",
        &[Repr::Int],
        &[Repr::Int, Repr::Value],
        straight(
            vec![
                assign(
                    1,
                    Rvalue::Checked {
                        op: ArithOp::Add,
                        lhs: op(0),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                ),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Int,
                    },
                ),
            ],
            2,
        ),
        &[LOC],
    );
    p.functions.push(function(
        FnKey::ValueShim(g),
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Int],
        Repr::Value,
        vec![block(
            vec![
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Unbox {
                        value: op(1),
                        to: Repr::Int,
                    },
                ),
                Stmt::Drop(local(1)),
            ],
            Terminator::TailCall {
                callee: Callee::Direct(g),
                args: vec![op(2)],
            },
        )],
        &[],
    ));
    g
}

/// The nominal descriptor `cb$desc$geo$Point` (layout 12, D-524) as static
/// `id`: the printed name and the module path, as the static's own
/// strings.
fn nominal_point(id: u32) -> StaticObject {
    StaticObject {
        id: StaticId::from_u32(id),
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

/// `call(k) { r = call k(41); return r }` where `make` gives the closure
/// `k`.
fn call_41(p: &mut Prog, index: u32, make: Rvalue) -> DefId {
    p.item(
        index,
        &format!("call{index}"),
        &[],
        &[Repr::Value, Repr::Value, Repr::Value],
        straight(
            vec![
                assign(0, make),
                assign(
                    1,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(41)),
                        from: Repr::Int,
                    },
                ),
                assign(
                    2,
                    Rvalue::Call {
                        callee: Callee::Indirect(local(0)),
                        args: vec![op(1)],
                    },
                ),
            ],
            2,
        ),
        &[],
    )
}

/// Static objects (D-523): a nominal descriptor with its own strings, and
/// a dictionary holding it and, as its function,
/// the function value `cb$fnval$app$g`, a static closure whose code word is
/// `cb$shim$app$g | 1`. Both have count 0; calling through the dictionary
/// and through `Const::FnValue` reaches `g`. `Const::EmptyList` is the
/// runtime's static empty list.
#[test]
#[expect(clippy::too_many_lines, reason = "one hand-built MIR program")]
fn static_objects_and_function_values() {
    let mut p = Prog::default();
    let g = g_and_shim(&mut p);
    p.statics = vec![
        nominal_point(0),
        StaticObject {
            id: StaticId::from_u32(1),
            kind: StaticKind::Dict,
            module: "app".into(),
            name: "Show$Point".into(),
            layout: LayoutRef::Dict {
                fns: 1,
                embedded: 0,
            },
            fields: Box::new([Const::Static(StaticId::from_u32(0)), Const::FnValue(g)]),
            strings: IndexVec::from_raw(Vec::new()).unwrap(),
        },
    ];
    let dict = p.item(
        30,
        "dict",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Static(StaticId::from_u32(1)))),
        )],
        &[],
    );
    let via_dict = call_41(&mut p, 31, Rvalue::Use(Operand::Const(Const::FnValue(g))));
    let via_field = p.item(
        32,
        "via_field",
        &[],
        &[Repr::Value, Repr::Value, Repr::Value, Repr::Value],
        straight(
            vec![
                assign(0, constant(Const::Static(StaticId::from_u32(1)))),
                assign(
                    1,
                    Rvalue::Field {
                        base: local(0),
                        layout: LayoutRef::Dict {
                            fns: 1,
                            embedded: 0,
                        },
                        field: 1,
                    },
                ),
                Stmt::Dup(local(1)),
                assign(
                    2,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(41)),
                        from: Repr::Int,
                    },
                ),
                assign(
                    3,
                    Rvalue::Call {
                        callee: Callee::Indirect(local(1)),
                        args: vec![op(2)],
                    },
                ),
            ],
            3,
        ),
        &[],
    );
    let empty = p.item(
        33,
        "empty",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::EmptyList)),
        )],
        &[],
    );
    let targets = [
        p.call(dict, Vec::new()),
        p.call(via_dict, Vec::new()),
        p.call(via_field, Vec::new()),
        p.call(empty, Vec::new()),
    ];
    let jit = p.compile();
    let dict = returned(run_const(&jit, targets[0]));
    assert_eq!(layout_of(dict), abi::layout::DICT);
    assert_eq!(rc_of(dict), abi::RC_STATIC);
    let desc = read_word(dict, 8);
    assert_eq!(layout_of(desc), abi::layout::DESC_NOMINAL);
    assert_eq!(rc_of(desc), abi::RC_STATIC);
    assert_eq!(text_of(read_word(desc, 8)), "Point");
    assert_eq!(text_of(read_word(desc, 16)), "geo");
    let fnval = read_word(dict, 16);
    assert_eq!(layout_of(fnval), abi::layout::CLOSURE);
    assert_eq!(rc_of(fnval), abi::RC_STATIC);
    assert_eq!(read_word(fnval, 8) & 0xf, 1, "the code word is addr | 1");
    assert_eq!(
        returned(run_const(&jit, targets[1])),
        abi::tag_int(42).unwrap()
    );
    assert_eq!(
        returned(run_const(&jit, targets[2])),
        abi::tag_int(42).unwrap()
    );
    let empty = returned(run_const(&jit, targets[3]));
    assert_eq!(rc_of(empty), abi::RC_STATIC);
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// A static of the same symbol in two batches, `cb$desc$geo$Point`: since
/// K4c every unit is compiled alone into a code record, and a record's
/// statics and shims are local copies, declared anonymously when it is
/// loaded (D-914, D-918), so the second batch has its own copy with the
/// first's contents, and its copy of `g`'s shim still calls `g` through
/// `g`'s slot (D-523's rule holds within one record).
#[test]
fn a_static_is_defined_once_across_batches() {
    let batch = |index: u32| {
        let mut p = Prog {
            thunk_base: index.checked_mul(100).unwrap(),
            ..Prog::default()
        };
        let mut via_fnval = None;
        if index == 0 {
            g_and_shim(&mut p);
        } else {
            let mut q = Prog::default();
            let g = g_and_shim(&mut q);
            let shim = q.functions.pop().unwrap();
            p.functions.push(shim);
            p.declare(g, "g", &[Repr::Int]);
            let call = call_41(&mut p, 40, constant(Const::FnValue(g)));
            via_fnval = Some(p.call(call, Vec::new()));
        }
        p.statics = vec![nominal_point(0)];
        let f = p.item(
            index,
            &format!("f{index}"),
            &[],
            &[],
            vec![block(
                Vec::new(),
                Terminator::Return(Operand::Const(Const::Static(StaticId::from_u32(0)))),
            )],
            &[],
        );
        let target = p.call(f, Vec::new());
        (p.units(), target, via_fnval)
    };
    let mut jit = Jit::new().unwrap();
    let (first, one, _) = batch(0);
    jit.compile(&first).unwrap();
    let (second, two, via_fnval) = batch(1);
    jit.compile(&second).unwrap();
    let a = returned(run_const(&jit, one));
    let b = returned(run_const(&jit, two));
    assert_ne!(a, b, "each record has its own copy (D-918)");
    for copy in [a, b] {
        assert_eq!(layout_of(copy), abi::layout::DESC_NOMINAL);
        assert_eq!(rc_of(copy), abi::RC_STATIC);
        assert_eq!(text_of(read_word(copy, 8)), "Point");
        assert_eq!(text_of(read_word(copy, 16)), "geo");
    }
    assert_eq!(
        returned(run_const(&jit, via_fnval.unwrap())),
        abi::tag_int(42).unwrap()
    );
}

/// The type `app::Wide` with one constructor `W`: `values` value fields,
/// then one raw `Int` field (plan §5.2).
fn wide_layout(values: u32) -> LayoutDesc {
    let wide = def(1, 950);
    let w = def(1, 951);
    let mut fields: Vec<FieldDesc> = (0..values)
        .map(|n| {
            field(
                None,
                Repr::Value,
                i32::try_from(n.checked_add(1).unwrap().checked_mul(8).unwrap()).unwrap(),
            )
        })
        .collect();
    fields.push(field(
        None,
        Repr::Int,
        i32::try_from(values.checked_add(1).unwrap().checked_mul(8).unwrap()).unwrap(),
    ));
    LayoutDesc {
        layout: LayoutRef::Ctor(w),
        name: "app::W".into(),
        tag: 0,
        only_ctor: true,
        fields,
        value_fields: values,
        raw_fields: 1,
        type_def: wide,
        type_name: "Wide".into(),
        ctor_name: "W".into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key("app", "Wide"),
    }
}

/// A closure body that returns its capture `capture`, for a closure of
/// `captures` captures: `clo(self, x) { c = field self capture; dup c;
/// drop self; drop x; return c }`.
fn returns_capture(owner: DefId, captures: u32, capture: u32) -> Function {
    function(
        FnKey::Closure { owner, index: 0 },
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Value],
        Repr::Value,
        straight(
            vec![
                assign(
                    2,
                    Rvalue::Field {
                        base: local(0),
                        layout: LayoutRef::Closure(captures),
                        field: capture,
                    },
                ),
                Stmt::Dup(local(2)),
                Stmt::Drop(local(0)),
                Stmt::Drop(local(1)),
            ],
            2,
        ),
        &[],
    )
}

/// D-279: an object with more pointer fields than the scan count holds has
/// the escape value. A user constructor keeps its fields where its layout
/// puts them, since freeing reads them from the layout table. An escaped
/// closure, dictionary or descriptor keeps its first field at offset 8 and
/// holds the number of its other fields as a tagged integer at offset 16,
/// then those fields (D-555). Each is allocated, read back, called where
/// it is a closure, printed or freed with the domain balanced; static
/// dictionaries and descriptors use the same shape.
#[test]
#[expect(clippy::too_many_lines, reason = "one hand-built MIR program")]
fn wide_objects_escape_the_scan_count() {
    let mut p = Prog::default();
    let wide = wide_layout(255);
    let w = def(1, 951);
    p.layouts = vec![wide];
    p.names.push(name(def(1, 950), "app", "Wide"));
    p.names.push(name(w, "app", "W"));
    let mut w_fields = nils(254);
    w_fields.push(the_field());
    w_fields.push(Operand::Const(Const::Int(7)));
    let mut reads = vec![(
        alloc_and_read(&mut p, 0, LayoutRef::Ctor(w), w_fields.clone(), 254),
        None,
    )];
    let with_last = |n: usize| {
        let mut fields = nils(n.checked_sub(1).unwrap());
        fields.push(the_field());
        fields
    };
    let cases = [
        (
            LayoutRef::Dict {
                fns: 300,
                embedded: 2,
            },
            with_last(303),
            302,
        ),
        (LayoutRef::Desc { args: 260 }, with_last(261), 260),
    ];
    for (index, (layout, fields, k)) in (1_u32..).zip(cases) {
        let f = alloc_and_read(&mut p, index, layout, fields, k);
        reads.push((f, builtin_layout_id(&layout)));
    }
    let raw = p.item(
        3,
        "raw",
        &[],
        &[Repr::Value, Repr::Int, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Alloc {
                        layout: LayoutRef::Ctor(w),
                        fields: w_fields.clone(),
                    },
                ),
                assign(
                    1,
                    Rvalue::Field {
                        base: local(0),
                        layout: LayoutRef::Ctor(w),
                        field: 255,
                    },
                ),
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Int,
                    },
                ),
            ],
            2,
        ),
        &["the field"],
    );
    let whole_w = p.item(
        4,
        "whole_w",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Alloc {
                    layout: LayoutRef::Ctor(w),
                    fields: w_fields,
                },
            )],
            0,
        ),
        &["the field"],
    );
    // Closures with 254 and 300 captures (255 and 301 uniform fields),
    // called indirectly, so the code word stays at offset 8.
    let mut calls = Vec::new();
    for (index, captures) in [(5_u32, 254_u32), (6, 300)] {
        // The closure belongs to the item that builds it.
        let owner = def(1, index);
        p.functions.push(returns_capture(owner, captures, captures));
        let mut fields = vec![Operand::Const(Const::Code(FnKey::Closure {
            owner,
            index: 0,
        }))];
        fields.extend(with_last(usize::try_from(captures).unwrap()));
        let f = p.item(
            index,
            &format!("call{index}"),
            &[],
            &[Repr::Value, Repr::Value],
            straight(
                vec![
                    assign(
                        0,
                        Rvalue::Alloc {
                            layout: LayoutRef::Closure(captures),
                            fields,
                        },
                    ),
                    assign(
                        1,
                        Rvalue::Call {
                            callee: Callee::Indirect(local(0)),
                            args: vec![Operand::Const(Const::Nil)],
                        },
                    ),
                ],
                1,
            ),
            &["the field"],
        );
        calls.push(p.call(f, Vec::new()));
    }
    // A static dictionary with 300 functions and a static descriptor with
    // 260 arguments; field 300 of the dictionary is called.
    let g = g_and_shim(&mut p);
    let mut dict_fields = vec![Const::Static(StaticId::from_u32(0))];
    dict_fields.extend(std::iter::repeat_n(Const::FnValue(g), 300));
    let mut desc_fields = vec![Const::Static(StaticId::from_u32(0))];
    desc_fields.extend(std::iter::repeat_n(
        Const::Static(StaticId::from_u32(0)),
        260,
    ));
    let dict_layout = LayoutRef::Dict {
        fns: 300,
        embedded: 0,
    };
    p.statics = vec![
        nominal_point(0),
        StaticObject {
            id: StaticId::from_u32(1),
            kind: StaticKind::Dict,
            module: "app".into(),
            name: "Wide$Point".into(),
            layout: dict_layout,
            fields: dict_fields.into_boxed_slice(),
            strings: IndexVec::from_raw(Vec::new()).unwrap(),
        },
        StaticObject {
            id: StaticId::from_u32(2),
            kind: StaticKind::Desc,
            module: "app".into(),
            name: "Wide".into(),
            layout: LayoutRef::Desc { args: 260 },
            fields: desc_fields.into_boxed_slice(),
            strings: IndexVec::from_raw(Vec::new()).unwrap(),
        },
    ];
    let via_static = p.item(
        7,
        "via_static",
        &[],
        &[Repr::Value, Repr::Value, Repr::Value, Repr::Value],
        straight(
            vec![
                assign(0, constant(Const::Static(StaticId::from_u32(1)))),
                assign(
                    1,
                    Rvalue::Field {
                        base: local(0),
                        layout: dict_layout,
                        field: 300,
                    },
                ),
                Stmt::Dup(local(1)),
                assign(
                    2,
                    Rvalue::Box {
                        value: Operand::Const(Const::Int(41)),
                        from: Repr::Int,
                    },
                ),
                assign(
                    3,
                    Rvalue::Call {
                        callee: Callee::Indirect(local(1)),
                        args: vec![op(2)],
                    },
                ),
            ],
            3,
        ),
        &[],
    );
    let static_desc = p.item(
        8,
        "static_desc",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Static(StaticId::from_u32(2)))),
        )],
        &[],
    );
    let reads: Vec<_> = reads
        .into_iter()
        .map(|(f, id)| (p.call(f, Vec::new()), id))
        .collect();
    let raw = p.call(raw, Vec::new());
    let whole_w = p.call(whole_w, Vec::new());
    let via_static = p.call(via_static, Vec::new());
    let static_desc = p.call(static_desc, Vec::new());
    let jit = p.compile();
    for (target, _) in reads {
        assert_eq!(text_of(returned(run_const(&jit, target))), "the field");
    }
    assert_eq!(int_of(returned(run_const(&jit, raw))), 7);
    let object = returned(run_const(&jit, whole_w));
    assert_eq!(abi::header_scan(read_word(object, 0)), abi::SCAN_ESCAPE);
    let text = cheby_runtime::alloc::with_domain(jit.domain(), || {
        // SAFETY: a live value the entry returned.
        unsafe { cheby_runtime::generic::debug_string(object) }
    });
    assert!(text.starts_with("W(Nil, Nil, "), "{text}");
    assert!(text.ends_with(", Nil, \"the field\", 7)"), "{text}");
    release(&jit, object);
    for target in calls {
        assert_eq!(text_of(returned(run_const(&jit, target))), "the field");
    }
    assert_eq!(
        returned(run_const(&jit, via_static)),
        abi::tag_int(42).unwrap()
    );
    let desc = returned(run_const(&jit, static_desc));
    assert_eq!(rc_of(desc), abi::RC_STATIC);
    assert_eq!(abi::header_scan(read_word(desc, 0)), abi::SCAN_ESCAPE);
    assert_eq!(read_word(desc, 16), abi::tag_int(260).unwrap());
    assert_eq!(layout_of(read_word(desc, 8)), abi::layout::DESC_NOMINAL);
    assert_eq!(
        layout_of(read_word(desc, 24 + 259 * 8)),
        abi::layout::DESC_NOMINAL
    );
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}
