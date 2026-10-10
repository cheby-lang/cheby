//! List literals and spreads (D2, D-157).
//!
//! W2-D fills this file (plan §4.2). Today a list literal is E0038.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::ListItem;

use crate::ir::Operand;
use crate::lower::body::BodyCx;

impl BodyCx<'_, '_> {
    /// A list literal of type `ty`, items in order.
    pub(super) fn list(
        &mut self,
        _items: &[ListItem],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "list expressions", ty)
    }
}
