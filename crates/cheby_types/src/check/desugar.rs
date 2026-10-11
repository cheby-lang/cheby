//! Desugaring (C7) into THIR, while checking:
//!
//! - `use p1, p2 <- f(a)` and the rest of the block: a call of `f` whose
//!   last argument is a closure with [`ParamOrigin::Use`] parameters, the
//!   binders' patterns, and the rest of the block as its body (§5.10).
//!   `use p <- f` calls `f` with the callback alone.
//! - `x |> f(a, _)`: the call `f(a, x)`, with `x` bound to a synthetic
//!   local first when something before it in the call could have an
//!   effect, so `x` is evaluated first (§5.15.1, D-244). `x |> g` calls
//!   `g` with `x` alone (D-245).
//! - A positional constructor or an interface function used as a value:
//!   a closure that calls it (D-500).
//!
//! Captures `f(a, _)` are in `call.rs` (D-155), parenthesized expressions
//! leave no node (`expr.rs`). No `use`, pipe, hole or parenthesized
//! expression reaches THIR.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, Ice, ItemSig, Ty};
use cheby_syntax::ast::{Arg, AstNode, BinExpr, Expr, Stmt, UseStmt};

use super::call::{ArgSrc, CallSpans, Signature, Target};
use super::{BodyCx, Deferred, Expected, PatSite, VarNote};
use crate::infer::{InferTy, instantiate};
use crate::thir::{self, Callee, ExprId, ExprKind, ParamOrigin};

/// The callback of a `use`: its binders, and the rest of the block as its
/// body.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Callback<'a> {
    pub use_stmt: UseStmt<'a>,
    /// The statements after the `use`.
    pub rest: &'a [Stmt<'a>],
    /// The rest of the block, or the `use` when nothing follows it.
    pub span: Span,
}

