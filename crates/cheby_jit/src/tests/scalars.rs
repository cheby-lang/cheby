//! Scalar operations on hand-built MIR: checked arithmetic (§3.3, §5.4.6),
//! comparisons (§3.3.2, D-527), float arithmetic, bitwise operators,
//! `!`, float negation, and boxing at the boundaries of D-521.

use super::*;

const INTEGERS: [Repr; 9] = [
    Repr::Int,
    Repr::I8,
    Repr::I16,
    Repr::I32,
    Repr::I64,
    Repr::U8,
    Repr::U16,
    Repr::U32,
    Repr::U64,
];

/// The minimum, maximum and width of an integer representation.
fn range(repr: Repr) -> (i128, i128, i128) {
    match repr {
        Repr::Int | Repr::I64 => (i64::MIN.into(), i64::MAX.into(), 64),
        Repr::I8 => (i8::MIN.into(), i8::MAX.into(), 8),
        Repr::I16 => (i16::MIN.into(), i16::MAX.into(), 16),
        Repr::I32 => (i32::MIN.into(), i32::MAX.into(), 32),
        Repr::U8 => (0, u8::MAX.into(), 8),
        Repr::U16 => (0, u16::MAX.into(), 16),
        Repr::U32 => (0, u32::MAX.into(), 32),
        Repr::U64 => (0, u64::MAX.into(), 64),
        other => panic!("{other} is not an integer"),
    }
}

/// A constant of the integer `repr` with the value `v`.
fn int_const(repr: Repr, v: i128) -> Operand {
    if repr == Repr::Int {
        return Operand::Const(Const::Int(i64::try_from(v).unwrap()));
    }
    let bits = if v < 0 {
        i64::try_from(v).unwrap().cast_unsigned()
    } else {
        u64::try_from(v).unwrap()
    };
    Operand::Const(Const::Sized { repr, bits })
}

/// What `a op b` gives on `repr` (§3.3, §5.4.6), computed in `i128`.
fn reference(op: ArithOp, a: i128, b: i128, repr: Repr) -> Result<i128, PanicKind> {
    let (min, max, width) = range(repr);
    let fits = |r: Option<i128>| {
        r.filter(|r| (min..=max).contains(r))
            .ok_or(PanicKind::Overflow)
    };
    match op {
        ArithOp::Add => fits(a.checked_add(b)),
        ArithOp::Sub => fits(a.checked_sub(b)),
        ArithOp::Mul => fits(a.checked_mul(b)),
        ArithOp::Div | ArithOp::Rem if b == 0 => Err(PanicKind::DivisionByZero),
        ArithOp::Div => fits(a.checked_div(b)),
        ArithOp::Rem => fits(a.checked_rem(b)),
        ArithOp::Shl | ArithOp::Shr if b < 0 || b >= width => Err(PanicKind::ShiftOutOfRange),
        ArithOp::Shl => {
            let n = u32::try_from(b).unwrap();
            fits(a.checked_mul(1_i128.checked_shl(n).unwrap()))
        }
        ArithOp::Shr => {
            let n = u32::try_from(b).unwrap();
            fits(Some(a.checked_shr(n).unwrap()))
        }
    }
}

