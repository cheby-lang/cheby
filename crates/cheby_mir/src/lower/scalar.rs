//! Literals and operators (D2, D9): numeric literals, `Binary`, `!`,
//! unary `-` and operators implemented by calls (`OpImpl::Call`).
//!
//! Built-in operators work on unboxed values (D-278): integer arithmetic
//! and shifts are [`Rvalue::Checked`] (they panic at the operator's
//! location, §3.3, §5.4.6, D-025, D-069), float arithmetic is
//! [`Rvalue::FloatArith`] (IEEE 754, never panics, D-527), `& | ^` are
//! [`Rvalue::Bitwise`], and comparisons of numbers and `Bool` are
//! [`Rvalue::Compare`], which follows `compare`'s total order on floats
//! (§3.14, D-135, D-527). `==` on `String` is [`RuntimeFn::StrEq`], on
//! every other uniform type structural [`RuntimeFn::Equal`] with the
//! operator's location (D-556); `!=` is the negation. An overloaded
//! operator (§5.4.3, §5.4.4) calls the operand type's function; a
//! comparison tests the `Order` it returns. `&&` and `||` branch, and
//! their right operand is in tail position when the whole expression is
//! (§5.15.2, D-120).
//!
//! The location of a failed operator is the start of the operator
//! expression, which is the start of its left operand for a binary
//! operator and the `-` for a negation (D-449, D-570): THIR keeps no span
//! for the operator token, and rustc reports the expression's start too.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{BinOpKind, Callee as ThirCallee, ExprId, FloatLit, Lit, OpImpl};