impl BodyCx<'_> {
    /// A `use` statement and the statements after it (§5.10): the value
    /// of the enclosing block.
    pub(crate) fn use_stmt<'a>(
        &mut self,
        u: UseStmt<'a>,
        rest: &'a [Stmt<'a>],
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = u.significant_span();
        let rest_span = match (rest.first(), rest.last()) {
            (Some(first), Some(last)) => Span::new(
                span.file,
                first
                    .significant_span()
                    .range
                    .cover(last.significant_span().range),
            ),
            _ => span,
        };
        let callback = Callback {
            use_stmt: u,
            rest,
            span: rest_span,
        };
        let Some(rhs) = u.value() else {
            return self.error_expr(span);
        };
        let call = match rhs {
            Expr::Call(c) if !c.args().any(|a| matches!(a, Arg::Hole(_))) => Some(c),
            _ => None,
        };
        if let Some(c) = call
            && let Some(callee) = c.callee()
        {
            let spans = CallSpans {
                call: span,
                callee: callee.significant_span(),
                args: c.arg_list().map_or(span, AstNode::significant_span),
            };
            let mut args: Vec<ArgSrc<'_>> = c
                .args()
                .filter_map(|a| match a {
                    Arg::Expr(e) => Some(ArgSrc::Expr(e)),
                    Arg::Hole(_) => None,
                })
                .collect();
            args.push(ArgSrc::Callback(callback));
            let sig = self.callee(callee, args.len(), spans)?;
            return self.apply(sig, &args, spans, expected);
        }
        // `use x <- f`: the function expression is called with the
        // callback alone.
        let spans = CallSpans {
            call: span,
            callee: rhs.significant_span(),
            args: rhs.significant_span(),
        };
        let sig = self.callee(rhs, 1, spans)?;
        self.apply(sig, &[ArgSrc::Callback(callback)], spans, expected)
    }

    /// The callback of a `use`, against the expected function type of the
    /// callee's last parameter.
    pub(crate) fn callback(
        &mut self,
        cb: Callback<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let binders: Vec<_> = cb.use_stmt.binders().collect();
        if let Some(e) = expected
            && let InferTy::Fn { params, .. } = self.infcx.resolve(&e.ty)?
            && params.len() != binders.len()
        {
            return self.binder_count(cb, params.len(), binders.len(), e);
        }
        let shape = match expected {
            Some(e) => match self.infcx.resolve(&e.ty)? {
                InferTy::Fn { params, ret } if params.len() == binders.len() => {
                    Some((params, *ret))
                }
                // The callee had an error: nothing more is reported.
                InferTy::Known(cheby_iface::Ty::ERROR) => {
                    Some((vec![InferTy::ERROR; binders.len()].into(), InferTy::ERROR))
                }
                _ => None,
            },
            None => None,
        };
        let mut params = Vec::with_capacity(binders.len());
        let mut tys = Vec::with_capacity(binders.len());
        for (n, binder) in binders.iter().enumerate() {
            let span = binder.significant_span();
            let Some(pattern) = binder.pattern() else {
                self.errored = true;
                continue;
            };
            let annotated = match binder.ty() {
                Some(ty) => Some(self.lower_written_ty(ty)?),
                None => None,
            };
            let from_context = shape.as_ref().and_then(|(ps, _)| ps.get(n)).cloned();
            let ty = match (annotated, from_context) {
                (Some(ann), Some(ctx)) => {
                    let _ = self.infcx.unify(&ann, &ctx)?;
                    ann
                }
                (Some(ty), None) | (None, Some(ty)) => ty,
                (None, None) => self.param_var(pattern, span)?,
            };
            let pat = self.check_pat(pattern, &ty, PatSite::UseBinder)?;
            tys.push(ty);
            params.push(thir::Param {
                pat,
                span,
                origin: ParamOrigin::Use,
            });
        }
        let ret_expected = shape.map(|(_, ret)| Expected::plain(ret));
        let body = self.block_stmts(cb.rest, cb.span, ret_expected.as_ref())?;
        let ret = match ret_expected {
            Some(e) => e.ty,
            None => self.ty_of(body)?,
        };
        let id = self.push(
            ExprKind::Closure {
                params: params.into(),
                body,
            },
            InferTy::Fn {
                params: tys.into(),
                ret: Box::new(ret),
            },
            cb.use_stmt.significant_span(),
        )?;
        self.coerce_expr(id, expected)
    }

    /// E0047 for a `use` with `given` binders whose callback must take
    /// `wanted` parameters (§5.10); the callback is checked without
    /// expectations, for its own errors (D-495).
    fn binder_count(
        &mut self,
        cb: Callback<'_>,
        wanted: usize,
        given: usize,
        expected: &Expected,
    ) -> Result<ExprId, Ice> {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut diag = Diagnostic::new(codes::TYPE_MISMATCH, "mismatched types")
            .with_primary(
                cb.use_stmt.significant_span(),
                format!(
                    "this `use` has {given} binder{}, but the callback takes {wanted} parameter{}",
                    plural(given),
                    plural(wanted)
                ),
            )
            .with_note(format!(
                "the callback must have type `{}`",
                self.show(&expected.ty)
            ));
        if let Some((span, _)) = &expected.origin {
            diag = diag.with_secondary(*span, "the function called by this `use`");
        }
        self.report(diag);
        let error = Expected::plain(InferTy::ERROR);
        let id = self.callback(cb, Some(&error))?;
        self.set_ty(id, InferTy::ERROR)?;
        Ok(id)
    }

    /// A fresh variable for a parameter whose type is not written, noted
    /// with its name for E0057.
    pub(crate) fn param_var(
        &mut self,
        pattern: cheby_syntax::ast::Pat<'_>,
        span: Span,
    ) -> Result<InferTy, Ice> {
        let name = match pattern {
            cheby_syntax::ast::Pat::Ident(p) => p.name().map(|n| {
                self.input
                    .globals
                    .names
                    .intern(cheby_syntax::ast::AstToken::text(n))
            }),
            _ => None,
        };
        self.noted_var(span, VarNote::Param { name })
    }

    /// A pipe `x |> rhs` (§5.16, D-244, D-245).
    pub(crate) fn pipe(
        &mut self,
        b: BinExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = b.significant_span();
        let (Some(lhs), Some(rhs)) = (b.lhs(), b.rhs()) else {
            return self.error_expr(span);
        };
        if let Expr::Call(c) = rhs
            && let Some(hole) = c.args().position(|a| matches!(a, Arg::Hole(_)))
            && let Some(callee) = c.callee()
        {
            let spans = CallSpans {
                call: span,
                callee: callee.significant_span(),
                args: c.arg_list().map_or(span, AstNode::significant_span),
            };
            let args: Vec<ArgSrc<'_>> = c
                .args()
                .map(|a| match a {
                    Arg::Expr(e) => ArgSrc::Expr(e),
                    Arg::Hole(_) => ArgSrc::Expr(lhs),
                })
                .collect();
            let sig = self.callee(callee, args.len(), spans)?;
            return self.pipe_call(sig, &args, hole, spans, expected);
        }
        let spans = CallSpans {
            call: span,
            callee: rhs.significant_span(),
            args: lhs.significant_span(),
        };
        if matches!(rhs, Expr::Path(_)) {
            // `x |> g`: `g(x)`; a path has no effect, so `x` is still
            // evaluated first.
            let sig = self.callee(rhs, 1, spans)?;
            return self.apply(sig, &[ArgSrc::Expr(lhs)], spans, expected);
        }
        // `x |> (make(c))`: `x` first, then the function value.
        let x = self.check_expr(lhs, None)?;
        let x_ty = self.ty_of(x)?;
        let ret = self.fresh_var(span)?;
        let shape = Expected::plain(InferTy::Fn {
            params: Box::new([x_ty]),
            ret: Box::new(ret),
        });
        let callee = self.check_expr(rhs, Some(&shape))?;
        let sig = self.value_callee(callee, 1, spans)?;
        if matches!(sig.target, Target::Error) {
            return self.error_expr(span);
        }
        let [param] = &sig.params[..] else {
            self.arity_error(None, sig.params.len(), 1, spans);
            return self.error_expr(span);
        };
        let param = Expected::new(
            param.clone(),
            Some((
                spans.callee,
                "arguments to this function are incorrect".to_owned(),
            )),
        );
        let x = self.coerce_expr(x, Some(&param))?;
        let mut stmts = Vec::new();
        let x = self.bind_once(x, &mut stmts, "%piped")?;
        let call = self.build_call(sig, vec![x], span)?;
        self.wrap_block(stmts, call, span)
    }

    /// The call of a pipe into `f(a, _)`: `args` with `x` at `hole`.
    fn pipe_call(
        &mut self,
        sig: Signature,
        args: &[ArgSrc<'_>],
        hole: usize,
        spans: CallSpans,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        if matches!(sig.target, Target::Error) || sig.params.len() != args.len() {
            return self.apply(sig, args, spans, expected);
        }
        self.expect_result(&sig, expected)?;
        let mut ids = self.check_args(&sig.params, args, spans)?;
        // `x` is evaluated before the rest of the right side (§5.15.1):
        // bind it first unless nothing comes before it (D-498).
        let mut stmts = Vec::new();
        let first = hole == 0 && !matches!(sig.target, Target::Value(_));
        if !first && let Some(slot) = ids.get_mut(hole) {
            *slot = self.bind_once(*slot, &mut stmts, "%piped")?;
        }
        let call = self.build_call(sig, ids, spans.call)?;
        self.wrap_block(stmts, call, spans.call)
    }

    /// `tail` after `stmts`, or `tail` alone.
    fn wrap_block(
        &mut self,
        stmts: Vec<thir::Stmt>,
        tail: ExprId,
        span: Span,
    ) -> Result<ExprId, Ice> {
        if stmts.is_empty() {
            return Ok(tail);
        }
        let ty = self.ty_of(tail)?;
        self.push(
            ExprKind::Block {
                stmts: stmts.into(),
                tail: Some(tail),
            },
            ty,
            span,
        )
    }

    /// A positional constructor used as a value, `Some`: the closure
    /// `fn(a) { Some(a) }`.
    pub(crate) fn ctor_closure(
        &mut self,
        def: DefId,
        fields: &[Ty],
        ty: InferTy,
        args: Box<[InferTy]>,
        span: Span,
    ) -> Result<ExprId, Ice> {
        let types = &self.input.globals.types;
        let params: Vec<InferTy> = fields
            .iter()
            .map(|&field| instantiate(types, field, &args))
            .collect();
        let (thir_params, refs) = self.closure_params(&params, span)?;
        let call = self.push(
            ExprKind::Call {
                callee: Callee::Ctor {
                    ctor: def,
                    ty_args: Box::new([]),
                },
                args: refs.into(),
            },
            ty.clone(),
            span,
        )?;
        if !args.is_empty() {
            self.inst.push((call, args));
        }
        self.push(
            ExprKind::Closure {
                params: thir_params.into(),
                body: call,
            },
            InferTy::Fn {
                params: params.into(),
                ret: Box::new(ty),
            },
            span,
        )
    }

    /// An interface function used as a value, `Show::show`: the closure
    /// `fn(a) { Show::show(a) }`, whose call is resolved at the end of the
    /// body like any interface call (D-500).
    pub(crate) fn iface_fn_closure(&mut self, def: DefId, span: Span) -> Result<ExprId, Ice> {
        let ItemSig::InterfaceFn(isig) = &self.input.item(self.own, def)?.sig else {
            return Err(Ice::new("an interface function without its signature"));
        };
        let isig = isig.clone();
        let self_ty = self.fresh_var(span)?;
        let types = &self.input.globals.types;
        let inst = [self_ty.clone()];
        let params: Vec<InferTy> = isig
            .params
            .iter()
            .map(|&param| instantiate(types, param, &inst))
            .collect();
        let ret = instantiate(types, isig.ret, &inst);
        let (thir_params, refs) = self.closure_params(&params, span)?;
        let call = self.push(
            ExprKind::Call {
                callee: Callee::Dyn { iface_fn: def },
                args: refs.into(),
            },
            ret.clone(),
            span,
        )?;
        self.defer(Deferred::IfaceCall {
            expr: call,
            iface_fn: def,
            self_ty,
        });
        self.push(
            ExprKind::Closure {
                params: thir_params.into(),
                body: call,
            },
            InferTy::Fn {
                params: params.into(),
                ret: Box::new(ret),
            },
            span,
        )
    }

    /// Synthetic parameters of the given types for a desugared closure, and
    /// a read of each.
    fn closure_params(
        &mut self,
        params: &[InferTy],
        span: Span,
    ) -> Result<(Vec<thir::Param>, Vec<ExprId>), Ice> {
        let name = self.synthetic_name("%param");
        let mut thir_params = Vec::with_capacity(params.len());
        let mut refs = Vec::with_capacity(params.len());
        for ty in params {
            let local = self.new_local(name, ty.clone(), span)?;
            let pat = self.bind_pat(local, ty.clone(), span)?;
            thir_params.push(thir::Param {
                pat,
                span,
                origin: ParamOrigin::Closure,
            });
            refs.push(self.push(ExprKind::Local(local), ty.clone(), span)?);
        }
        Ok((thir_params, refs))
    }
}
