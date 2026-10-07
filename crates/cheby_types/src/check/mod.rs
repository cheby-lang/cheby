//! Bodies (C3) into THIR (C15).
//!
//! Checking is bidirectional: an expression is checked against an expected
//! type when its context knows one (§3.12.5), and THIR is built while
//! checking, with inference types on the side. At the end of the body,
//! literals are defaulted (D-133), every type is zonked into a closed
//! [`Ty`], and the semantic gate runs. A body with any error is not
//! returned, so MIR never sees one.
//!
//! The order of inference is that of §3.12.4: statements in order,
//! subexpressions left to right, closure arguments of a call last (D-234).

mod call;
mod expr;
mod pat;
mod stmt;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefIndex, DefKind, Generics, Ice, IfaceItem, IndexVec, ItemSig, ModuleInterface, Ty, display,
    sym,
};
use cheby_resolve::{BodyRes, ItemDef, ItemStatus, LocalId};
use cheby_syntax::ast::AstNode;
use cheby_syntax::ast::{Block, FnDecl, RetType, Type};

use crate::gate::{not_supported, slice_type, unsupported_value};
use crate::infer::{InferCtxt, InferTy, Mismatch, TyVid};
use crate::sig::{fn_decl, lower_fn_sig};
use crate::thir::{self, Body, Callee, EntryInfo, Expr, ExprId, ExprKind, External, FnDef};
use crate::{TypesInput, has_error};

/// C3-C15: checks every body of the module against `own` (from
/// [`crate::build_interface`]) and the dependencies' interfaces, and returns
/// THIR. Items are checked in `DefIndex` order; [`check_item`] is the unit
/// that may run in parallel (D-263), with results kept in this order.
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub fn check_bodies(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
) -> Result<(thir::Module, Vec<Diagnostic>), Ice> {
    let mut module = thir::Module {
        module: input.module,
        fns: Vec::new(),
        consts: Vec::new(),
        tests: Vec::new(),
    };
    let mut diags = Vec::new();
    for (index, item) in input.resolved.items.iter_enumerated() {
        if item.parent.is_some()
            || !matches!(item.kind, DefKind::Fn | DefKind::Const | DefKind::Test)
        {
            continue;
        }
        let (checked, found) = check_item(input, own, index)?;
        diags.extend(found);
        match checked {
            Some(thir::Item::Fn(f)) => module.fns.push(f),
            Some(thir::Item::Const(c)) => module.consts.push(c),
            Some(thir::Item::Test(t)) => module.tests.push(t),
            None => {}
        }
    }
    Ok((module, diags))
}

/// Checks one body: a function, a constant's initializer or a test.
/// `None` for an item without a checked body: an unsupported or erroneous
/// item, or a body with errors.
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub fn check_item(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefIndex,
) -> Result<(Option<thir::Item>, Vec<Diagnostic>), Ice> {
    let item = input
        .resolved
        .item(def)
        .ok_or_else(|| Ice::new("checking a missing item"))?;
    let iface_item = own
        .item(def)
        .ok_or_else(|| Ice::new("an item is missing from its own interface"))?;
    match item.kind {
        DefKind::Fn => check_fn(input, own, def, item, iface_item),
        DefKind::Test => {
            let diags = if matches!(iface_item.sig, ItemSig::Test) {
                // The resolver's gate rejects tests in the slice; this is
                // the fallback if it ever lets one through (G4 replaces it).
                vec![not_supported(item.name_span, "tests")]
            } else {
                Vec::new()
            };
            Ok((None, diags))
        }
        // Constants are reported by the gates (C13 adds them); other kinds
        // have no body.
        _ => Ok((None, Vec::new())),
    }
}