/// The operand pairs each operation is run with on `repr`: the edges of
/// the range, overflow in both directions, zero divisors, `MIN / -1`,
/// `MIN % -1`, and shift counts at and beyond the width.
fn cases(op: ArithOp, repr: Repr) -> Vec<(i128, i128)> {
    let (min, max, w) = range(repr);
    let signed = min < 0;
    // The largest power of two of the type: 2^(w-2) signed, 2^(w-1)
    // unsigned; doubling it overflows.
    let top = if signed {
        w.checked_sub(2)
    } else {
        w.checked_sub(1)
    };
    let half = 1_i128
        .checked_shl(u32::try_from(top.unwrap()).unwrap())
        .unwrap();
    let less = |v: i128, d: i128| v.checked_sub(d).unwrap();
    let mut out = match op {
        ArithOp::Add => vec![
            (max, 1),
            (less(max, 1), 1),
            (min, 0),
            (1, 2),
            (0, 0),
            (max, 0),
        ],
        ArithOp::Sub => vec![(min, 1), (5, 3), (max, max), (max, 0)],
        ArithOp::Mul => vec![(max, 2), (max, 1), (3, 7), (0, max), (half, 2)],
        ArithOp::Div => vec![(7, 0), (max, max), (max, 1), (7, 2), (min, 1)],
        ArithOp::Rem => vec![(7, 0), (max, 7), (7, 2), (max, max)],
        ArithOp::Shl => vec![
            (1, w),
            (1, less(w, 1)),
            (3, 2),
            (max, 1),
            (0, less(w, 1)),
            (half, 1),
        ],
        ArithOp::Shr => vec![(1, w), (max, 1), (max, less(w, 1)), (1, 0)],
    };
    if signed {
        out.extend(match op {
            ArithOp::Add => vec![(min, -1), (-3, 1), (max, min)],
            ArithOp::Sub => vec![(max, -1), (-1, max), (0, max), (min, -1)],
            ArithOp::Mul => vec![(min, -1), (-1, -1), (min, 1), (-3, 7), (max, -1)],
            ArithOp::Div => vec![(min, -1), (-7, 2), (7, -2), (-7, -2), (max, -1)],
            ArithOp::Rem => vec![(min, -1), (-7, 2), (7, -2), (-1, -1), (min, 7)],
            ArithOp::Shl => vec![(1, -1), (-1, less(w, 1)), (-1, 1), (min, 1), (-3, 2)],
            ArithOp::Shr => vec![(min, less(w, 1)), (-8, 1), (1, -1), (min, 1)],
        });
    } else {
        out.extend(match op {
            ArithOp::Sub => vec![(0, 1), (max, max)],
            ArithOp::Shl => vec![(128, 1), (max, less(w, 1))],
            _ => Vec::new(),
        });
    }
    out
}

/// `f(a: repr, b: repr) { r = checked op a b at LOC; return box r }`.
fn checked_fn(p: &mut Prog, index: u32, arith: ArithOp, repr: Repr) -> DefId {
    p.item(
        index,
        &format!("{}_{}", arith.name(), repr.name()),
        &[repr, repr],
        &[repr, Repr::Value],
        straight(
            vec![
                assign(
                    2,
                    Rvalue::Checked {
                        op: arith,
                        lhs: op(0),
                        rhs: op(1),
                        repr,
                        loc: StrId::from_u32(1),
                    },
                ),
                assign(
                    3,
                    Rvalue::Box {
                        value: op(2),
                        from: repr,
                    },
                ),
            ],
            3,
        ),
        &[LOC],
    )
}

/// Every checked operation on `Int` and each sized integer, run as entries
/// against the `i128` reference: results, `Overflow` (`<<` included),
/// `DivisionByZero`, `ShiftOutOfRange`, `MIN / -1` panicking and
/// `MIN % -1` giving 0, each panic with its kind and location.
#[test]
fn checked_arithmetic_matches_the_reference() {
    let ops = [
        ArithOp::Add,
        ArithOp::Sub,
        ArithOp::Mul,
        ArithOp::Div,
        ArithOp::Rem,
        ArithOp::Shl,
        ArithOp::Shr,
    ];
    for repr in INTEGERS {
        let mut p = Prog::default();
        let mut runs = Vec::new();
        for (index, op) in (0_u32..).zip(ops) {
            let f = checked_fn(&mut p, index, op, repr);
            for (a, b) in cases(op, repr) {
                let target = p.call(f, vec![int_const(repr, a), int_const(repr, b)]);
                runs.push((op, a, b, target));
            }
        }
        let jit = p.compile();
        for (op, a, b, target) in runs {
            let what = format!("{a} {} {b} on {repr}", op.name());
            match (reference(op, a, b, repr), run_const(&jit, target)) {
                (Ok(expected), EntryOutcome::Returned(word)) => {
                    assert_eq!(int_of(word), expected, "{what}");
                    release(&jit, word);
                }
                (Err(kind), EntryOutcome::Panicked(report)) => {
                    assert_eq!(report.kind.code(), kind.code(), "{what}");
                    assert_eq!(report.location, LOC, "{what}");
                }
                (expected, outcome) => panic!("{what}: expected {expected:?}, got {outcome:?}"),
            }
        }
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(jit.domain().live_objects(), 0, "{repr}");
        }
    }
}

