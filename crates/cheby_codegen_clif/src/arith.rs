//! Scalar operations (E1, D6): checked integer arithmetic (§3.3, §5.4.6,
//! D-025, D-069), comparisons with the float rules of §3.3.2 and D-527,
//! IEEE float arithmetic, and bitwise operators.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{self, InstBuilder, Value};

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

impl Translator<'_, '_> {
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
                    _ => return Err(unsupported(&format!("float `%` on {repr}"))),
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
