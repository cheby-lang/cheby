//! Calls (§5.5, §8.5): of top-level functions, of positional constructors,
//! of interface functions and of function values, and function captures
//! (§5.6, D-050, D-155, D-202).
//!
//! A call first resolves its callee into a [`Callee`] signature, with
//! fresh variables (or the turbofish's types) for a generic callee's type
//! parameters. Then the arguments are checked against the parameter types
//! as instantiated so far: those that are not closures or captures first,
//! left to right, then the closures and captures (§3.12.4, D-234). The
//! same machinery serves the calls that pipes and `use` desugar to
//! (`desugar.rs`).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{CtorFields, DefId, Ice, ItemSig, Ty};
use cheby_resolve::Res;
use cheby_syntax::ast::{Arg, AstNode, CallExpr, Expr, PathExpr};

use super::constraints::placeholder_dict;
use super::desugar::Callback;
use super::expr::{bounds_of, item_params, named_ctor_as_value};
use super::{BodyCx, Deferred, Expected};
use crate::infer::{InferTy, instantiate};
use crate::thir::{self, Callee, Dict, ExprId, ExprKind, ParamOrigin, PatKind};

/// Where a call's parts are, for diagnostics.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CallSpans {
    pub call: Span,
    pub callee: Span,
    pub args: Span,
}

/// What a call calls, before its arguments are checked.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A top-level function, with its type arguments and its bounds,
    /// instantiated.
    Fn {
        def: DefId,
        args: Box<[InferTy]>,
        bounds: Vec<(InferTy, DefId)>,
    },
    /// A positional constructor, with the type's type arguments.
    Ctor { def: DefId, args: Box<[InferTy]> },
    /// An interface function `I::f`, with the variable that stands for
    /// `Self` (§8.5).
    Iface { iface_fn: DefId, self_ty: InferTy },
    /// Any other expression of function type.
    Value(ExprId),
    /// The callee had an error, already reported: the arguments are still
    /// checked, without expectations.
    Error,
}

/// A callee with its parameter and result types as instantiated.
#[derive(Clone, Debug)]
pub(crate) struct Signature {
    pub target: Target,
    pub params: Vec<InferTy>,
    pub ret: InferTy,
    /// The callee's name and declaration, for E0048.
    pub named: Option<(String, Option<Span>)>,
}

impl Signature {
    const fn error() -> Self {
        Self {
            target: Target::Error,
            params: Vec::new(),
            ret: InferTy::ERROR,
            named: None,
        }
    }

    /// Whether the result type mentions the callee's type parameters.
    const fn is_generic(&self) -> bool {
        match &self.target {
            Target::Fn { args, .. } | Target::Ctor { args, .. } => !args.is_empty(),
            Target::Iface { .. } => true,
            Target::Value(_) | Target::Error => false,
        }
    }
}

/// An argument of a call as written, or one that desugaring adds.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArgSrc<'t> {
    /// A written argument; a closure or a capture is checked late.
    Expr(Expr<'t>),
    /// The callback of a `use` (§5.10), checked late.
    Callback(Callback<'t>),
}

impl ArgSrc<'_> {
    /// Whether the argument is checked after the others (§3.12.4).
    fn is_late(self) -> bool {
        match self {
            Self::Expr(e) => is_late(e),
            Self::Callback(_) => true,
        }
    }
}

/// Expectations for the arguments of a call whose callee had an error:
/// [`InferTy::ERROR`], which unifies with anything and gives a closure
/// argument's parameters that type, so nothing more is reported.
pub(crate) fn error_params(n: usize) -> Vec<InferTy> {
    vec![InferTy::ERROR; n]
}

/// Whether `e` is a closure or a function capture, which a call checks
/// after its other arguments.
pub(crate) fn is_late(e: Expr<'_>) -> bool {
    match e {
        Expr::Closure(_) => true,
        Expr::Call(c) => c.args().any(|a| matches!(a, Arg::Hole(_))),
        Expr::Paren(p) => p.inner().is_some_and(is_late),
        _ => false,
    }
}

