//! Patterns (D3): `case`, `let`, `let assert` and parameter patterns, as
//! decision trees.
//!
//! W2-B fills this file (plan §4.2). Today `let` with a name or `_`
//! lowers; every other pattern is E0038.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{self, Arm, ExprId, PatKind};

use crate::ir::Operand;
use crate::lower::body::BodyCx;
use crate::lower::not_supported;

impl BodyCx<'_, '_> {
    /// `case subjects { arms }` of type `ty`; `tail` marks §5.15.2 tail
    /// position, which the arms' bodies inherit.
    pub(super) fn case(
        &mut self,
        _subjects: &[ExprId],
        _arms: &[Arm],
        ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "`case` expressions", ty)
    }

    /// `let pat = init`. `None` means `init` diverged.
    pub(super) fn let_stmt(&mut self, pat: &thir::Pat, init: ExprId) -> Result<Option<()>, Ice> {
        let Some(op) = self.expr(init, false)? else {
            return Ok(None);
        };
        match &pat.kind {
            PatKind::Bind(id) => {
                let decl = self
                    .body
                    .local(*id)
                    .ok_or_else(|| Ice::new("a `let` without its local"))?;
                let (name, ty) = (decl.name, decl.ty);
                let init_ty = self.ty(init)?;
                let op = self.to_repr(op, self.repr_of(init_ty), self.repr_of(ty), init_ty)?;
                self.bind_name(*id, name, ty, op)?;
            }
            // The value is unused; the RC pass drops it.
            PatKind::Wild => {}
            _ => {
                self.diags.push(not_supported(
                    pat.span,
                    "patterns other than a name or `_` in `let`",
                ));
                self.bind_pattern(pat)?;
            }
        }
        Ok(Some(()))
    }

    /// `let assert pat = init as message` (§11.4). `None` means it
    /// diverged.
    pub(super) fn let_assert(
        &mut self,
        pat: &thir::Pat,
        _init: ExprId,
        _message: Option<ExprId>,
    ) -> Result<Option<()>, Ice> {
        self.diags
            .push(not_supported(pat.span, "`let assert` statements"));
        self.bind_pattern(pat)?;
        Ok(Some(()))
    }

    /// Binds a closure, local-function, `use`, capture or pipe parameter
    /// pattern (irrefutable, D-242) to `value`, a uniform word (D-453).
    #[expect(
        dead_code,
        reason = "called by closure lowering, which W2-C adds (plan §4.2)"
    )]
    pub(super) fn bind_param(&mut self, pat: &thir::Pat, value: Operand) -> Result<(), Ice> {
        match &pat.kind {
            PatKind::Bind(id) => {
                let decl = self
                    .body
                    .local(*id)
                    .ok_or_else(|| Ice::new("a parameter without its local"))?;
                let (name, ty) = (decl.name, decl.ty);
                let op = self.from_uniform(value, ty)?;
                self.bind_name(*id, name, ty, op)
            }
            // The RC pass drops the unused value.
            PatKind::Wild => Ok(()),
            _ => {
                self.diags.push(not_supported(
                    pat.span,
                    "patterns other than a name or `_` in parameters",
                ));
                self.bind_pattern(pat)
            }
        }
    }
}