fn check_fn(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    index: DefIndex,
    item: &ItemDef,
    iface_item: &IfaceItem,
) -> Result<(Option<thir::Item>, Vec<Diagnostic>), Ice> {
    let decl = fn_decl(input, item)?;
    let (params, ret, generics, sig_ok) = match &iface_item.sig {
        ItemSig::Unsupported => return Ok((None, Vec::new())),
        ItemSig::Fn(sig) => (
            sig.params.iter().map(|p| p.ty).collect::<Vec<Ty>>(),
            sig.ret,
            sig.generics.clone(),
            true,
        ),
        // The errors were reported with the signature; check the body
        // anyway for its own errors, but never return it.
        ItemSig::Error => {
            let lowered = lower_fn_sig(input, decl, &mut Vec::new())?;
            (
                lowered.params.iter().map(|p| p.ty).collect(),
                lowered.ret,
                lowered.generics,
                false,
            )
        }
        _ => return Err(Ice::new("a function without a function signature")),
    };
    let def = input.def_id(index);
    let mut diags = Vec::new();
    let native_external = item.attrs.native_external().map(|(symbol, span)| External {
        symbol: symbol.into(),
        span,
    });
    let entry = if item.name == sym::MAIN && sig_ok {
        if ret == Ty::NIL {
            Some(EntryInfo::Nil)
        } else {
            // `Result` is rejected by the signature gate in the slice;
            // C9 resolves the `Show` dictionary (D-182).
            let span = ret_span(decl).unwrap_or(item.name_span);
            diags.push(not_supported(span, "`main` functions that return `Result`"));
            return Ok((None, diags));
        }
    } else {
        None
    };
    if item.status == ItemStatus::BodyUnsupported {
        // E0038 was reported in the body by the resolver; the signature
        // is published and checked against, the body is not (D-246).
        return Ok((None, diags));
    }
    let body = match decl.body() {
        None => None,
        Some(block) => {
            let res = input
                .resolved
                .bodies
                .get(&index)
                .ok_or_else(|| Ice::new("a supported function body was not resolved"))?;
            let origin = match ret_span(decl) {
                Some(span) => (span, "expected because of this return type".to_owned()),
                None => (
                    item.name_span,
                    "the return type is omitted, so it is `Nil`".to_owned(),
                ),
            };
            let cx = BodyCx::new(input, own, res, generics)?;
            let (body, found) = cx.check_fn_body(block, &params, ret, origin)?;
            diags.extend(found);
            match body {
                Some(body) => Some(body),
                None => return Ok((None, diags)),
            }
        }
    };
    if !sig_ok || has_error(&diags) || (body.is_none() && native_external.is_none()) {
        // A function with neither a body nor a native implementation was
        // reported by the resolver (E0046 or E0038).
        return Ok((None, diags));
    }
    Ok((
        Some(thir::Item::Fn(FnDef {
            def,
            span: item.name_span,
            native_external,
            body,
            entry,
        })),
        diags,
    ))
}

fn ret_span(decl: FnDecl<'_>) -> Option<Span> {
    decl.ret_type()
        .and_then(RetType::ty)
        .map(Type::significant_span)
}

/// What an expression is checked against: the type, and where the
/// expectation comes from, for E0047's secondary label.
#[derive(Clone, Debug)]
pub(crate) struct Expected {
    pub ty: InferTy,
    pub origin: Option<(Span, String)>,
}

impl Expected {
    pub const fn new(ty: InferTy, origin: Option<(Span, String)>) -> Self {
        Self { ty, origin }
    }
}

/// The state of checking one top-level body.
pub(crate) struct BodyCx<'a> {
    pub input: &'a TypesInput<'a>,
    pub own: &'a ModuleInterface,
    pub res: &'a BodyRes,
    /// The enclosing item's type parameters, for display.
    pub generics: Generics,
    pub infcx: InferCtxt<'a>,
    /// THIR as it is built; types are filled in by [`Self::finish`].
    pub exprs: IndexVec<ExprId, Expr>,
    /// The inference type of each expression.
    pub expr_tys: IndexVec<ExprId, InferTy>,
    /// Type arguments of generic uses (calls, constructors), zonked into
    /// the expression's kind by [`Self::finish`].
    pub inst: Vec<(ExprId, Box<[InferTy]>)>,
    /// The type of each local, once its binding is checked.
    pub locals: IndexVec<LocalId, Option<InferTy>>,
    pub diags: Vec<Diagnostic>,
    /// Whether the body has an error, reported here or before (a silent
    /// [`cheby_resolve::Res::Err`] or an erroneous signature).
    pub errored: bool,
}