impl BodyCx<'_> {
    /// A call `f(a, b)`, or a capture `f(a, _)` (§5.6).
    pub(crate) fn call(
        &mut self,
        call: CallExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = call.significant_span();
        let Some(callee) = call.callee() else {
            return self.error_expr(span);
        };
        let spans = CallSpans {
            call: span,
            callee: callee.significant_span(),
            args: call.arg_list().map_or(span, AstNode::significant_span),
        };
        let written: Vec<Arg<'_>> = call.args().collect();
        if let Some(hole) = written.iter().position(|a| matches!(a, Arg::Hole(_))) {
            return self.capture(callee, &written, hole, spans, expected);
        }
        let args: Vec<ArgSrc<'_>> = written
            .iter()
            .filter_map(|a| match a {
                Arg::Expr(e) => Some(ArgSrc::Expr(*e)),
                Arg::Hole(_) => None,
            })
            .collect();
        let sig = self.callee(callee, args.len(), spans)?;
        self.apply(sig, &args, spans, expected)
    }

    /// Resolves the callee of a call with `arity` arguments.
    pub(crate) fn callee(
        &mut self,
        callee: Expr<'_>,
        arity: usize,
        spans: CallSpans,
    ) -> Result<Signature, Ice> {
        if let Expr::Path(p) = callee
            && let Some(path) = p.path()
            && let Res::Def(def) = self.path_res(path.syntax().id())?
            && let Some(sig) = self.def_callee(def, p, spans)?
        {
            return Ok(sig);
        }
        let id = self.check_expr(callee, None)?;
        self.value_callee(id, arity, spans)
    }

    /// A path to an item as a callee: `None` for items that are called as
    /// values (constants).
    fn def_callee(
        &mut self,
        def: DefId,
        p: PathExpr<'_>,
        spans: CallSpans,
    ) -> Result<Option<Signature>, Ice> {
        let item = self.input.item(self.own, def)?;
        let named = Some((self.input.name_of(self.own, def).to_owned(), item.span));
        let types = &self.input.globals.types;
        Ok(Some(match &item.sig {
            ItemSig::Error => {
                self.errored = true;
                Signature::error()
            }
            ItemSig::Fn(sig) => {
                let params = item_params(&sig.generics);
                let Some(inst) = self.instance(def, &params, p.type_args(), spans.callee)? else {
                    self.errored = true;
                    return Ok(Some(Signature::error()));
                };
                let bounds = bounds_of(&sig.generics, &inst.args);
                Signature {
                    params: sig
                        .params
                        .iter()
                        .map(|param| instantiate(types, param.ty, &inst.args))
                        .collect(),
                    ret: instantiate(types, sig.ret, &inst.args),
                    target: Target::Fn {
                        def,
                        args: inst.args,
                        bounds,
                    },
                    named,
                }
            }
            ItemSig::Ctor(ctor) => match &ctor.fields {
                CtorFields::Positional(fields) => {
                    let fields = fields.clone();
                    let Some((ty, args)) =
                        self.ctor_instance(def, ctor, p.type_args(), spans.callee)?
                    else {
                        self.errored = true;
                        return Ok(Some(Signature::error()));
                    };
                    Signature {
                        params: fields
                            .iter()
                            .map(|&field| instantiate(types, field, &args))
                            .collect(),
                        ret: ty,
                        target: Target::Ctor { def, args },
                        named,
                    }
                }
                CtorFields::Unit => {
                    let (ty, _) = self.ctor_type(def, ctor, spans.callee)?;
                    self.not_a_function(&ty, spans);
                    Signature::error()
                }
                CtorFields::Named(_) => {
                    let name = self.input.name_of(self.own, def);
                    self.report(named_ctor_as_value(spans.callee, name, true));
                    Signature::error()
                }
            },
            ItemSig::InterfaceFn(isig) => {
                let self_ty = self.fresh_var(spans.callee)?;
                let args = [self_ty.clone()];
                Signature {
                    params: isig
                        .params
                        .iter()
                        .map(|&param| instantiate(types, param, &args))
                        .collect(),
                    ret: instantiate(types, isig.ret, &args),
                    target: Target::Iface {
                        iface_fn: def,
                        self_ty,
                    },
                    named,
                }
            }
            _ => return Ok(None),
        }))
    }

    /// A value of function type as a callee (§5.5). A value whose type is
    /// not known yet becomes a function of `arity` parameters (local,
    /// monomorphic, D-123).
    pub(crate) fn value_callee(
        &mut self,
        id: ExprId,
        arity: usize,
        spans: CallSpans,
    ) -> Result<Signature, Ice> {
        let ty = self.ty_of(id)?;
        let (params, ret) = match self.infcx.resolve(&ty)? {
            InferTy::Fn { params, ret } => (params.into_vec(), *ret),
            InferTy::Known(Ty::ERROR) => return Ok(Signature::error()),
            InferTy::Var(_) => {
                let params = (0..arity)
                    .map(|_| self.fresh_var(spans.args))
                    .collect::<Result<Vec<InferTy>, Ice>>()?;
                let ret = self.fresh_var(spans.call)?;
                let shape = InferTy::Fn {
                    params: params.clone().into(),
                    ret: Box::new(ret.clone()),
                };
                if self.infcx.unify(&ty, &shape)?.is_err() {
                    self.not_a_function(&ty, spans);
                    return Ok(Signature::error());
                }
                (params, ret)
            }
            _ => {
                self.not_a_function(&ty, spans);
                return Ok(Signature::error());
            }
        };
        Ok(Signature {
            target: Target::Value(id),
            params,
            ret,
            named: None,
        })
    }

    /// Checks `args` against `sig` and builds the call, against `expected`
    /// for a generic callee's result before the arguments (so `Some(c)`
    /// fits `Option<dyn Shape>`, §3.12.5).
    pub(crate) fn apply(
        &mut self,
        sig: Signature,
        args: &[ArgSrc<'_>],
        spans: CallSpans,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        if matches!(sig.target, Target::Error) {
            self.check_args(&error_params(args.len()), args, spans)?;
            return self.error_expr(spans.call);
        }
        if sig.params.len() != args.len() {
            self.arity_error(sig.named.as_ref(), sig.params.len(), args.len(), spans);
            self.check_args(&error_params(args.len()), args, spans)?;
            return self.error_expr(spans.call);
        }
        self.expect_result(&sig, expected)?;
        let ids = self.check_args(&sig.params, args, spans)?;
        self.build_call(sig, ids, spans.call)
    }

    /// Unifies a generic callee's result type with the expected type when
    /// they fit, so the expectation reaches the arguments. A `dyn`
    /// expectation is left to the conversion of the result
    /// (D-497).
    pub(crate) fn expect_result(
        &mut self,
        sig: &Signature,
        expected: Option<&Expected>,
    ) -> Result<(), Ice> {
        if let Some(e) = expected
            && sig.is_generic()
            && self.dyn_iface(&e.ty)?.is_none()
        {
            // A failed unification binds nothing; the mismatch is reported
            // on the result.
            let _ = self.infcx.unify(&sig.ret, &e.ty)?;
        }
        Ok(())
    }

    /// The call expression of `sig` with checked arguments `args`.
    pub(crate) fn build_call(
        &mut self,
        sig: Signature,
        args: Vec<ExprId>,
        span: Span,
    ) -> Result<ExprId, Ice> {
        let args: Box<[ExprId]> = args.into();
        match sig.target {
            Target::Fn {
                def,
                args: ty_args,
                bounds,
            } => {
                let dicts: Box<[Dict]> = bounds
                    .iter()
                    .map(|(_, iface)| placeholder_dict(*iface))
                    .collect();
                let id = self.push(
                    ExprKind::Call {
                        callee: Callee::Fn {
                            def,
                            ty_args: Box::new([]),
                            dicts,
                        },
                        args,
                    },
                    sig.ret,
                    span,
                )?;
                self.record_instance(id, def, ty_args, bounds);
                Ok(id)
            }
            Target::Ctor { def, args: ty_args } => {
                let id = self.push(
                    ExprKind::Call {
                        callee: Callee::Ctor {
                            ctor: def,
                            ty_args: Box::new([]),
                        },
                        args,
                    },
                    sig.ret,
                    span,
                )?;
                if !ty_args.is_empty() {
                    self.inst.push((id, ty_args));
                }
                Ok(id)
            }
            Target::Iface { iface_fn, self_ty } => {
                // The callee is chosen at the end of the body (§8.5).
                let id = self.push(
                    ExprKind::Call {
                        callee: Callee::Dyn { iface_fn },
                        args,
                    },
                    sig.ret,
                    span,
                )?;
                self.defer(Deferred::IfaceCall {
                    expr: id,
                    iface_fn,
                    self_ty,
                });
                Ok(id)
            }
            Target::Value(callee) => self.push(
                ExprKind::Call {
                    callee: Callee::Value(callee),
                    args,
                },
                sig.ret,
                span,
            ),
            Target::Error => self.error_expr(span),
        }
    }

    /// Checks the arguments: those that are not closures, captures or
    /// callbacks first, left to right, then those, left to right
    /// (§3.12.4). Argument `n` is checked against `params[n]` when there is
    /// one. Returns them in written order.
    pub(crate) fn check_args(
        &mut self,
        params: &[InferTy],
        args: &[ArgSrc<'_>],
        spans: CallSpans,
    ) -> Result<Vec<ExprId>, Ice> {
        let mut out: Vec<Option<ExprId>> = vec![None; args.len()];
        let order = args
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.is_late())
            .chain(args.iter().enumerate().filter(|(_, a)| a.is_late()));
        for (n, &arg) in order {
            let expected = params.get(n).map(|ty| {
                Expected::new(
                    ty.clone(),
                    Some((
                        spans.callee,
                        "arguments to this function are incorrect".to_owned(),
                    )),
                )
            });
            let id = match arg {
                ArgSrc::Expr(e) => self.check_expr(e, expected.as_ref())?,
                ArgSrc::Callback(cb) => self.callback(cb, expected.as_ref())?,
            };
            if let Some(slot) = out.get_mut(n) {
                *slot = Some(id);
            }
        }
        out.into_iter()
            .map(|id| id.ok_or_else(|| Ice::new("an argument was not checked")))
            .collect()
    }

    // ---- captures ------------------------------------------------------

    /// A function capture `f(a, _, b)` (§5.6): a block that evaluates the
    /// callee and the other arguments once, binding them to synthetic
    /// locals, and ends in a one-parameter closure that calls `f` (D-155).
    fn capture(
        &mut self,
        callee: Expr<'_>,
        written: &[Arg<'_>],
        hole: usize,
        spans: CallSpans,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let hole_span = written.get(hole).map_or(spans.args, |a| a.syntax().span());
        let others: Vec<ArgSrc<'_>> = written
            .iter()
            .filter_map(|a| match a {
                Arg::Expr(e) => Some(ArgSrc::Expr(*e)),
                Arg::Hole(_) => None,
            })
            .collect();
        let sig = self.callee(callee, written.len(), spans)?;
        if matches!(sig.target, Target::Error) {
            self.check_args(&error_params(others.len()), &others, spans)?;
            return self.error_expr(spans.call);
        }
        if sig.params.len() != written.len() {
            self.arity_error(sig.named.as_ref(), sig.params.len(), written.len(), spans);
            self.check_args(&error_params(others.len()), &others, spans)?;
            return self.error_expr(spans.call);
        }
        let hole_ty = sig
            .params
            .get(hole)
            .cloned()
            .ok_or_else(|| Ice::new("a capture's hole past its parameters"))?;
        // The expected function type reaches the hole's parameter and the
        // result before the arguments are checked.
        if let Some(e) = expected
            && let InferTy::Fn { params, ret } = self.infcx.resolve(&e.ty)?
            && let [param] = &params[..]
        {
            let _ = self.infcx.unify(&hole_ty, param)?;
            if sig.is_generic() {
                let _ = self.infcx.unify(&sig.ret, &ret)?;
            }
        }
        let other_params: Vec<InferTy> = sig
            .params
            .iter()
            .enumerate()
            .filter(|&(n, _)| n != hole)
            .map(|(_, ty)| ty.clone())
            .collect();
        let checked = self.check_args(&other_params, &others, spans)?;
        // Evaluate the callee and every argument once, in written order.
        let mut stmts = Vec::new();
        let sig = match sig.target {
            Target::Value(id) => {
                let id = self.bind_once(id, &mut stmts, "%callee")?;
                Signature {
                    target: Target::Value(id),
                    ..sig
                }
            }
            _ => sig,
        };
        let mut inner = Vec::with_capacity(written.len());
        for id in checked {
            inner.push(self.bind_once(id, &mut stmts, "%arg")?);
        }
        let name = self.synthetic_name("%hole");
        let local = self.new_local(name, hole_ty.clone(), hole_span)?;
        let param_ref = self.push(ExprKind::Local(local), hole_ty.clone(), hole_span)?;
        inner.insert(hole.min(inner.len()), param_ref);
        let ret = sig.ret.clone();
        let call = self.build_call(sig, inner, spans.call)?;
        let pat = self.bind_pat(local, hole_ty.clone(), hole_span)?;
        let closure_ty = InferTy::Fn {
            params: Box::new([hole_ty]),
            ret: Box::new(ret),
        };
        let closure = self.push(
            ExprKind::Closure {
                params: Box::new([thir::Param {
                    pat,
                    span: hole_span,
                    origin: ParamOrigin::Capture,
                }]),
                body: call,
            },
            closure_ty.clone(),
            spans.call,
        )?;
        if stmts.is_empty() {
            return Ok(closure);
        }
        self.push(
            ExprKind::Block {
                stmts: stmts.into(),
                tail: Some(closure),
            },
            closure_ty,
            spans.call,
        )
    }

    /// The expression `id` evaluated once: kept in place when it has no
    /// effect and costs nothing (a literal, a local, an item), otherwise
    /// bound to a synthetic local by a `let` appended to `stmts`, and
    /// read from it.
    pub(crate) fn bind_once(
        &mut self,
        id: ExprId,
        stmts: &mut Vec<thir::Stmt>,
        name: &str,
    ) -> Result<ExprId, Ice> {
        // An argument or callee without effects is used in place; anything
        // else is bound once, in written order (D-155, D-498).
        let pure = matches!(
            self.exprs.try_get(id, "expression")?.kind,
            ExprKind::Lit(_)
                | ExprKind::Local(_)
                | ExprKind::SelfRef { .. }
                | ExprKind::FnRef { .. }
                | ExprKind::ConstRef { .. }
                | ExprKind::Ctor { .. }
        );
        if pure {
            return Ok(id);
        }
        let ty = self.ty_of(id)?;
        let span = self.span_of(id)?;
        let name = self.synthetic_name(name);
        let local = self.new_local(name, ty.clone(), span)?;
        let pat = self.bind_pat(local, ty.clone(), span)?;
        stmts.push(thir::Stmt::Let { pat, init: id });
        self.push(ExprKind::Local(local), ty, span)
    }

    /// The pattern that binds `local` of type `ty`.
    pub(crate) fn bind_pat(
        &mut self,
        local: cheby_resolve::LocalId,
        ty: InferTy,
        span: Span,
    ) -> Result<thir::Pat, Ice> {
        Ok(thir::Pat {
            id: self.new_pat_id(ty, span)?,
            kind: PatKind::Bind(local),
            ty: Ty::ERROR,
            span,
        })
    }

    // ---- errors --------------------------------------------------------

    /// E0049.
    fn not_a_function(&mut self, ty: &InferTy, spans: CallSpans) {
        let shown = self.show(ty);
        self.report(
            Diagnostic::new(
                codes::NOT_A_FUNCTION,
                format!("expected a function, found `{shown}`"),
            )
            .with_primary(spans.callee, format!("this has type `{shown}`")),
        );
    }

    /// E0048.
    pub(crate) fn arity_error(
        &mut self,
        callee: Option<&(String, Option<Span>)>,
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
            diag = diag.with_secondary(*decl, format!("`{name}` is declared here"));
        }
        self.report(diag);
    }
}
