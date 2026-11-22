//! Scalar operations (E1, D6): checked integer arithmetic (§3.3, §5.4.6,
//! D-025, D-069), comparisons with the float rules of §3.3.2 and D-527,
//! IEEE float arithmetic, and bitwise operators. Also the std kernels that
//! are instructions (`cheby_mir::kernels_instr`): conversions that cannot
//! fail, the square root and wrapping arithmetic, each one Cranelift
//! instruction, or none for `Int` and `I64`, which are the same word.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{self, InstBuilder, Value, types};
use cranelift_frontend::FunctionBuilder;

use cheby_mir::{ArithOp, BitOp, CmpOp, Operand, PanicKind, Repr, StrId};

use crate::CodegenError;
use crate::func::{Translator, truncate, unsupported};
use crate::runtime::RuntimeFunc;
use crate::sig::repr_type;

/// Whether `repr` is a signed integer, or `None` if it is no integer.
const fn signedness(repr: Repr) -> Option<bool> {
    if repr.is_signed_int() {
        Some(true)
    } else if repr.is_unsigned_int() {
        Some(false)
    } else {
        None
    }
}

/// [`cheby_mir::Rvalue::Convert`] of `v`, a raw `from`, into `to`.
/// An integer widens by `sextend` from a signed type and `uextend` from
/// an unsigned one, and stays as it is at the same width (`Int` and
/// `I64`). An integer becomes a float by `fcvt_from_sint` or
/// `fcvt_from_uint`, which round to nearest, ties to even, as Rust's `as`
/// does in the kernels (D-359). `fpromote` and `fdemote` convert between
/// `F32` and `Float`. None of these is a libcall on the targets of
/// D-921: `fcvt_from_uint` of a 64-bit integer is a short sequence on
/// x86-64, one instruction on aarch64.
pub(crate) fn convert(
    b: &mut FunctionBuilder<'_>,
    v: Value,
    from: Repr,
    to: Repr,
) -> Result<Value, CodegenError> {
    if !from.converts_exactly(to) {
        return Err(unsupported(&format!("a conversion from {from} to {to}")));
    }
    let to_ty = repr_type(to);
    Ok(match (signedness(from), to) {
        (Some(_), _) if to_ty == repr_type(from) => v,
        (Some(true), Repr::Float | Repr::F32) => b.ins().fcvt_from_sint(to_ty, v),
        (Some(false), Repr::Float | Repr::F32) => b.ins().fcvt_from_uint(to_ty, v),
        (Some(true), _) => b.ins().sextend(to_ty, v),
        (Some(false), _) => b.ins().uextend(to_ty, v),
        (None, Repr::Float) => b.ins().fpromote(types::F64, v),
        (None, _) => b.ins().fdemote(types::F32, v),
    })
}

/// [`cheby_mir::Rvalue::Wrapping`]: `a op c` at the width of the integer
/// `repr`, modulo 2^width (D-025), as Rust's `wrapping_` methods compute
/// it in the kernels.
pub(crate) fn wrapping(
    b: &mut FunctionBuilder<'_>,
    op: ArithOp,
    a: Value,
    c: Value,
    repr: Repr,
) -> Result<Value, CodegenError> {
    signedness(repr).ok_or_else(|| unsupported(&format!("wrapping arithmetic on {repr}")))?;
    Ok(match op {
        ArithOp::Add => b.ins().iadd(a, c),
        ArithOp::Sub => b.ins().isub(a, c),
        ArithOp::Mul => b.ins().imul(a, c),
        ArithOp::Div | ArithOp::Rem | ArithOp::Shl | ArithOp::Shr => {
            return Err(unsupported(&format!("wrapping {}", op.name())));
        }
    })
}

impl Translator<'_, '_> {
    /// [`cheby_mir::Rvalue::Convert`] of `value` ([`convert`]).
    pub(crate) fn convert(
        &mut self,
        value: &Operand,
        from: Repr,
        to: Repr,
    ) -> Result<Value, CodegenError> {
        let v = self.operand(value)?;
        convert(&mut self.b, v, from, to)
    }

    /// [`cheby_mir::Rvalue::FloatSqrt`] of `value`: `sqrt`, IEEE 754's
    /// square root, as `f64::sqrt` computes it in the kernel.
    pub(crate) fn sqrt(&mut self, value: &Operand) -> Result<Value, CodegenError> {
        let v = self.operand(value)?;
        Ok(self.b.ins().sqrt(v))
    }

    /// [`cheby_mir::Rvalue::Wrapping`] of `lhs` and `rhs` ([`wrapping`]).
    pub(crate) fn wrapping(
        &mut self,
        op: ArithOp,
        lhs: &Operand,
        rhs: &Operand,
        repr: Repr,
    ) -> Result<Value, CodegenError> {
        let a = self.operand(lhs)?;
        let c = self.operand(rhs)?;
        wrapping(&mut self.b, op, a, c, repr)
    }