use crate::ir::{ArithOp, BitOp, CmpOp, Const, Operand, RuntimeFn, Rvalue, TagType};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// A literal of type `ty`: a numeric literal is a constant of its
    /// type's representation (§3.3.4, D-084), a string literal a static
    /// `String`.
    pub(super) fn lit(&mut self, lit: &Lit, ty: Ty, span: Span) -> Result<Option<Operand>, Ice> {
        let repr = self.repr_of(ty);
        let c = match lit {
            Lit::Str(text) => {
                let s = self.string_const(text)?;
                let l = self.define(Repr::Value, Rvalue::Use(s))?;
                return Ok(Some(Operand::Local(l)));
            }
            Lit::Int(v) => int_const(*v, repr),
            Lit::Float(v) => float_const(*v, repr),
            _ => return self.unsupported(span, "this kind of literal", ty),
        };
        c.map(|c| Some(Operand::Const(c))).ok_or_else(|| {
            Ice::new(format!(
                "the literal {lit:?} of type `{}` does not fit {repr}",
                self.show(ty)
            ))
        })
    }

    /// `lhs op rhs`, implemented as `imp` says, of type `ty`; `tail`
    /// marks §5.15.2 tail position (the right operand of `&&` and `||`,
    /// and the call of an overloaded arithmetic operator).
    #[expect(
        clippy::too_many_arguments,
        reason = "the operator's parts, as THIR holds them (plan §4.2)"
    )]
    pub(super) fn binary(
        &mut self,
        op: BinOpKind,
        lhs: ExprId,
        rhs: ExprId,
        imp: &OpImpl,
        ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        if let Some(and) = logical(op) {
            return self.logical(and, lhs, rhs, tail);
        }
        let operand_ty = self.operand_ty(lhs, rhs)?;
        let Some((l, r)) = self.operands(lhs, rhs, operand_ty)? else {
            return Ok(None);
        };
        self.apply_binary(
            op,
            BinaryOperands {
                lhs: l,
                rhs: r,
                ty: operand_ty,
            },
            imp,
            ty,
            span,
            tail,
        )
    }

    /// `!operand`.
    pub(super) fn not(
        &mut self,
        operand: ExprId,
        _ty: Ty,
        _span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let Some(v) = self.expr_as(operand, Repr::Bool)? else {
            return Ok(None);
        };
        let l = self.define(Repr::Bool, Rvalue::Not(v))?;
        Ok(Some(Operand::Local(l)))
    }

    /// `-operand`, implemented as `imp` says: on an integer `0 - x`,
    /// which panics on the minimum (§5.4.2, D-025); on a float a sign
    /// flip (D-527); otherwise a call of the type's `neg` (D-118), a tail
    /// call in tail position.
    pub(super) fn neg(
        &mut self,
        operand: ExprId,
        imp: &OpImpl,
        ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let operand_ty = self.ty(operand)?;
        let repr = self.repr_of(operand_ty);
        let Some(v) = self.expr_as(operand, repr)? else {
            return Ok(None);
        };
        match imp {
            OpImpl::Builtin => {
                let value = if is_integer(repr) {
                    Rvalue::Checked {
                        op: ArithOp::Sub,
                        lhs: Operand::Const(zero(repr)?),
                        rhs: v,
                        repr,
                        loc: self.location_id(span)?,
                    }
                } else if is_float(repr) {
                    Rvalue::FloatNeg { value: v, repr }
                } else {
                    return Err(Ice::new(format!(
                        "built-in `-` on `{}`",
                        self.show(operand_ty)
                    )));
                };
                let l = self.define(repr, value)?;
                Ok(Some(Operand::Local(l)))
            }
            OpImpl::Call(callee) => self.call_op(callee, vec![(v, operand_ty)], ty, span, tail),
        }
    }

    /// The type both operands of a binary operator have: the left one's,
    /// or the right one's when the left is `Never` (§5.4.1, D-492).
    pub(super) fn operand_ty(&self, lhs: ExprId, rhs: ExprId) -> Result<Ty, Ice> {
        let l = self.ty(lhs)?;
        if l == Ty::NEVER { self.ty(rhs) } else { Ok(l) }
    }

    /// Both operands, left to right (§5.15.1), each in the representation
    /// of `operand_ty`. `None` if one diverged.
    pub(super) fn operands(
        &mut self,
        lhs: ExprId,
        rhs: ExprId,
        operand_ty: Ty,
    ) -> Result<Option<(Operand, Operand)>, Ice> {
        let repr = self.repr_of(operand_ty);
        let Some(l) = self.expr_as(lhs, repr)? else {
            return Ok(None);
        };
        let Some(r) = self.expr_as(rhs, repr)? else {
            return Ok(None);
        };
        Ok(Some((l, r)))
    }

    /// `op` (not `&&` or `||`) on operands already evaluated, giving a
    /// value of type `ty`. `tail` applies to the call of an overloaded
    /// arithmetic operator only.
    pub(super) fn apply_binary(
        &mut self,
        op: BinOpKind,
        operands: BinaryOperands,
        imp: &OpImpl,
        ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        match imp {
            OpImpl::Builtin => self.builtin_binary(op, operands, span).map(Some),
            OpImpl::Call(callee) => {
                let BinaryOperands { lhs, rhs, ty: oty } = operands;
                let args = vec![(lhs, oty), (rhs, oty)];
                let Some(test) = order_test(op) else {
                    return self.call_op(callee, args, ty, span, tail);
                };
                // `compare(a, b)` and a test of its `Order` (§5.4.4).
                let Some(order) = self.call_op(callee, args, Ty::ORDER, span, false)? else {
                    return Ok(None);
                };
                let tag = self.define(
                    Repr::Int,
                    Rvalue::ReadTag {
                        base: order,
                        ty: TagType::Order,
                    },
                )?;
                let (cmp, k) = test;
                let l = self.define(
                    Repr::Bool,
                    Rvalue::Compare {
                        op: cmp,
                        lhs: Operand::Local(tag),
                        rhs: Operand::Const(Const::Int(k)),
                        repr: Repr::Int,
                    },
                )?;
                Ok(Some(Operand::Local(l)))
            }
        }
    }

    /// `==` (or `!=` when `ne`) on two operands of the same type: a raw
    /// comparison of unboxed values, byte equality of strings, otherwise
    /// structural equality located for its panic on functions (§3.13,
    /// D-556).
    fn equality(&mut self, ne: bool, operands: BinaryOperands, span: Span) -> Result<Operand, Ice> {
        let BinaryOperands { lhs, rhs, ty } = operands;
        let repr = self.repr_of(ty);
        let eq = if repr == Repr::Value {
            if ty == Ty::STRING {
                Rvalue::CallRuntime {
                    func: RuntimeFn::StrEq,
                    args: vec![lhs, rhs],
                }
            } else {
                let loc = self.location(span)?;
                Rvalue::CallRuntime {
                    func: RuntimeFn::Equal,
                    args: vec![lhs, rhs, loc],
                }
            }
        } else {
            let op = if ne { CmpOp::Ne } else { CmpOp::Eq };
            let l = self.define(Repr::Bool, Rvalue::Compare { op, lhs, rhs, repr })?;
            return Ok(Operand::Local(l));
        };
        let mut l = self.define(Repr::Bool, eq)?;
        if ne {
            l = self.define(Repr::Bool, Rvalue::Not(Operand::Local(l)))?;
        }
        Ok(Operand::Local(l))
    }

    /// A built-in binary operator on two operands of the same type.
    fn builtin_binary(
        &mut self,
        op: BinOpKind,
        operands: BinaryOperands,
        span: Span,
    ) -> Result<Operand, Ice> {
        let BinaryOperands { lhs, rhs, ty } = operands;
        let repr = self.repr_of(ty);
        let bad = |cx: &Self| {
            Err(Ice::new(format!(
                "built-in `{}` on `{}`",
                op_text(op),
                cx.show(ty)
            )))
        };
        let (out, value) = match op {
            BinOpKind::Eq | BinOpKind::Ne => {
                let eq = BinaryOperands { lhs, rhs, ty };
                return self.equality(op == BinOpKind::Ne, eq, span);
            }
            BinOpKind::Lt | BinOpKind::Le | BinOpKind::Gt | BinOpKind::Ge => {
                if !is_integer(repr) && !is_float(repr) {
                    return bad(self);
                }
                let op = match op {
                    BinOpKind::Lt => CmpOp::Lt,
                    BinOpKind::Le => CmpOp::Le,
                    BinOpKind::Gt => CmpOp::Gt,
                    _ => CmpOp::Ge,
                };
                (Repr::Bool, Rvalue::Compare { op, lhs, rhs, repr })
            }
            BinOpKind::Add
            | BinOpKind::Sub
            | BinOpKind::Mul
            | BinOpKind::Div
            | BinOpKind::Rem
            | BinOpKind::Shl
            | BinOpKind::Shr => {
                let arith = match op {
                    BinOpKind::Add => ArithOp::Add,
                    BinOpKind::Sub => ArithOp::Sub,
                    BinOpKind::Mul => ArithOp::Mul,
                    BinOpKind::Div => ArithOp::Div,
                    BinOpKind::Rem => ArithOp::Rem,
                    BinOpKind::Shl => ArithOp::Shl,
                    _ => ArithOp::Shr,
                };
                let shift = matches!(arith, ArithOp::Shl | ArithOp::Shr);
                if is_integer(repr) {
                    let loc = self.location_id(span)?;
                    (
                        repr,
                        Rvalue::Checked {
                            op: arith,
                            lhs,
                            rhs,
                            repr,
                            loc,
                        },
                    )
                } else if is_float(repr) && !shift {
                    (
                        repr,
                        Rvalue::FloatArith {
                            op: arith,
                            lhs,
                            rhs,
                            repr,
                        },
                    )
                } else {
                    return bad(self);
                }
            }
            BinOpKind::BitAnd | BinOpKind::BitOr | BinOpKind::BitXor => {
                if !is_integer(repr) {
                    return bad(self);
                }
                let op = match op {
                    BinOpKind::BitAnd => BitOp::And,
                    BinOpKind::BitOr => BitOp::Or,
                    _ => BitOp::Xor,
                };
                (repr, Rvalue::Bitwise { op, lhs, rhs, repr })
            }
            _ => return bad(self),
        };
        Ok(Operand::Local(self.define(out, value)?))
    }

    /// A call of an operator's function (`OpImpl::Call`) on arguments
    /// already evaluated, each in its type's representation, giving a
    /// value of type `ret_ty`; a tail call in tail position.
    fn call_op(
        &mut self,
        callee: &ThirCallee,
        args: Vec<(Operand, Ty)>,
        ret_ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        match callee {
            ThirCallee::Fn { def, dicts, .. } => {
                let dict_ops = match self.call_dicts(dicts, ret_ty, span)? {
                    Ok(ops) => ops,
                    Err(early) => return Ok(early),
                };
                let sig = crate::abi::abi_sig(self.cx.defs.fn_sig(*def)?, self.cx.defs.globals);
                let want = sig.params.get(dicts.len()..).unwrap_or_default();
                if want.len() != args.len() {
                    return Err(Ice::new(format!(
                        "operator call of {def:?} with {} arguments for {} parameters",
                        args.len(),
                        want.len()
                    )));
                }
                let mut ops = Vec::with_capacity(args.len());
                for ((op, ty), &repr) in args.into_iter().zip(want) {
                    let from = self.repr_of(ty);
                    ops.push(self.to_repr(op, from, repr, ty)?);
                }
                self.call_fn(*def, dict_ops, ops, ret_ty, tail)
            }
            ThirCallee::Iface { iface_fn, dict } => {
                self.call_iface_ops(*iface_fn, dict, args, ret_ty, span, tail)
            }
            _ => Err(Ice::new(format!(
                "an operator implemented by {callee:?} reached MIR lowering"
            ))),
        }
    }

    /// `a && b` (`and`) or `a || b`: `b` is evaluated only when `a` does
    /// not decide the result (§5.4.5), in tail position when the whole
    /// expression is (§5.15.2). The two paths join with a block
    /// parameter.
    fn logical(
        &mut self,
        and: bool,
        lhs: ExprId,
        rhs: ExprId,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let Some(l) = self.expr_as(lhs, Repr::Bool)? else {
            return Ok(None);
        };
        let right = self.new_block()?;
        let short = self.new_block()?;
        let join = self.new_block()?;
        let result = self.block_param(join, Repr::Bool)?;
        let (then_, else_) = if and { (right, short) } else { (short, right) };
        self.terminate(crate::ir::Terminator::Branch {
            cond: l,
            then_,
            else_,
        })?;
        self.switch_to(short)?;
        self.goto(join, vec![Operand::Const(Const::Bool(!and))])?;
        self.switch_to(right)?;
        if let Some(r) = self.expr(rhs, tail)? {
            let rty = self.ty(rhs)?;
            let r = self.to_repr(r, self.repr_of(rty), Repr::Bool, rty)?;
            self.goto(join, vec![r])?;
        }
        self.switch_to(join)?;
        Ok(Some(Operand::Local(result)))
    }
}

