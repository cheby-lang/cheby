//! The MIR forms the other test files do not reach: `Goto` with block
//! arguments (a loop and a join), `Switch` with an `Unreachable` default,
//! calls and tail calls by `Callee::Code`, `ReadLayout`, and the import of
//! a builtin descriptor (`Const::BuiltinDesc`). Each program is also
//! checked by the MIR validator, so the hand-built MIR is well formed.

use cheby_mir::validate::{Stage, validate};
use cheby_mir::{BuiltinDesc, LayoutRef};

use super::*;

/// Checks both units of `p` with the validator, as counted MIR.
fn validated(p: Prog) -> [MirModule; 2] {
    let units = p.units();
    for unit in &units {
        if let Err(errors) = validate(unit, Stage::Counted) {
            panic!("invalid MIR:\n{}", errors.join("\n"));
        }
    }
    units
}

fn compile(units: &[MirModule]) -> Jit {
    let mut jit = Jit::new().unwrap();
    jit.compile(units).unwrap();
    jit
}

/// `sum(n: Int)`: a loop through `Goto` back edges whose block parameters
/// carry the counter and the total, so values cross a back edge.
///
/// ```text
/// bb0:           goto bb1(n, 0)
/// bb1(i, acc):   done = i == 0; branch done bb2 bb3
/// bb2:           r = box acc; return r
/// bb3:           acc2 = acc + i; i2 = i - 1; goto bb1(i2, acc2)
/// ```
fn sum_item(p: &mut Prog) -> DefId {
    let ints = Operand::Const;
    p.item(
        0,
        "sum",
        &[Repr::Int],
        // 1 i, 2 acc, 3 done, 4 r, 5 acc2, 6 i2
        &[
            Repr::Int,
            Repr::Int,
            Repr::Bool,
            Repr::Value,
            Repr::Int,
            Repr::Int,
        ],
        vec![
            block(
                Vec::new(),
                Terminator::Goto {
                    target: bb(1),
                    args: vec![op(0), ints(Const::Int(0))],
                },
            ),
            Block {
                pos: Vec::new(),
                params: vec![local(1), local(2)],
                stmts: vec![assign(
                    3,
                    Rvalue::Compare {
                        op: CmpOp::Eq,
                        lhs: op(1),
                        rhs: ints(Const::Int(0)),
                        repr: Repr::Int,
                    },
                )],
                term: Terminator::Branch {
                    cond: op(3),
                    then_: bb(2),
                    else_: bb(3),
                },
                unwind: Vec::new(),
            },
            block(
                vec![assign(
                    4,
                    Rvalue::Box {
                        value: op(2),
                        from: Repr::Int,
                    },
                )],
                ret(4),
            ),
            block(
                vec![
                    assign(
                        5,
                        Rvalue::Checked {
                            op: ArithOp::Add,
                            lhs: op(2),
                            rhs: op(1),
                            repr: Repr::Int,
                            loc: StrId::from_u32(1),
                        },
                    ),
                    assign(
                        6,
                        Rvalue::Checked {
                            op: ArithOp::Sub,
                            lhs: op(1),
                            rhs: ints(Const::Int(1)),
                            repr: Repr::Int,
                            loc: StrId::from_u32(1),
                        },
                    ),
                ],
                Terminator::Goto {
                    target: bb(1),
                    args: vec![op(6), op(5)],
                },
            ),
        ],
        &[LOC],
    )
}

/// `name(n: Int)`: a `Switch` on 0 and 1 whose default block is
/// `Unreachable`; both cases join in a block that takes the chosen string
/// as a `Value` parameter.
fn name_item(p: &mut Prog) -> DefId {
    p.item(
        1,
        "name",
        &[Repr::Int],
        &[Repr::Value],
        vec![
            block(
                Vec::new(),
                Terminator::Switch {
                    on: op(0),
                    cases: vec![(0, bb(1)), (1, bb(2))],
                    default: Some(bb(4)),
                },
            ),
            block(
                Vec::new(),
                Terminator::Goto {
                    target: bb(3),
                    args: vec![Operand::Const(Const::Str(StrId::from_u32(1)))],
                },
            ),
            block(
                Vec::new(),
                Terminator::Goto {
                    target: bb(3),
                    args: vec![Operand::Const(Const::Str(StrId::from_u32(2)))],
                },
            ),
            Block {
                pos: Vec::new(),
                params: vec![local(1)],
                stmts: Vec::new(),
                term: ret(1),
                unwind: Vec::new(),
            },
            block(Vec::new(), Terminator::Unreachable),
        ],
        &["zero", "one"],
    )
}

/// `Goto` passes block arguments, on a loop's back edge and into a join;
/// a `Switch` default that is `Unreachable` compiles to a trap.
#[test]
fn gotos_pass_block_arguments_and_unreachable_traps() {
    let mut p = Prog::default();
    let sum = sum_item(&mut p);
    let name = name_item(&mut p);
    let sum_100 = p.call(sum, vec![Operand::Const(Const::Int(100))]);
    let sum_0 = p.call(sum, vec![Operand::Const(Const::Int(0))]);
    let zero = p.call(name, vec![Operand::Const(Const::Int(0))]);
    let one = p.call(name, vec![Operand::Const(Const::Int(1))]);
    let units = validated(p);
    let mut jit = Jit::with_options(CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    })
    .unwrap();
    jit.compile(&units).unwrap();
    assert_eq!(int_of(returned(run_const(&jit, sum_100))), 5050);
    assert_eq!(int_of(returned(run_const(&jit, sum_0))), 0);
    assert_eq!(text_of(returned(run_const(&jit, zero))), "zero");
    assert_eq!(text_of(returned(run_const(&jit, one))), "one");
    let clif = jit.take_clif().join("\n");
    let name_clif = clif
        .split("; cb$")
        .find(|f| f.starts_with("fn$app$name"))
        .unwrap();
    assert!(name_clif.contains("trap"), "{name_clif}");
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `clo(self, x) { c = field self 1; dup c; drop self; drop x; return c }`.
fn capture_reader(key: FnKey) -> Function {
    function(
        key,
        Abi::Cheby,
        &[Repr::Value, Repr::Value],
        &[Repr::Value],
        Repr::Value,
        straight(
            vec![
                // The unused parameter goes first: no drop moves past a
                // later instruction (D-851).
                Stmt::Drop(local(1)),
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
            ],
            2,
        ),
        &[],
    )
}