impl<'a> BodyCx<'a> {
    fn new(
        input: &'a TypesInput<'a>,
        own: &'a ModuleInterface,
        res: &'a BodyRes,
        generics: Generics,
    ) -> Result<Self, Ice> {
        let locals = res.locals.indices().map(|_| None).collect();
        Ok(Self {
            input,
            own,
            res,
            generics,
            infcx: InferCtxt::new(input.globals),
            exprs: IndexVec::new(),
            expr_tys: IndexVec::new(),
            inst: Vec::new(),
            locals: IndexVec::from_raw(locals)?,
            diags: Vec::new(),
            errored: false,
        })
    }

    fn check_fn_body(
        mut self,
        block: Block<'_>,
        params: &[Ty],
        ret: Ty,
        origin: (Span, String),
    ) -> Result<(Option<Body>, Vec<Diagnostic>), Ice> {
        if self.res.params.len() != params.len() {
            return Err(Ice::new(
                "a function's parameters and its signature have different lengths",
            ));
        }
        for (&local, &ty) in self.res.params.iter().zip(params) {
            self.set_local(local, InferTy::Known(ty))?;
        }
        let expected = Expected::new(InferTy::Known(ret), Some(origin));
        let value = self.block(block, Some(&expected))?;
        self.finish(value, ret)
    }

    // ---- building THIR ----------------------------------------------

    pub fn push(&mut self, kind: ExprKind, ty: InferTy, span: Span) -> Result<ExprId, Ice> {
        let id = self.exprs.push(Expr {
            kind,
            ty: Ty::ERROR,
            span,
        })?;
        let ty_id = self.expr_tys.push(ty)?;
        if ty_id != id {
            return Err(Ice::new("expression tables out of step"));
        }
        Ok(id)
    }

    /// An expression whose error is already reported, or is reported by
    /// the caller.
    pub fn error_expr(&mut self, span: Span) -> Result<ExprId, Ice> {
        self.errored = true;
        self.push(ExprKind::Error, InferTy::ERROR, span)
    }

    pub fn report(&mut self, diag: Diagnostic) {
        if diag.is_error() {
            self.errored = true;
        }
        self.diags.push(diag);
    }

    /// E0038 for `what` at `span`, as an error expression.
    pub fn unsupported(&mut self, span: Span, what: &str) -> Result<ExprId, Ice> {
        self.report(not_supported(span, what));
        self.error_expr(span)
    }

    pub fn ty_of(&self, id: ExprId) -> Result<InferTy, Ice> {
        self.expr_tys.try_get(id, "expression type").cloned()
    }

    pub fn span_of(&self, id: ExprId) -> Result<Span, Ice> {
        Ok(self.exprs.try_get(id, "expression")?.span)
    }

    /// The type of `local`. A local bound by a rejected construct (its
    /// error already reported) has type [`Ty::ERROR`].
    pub fn local_ty(&self, local: LocalId) -> Result<InferTy, Ice> {
        match self.locals.try_get(local, "local")? {
            Some(ty) => Ok(ty.clone()),
            None if self.errored => Ok(InferTy::ERROR),
            None => Err(Ice::new(format!(
                "{local:?} is used before its binding is checked"
            ))),
        }
    }

    pub fn set_local(&mut self, local: LocalId, ty: InferTy) -> Result<(), Ice> {
        let slot = self
            .locals
            .get_mut(local)
            .ok_or_else(|| Ice::new(format!("no local {local:?}")))?;
        *slot = Some(ty);
        Ok(())
    }

    /// `t` as users write it.
    pub fn show(&self, t: &InferTy) -> String {
        let cx = self.input.display_cx(self.own, Some(&self.generics));
        self.infcx.display(t, &cx)
    }

    // ---- expectations ------------------------------------------------

