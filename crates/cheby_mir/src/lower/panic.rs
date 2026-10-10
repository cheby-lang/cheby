//! `panic`, `todo` and `assert` (D9, §5.14, §11.5, D-096, D-528).
//!
//! W2-A fills this file (plan §4.2). Today both are E0038.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{ExprId, PanicKind};

use crate::ir::Operand;
use crate::lower::body::BodyCx;
use crate::lower::not_supported;

impl BodyCx<'_, '_> {
    /// `panic` or `todo` with an optional message, of type `ty`.
    pub(super) fn panic_expr(
        &mut self,
        kind: PanicKind,
        _message: Option<ExprId>,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let what = match kind {
            PanicKind::Todo => "`todo` expressions",
            PanicKind::Panic => "`panic` expressions",
        };
        self.unsupported(span, what, ty)
    }

    /// `assert cond as message` (D-160). `None` means it diverged.
    pub(super) fn assert_stmt(
        &mut self,
        cond: ExprId,
        _message: Option<ExprId>,
    ) -> Result<Option<()>, Ice> {
        let at = self.expr_data(cond)?.span;
        self.diags.push(not_supported(at, "`assert` statements"));
        Ok(Some(()))
    }
}