/// The two evaluated operands of a binary operator and their type.
pub(super) struct BinaryOperands {
    pub(super) lhs: Operand,
    pub(super) rhs: Operand,
    pub(super) ty: Ty,
}

/// `Some(true)` for `&&`, `Some(false)` for `||`.
const fn logical(op: BinOpKind) -> Option<bool> {
    match op {
        BinOpKind::And => Some(true),
        BinOpKind::Or => Some(false),
        _ => None,
    }
}

/// Whether `op` is one of `==`, `!=`, `<`, `<=`, `>`, `>=` (§11.5).
pub(super) const fn is_comparison(op: BinOpKind) -> bool {
    matches!(
        op,
        BinOpKind::Eq
            | BinOpKind::Ne
            | BinOpKind::Lt
            | BinOpKind::Le
            | BinOpKind::Gt
            | BinOpKind::Ge
    )
}

/// How an ordering operator tests the tag of `compare`'s result
/// (`Less` 0, `Equal` 1, `Greater` 2): `a < b` is `== Less`, `a <= b`
/// is `!= Greater`, `a > b` is `== Greater`, `a >= b` is `!= Less`
/// (§5.4.4).
const fn order_test(op: BinOpKind) -> Option<(CmpOp, i64)> {
    match op {
        BinOpKind::Lt => Some((CmpOp::Eq, 0)),
        BinOpKind::Le => Some((CmpOp::Ne, 2)),
        BinOpKind::Gt => Some((CmpOp::Eq, 2)),
        BinOpKind::Ge => Some((CmpOp::Ne, 0)),
        _ => None,
    }
}

