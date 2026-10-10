//! Constructors, records, field access, record updates and tuples (D2,
//! D6, D-454).
//!
//! W2-D fills this file (plan §4.2). Today unit constructors lower: the
//! builtin ones and every user one are static objects (D-454), `Nil` and
//! an unboxed `Bool` are constants. The rest is E0038.

use cheby_diag::Span;
use cheby_iface::{CtorFields, DefId, Ice, ItemSig, Ty, builtin};
use cheby_types::thir::{ExprId, UpdateField};

use crate::ir::{Const, Operand};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// A nullary constructor as a value of type `ty`: `Nil` and unboxed
    /// `Bool`s as constants, every other one as its static object,
    /// the runtime's for builtin constructors and `cb$ctor$…` for user
    /// ones (D-454). In a uniform position a `Bool` is the static `True`
    /// or `False` too.
    pub(super) fn unit_ctor(
        &mut self,
        ctor: DefId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let item = self.cx.defs.item(ctor)?;
        if !matches!(&item.sig, ItemSig::Ctor(sig) if sig.fields == CtorFields::Unit) {
            return Err(Ice::new(format!(
                "a constructor with fields as a unit constructor at {span:?}"
            )));
        }
        let repr = self.repr_of(ty);
        let c = match ctor {
            c if c == builtin::NIL => Const::Nil,
            c if c == builtin::TRUE && repr == Repr::Bool => Const::Bool(true),
            c if c == builtin::FALSE && repr == Repr::Bool => Const::Bool(false),
            c => Const::StaticCtor(c),
        };
        Ok(Some(Operand::Const(c)))
    }

    /// A positional constructor applied to `args`.
    pub(super) fn ctor_call(
        &mut self,
        _ctor: DefId,
        _ty_args: &[Ty],
        _args: &[ExprId],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "constructors with fields", ty)
    }

    /// A named-field construction; `fields` in written order with their
    /// declaration indices.
    pub(super) fn record(
        &mut self,
        _ctor: DefId,
        _ty_args: &[Ty],
        _fields: &[(u32, ExprId)],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "record expressions", ty)
    }

    /// `base.name`, with the field's index in every constructor of the
    /// base's type (D-071).
    pub(super) fn field(
        &mut self,
        _base: ExprId,
        _index_per_ctor: &[(DefId, u32)],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "field access", ty)
    }

    /// `T { ..base, fields }` (D-156).
    pub(super) fn record_update(
        &mut self,
        _base: ExprId,
        _fields: &[UpdateField],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "record updates", ty)
    }

    /// A tuple of `elems`.
    pub(super) fn tuple(
        &mut self,
        _elems: &[ExprId],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "tuples", ty)
    }
}
