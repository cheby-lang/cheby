//! Reading module constants from their slots (E6, D-478).
//!
//! W3-D fills this file (plan §4.2). Today a constant reference is E0038.

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, Ty};

use crate::ir::Operand;
use crate::lower::body::BodyCx;

impl BodyCx<'_, '_> {
    /// The constant `def`, of type `ty`.
    pub(super) fn const_ref(
        &mut self,
        _def: DefId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "constants", ty)
    }
}
