//! Dictionaries, interface calls, `dyn` calls, conversions to `dyn` and
//! type descriptors (D5, D-236, D-453, D-499, D-524).
//!
//! W3-A fills this file and W4-A the `dyn` parts (plan §4.2). Today all
//! of them are E0038.

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, Ty};
use cheby_types::thir::{Dict, ExprId};

use crate::ir::Operand;
use crate::lower::body::BodyCx;

impl BodyCx<'_, '_> {
    /// The dictionary object for `dict`, used by the call at `span`.
    pub(super) fn dict(&mut self, _dict: &Dict, span: Span) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "calls of functions with interface bounds", Ty::ERROR)
    }

    /// A call of the interface function `iface_fn` through `dict`.
    pub(super) fn call_iface(
        &mut self,
        _iface_fn: DefId,
        _dict: &Dict,
        _args: &[ExprId],
        ret_ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "calls of interface functions", ret_ty)
    }

    /// A call of the interface function `iface_fn` on the `dyn` value
    /// `args[0]` (D-117).
    pub(super) fn call_dyn(
        &mut self,
        _iface_fn: DefId,
        _args: &[ExprId],
        ret_ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "calls through `dyn` values", ret_ty)
    }

    /// `value` converted to `dyn iface`, of type `ty` (C10).
    pub(super) fn dyn_coerce(
        &mut self,
        _value: ExprId,
        _iface: DefId,
        _dict: &Dict,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "conversions to `dyn`", ty)
    }
}