/// The cases the plan names, spelled out: `1 << 63` on `Int`, `1 << 7` on
/// `I8` and `0x80 << 1` on `U8` overflow; `MIN % -1` is 0 and `MIN / -1`
/// overflows on every signed type.
#[test]
fn named_edge_cases() {
    assert_eq!(
        reference(ArithOp::Shl, 1, 63, Repr::Int),
        Err(PanicKind::Overflow)
    );
    assert_eq!(
        reference(ArithOp::Shl, 1, 7, Repr::I8),
        Err(PanicKind::Overflow)
    );
    assert_eq!(
        reference(ArithOp::Shl, 0x80, 1, Repr::U8),
        Err(PanicKind::Overflow)
    );
    for repr in [Repr::Int, Repr::I8, Repr::I16, Repr::I32, Repr::I64] {
        let (min, _, w) = range(repr);
        assert!(cases(ArithOp::Rem, repr).contains(&(min, -1)));
        assert!(cases(ArithOp::Div, repr).contains(&(min, -1)));
        assert!(cases(ArithOp::Shl, repr).contains(&(1, w.checked_sub(1).unwrap())));
        assert_eq!(reference(ArithOp::Rem, min, -1, repr), Ok(0));
        assert_eq!(
            reference(ArithOp::Div, min, -1, repr),
            Err(PanicKind::Overflow)
        );
    }
    assert!(cases(ArithOp::Shl, Repr::U8).contains(&(0x80, 1)));
}

/// `f(a, b) { c = compare op a b; return box c }` for `repr`.
fn compare_fn(p: &mut Prog, index: u32, cmp: CmpOp, repr: Repr) -> DefId {
    p.item(
        index,
        &format!("{}_{}", cmp.name(), repr.name()),
        &[repr, repr],
        &[Repr::Bool, Repr::Value],
        straight(
            vec![
                assign(
                    2,
                    Rvalue::Compare {
                        op: cmp,
                        lhs: op(0),
                        rhs: op(1),
                        repr,
                    },
                ),
                assign(
                    3,
                    Rvalue::Box {
                        value: op(2),
                        from: Repr::Bool,
                    },
                ),
            ],
            3,
        ),
        &[],
    )
}

fn float(repr: Repr, v: f64) -> Operand {
    if repr == Repr::Float {
        Operand::Const(Const::Float(v.to_bits()))
    } else {
        Operand::Const(Const::Sized {
            repr: Repr::F32,
            bits: bits_of(Repr::F32, v),
        })
    }
}

fn truth(b: bool) -> u64 {
    static_word(if b {
        abi::layout::TRUE
    } else {
        abi::layout::FALSE
    })
}

