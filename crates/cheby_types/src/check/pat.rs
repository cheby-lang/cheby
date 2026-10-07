//! Patterns (chapter 6). The slice binds only a name or `_` in `let`; the
//! others are E0038 until C5 lands.

use cheby_iface::Ice;
use cheby_syntax::ast::{AstNode, Pat};

use super::BodyCx;
use crate::infer::InferTy;
use crate::thir;

impl BodyCx<'_> {
    /// The pattern of a `let` whose value has type `ty`. `None` if it was
    /// rejected.
    pub(crate) fn let_pattern(
        &mut self,
        pat: Pat<'_>,
        ty: InferTy,
    ) -> Result<Option<thir::Pat>, Ice> {
        match pat {
            Pat::Ident(p) => {
                let local = self
                    .res
                    .bindings
                    .get(&p.syntax().id())
                    .copied()
                    .ok_or_else(|| Ice::new("a `let` binding was not resolved"))?;
                self.set_local(local, ty)?;
                Ok(Some(thir::Pat::Bind(local)))
            }
            Pat::Wildcard(_) => Ok(Some(thir::Pat::Wild)),
            _ => {
                self.unsupported(
                    pat.significant_span(),
                    "patterns other than a name or `_` in `let`",
                )?;
                Ok(None)
            }
        }
    }
}