    /// An integer constant of type `ty` with the value `n` (wrapped to the
    /// width), as Cranelift wants narrow immediates.
    fn int_const(&mut self, ty: ir::Type, n: i64) -> Value {
        self.b.ins().iconst(ty, truncate(n.cast_unsigned(), ty))
    }

    /// `lhs op rhs` on an integer `repr`, panicking at `loc` with
    /// `Overflow`, `DivisionByZero` or `ShiftOutOfRange`. `MIN / -1`
    /// overflows; `MIN % -1` is 0.
    pub(crate) fn checked(
        &mut self,
        op: ArithOp,
        lhs: &Operand,
        rhs: &Operand,
        repr: Repr,
        loc: StrId,
    ) -> Result<Value, CodegenError> {
        let signed = signedness(repr)
            .ok_or_else(|| unsupported(&format!("checked arithmetic on {repr}")))?;
        let ty = repr_type(repr);
        let a = self.operand(lhs)?;
        let b = self.operand(rhs)?;
        Ok(match op {
            ArithOp::Add | ArithOp::Sub | ArithOp::Mul => {
                let ins = self.b.ins();
                let (result, overflow) = match (op, signed) {
                    (ArithOp::Add, true) => ins.sadd_overflow(a, b),
                    (ArithOp::Add, false) => ins.uadd_overflow(a, b),
                    (ArithOp::Sub, true) => ins.ssub_overflow(a, b),
                    (ArithOp::Sub, false) => ins.usub_overflow(a, b),
                    (_, true) => ins.smul_overflow(a, b),
                    (_, false) => ins.umul_overflow(a, b),
                };
                self.panic_if(overflow, PanicKind::Overflow, loc);
                result
            }
            ArithOp::Div | ArithOp::Rem => {
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                self.panic_if(zero, PanicKind::DivisionByZero, loc);
                if !signed {
                    if op == ArithOp::Div {
                        self.b.ins().udiv(a, b)
                    } else {
                        self.b.ins().urem(a, b)
                    }
                } else if op == ArithOp::Div {
                    // `MIN / -1` overflows (§3.3.1).
                    let minus_one = self.int_const(ty, -1);
                    let by_minus_one = self.b.ins().icmp(IntCC::Equal, b, minus_one);
                    let min = self.int_const(ty, i64::MIN >> (64_u32.saturating_sub(ty.bits())));
                    let is_min = self.b.ins().icmp(IntCC::Equal, a, min);
                    let overflow = self.b.ins().band(by_minus_one, is_min);
                    self.panic_if(overflow, PanicKind::Overflow, loc);
                    self.b.ins().sdiv(a, b)
                } else {
                    // `x % -1` is 0 for every `x`, `MIN` included; dividing
                    // by 1 instead never traps and gives the same 0.
                    let minus_one = self.int_const(ty, -1);
                    let by_minus_one = self.b.ins().icmp(IntCC::Equal, b, minus_one);
                    let one = self.int_const(ty, 1);
                    let divisor = self.b.ins().select(by_minus_one, one, b);
                    self.b.ins().srem(a, divisor)
                }
            }
            ArithOp::Shl | ArithOp::Shr => {
                // A count that is negative or not below the width panics
                // (§5.4.6); as an unsigned number a negative count is huge.
                let width = self.int_const(ty, i64::from(ty.bits()));
                let out_of_range = self
                    .b
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, b, width);
                self.panic_if(out_of_range, PanicKind::ShiftOutOfRange, loc);
                if op == ArithOp::Shl {
                    // `a << n` is `a * 2^n` and overflows like it: shifting
                    // back must give `a`.
                    let shifted = self.b.ins().ishl(a, b);
                    let back = if signed {
                        self.b.ins().sshr(shifted, b)
                    } else {
                        self.b.ins().ushr(shifted, b)
                    };
                    let overflow = self.b.ins().icmp(IntCC::NotEqual, back, a);
                    self.panic_if(overflow, PanicKind::Overflow, loc);
                    shifted
                } else if signed {
                    self.b.ins().sshr(a, b)
                } else {
                    self.b.ins().ushr(a, b)
                }
            }
        })
    }

    /// A comparison giving a `Bool` (`i8`). Floats follow §3.3.2 for `==`
    /// and the total order of `compare` for the others (D-527): NaNs equal
    /// each other and are greater than every other float.
    pub(crate) fn compare(
        &mut self,
        op: CmpOp,
        lhs: &Operand,
        rhs: &Operand,
        repr: Repr,
    ) -> Result<Value, CodegenError> {
        let a = self.operand(lhs)?;
        let b = self.operand(rhs)?;
        if matches!(repr, Repr::Float | Repr::F32) {
            return Ok(self.float_compare(op, a, b));
        }
        let signed = match (repr, op) {
            (Repr::Bool, CmpOp::Eq | CmpOp::Ne) => false,
            _ => signedness(repr)
                .ok_or_else(|| unsupported(&format!("`{}` on {repr}", op.name())))?,
        };
        let cc = match (op, signed) {
            (CmpOp::Eq, _) => IntCC::Equal,
            (CmpOp::Ne, _) => IntCC::NotEqual,
            (CmpOp::Lt, true) => IntCC::SignedLessThan,
            (CmpOp::Lt, false) => IntCC::UnsignedLessThan,
            (CmpOp::Le, true) => IntCC::SignedLessThanOrEqual,
            (CmpOp::Le, false) => IntCC::UnsignedLessThanOrEqual,
            (CmpOp::Gt, true) => IntCC::SignedGreaterThan,
            (CmpOp::Gt, false) => IntCC::UnsignedGreaterThan,
            (CmpOp::Ge, true) => IntCC::SignedGreaterThanOrEqual,
            (CmpOp::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
        };
        Ok(self.b.ins().icmp(cc, a, b))
    }

    fn float_compare(&mut self, op: CmpOp, a: Value, b: Value) -> Value {
        match op {
            CmpOp::Eq => self.float_eq(a, b),
            CmpOp::Ne => {
                let eq = self.float_eq(a, b);
                self.b.ins().bxor_imm_u(eq, 1)
            }
            CmpOp::Lt => self.float_lt(a, b),
            CmpOp::Le => self.float_le(a, b),
            CmpOp::Gt => self.float_lt(b, a),
            CmpOp::Ge => self.float_le(b, a),
        }
    }

    /// `a == b` under §3.3.2: IEEE equality (so `-0.0 == 0.0`), or both
    /// NaN.
    fn float_eq(&mut self, a: Value, b: Value) -> Value {
        let ieee = self.b.ins().fcmp(FloatCC::Equal, a, b);
        let a_nan = self.b.ins().fcmp(FloatCC::Unordered, a, a);
        let b_nan = self.b.ins().fcmp(FloatCC::Unordered, b, b);
        let both_nan = self.b.ins().band(a_nan, b_nan);
        self.b.ins().bor(ieee, both_nan)
    }

    /// `a < b` in `compare`'s order: `a` is not NaN, and `b` is NaN or
    /// greater.
    fn float_lt(&mut self, a: Value, b: Value) -> Value {
        let ieee = self.b.ins().fcmp(FloatCC::LessThan, a, b);
        let a_ordered = self.b.ins().fcmp(FloatCC::Ordered, a, a);
        let b_nan = self.b.ins().fcmp(FloatCC::Unordered, b, b);
        let below = self.b.ins().bor(ieee, b_nan);
        self.b.ins().band(a_ordered, below)
    }

    /// `a <= b` in `compare`'s order: `b` is NaN, or `a` is not NaN and
    /// not greater.
    fn float_le(&mut self, a: Value, b: Value) -> Value {
        let ieee = self.b.ins().fcmp(FloatCC::LessThanOrEqual, a, b);
        let b_nan = self.b.ins().fcmp(FloatCC::Unordered, b, b);
        self.b.ins().bor(ieee, b_nan)
    }

    /// IEEE arithmetic on `Float` or `F32`; `%` is `fmod`, with the
    /// dividend's sign (D-527).
    pub(crate) fn float_arith(
        &mut self,
        op: ArithOp,
        lhs: &Operand,
        rhs: &Operand,
        repr: Repr,
    ) -> Result<Value, CodegenError> {
        let a = self.operand(lhs)?;
        let b = self.operand(rhs)?;
        Ok(match op {
            ArithOp::Add => self.b.ins().fadd(a, b),
            ArithOp::Sub => self.b.ins().fsub(a, b),
            ArithOp::Mul => self.b.ins().fmul(a, b),
            ArithOp::Div => self.b.ins().fdiv(a, b),
            ArithOp::Rem => {
                let f = match repr {
                    Repr::Float => RuntimeFunc::FmodF64,
                    Repr::F32 => RuntimeFunc::FmodF32,
                    Repr::Value
                    | Repr::Int
                    | Repr::Bool
                    | Repr::I8
                    | Repr::I16
                    | Repr::I32
                    | Repr::I64
                    | Repr::U8
                    | Repr::U16
                    | Repr::U32
                    | Repr::U64 => return Err(unsupported(&format!("float `%` on {repr}"))),
                };
                self.runtime_value(f, &[a, b])?
            }
            ArithOp::Shl | ArithOp::Shr => {
                return Err(unsupported(&format!("`{}` on floats", op.name())));
            }
        })
    }

    /// `&`, `|` or `^` at the width of an integer `repr` (§5.4.6).
    pub(crate) fn bitwise(
        &mut self,
        op: BitOp,
        lhs: &Operand,
        rhs: &Operand,
        repr: Repr,
    ) -> Result<Value, CodegenError> {
        signedness(repr).ok_or_else(|| unsupported(&format!("bitwise operators on {repr}")))?;
        let a = self.operand(lhs)?;
        let b = self.operand(rhs)?;
        Ok(match op {
            BitOp::And => self.b.ins().band(a, b),
            BitOp::Or => self.b.ins().bor(a, b),
            BitOp::Xor => self.b.ins().bxor(a, b),
        })
    }
}