/// Comparisons: signed and unsigned integers, `Bool`, and floats with
/// §3.3.2's `==` (NaN equals NaN, `-0.0 == 0.0`) and `compare`'s order for
/// the others (NaN above every value, D-527). Results are the static
/// `True`/`False` after boxing.
#[test]
fn comparisons_follow_the_spec() {
    let nan = f64::NAN;
    let inf = f64::INFINITY;
    let ops = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ];
    // (a, b, [eq, ne, lt, le, gt, ge])
    let float_cases: [(f64, f64, [bool; 6]); 9] = [
        (nan, nan, [true, false, false, true, false, true]),
        (-0.0, 0.0, [true, false, false, true, false, true]),
        (1.0, nan, [false, true, true, true, false, false]),
        (nan, 1.0, [false, true, false, false, true, true]),
        (nan, inf, [false, true, false, false, true, true]),
        (-inf, 1.0, [false, true, true, true, false, false]),
        (2.5, 2.5, [true, false, false, true, false, true]),
        (3.0, 2.0, [false, true, false, false, true, true]),
        (-nan, nan, [true, false, false, true, false, true]),
    ];
    let int_cases: [(Repr, i128, i128, [bool; 6]); 5] = [
        (Repr::U8, 255, 1, [false, true, false, false, true, true]),
        (Repr::I8, -1, 1, [false, true, true, true, false, false]),
        (
            Repr::Int,
            i128::from(i64::MIN),
            0,
            [false, true, true, true, false, false],
        ),
        (
            Repr::U64,
            i128::from(u64::MAX),
            0,
            [false, true, false, false, true, true],
        ),
        (Repr::I32, 7, 7, [true, false, false, true, false, true]),
    ];
    let mut p = Prog::default();
    let mut runs = Vec::new();
    let mut index = 0_u32;
    let mut next = || {
        index = index.checked_add(1).unwrap();
        index
    };
    for repr in [Repr::Float, Repr::F32] {
        for (k, cmp) in ops.into_iter().enumerate() {
            let f = compare_fn(&mut p, next(), cmp, repr);
            for (a, b, expected) in float_cases {
                let target = p.call(f, vec![float(repr, a), float(repr, b)]);
                runs.push((
                    format!("{a} {} {b} on {repr}", cmp.name()),
                    target,
                    expected[k],
                ));
            }
        }
    }
    for (repr, a, b, expected) in int_cases {
        for (k, cmp) in ops.into_iter().enumerate() {
            let f = compare_fn(&mut p, next(), cmp, repr);
            let target = p.call(f, vec![int_const(repr, a), int_const(repr, b)]);
            runs.push((
                format!("{a} {} {b} on {repr}", cmp.name()),
                target,
                expected[k],
            ));
        }
    }
    for cmp in [CmpOp::Eq, CmpOp::Ne] {
        let f = compare_fn(&mut p, next(), cmp, Repr::Bool);
        for (a, b) in [(true, true), (true, false), (false, false)] {
            let target = p.call(
                f,
                vec![
                    Operand::Const(Const::Bool(a)),
                    Operand::Const(Const::Bool(b)),
                ],
            );
            runs.push((
                format!("{a} {} {b}", cmp.name()),
                target,
                (a == b) == (cmp == CmpOp::Eq),
            ));
        }
    }
    let jit = p.compile();
    for (what, target, expected) in runs {
        assert_eq!(returned(run_const(&jit, target)), truth(expected), "{what}");
    }
}

/// `f(a, b) { r = float_arith op a b; return box r }`; the boxed float's
/// bits.
fn float_bits(jit: &Jit, target: DefId, repr: Repr) -> u64 {
    let word = returned(run_const(jit, target));
    let expected = if repr == Repr::Float {
        abi::layout::BOXED_FLOAT
    } else {
        abi::layout::BOXED_F32
    };
    assert_eq!(layout_of(word), expected);
    let bits = read_word(word, 8);
    release(jit, word);
    if repr == Repr::F32 {
        bits & u64::from(u32::MAX)
    } else {
        bits
    }
}

fn bits_of(repr: Repr, v: f64) -> u64 {
    if repr == Repr::Float {
        v.to_bits()
    } else {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "F32 test values are written as f64 literals"
        )]
        let narrow = v as f32;
        u64::from(narrow.to_bits())
    }
}

