//! Expected types and conversions (§3.12.5): where a checked expression
//! meets the type its context expects, and E0047.
//!
//! There is no subtyping (D-235). The two conversions happen only where an
//! expression is checked against an expected type, never inside another
//! type:
//!
//! - `Never` takes the expected type ([`BodyCx::coerce`]).
//! - A value converts to `dyn I` where `dyn I` is expected and its type is
//!   not a plain type variable ([`BodyCx::coerce_expr`]): the expression is
//!   wrapped in a [`ExprKind::DynCoerce`], and the check that its type
//!   satisfies `I` waits for the end of the body (D-237). A numeric literal
//!   converts too, and its type is still fixed later. A plain type
//!   variable unifies with `dyn I`.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, Ice, Ty, TyKind};

use super::constraints::placeholder_dict;
use super::{BodyCx, Deferred, Expected};
use crate::infer::{InferTy, Mismatch, VarKind};
use crate::thir::{ExprId, ExprKind};

impl BodyCx<'_> {
    /// Checks that a value of type `found` at `span` fits `expected`, and
    /// returns its type in context. An expression of type `Never` takes
    /// the expected type when one is known (§3.10, D-235); otherwise the
    /// types are unified, and a mismatch is E0047 and gives
    /// [`InferTy::ERROR`]. This never packages a value as `dyn`: use
    /// [`BodyCx::coerce_expr`] where the expression is at hand.
    pub fn coerce(
        &mut self,
        found: &InferTy,
        expected: Option<&Expected>,
        span: Span,
    ) -> Result<InferTy, Ice> {
        let Some(expected) = expected else {
            return Ok(found.clone());
        };
        if self.infcx.shallow(found)? == InferTy::NEVER {
            let want = self.infcx.shallow(&expected.ty)?;
            // A general variable learns `Never`; a literal's variable is
            // numeric whatever it becomes, and `Never` converts to it.
            let general = matches!(want, InferTy::Var(_))
                && self.infcx.var_kind(&want)? == Some(VarKind::General);
            if !general {
                return Ok(expected.ty.clone());
            }
        }
        match self.infcx.unify(&expected.ty, found)? {
            Ok(()) => Ok(found.clone()),
            Err(mismatch) => {
                self.mismatch(&expected.ty, found, mismatch, span, expected.origin.clone());
                Ok(InferTy::ERROR)
            }
        }
    }

    /// Checks the expression `id` against `expected`, with both
    /// conversions of §3.12.5, and returns the expression to use in its
    /// place: `id` itself, or a new [`ExprKind::DynCoerce`] around it. On a
    /// mismatch (E0047) the expression's type becomes [`InferTy::ERROR`],
    /// so enclosing expressions do not report it again.
    pub fn coerce_expr(&mut self, id: ExprId, expected: Option<&Expected>) -> Result<ExprId, Ice> {
        let Some(expected) = expected else {
            return Ok(id);
        };
        let found = self.infcx.shallow(&self.ty_of(id)?)?;
        let span = self.span_of(id)?;
        if found != InferTy::NEVER
            && let Some(iface) = self.dyn_iface(&expected.ty)?
        {
            let want = self.infcx.shallow(&expected.ty)?;
            let packages = match &found {
                InferTy::Known(Ty::ERROR) => false,
                // A plain variable becomes `dyn I`; a numeric literal is
                // known to be numeric and converts (D-237).
                InferTy::Var(_) => self.infcx.var_kind(&found)? != Some(VarKind::General),
                // Already this `dyn`: not packaged again.
                other => *other != want,
            };
            if packages {
                let coerced = self.push(
                    ExprKind::DynCoerce {
                        value: id,
                        iface,
                        dict: placeholder_dict(iface),
                    },
                    want,
                    span,
                )?;
                self.defer(Deferred::DynCoerce {
                    expr: coerced,
                    iface,
                    from: found,
                });
                return Ok(coerced);
            }
        }
        if self.coerce(&found, Some(expected), span)? == InferTy::ERROR && found != InferTy::ERROR {
            self.set_ty(id, InferTy::ERROR)?;
        }
        Ok(id)
    }

    /// The interface of `t` if it is a `dyn` type.
    pub(crate) fn dyn_iface(&mut self, t: &InferTy) -> Result<Option<DefId>, Ice> {
        Ok(match self.infcx.shallow(t)? {
            InferTy::Known(ty) => match self.input.globals.types.kind(ty) {
                TyKind::Dyn { iface, .. } => Some(*iface),
                _ => None,
            },
            _ => None,
        })
    }

    /// Replaces the inference type of expression `id`.
    pub(crate) fn set_ty(&mut self, id: ExprId, ty: InferTy) -> Result<(), Ice> {
        let slot = self
            .expr_tys
            .get_mut(id)
            .ok_or_else(|| Ice::new(format!("no expression {id:?}")))?;
        *slot = ty;
        Ok(())
    }

    /// E0047.
    pub fn mismatch(
        &mut self,
        expected: &InferTy,
        found: &InferTy,
        mismatch: Mismatch,
        span: Span,
        origin: Option<(Span, String)>,
    ) {
        let label = format!(
            "expected `{}`, found `{}`",
            self.show(expected),
            self.show(found)
        );
        let mut diag =
            Diagnostic::new(codes::TYPE_MISMATCH, "mismatched types").with_primary(span, label);
        if let Some((origin_span, text)) = origin {
            diag = diag.with_secondary(origin_span, text);
        }
        if mismatch == Mismatch::Occurs {
            diag = diag.with_note("the type would have to contain itself");
        }
        self.report(diag);
    }
}
