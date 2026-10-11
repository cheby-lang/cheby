//! Conversions between unboxed and uniform representations (D6, D-278,
//! D-453, D-521).
//!
//! Monomorphic code keeps `Int`, `Float`, `Bool` and the sized numbers
//! unboxed; a value is boxed where it enters a uniform position (an
//! argument at a generic parameter, a function's result, a value field of
//! an object) and unboxed where it leaves one (the result of a call, a
//! value read from a uniform position into a known type). `Box` is
//! canonical (plan §5.1): codegen tags what fits and allocates the rest.
//! A `Bool` constant needs no code: it is the static `True` or `False`
//! (D-454).

use cheby_iface::{Ice, Ty, builtin};

use crate::ir::{Const, Operand, Rvalue};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// `op`, a value of type `ty` held in `from`, converted to `to`: a
    /// `box` from an unboxed representation into [`Repr::Value`], an
    /// `unbox` the other way, nothing when they agree.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a conversion between two different unboxed
    /// representations, which closed types never need.
    #[expect(
        clippy::wrong_self_convention,
        reason = "the names of plan §4.2, which the lowering files use"
    )]
    pub(super) fn to_repr(
        &mut self,
        op: Operand,
        from: Repr,
        to: Repr,
        ty: Ty,
    ) -> Result<Operand, Ice> {
        if from == to {
            return Ok(op);
        }
        match (from, to) {
            (Repr::Bool, Repr::Value) => {
                if let Operand::Const(Const::Bool(b)) = op {
                    let ctor = if b { builtin::TRUE } else { builtin::FALSE };
                    return Ok(Operand::Const(Const::StaticCtor(ctor)));
                }
                self.box_value(op, from)
            }
            (_, Repr::Value) => self.box_value(op, from),
            (Repr::Value, _) => {
                let local = self.define(to, Rvalue::Unbox { value: op, to })?;
                Ok(Operand::Local(local))
            }
            _ => Err(Ice::new(format!(
                "a value of type `{}` converted from {from} to {to}",
                self.show(ty)
            ))),
        }
    }

    /// `op`, a value of type `ty` in its type's representation, as a
    /// uniform word.
    #[expect(
        clippy::wrong_self_convention,
        reason = "the names of plan §4.2, which the lowering files use"
    )]
    pub(super) fn to_uniform(&mut self, op: Operand, ty: Ty) -> Result<Operand, Ice> {
        let from = self.repr_of(ty);
        self.to_repr(op, from, Repr::Value, ty)
    }

    /// `op`, a uniform word holding a value of type `ty`, in its type's
    /// representation.
    #[expect(
        clippy::wrong_self_convention,
        reason = "the names of plan §4.2, which the lowering files use"
    )]
    pub(super) fn from_uniform(&mut self, op: Operand, ty: Ty) -> Result<Operand, Ice> {
        let to = self.repr_of(ty);
        self.to_repr(op, Repr::Value, to, ty)
    }

    fn box_value(&mut self, op: Operand, from: Repr) -> Result<Operand, Ice> {
        let local = self.define(Repr::Value, Rvalue::Box { value: op, from })?;
        Ok(Operand::Local(local))
    }
}
