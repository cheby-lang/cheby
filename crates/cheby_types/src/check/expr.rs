//! Expressions (chapter 5): literals, paths and the dispatch to calls.

use cheby_iface::{CtorFields, CtorSig, DefId, DefKind, Ice, ItemSig, builtin};
use cheby_resolve::Res;
use cheby_syntax::ast::{AstNode, Expr, PathExpr};

use super::{BodyCx, Expected};
use crate::gate::{unimplemented_expr, unsupported_value};
use crate::infer::{InferTy, VarKind};
use crate::thir::{ExprId, ExprKind, Lit};

impl BodyCx<'_> {
    /// Checks `expr`, against `expected` if given.
    pub(crate) fn check_expr(
        &mut self,
        expr: Expr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let id = self.synth(expr)?;
        let found = self.ty_of(id)?;
        let span = self.span_of(id)?;
        self.coerce(&found, expected, span)?;
        Ok(id)
    }

    /// The expression and its own type, before its context's expectation.
    fn synth(&mut self, expr: Expr<'_>) -> Result<ExprId, Ice> {
        let span = expr.significant_span();
        if let Some(what) = unimplemented_expr(expr) {
            return self.unsupported(span, what);
        }
        match expr {
            Expr::String(s) => match s.plain_value() {
                Some(value) => {
                    self.push(ExprKind::Lit(Lit::Str(value.into())), InferTy::STRING, span)
                }
                None => self.unsupported(span, "interpolated strings"),
            },
            Expr::Path(p) => self.path_expr(p),
            Expr::Call(c) => self.call(c),
            // A `_` outside an argument list is E0025 (phase A).
            Expr::Hole(_) => self.error_expr(span),
            _ => Err(Ice::new("an expression kind the gate does not cover")),
        }
    }

    fn path_expr(&mut self, p: PathExpr<'_>) -> Result<ExprId, Ice> {
        let span = p.significant_span();
        if p.type_args().is_some() {
            return self.unsupported(span, "type arguments on paths");
        }
        let Some(path) = p.path() else {
            return self.error_expr(span);
        };
        match self.path_res(path.syntax().id())? {
            Res::Local(local) => {
                let ty = self.local_ty(local)?;
                self.push(ExprKind::Local(local), ty, span)
            }
            Res::Def(def) => self.def_value(def, span),
            Res::Err => self.error_expr(span),
            Res::Module(_) | Res::TyParam(_) | Res::SelfTy => {
                Err(Ice::new("a value path resolved to a module or a type"))
            }
        }
    }

    pub(crate) fn path_res(&self, node: cheby_syntax::NodeId) -> Result<Res, Ice> {
        self.res
            .paths
            .get(&node)
            .copied()
            .ok_or_else(|| Ice::new("a path of the body was not resolved"))
    }

    /// An item used as a value: a unit constructor in the slice.
    fn def_value(&mut self, def: DefId, span: cheby_diag::Span) -> Result<ExprId, Ice> {
        let item = self.input.item(self.own, def)?;
        match (&item.sig, item.kind) {
            (ItemSig::Unsupported | ItemSig::Error, _) => self.error_expr(span),
            (ItemSig::Ctor(ctor), _) => match ctor.fields {
                CtorFields::Unit => {
                    let (ty, args) = self.ctor_type(def, ctor, span)?;
                    if def != builtin::NIL {
                        // The semantic gate (D-451): only `Nil`.
                        let shown = self.show(&ty);
                        self.report(unsupported_value(span, &shown));
                        return self.error_expr(span);
                    }
                    let id = self.push(
                        ExprKind::Ctor {
                            ctor: def,
                            ty_args: Box::new([]),
                        },
                        ty,
                        span,
                    )?;
                    if !args.is_empty() {
                        self.inst.push((id, args));
                    }
                    Ok(id)
                }
                CtorFields::Positional(_) | CtorFields::Named(_) => {
                    self.unsupported(span, "constructors used as values")
                }
            },
            // The resolver gates both in the slice; these are fallbacks.
            (_, DefKind::Fn) => self.unsupported(span, "functions used as values"),
            (_, DefKind::Const) => self.unsupported(span, "constants"),
            (_, kind) => Err(Ice::new(format!(
                "a value path resolved to a {}",
                kind.describe()
            ))),
        }
    }

    /// The type a constructor builds, with fresh variables for the type's
    /// parameters, and those variables.
    pub(crate) fn ctor_type(
        &mut self,
        ctor_def: DefId,
        ctor: &CtorSig,
        span: cheby_diag::Span,
    ) -> Result<(InferTy, Box<[InferTy]>), Ice> {
        let ty_def = DefId::new(ctor_def.module, ctor.ty);
        let arity = match &self.input.item(self.own, ty_def)?.sig {
            ItemSig::Type(def) => def.generics.len(),
            _ => return Err(Ice::new("a constructor's type is not a type")),
        };
        let args = (0..arity)
            .map(|_| self.infcx.new_var(span, VarKind::General))
            .collect::<Result<Box<[InferTy]>, Ice>>()?;
        let ty = if args.is_empty() {
            InferTy::Known(self.input.globals.types.named(ty_def, []))
        } else {
            InferTy::Named {
                def: ty_def,
                args: args.clone(),
            }
        };
        Ok((ty, args))
    }
}