/// `k = alloc closure(code, "captured")`, the first statement of the
/// callers below.
fn make_closure(key: FnKey) -> Stmt {
    assign(
        0,
        Rvalue::Alloc {
            layout: LayoutRef::Closure(1),
            fields: vec![
                Operand::Const(Const::Code(key)),
                Operand::Const(Const::Str(StrId::from_u32(1))),
            ],
        },
    )
}

/// `Callee::Code` calls a function of the unit by key, not through a slot
/// or a closure's code word: here a closure body, with the closure passed
/// as `self` explicitly, in an ordinary call and in a tail call.
#[test]
fn code_callees_call_a_function_by_key() {
    let mut p = Prog::default();
    let key = FnKey::Closure {
        owner: def(1, 0),
        index: 0,
    };
    p.functions.push(capture_reader(key));
    // `call() { k = …; r = call code(key)(k, nil); return r }`.
    let call = p.item(
        0,
        "call",
        &[],
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                make_closure(key),
                assign(
                    1,
                    Rvalue::Call {
                        callee: Callee::Code(key),
                        args: vec![op(0), Operand::Const(Const::Nil)],
                    },
                ),
            ],
            1,
        ),
        &["captured"],
    );
    // `tail() { k = …; tail code(key)(k, nil) }`.
    let tail = p.item(
        1,
        "tail",
        &[],
        &[Repr::Value],
        vec![block(
            vec![make_closure(key)],
            Terminator::TailCall {
                callee: Callee::Code(key),
                args: vec![op(0), Operand::Const(Const::Nil)],
            },
        )],
        &["captured"],
    );
    let by_call = p.call(call, Vec::new());
    let by_tail = p.call(tail, Vec::new());
    let jit = compile(&validated(p));
    assert_eq!(text_of(returned(run_const(&jit, by_call))), "captured");
    assert_eq!(text_of(returned(run_const(&jit, by_tail))), "captured");
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `ReadLayout` gives the layout id in a pointer's header, as `U32`: here
/// of a static string and of a closure the function allocates.
#[test]
fn read_layout_gives_the_header_id() {
    let mut p = Prog::default();
    let key = FnKey::Closure {
        owner: def(1, 0),
        index: 0,
    };
    p.functions.push(capture_reader(key));
    // `of_string() { s = "text"; id = read_layout s; r = box id; return r }`.
    let of_string = p.item(
        0,
        "of_string",
        &[],
        &[Repr::Value, Repr::U32, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::Use(Operand::Const(Const::Str(StrId::from_u32(1)))),
                ),
                assign(1, Rvalue::ReadLayout { base: local(0) }),
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::U32,
                    },
                ),
            ],
            2,
        ),
        &["text"],
    );
    // `of_closure() { k = …; id = read_layout k; drop k; r = box id; return r }`.
    let of_closure = p.item(
        1,
        "of_closure",
        &[],
        &[Repr::Value, Repr::U32, Repr::Value],
        straight(
            vec![
                make_closure(key),
                assign(1, Rvalue::ReadLayout { base: local(0) }),
                Stmt::Drop(local(0)),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::U32,
                    },
                ),
            ],
            2,
        ),
        &["captured"],
    );
    let string = p.call(of_string, Vec::new());
    let closure = p.call(of_closure, Vec::new());
    let jit = compile(&validated(p));
    assert_eq!(
        int_of(returned(run_const(&jit, string))),
        i128::from(abi::layout::STRING)
    );
    assert_eq!(
        int_of(returned(run_const(&jit, closure))),
        i128::from(abi::layout::CLOSURE)
    );
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// `Const::BuiltinDesc` imports the runtime's `cheby_rt_desc_<name>`
/// object (D-523): running `desc` returns the address of
/// `cheby_rt_desc_int`, a static nominal descriptor that prints as `Int`.
#[test]
fn builtin_descriptors_are_runtime_objects() {
    let mut p = Prog::default();
    // `desc() { return builtin_desc int }`.
    let desc = p.item(
        0,
        "desc",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::BuiltinDesc(BuiltinDesc::Int))),
        )],
        &[],
    );
    let entry = p.call(desc, Vec::new());
    let units = validated(p);
    let symbol = format!("{}{}", abi::sym::DESC_PREFIX, BuiltinDesc::Int.name());
    assert_eq!(
        abi::sym::desc_symbol(BuiltinDesc::Int.name()),
        Some(&*symbol)
    );
    assert!(
        cheby_runtime::symbols()
            .iter()
            .any(|(name, _)| *name == symbol),
        "the runtime lacks `{symbol}`"
    );
    let jit = compile(&units);
    // The generated code returns the address of the runtime's static.
    let word = returned(run_const(&jit, entry));
    assert_eq!(Some(word), cheby_runtime::desc::builtin("int"));
    // SAFETY: `word` is the static nominal descriptor of `Int`.
    assert_eq!(unsafe { cheby_runtime::generic::debug_string(word) }, "Int");
}