    /// Checks that a value of type `found` at `span` fits `expected`, and
    /// returns its type in context. An expression of type `Never` takes
    /// the expected type when one is known (§3.10, D-235); otherwise the
    /// types are unified, and a mismatch is E0047.
    pub fn coerce(
        &mut self,
        found: &InferTy,
        expected: Option<&Expected>,
        span: Span,
    ) -> Result<InferTy, Ice> {
        let Some(expected) = expected else {
            return Ok(found.clone());
        };
        if self.infcx.shallow(found)? == InferTy::NEVER
            && !matches!(self.infcx.shallow(&expected.ty)?, InferTy::Var(_))
        {
            return Ok(expected.ty.clone());
        }
        match self.infcx.unify(&expected.ty, found)? {
            Ok(()) => Ok(found.clone()),
            Err(mismatch) => {
                self.mismatch(&expected.ty, found, mismatch, span, expected.origin.clone());
                Ok(InferTy::ERROR)
            }
        }
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

    // ---- the end of the body -------------------------------------------

    /// Defaults literals, zonks every type into THIR, reports types that
    /// stayed unknown, and runs the semantic gate.
    fn finish(mut self, value: ExprId, ret: Ty) -> Result<(Option<Body>, Vec<Diagnostic>), Ice> {
        self.infcx.default_literals()?;
        let mut unresolved: Vec<TyVid> = Vec::new();
        let mut exprs = std::mem::take(&mut self.exprs);
        for (id, ty) in self.expr_tys.iter_enumerated() {
            let zonked = self.infcx.zonk(ty, &mut unresolved)?;
            exprs
                .get_mut(id)
                .ok_or_else(|| Ice::new("expression tables out of step"))?
                .ty = zonked;
        }
        for (id, args) in &self.inst {
            let args = args
                .iter()
                .map(|t| self.infcx.zonk(t, &mut unresolved))
                .collect::<Result<Box<[Ty]>, Ice>>()?;
            let expr = exprs
                .get_mut(*id)
                .ok_or_else(|| Ice::new("type arguments of a missing expression"))?;
            set_ty_args(&mut expr.kind, args)?;
        }
        let mut locals = IndexVec::new();
        for (local, decl) in self.res.locals.iter_enumerated() {
            let ty = match self.locals.get(local).and_then(Option::as_ref) {
                Some(t) => self.infcx.zonk(t, &mut unresolved)?,
                None => Ty::ERROR,
            };
            locals.push(thir::Local {
                name: decl.name,
                ty,
                span: decl.span,
            })?;
        }
        unresolved.sort_unstable();
        for vid in unresolved {
            // A type nothing fixes is E0038 until C3 gives it a code of
            // its own (D-451). Unreachable in the slice, which has no
            // generic calls or empty literals.
            if let Some(span) = self.infcx.origin(vid) {
                self.report(not_supported(
                    span,
                    "expressions whose type nothing in the body fixes",
                ));
            }
            self.errored = true;
        }
        if !self.errored {
            self.gate_values(&exprs, value)?;
        }
        let diags = std::mem::take(&mut self.diags);
        if self.errored || has_error(&diags) {
            return Ok((None, diags));
        }
        if exprs.iter().any(|e| matches!(e.kind, ExprKind::Error)) {
            return Err(Ice::new("an error expression in a body without errors"));
        }
        Ok((
            Some(Body {
                params: self.res.params.clone(),
                locals,
                exprs,
                value,
                ret,
            }),
            diags,
        ))
    }

    /// The semantic gate on values (D-451): E0038 on each outermost
    /// expression whose type the slice cannot run.
    fn gate_values(&mut self, exprs: &IndexVec<ExprId, Expr>, id: ExprId) -> Result<(), Ice> {
        let expr = exprs.try_get(id, "expression")?;
        if !slice_type(expr.ty) {
            let cx = self.input.display_cx(self.own, Some(&self.generics));
            let shown = display(expr.ty, &cx);
            self.report(unsupported_value(expr.span, &shown));
            return Ok(());
        }
        for child in expr.kind.children() {
            self.gate_values(exprs, child)?;
        }
        Ok(())
    }
}

/// Writes zonked type arguments into the expression that took them.
fn set_ty_args(kind: &mut ExprKind, args: Box<[Ty]>) -> Result<(), Ice> {
    match kind {
        ExprKind::Ctor { ty_args, .. }
        | ExprKind::FnRef { ty_args, .. }
        | ExprKind::Record { ty_args, .. }
        | ExprKind::Call {
            callee: Callee::Fn { ty_args, .. } | Callee::Ctor { ty_args, .. },
            ..
        } => {
            *ty_args = args;
            Ok(())
        }
        // An error expression keeps no type arguments.
        ExprKind::Error => Ok(()),
        _ => Err(Ice::new("type arguments for an expression that takes none")),
    }
}