/// IEEE arithmetic on `Float` and `F32`, `%` with the dividend's sign
/// (D-527), and unary `-` turning `0.0` into `-0.0`.
#[test]
fn float_arithmetic_and_negation() {
    let cases = [
        (ArithOp::Add, 1.5, 2.25, 3.75),
        (ArithOp::Sub, 1.5, 2.25, -0.75),
        (ArithOp::Mul, 1.5, -2.0, -3.0),
        (ArithOp::Div, 1.0, 0.0, f64::INFINITY),
        (ArithOp::Rem, 5.5, 2.0, 1.5),
        (ArithOp::Rem, -5.5, 2.0, -1.5),
        (ArithOp::Rem, 5.5, -2.0, 1.5),
        (ArithOp::Rem, -0.0, 3.0, -0.0),
    ];
    for repr in [Repr::Float, Repr::F32] {
        let mut p = Prog::default();
        let mut runs = Vec::new();
        for (index, (arith, a, b, expected)) in (0_u32..).zip(cases) {
            let f = p.item(
                index,
                &format!("f{index}"),
                &[repr, repr],
                &[repr, Repr::Value],
                straight(
                    vec![
                        assign(
                            2,
                            Rvalue::FloatArith {
                                op: arith,
                                lhs: op(0),
                                rhs: op(1),
                                repr,
                            },
                        ),
                        assign(
                            3,
                            Rvalue::Box {
                                value: op(2),
                                from: repr,
                            },
                        ),
                    ],
                    3,
                ),
                &[],
            );
            runs.push((
                p.call(f, vec![float(repr, a), float(repr, b)]),
                bits_of(repr, expected),
            ));
        }
        let neg = p.item(
            100,
            "neg",
            &[repr],
            &[repr, Repr::Value],
            straight(
                vec![
                    assign(1, Rvalue::FloatNeg { value: op(0), repr }),
                    assign(
                        2,
                        Rvalue::Box {
                            value: op(1),
                            from: repr,
                        },
                    ),
                ],
                2,
            ),
            &[],
        );
        runs.push((p.call(neg, vec![float(repr, 0.0)]), bits_of(repr, -0.0)));
        runs.push((p.call(neg, vec![float(repr, -2.5)]), bits_of(repr, 2.5)));
        let jit = p.compile();
        for (target, expected) in runs {
            assert_eq!(float_bits(&jit, target, repr), expected, "{repr}");
        }
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(jit.domain().live_objects(), 0);
        }
    }
}

/// `&`, `|`, `^` at each width, and `!` on `Bool`.
#[test]
fn bitwise_operators_and_not() {
    let cases: [(BitOp, Repr, i128, i128, i128); 6] = [
        (BitOp::And, Repr::I8, -1, 0x0f, 0x0f),
        (BitOp::Or, Repr::U16, 0xf000, 0x000f, 0xf00f),
        (BitOp::Xor, Repr::Int, -1, 0, -1),
        (
            BitOp::Xor,
            Repr::U64,
            i128::from(u64::MAX),
            1,
            i128::from(u64::MAX.checked_sub(1).unwrap()),
        ),
        (BitOp::And, Repr::I32, -8, 12, 8),
        (
            BitOp::Or,
            Repr::I64,
            i128::from(i64::MIN),
            1,
            i128::from(i64::MIN.checked_add(1).unwrap()),
        ),
    ];
    let mut p = Prog::default();
    let mut runs = Vec::new();
    for (index, (bit, repr, a, b, expected)) in (0_u32..).zip(cases) {
        let f = p.item(
            index,
            &format!("f{index}"),
            &[repr, repr],
            &[repr, Repr::Value],
            straight(
                vec![
                    assign(
                        2,
                        Rvalue::Bitwise {
                            op: bit,
                            lhs: op(0),
                            rhs: op(1),
                            repr,
                        },
                    ),
                    assign(
                        3,
                        Rvalue::Box {
                            value: op(2),
                            from: repr,
                        },
                    ),
                ],
                3,
            ),
            &[],
        );
        runs.push((
            p.call(f, vec![int_const(repr, a), int_const(repr, b)]),
            expected,
        ));
    }
    let not = p.item(
        100,
        "not",
        &[Repr::Bool],
        &[Repr::Bool, Repr::Value],
        straight(
            vec![
                assign(1, Rvalue::Not(op(0))),
                assign(
                    2,
                    Rvalue::Box {
                        value: op(1),
                        from: Repr::Bool,
                    },
                ),
            ],
            2,
        ),
        &[],
    );
    let not_true = p.call(not, vec![Operand::Const(Const::Bool(true))]);
    let not_false = p.call(not, vec![Operand::Const(Const::Bool(false))]);
    let jit = p.compile();
    for (target, expected) in runs {
        let word = returned(run_const(&jit, target));
        assert_eq!(int_of(word), expected);
        release(&jit, word);
    }
    assert_eq!(returned(run_const(&jit, not_true)), truth(false));
    assert_eq!(returned(run_const(&jit, not_false)), truth(true));
}