/// The operator as written, for internal errors.
const fn op_text(op: BinOpKind) -> &'static str {
    match op {
        BinOpKind::Or => "||",
        BinOpKind::And => "&&",
        BinOpKind::Eq => "==",
        BinOpKind::Ne => "!=",
        BinOpKind::Lt => "<",
        BinOpKind::Le => "<=",
        BinOpKind::Gt => ">",
        BinOpKind::Ge => ">=",
        BinOpKind::BitOr => "|",
        BinOpKind::BitXor => "^",
        BinOpKind::BitAnd => "&",
        BinOpKind::Shl => "<<",
        BinOpKind::Shr => ">>",
        BinOpKind::Add => "+",
        BinOpKind::Sub => "-",
        BinOpKind::Mul => "*",
        BinOpKind::Div => "/",
        BinOpKind::Rem => "%",
    }
}

const fn is_integer(repr: Repr) -> bool {
    repr.is_signed_int() || repr.is_unsigned_int()
}

const fn is_float(repr: Repr) -> bool {
    matches!(repr, Repr::Float | Repr::F32)
}

/// The constant 0 of an integer representation.
fn zero(repr: Repr) -> Result<Const, Ice> {
    int_const(0, repr).ok_or_else(|| Ice::new(format!("no integer zero of {repr}")))
}

