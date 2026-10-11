//! String interpolation (§5.3.2, C11).
//!
//! Each `{path}` is evaluated like a field-access expression: its root
//! name is resolved by the resolver (`BodyRes::names`), and each `.field`
//! is a field access (`record.rs`), which needs the type before it to be
//! known at that point (D-213). `{x}` needs `Show`, resolved at the end of the body
//! (D-086, D-234); `{x:?}` uses debug printing and needs nothing.

use cheby_diag::Span;
use cheby_iface::Ice;
use cheby_resolve::Res;
use cheby_syntax::SyntaxKind;
use cheby_syntax::ast::{AstNode, AstToken, Interpolation, StringLit, StringPart};
use cheby_syntax::lexer::literal::string_value;

use super::{BodyCx, Deferred};
use crate::infer::InferTy;
use crate::thir::{ExprId, ExprKind, InterpPart};

impl BodyCx<'_> {
    /// An interpolated string: its text and values in order, with a
    /// deferred `Show` constraint for each `{x}`.
    pub(crate) fn interpolation(&mut self, s: StringLit<'_>) -> Result<ExprId, Ice> {
        let span = s.significant_span();
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut shows = Vec::new();
        for part in s.parts() {
            match part {
                StringPart::Text(t) => {
                    text.push_str(&string_value([(SyntaxKind::STRING_TEXT, t.text())]));
                }
                StringPart::Escape(e) => {
                    text.push_str(&string_value([(SyntaxKind::STRING_ESCAPE, e.text())]));
                }
                StringPart::Interpolation(i) => {
                    if !text.is_empty() {
                        parts.push(InterpPart::Text(std::mem::take(&mut text).into()));
                    }
                    let value = self.interp_value(i)?;
                    let debug = i.is_debug();
                    if !debug {
                        shows.push((parts.len(), self.ty_of(value)?));
                    }
                    parts.push(InterpPart::Value {
                        expr: value,
                        debug,
                        show: None,
                    });
                }
            }
        }
        if !text.is_empty() {
            parts.push(InterpPart::Text(text.into()));
        }
        let id = self.push(
            ExprKind::Interpolate {
                parts: parts.into(),
            },
            InferTy::STRING,
            span,
        )?;
        for (part, ty) in shows {
            if ty != InferTy::ERROR {
                self.defer(Deferred::Show { expr: id, part, ty });
            }
        }
        Ok(id)
    }

    /// The value of `{root.field…}`.
    fn interp_value(&mut self, i: Interpolation<'_>) -> Result<ExprId, Ice> {
        let span = i.significant_span();
        let Some(root) = i.root() else {
            return self.error_expr(span);
        };
        let root_span = root.span();
        let res = self
            .res
            .names
            .get(&i.syntax().id())
            .copied()
            .ok_or_else(|| Ice::new("an interpolation was not resolved"))?;
        let mut value = match res {
            Res::Local(local) => {
                let ty = self.local_ty(local)?;
                let kind = if self.local_fns.contains(&local) {
                    ExprKind::SelfRef { local }
                } else {
                    ExprKind::Local(local)
                };
                self.push(kind, ty, root_span)?
            }
            Res::Def(def) => self.def_value(def, None, root_span)?,
            Res::Err => return self.error_expr(span),
            Res::Module(_) | Res::TyParam(_) | Res::SelfTy => {
                return Err(Ice::new("an interpolation resolved to a module or a type"));
            }
        };
        for field in i.fields() {
            let field_span = field.span();
            let whole = Span::new(root_span.file, root_span.range.cover(field_span.range));
            value = self.read_field(value, field.text(), field_span, whole)?;
        }
        Ok(value)
    }
}