/// What boxing a value must give (D-521).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Tagged(i64),
    Boxed(u32),
}

/// Boxing at every boundary of D-521: tagged up to 63 bits (62 for `U64`)
/// and a boxed number beyond, sized integers always tagged with their
/// sign-extended value, floats always boxed, `Bool` as the static objects;
/// unboxing gives the value back, from a tagged and from a boxed word.
#[test]
#[expect(clippy::too_many_lines, reason = "one table of boundary cases")]
fn boxing_is_canonical_at_the_boundaries() {
    let int63_max = i128::from(abi::INT63_MAX);
    let int63_min = i128::from(abi::INT63_MIN);
    let above = int63_max.checked_add(1).unwrap();
    let below = int63_min.checked_sub(1).unwrap();
    let int_cases = [
        (Repr::Int, int63_max, Shape::Tagged(abi::INT63_MAX)),
        (Repr::Int, above, Shape::Boxed(abi::layout::BOXED_INT)),
        (Repr::Int, int63_min, Shape::Tagged(abi::INT63_MIN)),
        (Repr::Int, below, Shape::Boxed(abi::layout::BOXED_INT)),
        (
            Repr::I64,
            i128::from(i64::MAX),
            Shape::Boxed(abi::layout::BOXED_INT),
        ),
        (
            Repr::I64,
            i128::from(i64::MIN),
            Shape::Boxed(abi::layout::BOXED_INT),
        ),
        (Repr::I64, -1, Shape::Tagged(-1)),
        (Repr::U64, int63_max, Shape::Tagged(abi::INT63_MAX)),
        (Repr::U64, above, Shape::Boxed(abi::layout::BOXED_U64)),
        (
            Repr::U64,
            i128::from(u64::MAX),
            Shape::Boxed(abi::layout::BOXED_U64),
        ),
        (Repr::I8, -128, Shape::Tagged(-128)),
        (Repr::I8, 127, Shape::Tagged(127)),
        (Repr::U8, 255, Shape::Tagged(255)),
        (Repr::I16, -32768, Shape::Tagged(-32768)),
        (Repr::U16, 65535, Shape::Tagged(65535)),
        (
            Repr::I32,
            i128::from(i32::MIN),
            Shape::Tagged(i32::MIN.into()),
        ),
        (
            Repr::U32,
            i128::from(u32::MAX),
            Shape::Tagged(u32::MAX.into()),
        ),
    ];
    let mut p = Prog::default();
    let mut index = 0_u32;
    let mut runs = Vec::new();
    for (repr, v, shape) in int_cases {
        // `box(x)`, and `box(unbox(box(x)))` (the round trip).
        let boxed = p.item(
            index,
            &format!("box{index}"),
            &[repr],
            &[Repr::Value],
            straight(
                vec![assign(
                    1,
                    Rvalue::Box {
                        value: op(0),
                        from: repr,
                    },
                )],
                1,
            ),
            &[],
        );
        let round = p.item(
            index.checked_add(1).unwrap(),
            &format!("round{index}"),
            &[repr],
            &[Repr::Value, repr, Repr::Value],
            straight(
                vec![
                    assign(
                        1,
                        Rvalue::Box {
                            value: op(0),
                            from: repr,
                        },
                    ),
                    assign(
                        2,
                        Rvalue::Unbox {
                            value: op(1),
                            to: repr,
                        },
                    ),
                    Stmt::Drop(local(1)),
                    assign(
                        3,
                        Rvalue::Box {
                            value: op(2),
                            from: repr,
                        },
                    ),
                ],
                3,
            ),
            &[],
        );
        index = index.checked_add(2).unwrap();
        runs.push((
            repr,
            v,
            shape,
            p.call(boxed, vec![int_const(repr, v)]),
            p.call(round, vec![int_const(repr, v)]),
        ));
    }
    let jit = p.compile();
    for (repr, v, shape, boxed, round) in runs {
        let word = returned(run_const(&jit, boxed));
        match shape {
            Shape::Tagged(n) => assert_eq!(word, abi::tag_int(n).unwrap(), "{v} as {repr}"),
            Shape::Boxed(id) => assert_eq!(layout_of(word), id, "{v} as {repr}"),
        }
        assert_eq!(int_of(word), v, "{v} as {repr}");
        release(&jit, word);
        let word = returned(run_const(&jit, round));
        assert_eq!(int_of(word), v, "{v} as {repr} round trip");
        release(&jit, word);
    }
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// Floats and `F32` box to their boxed objects (NaN and `-0.0` keep their
/// bits) and unbox back; `Bool` boxes to the static `True`/`False` and
/// unboxes from them.
#[test]
fn floats_and_bools_box_and_unbox() {
    let mut p = Prog::default();
    let mut runs = Vec::new();
    let mut index = 0_u32;
    for repr in [Repr::Float, Repr::F32] {
        for v in [f64::NAN, -0.0, 1.5, f64::INFINITY] {
            let f = p.item(
                index,
                &format!("f{index}"),
                &[repr],
                &[Repr::Value, repr, Repr::Value],
                straight(
                    vec![
                        assign(
                            1,
                            Rvalue::Box {
                                value: op(0),
                                from: repr,
                            },
                        ),
                        assign(
                            2,
                            Rvalue::Unbox {
                                value: op(1),
                                to: repr,
                            },
                        ),
                        Stmt::Drop(local(1)),
                        assign(
                            3,
                            Rvalue::Box {
                                value: op(2),
                                from: repr,
                            },
                        ),
                    ],
                    3,
                ),
                &[],
            );
            index = index.checked_add(1).unwrap();
            runs.push((repr, bits_of(repr, v), p.call(f, vec![float(repr, v)])));
        }
    }
    let flip = p.item(
        100,
        "flip",
        &[Repr::Bool],
        &[Repr::Value, Repr::Bool, Repr::Bool, Repr::Value],
        straight(
            vec![
                assign(
                    1,
                    Rvalue::Box {
                        value: op(0),
                        from: Repr::Bool,
                    },
                ),
                assign(
                    2,
                    Rvalue::Unbox {
                        value: op(1),
                        to: Repr::Bool,
                    },
                ),
                assign(3, Rvalue::Not(op(2))),
                assign(
                    4,
                    Rvalue::Box {
                        value: op(3),
                        from: Repr::Bool,
                    },
                ),
            ],
            4,
        ),
        &[],
    );
    let flip_true = p.call(flip, vec![Operand::Const(Const::Bool(true))]);
    let flip_false = p.call(flip, vec![Operand::Const(Const::Bool(false))]);
    let jit = p.compile();
    for (repr, expected, target) in runs {
        assert_eq!(float_bits(&jit, target, repr), expected, "{repr}");
    }
    assert_eq!(returned(run_const(&jit, flip_true)), truth(false));
    assert_eq!(returned(run_const(&jit, flip_false)), truth(true));
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}