/// The integer `v` as a constant of `repr`, if it is an integer
/// representation that holds `v` (the front end has checked the range,
/// D-084).
fn int_const(v: i128, repr: Repr) -> Option<Const> {
    let (min, max): (i128, i128) = match repr {
        Repr::Int | Repr::I64 => (i64::MIN.into(), i64::MAX.into()),
        Repr::I8 => (i8::MIN.into(), i8::MAX.into()),
        Repr::I16 => (i16::MIN.into(), i16::MAX.into()),
        Repr::I32 => (i32::MIN.into(), i32::MAX.into()),
        Repr::U8 => (0, u8::MAX.into()),
        Repr::U16 => (0, u16::MAX.into()),
        Repr::U32 => (0, u32::MAX.into()),
        Repr::U64 => (0, u64::MAX.into()),
        _ => return None,
    };
    if v < min || v > max {
        return None;
    }
    if repr == Repr::Int {
        return i64::try_from(v).ok().map(Const::Int);
    }
    // Signed values are sign-extended to 64 bits (`Const::Sized`).
    let bits = if repr.is_signed_int() {
        i64::try_from(v).ok()?.cast_unsigned()
    } else {
        u64::try_from(v).ok()?
    };
    Some(Const::Sized { repr, bits })
}

/// The float literal `v` as a constant of `repr` (`Float` or `F32`). An
/// `F32` literal is the nearest `F32` to its decimal text, rounded once by
/// the front end (§3.3.4, D-496, D-569).
fn float_const(v: FloatLit, repr: Repr) -> Option<Const> {
    match repr {
        Repr::Float => Some(Const::Float(v.wide.to_bits())),
        Repr::F32 => Some(Const::Sized {
            repr,
            bits: u64::from(v.narrow.to_bits()),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_constants_hold_their_type_range() {
        assert_eq!(int_const(-1, Repr::Int), Some(Const::Int(-1)));
        assert_eq!(
            int_const(-128, Repr::I8),
            Some(Const::Sized {
                repr: Repr::I8,
                bits: u64::MAX - 127
            })
        );
        assert_eq!(int_const(128, Repr::I8), None);
        assert_eq!(int_const(-1, Repr::U8), None);
        assert_eq!(
            int_const(u64::MAX.into(), Repr::U64),
            Some(Const::Sized {
                repr: Repr::U64,
                bits: u64::MAX
            })
        );
        assert_eq!(int_const(1, Repr::Float), None);
    }

    #[test]
    fn float_constants_take_the_value_of_their_type() {
        let lit = FloatLit {
            wide: 0.1,
            narrow: 0.1,
        };
        assert_eq!(
            float_const(lit, Repr::F32),
            Some(Const::Sized {
                repr: Repr::F32,
                bits: u64::from(0.1f32.to_bits())
            })
        );
        assert_eq!(
            float_const(lit, Repr::Float),
            Some(Const::Float(0.1f64.to_bits()))
        );
        let zero = FloatLit {
            wide: -0.0,
            narrow: -0.0,
        };
        assert_eq!(
            float_const(zero, Repr::Float),
            Some(Const::Float((-0.0f64).to_bits()))
        );
    }
}
