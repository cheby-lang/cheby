//! Interpolated strings, with `show` (D-086) or debug text for `{x:?}`
//! (D2, C11).
//!
//! W2-D fills this file (plan §4.2). Today an interpolated string is
//! E0038.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::InterpPart;

use crate::ir::Operand;
use crate::lower::body::BodyCx;

impl BodyCx<'_, '_> {
    /// An interpolated string of type `ty`.
    pub(super) fn interpolate(
        &mut self,
        _parts: &[InterpPart],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "interpolated strings", ty)
    }
}
