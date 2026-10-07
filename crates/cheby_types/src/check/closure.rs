//! Anonymous and local named functions (§5.7, §5.8, C3).
//!
//! Their parameters are irrefutable patterns (D-242), checked with
//! [`super::BodyCx::check_pat`] at [`super::PatSite::Param`]; a parameter
//! without an annotation takes its type from the expected function type,
//! or a fresh variable. Both are monomorphic (D-123). A local function is
//! checked where it is declared, and inside its own body its name is a
//! [`ExprKind::SelfRef`] (D-110).

use cheby_iface::Ice;
use cheby_syntax::ast::{self, AstNode, AstToken, LocalFn, Param, RetType};

use super::{BodyCx, Expected, PatSite, VarNote};
use crate::infer::InferTy;
use crate::thir::{self, ExprId, ExprKind, ParamOrigin};

impl BodyCx<'_> {
    /// An anonymous function `fn(x) { … }`, against `expected`.
    pub(crate) fn closure(
        &mut self,
        c: ast::Closure<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = c.significant_span();
        let written: Vec<Param<'_>> = c.params().collect();
        let shape = match expected {
            Some(e) => match self.infcx.resolve(&e.ty)? {
                InferTy::Fn { params, ret } if params.len() == written.len() => {
                    Some((params, *ret))
                }
                // The callee had an error: nothing more is reported.
                InferTy::Known(cheby_iface::Ty::ERROR) => {
                    Some((vec![InferTy::ERROR; written.len()].into(), InferTy::ERROR))
                }
                _ => None,
            },
            None => None,
        };
        let context = shape.as_ref().map(|(ps, _)| &ps[..]);
        let (params, tys) = self.fn_params(&written, context, ParamOrigin::Closure)?;
        let ret_expected = self.ret_expected(c.ret_type(), shape.map(|(_, ret)| ret))?;
        let Some(block) = c.body() else {
            return self.error_expr(span);
        };
        let body = self.block(block, ret_expected.as_ref())?;
        let ret = match ret_expected {
            Some(e) => e.ty,
            None => self.ty_of(body)?,
        };
        self.push(
            ExprKind::Closure {
                params: params.into(),
                body,
            },
            InferTy::Fn {
                params: tys.into(),
                ret: Box::new(ret),
            },
            span,
        )
    }

    /// The parameters of a closure or local function: each typed by its
    /// annotation, else by `context` (the expected function type's
    /// parameters), else by a fresh variable, then checked as a pattern.
    fn fn_params(
        &mut self,
        written: &[Param<'_>],
        context: Option<&[InferTy]>,
        origin: ParamOrigin,
    ) -> Result<(Vec<thir::Param>, Vec<InferTy>), Ice> {
        let mut params = Vec::with_capacity(written.len());
        let mut tys = Vec::with_capacity(written.len());
        for (n, param) in written.iter().enumerate() {
            let span = param.significant_span();
            let Some(pattern) = param.pattern() else {
                self.errored = true;
                continue;
            };
            let annotated = match param.ty() {
                Some(ty) => Some(self.lower_written_ty(ty)?),
                None => None,
            };
            let from_context = context.and_then(|ps| ps.get(n)).cloned();
            let ty = match (annotated, from_context) {
                // A disagreement shows as a mismatch of the whole function
                // type where it meets the expected type.
                (Some(ann), Some(ctx)) => {
                    let _ = self.infcx.unify(&ann, &ctx)?;
                    ann
                }
                (Some(ty), None) | (None, Some(ty)) => ty,
                (None, None) => self.param_var(pattern, span)?,
            };
            let pat = self.check_pat(pattern, &ty, PatSite::Param)?;
            tys.push(ty);
            params.push(thir::Param { pat, span, origin });
        }
        Ok((params, tys))
    }

    /// The expectation of a function body: its written return type, else
    /// the expected function type's result.
    fn ret_expected(
        &mut self,
        written: Option<RetType<'_>>,
        context: Option<InferTy>,
    ) -> Result<Option<Expected>, Ice> {
        if let Some(ty) = written.and_then(RetType::ty) {
            let span = ty.significant_span();
            let lowered = self.lower_written_ty(ty)?;
            if let Some(ctx) = context {
                let _ = self.infcx.unify(&lowered, &ctx)?;
            }
            return Ok(Some(Expected::new(
                lowered,
                Some((span, "expected because of this return type".to_owned())),
            )));
        }
        Ok(context.map(Expected::plain))
    }

    /// A local named function (§5.8), as the statement
    /// [`ExprKind::LocalFn`] of type `Nil`. Its type is fixed before its
    /// body is checked, so calls of itself in the body unify with it.
    pub(crate) fn local_fn(&mut self, f: LocalFn<'_>) -> Result<Option<ExprId>, Ice> {
        let span = f.significant_span();
        let Some(local) = self.res.bindings.get(&f.syntax().id()).copied() else {
            // A local function without a name is a syntax error.
            self.errored = true;
            return Ok(None);
        };
        let name = f.name().map(|n| self.input.globals.names.intern(n.text()));
        let name_span = f.name().map_or(span, AstToken::span);
        let written: Vec<Param<'_>> = f.params().collect();
        // Parameters and result first, so the function's type is known
        // when its body calls it.
        let ret_expected = if f.ret_type().and_then(RetType::ty).is_some() {
            self.ret_expected(f.ret_type(), None)?
        } else {
            let var = match name {
                Some(name) => self.noted_var(name_span, VarNote::LocalFnRet { name })?,
                None => self.fresh_var(name_span)?,
            };
            Some(Expected::plain(var))
        };
        let ret = ret_expected
            .as_ref()
            .map_or(InferTy::ERROR, |e| e.ty.clone());
        let (params, tys) = self.fn_params(&written, None, ParamOrigin::LocalFn)?;
        self.set_local(
            local,
            InferTy::Fn {
                params: tys.into(),
                ret: Box::new(ret),
            },
        )?;
        let Some(block) = f.body() else {
            self.errored = true;
            return Ok(None);
        };
        self.local_fns.push(local);
        let body = self.block(block, ret_expected.as_ref());
        self.local_fns.pop();
        let body = body?;
        let id = self.push(
            ExprKind::LocalFn {
                local,
                params: params.into(),
                body,
            },
            InferTy::NIL,
            span,
        )?;
        Ok(Some(id))
    }
}
