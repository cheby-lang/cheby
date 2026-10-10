//! Literals and operators (D2, D9): numeric literals, `Binary`, `!`,
//! unary `-` and operators implemented by calls (`OpImpl::Call`).
//!
//! W2-A fills this file (plan §4.2). Today only string literals lower;
//! the rest is E0038.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{BinOpKind, ExprId, Lit, OpImpl};

use crate::ir::{Operand, Rvalue};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// A literal of type `ty`.
    pub(super) fn lit(&mut self, lit: &Lit, ty: Ty, span: Span) -> Result<Option<Operand>, Ice> {
        match lit {
            Lit::Str(text) => {
                let s = self.string_const(text)?;
                let l = self.define(Repr::Value, Rvalue::Use(s))?;
                Ok(Some(Operand::Local(l)))
            }
            Lit::Int(_) | Lit::Float(_) => self.unsupported(span, "numeric literals", ty),
            _ => self.unsupported(span, "this kind of literal", ty),
        }
    }

    /// `lhs op rhs`, implemented as `imp` says; `tail` marks §5.15.2 tail
    /// position (the right operand of `&&` and `||`, an operator call).
    #[expect(
        clippy::too_many_arguments,
        reason = "the operator's parts, as THIR holds them (plan §4.2)"
    )]
    pub(super) fn binary(
        &mut self,
        _op: BinOpKind,
        _lhs: ExprId,
        _rhs: ExprId,
        _imp: &OpImpl,
        ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "operators", ty)
    }

    /// `!operand`.
    pub(super) fn not(
        &mut self,
        _operand: ExprId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "operators", ty)
    }

    /// `-operand`, implemented as `imp` says.
    pub(super) fn neg(
        &mut self,
        _operand: ExprId,
        _imp: &OpImpl,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "operators", ty)
    }
}
