//! Calls (§5.5): of top-level functions, of positional constructors and of
//! function values. Arguments are checked against the callee's parameter
//! types as instantiated so far, closures last (§3.12.4, D-234).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{CtorFields, CtorSig, DefId, FnSig, Ice, ItemSig, Ty};
use cheby_resolve::Res;
use cheby_syntax::ast::{Arg, AstNode, CallExpr, Expr};

use super::{BodyCx, Expected};
use crate::gate::{slice_type, unsupported_value};
use crate::infer::{InferTy, VarKind, instantiate};
use crate::thir::{Callee, ExprId, ExprKind};

/// Where a call's parts are, for diagnostics.
#[derive(Clone, Copy)]
struct CallSpans {
    call: Span,
    callee: Span,
    args: Span,
}

impl BodyCx<'_> {
    pub(crate) fn call(&mut self, call: CallExpr<'_>) -> Result<ExprId, Ice> {
        let span = call.significant_span();
        let mut args = Vec::new();
        for arg in call.args() {
            match arg {
                Arg::Expr(e) => args.push(e),
                Arg::Hole(hole) => {
                    return self.unsupported(hole.significant_span(), "function captures");
                }
            }
        }
        let Some(callee) = call.callee() else {
            return self.error_expr(span);
        };
        let spans = CallSpans {
            call: span,
            callee: callee.significant_span(),
            args: call.arg_list().map_or(span, AstNode::significant_span),
        };
        // A path to a function or constructor is a direct call.
        if let Expr::Path(p) = callee
            && p.type_args().is_none()
            && let Some(path) = p.path()
            && let Res::Def(def) = self.path_res(path.syntax().id())?
        {
            let item = self.input.item(self.own, def)?;
            match &item.sig {
                ItemSig::Unsupported | ItemSig::Error => {
                    self.args(&[], &args, spans)?;
                    return self.error_expr(span);
                }
                ItemSig::Fn(sig) => return self.fn_call(def, sig, &args, spans),
                ItemSig::Ctor(ctor) => return self.ctor_call(def, ctor, &args, spans),
                _ => {}
            }
        }
        let callee = self.check_expr(callee, None)?;
        self.value_call(callee, &args, spans)
    }

    /// A call of a top-level function: `f(…)`, `m::f(…)`.
    fn fn_call(
        &mut self,
        def: DefId,
        sig: &FnSig,
        args: &[Expr<'_>],
        spans: CallSpans,
    ) -> Result<ExprId, Ice> {
        if sig.generics.params.iter().any(|p| !p.bounds.is_empty()) {
            // C9 resolves the dictionaries.
            return self.unsupported(
                spans.call,
                "calls of functions with bounded type parameters",
            );
        }
        let types = &self.input.globals.types;
        let inst = sig
            .generics
            .params
            .indices()
            .map(|_| self.infcx.new_var(spans.call, VarKind::General))
            .collect::<Result<Box<[InferTy]>, Ice>>()?;
        let params: Vec<InferTy> = sig
            .params
            .iter()
            .map(|p| instantiate(types, p.ty, &inst))
            .collect();
        let ret = instantiate(types, sig.ret, &inst);
        if params.len() != args.len() {
            let decl = self.input.item(self.own, def)?.span;
            let name = self.input.name_of(self.own, def);
            self.arity_error(Some((name, decl)), params.len(), args.len(), spans);
            self.args(&params, args, spans)?;
            return self.error_expr(spans.call);
        }
        let args = self.args(&params, args, spans)?;
        let id = self.push(
            ExprKind::Call {
                callee: Callee::Fn {
                    def,
                    ty_args: Box::new([]),
                    dicts: Box::new([]),
                },
                args,
            },
            ret,
            spans.call,
        )?;
        if !inst.is_empty() {
            self.inst.push((id, inst));
        }
        Ok(id)
    }

    /// A positional constructor applied to its fields: `Some(x)`.
    fn ctor_call(
        &mut self,
        def: DefId,
        ctor: &CtorSig,
        args: &[Expr<'_>],
        spans: CallSpans,
    ) -> Result<ExprId, Ice> {
        let fields = match &ctor.fields {
            CtorFields::Positional(fields) => fields,
            CtorFields::Unit => {
                let (ty, _) = self.ctor_type(def, ctor, spans.call)?;
                self.not_a_function(&ty, args, spans)?;
                return self.error_expr(spans.call);
            }
            // A named-field constructor is used only with braces (§4.3.2).
            CtorFields::Named(_) => {
                return self.unsupported(spans.call, "calls of named-field constructors");
            }
        };
        let (ty, inst) = self.ctor_type(def, ctor, spans.call)?;
        if !self.is_slice_value(&ty)? {
            // The semantic gate (D-451), before the arguments so
            // that only the outermost construct is reported.
            let shown = self.show(&ty);
            self.report(unsupported_value(spans.call, &shown));
            return self.error_expr(spans.call);
        }
        let types = &self.input.globals.types;
        let params: Vec<InferTy> = fields
            .iter()
            .map(|&field| instantiate(types, field, &inst))
            .collect();
        if params.len() != args.len() {
            let decl = self.input.item(self.own, def)?.span;
            let name = self.input.name_of(self.own, def);
            self.arity_error(Some((name, decl)), params.len(), args.len(), spans);
            self.args(&params, args, spans)?;
            return self.error_expr(spans.call);
        }
        let args = self.args(&params, args, spans)?;
        let id = self.push(
            ExprKind::Call {
                callee: Callee::Ctor {
                    ctor: def,
                    ty_args: Box::new([]),
                },
                args,
            },
            ty,
            spans.call,
        )?;
        if !inst.is_empty() {
            self.inst.push((id, inst));
        }
        Ok(id)
    }

    /// A call of any other expression, which must have a function type.
    fn value_call(
        &mut self,
        callee: ExprId,
        args: &[Expr<'_>],
        spans: CallSpans,
    ) -> Result<ExprId, Ice> {
        let callee_ty = self.ty_of(callee)?;
        let (params, ret) = match self.infcx.resolve(&callee_ty)? {
            InferTy::Fn { params, ret } => (params.into_vec(), *ret),
            InferTy::Known(Ty::ERROR) => {
                self.args(&[], args, spans)?;
                return self.error_expr(spans.call);
            }
            InferTy::Var(_) => {
                // A value whose type is not known yet becomes a function
                // of this many parameters (local, monomorphic, D-123).
                let params = args
                    .iter()
                    .map(|arg| self.infcx.new_var(arg.significant_span(), VarKind::General))
                    .collect::<Result<Vec<InferTy>, Ice>>()?;
                let ret = self.infcx.new_var(spans.call, VarKind::General)?;
                let shape = InferTy::Fn {
                    params: params.clone().into(),
                    ret: Box::new(ret.clone()),
                };
                if self.infcx.unify(&callee_ty, &shape)?.is_err() {
                    self.not_a_function(&callee_ty, args, spans)?;
                    return self.error_expr(spans.call);
                }
                (params, ret)
            }
            _ => {
                self.not_a_function(&callee_ty, args, spans)?;
                return self.error_expr(spans.call);
            }
        };
        if params.len() != args.len() {
            self.arity_error(None, params.len(), args.len(), spans);
            self.args(&params, args, spans)?;
            return self.error_expr(spans.call);
        }
        let args = self.args(&params, args, spans)?;
        self.push(
            ExprKind::Call {
                callee: Callee::Value(callee),
                args,
            },
            ret,
            spans.call,
        )
    }

    /// Checks the arguments: those that are not closures first, left to
    /// right, then the closures (§3.12.4). Argument `n` is checked against
    /// `params[n]` when there is one. Returns them in source order.
    fn args(
        &mut self,
        params: &[InferTy],
        args: &[Expr<'_>],
        spans: CallSpans,
    ) -> Result<Box<[ExprId]>, Ice> {
        let mut out: Vec<Option<ExprId>> = vec![None; args.len()];
        let closures_last = args
            .iter()
            .enumerate()
            .filter(|(_, a)| !matches!(a, Expr::Closure(_)))
            .chain(
                args.iter()
                    .enumerate()
                    .filter(|(_, a)| matches!(a, Expr::Closure(_))),
            );
        for (n, &arg) in closures_last {
            let expected = params.get(n).map(|ty| {
                Expected::new(
                    ty.clone(),
                    Some((
                        spans.callee,
                        "arguments to this function are incorrect".to_owned(),
                    )),
                )
            });
            let id = self.check_expr(arg, expected.as_ref())?;
            if let Some(slot) = out.get_mut(n) {
                *slot = Some(id);
            }
        }
        out.into_iter()
            .map(|id| id.ok_or_else(|| Ice::new("an argument was not checked")))
            .collect()
    }

    /// Whether a value of type `ty` is one the slice can run.
    fn is_slice_value(&mut self, ty: &InferTy) -> Result<bool, Ice> {
        Ok(match self.infcx.shallow(ty)? {
            InferTy::Known(t) => slice_type(t),
            _ => false,
        })
    }

    /// E0049, after checking the arguments for their own errors.
    fn not_a_function(
        &mut self,
        ty: &InferTy,
        args: &[Expr<'_>],
        spans: CallSpans,
    ) -> Result<(), Ice> {
        let shown = self.show(ty);
        self.report(
            Diagnostic::new(
                codes::NOT_A_FUNCTION,
                format!("expected a function, found `{shown}`"),
            )
            .with_primary(spans.callee, format!("this has type `{shown}`")),
        );
        self.args(&[], args, spans)?;
        Ok(())
    }

    /// E0048.
    fn arity_error(
        &mut self,
        callee: Option<(&str, Option<Span>)>,
        expected: usize,
        given: usize,
        spans: CallSpans,
    ) {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut diag = Diagnostic::new(
            codes::WRONG_ARGUMENT_COUNT,
            format!(
                "this function takes {expected} argument{} but {given} argument{} {} supplied",
                plural(expected),
                plural(given),
                if given == 1 { "was" } else { "were" }
            ),
        )
        .with_primary(
            spans.args,
            format!("expected {expected} argument{}", plural(expected)),
        );
        if let Some((name, Some(decl))) = callee {
            diag = diag.with_secondary(decl, format!("`{name}` is declared here"));
        }
        self.report(diag);
    }
}
