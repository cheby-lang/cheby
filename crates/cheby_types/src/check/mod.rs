//! Bodies (C3) into THIR (C15).
//!
//! Checking is bidirectional: an expression is checked against an expected
//! type when its context knows one (§3.12.5), and THIR is built while
//! checking, with inference types on the side. At the end of the body,
//! literals are defaulted (D-133), every type is zonked into a closed
//! [`Ty`], patterns get their types, types nothing determined are reported
//! (E0057), literals are checked against their final types (E0071), the
//! deferred constraints are resolved through the interface API (D-234),
//! and a body without errors goes through the exhaustiveness checks. A body
//! with any error is not returned, so MIR never sees one.
//!
//! The order of inference is that of §3.12.4: statements in order,
//! subexpressions left to right, closure and capture arguments of a call
//! last (D-234).
//!
//! The files of this directory: `mod.rs` holds [`BodyCx`], the items and
//! the end of a body; `expr`, `call`, `stmt`, `closure`,
//! `desugar`, `coerce`, `constraints` and `interp` the expression core;
//! `pat`, `case` and `record` patterns, `case` and records.

mod call;
mod case;
mod closure;
mod coerce;
mod constraints;
mod desugar;
mod expr;
mod interp;
mod pat;
mod record;
mod stmt;

pub(crate) use constraints::Deferred;
pub(crate) use pat::PatSite;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefIndex, DefKind, Generics, Ice, IfaceItem, IndexVec, ItemSig, ModuleInterface, Name,
    PackageId, Ty, TyKind, builtin, sym,
};
use cheby_resolve::{BodyRes, ItemDef, LocalId, Res};
use cheby_syntax::ast::{self, AstNode, Block, ConstDecl, FnDecl, RetType, TestDecl, Type};
use rayon::prelude::*;

use crate::ifaces::{self, IfaceCx};
use crate::infer::{InferCtxt, InferTy, TyVid, VarKind, contains_error};
use crate::lower_ty::TyLowering;
use crate::sig::{fn_decl, lower_fn_sig};
use crate::thir::{
    self, Body, Callee, ConstDef, EntryInfo, Expr, ExprId, ExprKind, External, FnDef, Lit, PatId,
    PatKind, TestDef,
};
use crate::{TypesInput, consts, exhaust, has_error, targets, warnings};

/// C3-C15: checks every body of the module against `own` (from
/// [`crate::build_interface`]) and the dependencies' interfaces, and returns
/// THIR. Items are checked in `DefIndex` order; [`check_item`] is the unit
/// that may run in parallel (D-263), with results kept in this order. Then
/// the module-wide passes run, in this order: the cross-target check (C12),
/// constant restrictions and cycles (C13), and the module-local warnings
/// (C14).
///
/// This is a user build: the `test` blocks of standard-library modules are
/// skipped ([`Tests::SkipStd`]).
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub fn check_bodies(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
) -> Result<(thir::Module, Vec<Diagnostic>), Ice> {
    check_bodies_with(input, own, Tests::SkipStd)
}

/// Which `test` blocks [`check_bodies_with`] checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tests {
    /// Every module's tests: testing the standard library itself (G4).
    All,
    /// A user build: the tests of standard-library modules are neither
    /// checked nor in the THIR, because only testing std runs them. The
    /// module's other passes see no tests either (D-479).
    SkipStd,
}

/// [`check_bodies`], with the `test` blocks chosen by `tests`.
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub fn check_bodies_with(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    tests: Tests,
) -> Result<(thir::Module, Vec<Diagnostic>), Ice> {
    let skip_tests = tests == Tests::SkipStd && input.package == PackageId::STD;
    let mut module = thir::Module {
        module: input.module,
        fns: Vec::new(),
        consts: Vec::new(),
        tests: Vec::new(),
    };
    let mut diags = Vec::new();
    let bodies: Vec<DefIndex> = input
        .resolved
        .items
        .iter_enumerated()
        .filter(|(_, item)| {
            item.parent.is_none()
                && matches!(item.kind, DefKind::Fn | DefKind::Const | DefKind::Test)
                && !(skip_tests && item.kind == DefKind::Test)
        })
        .map(|(index, _)| index)
        .collect();
    // Bodies check in parallel on rayon's global pool (D-263): inference
    // is local to one body (D-011), and `collect` keeps `DefIndex` order,
    // so the result never depends on the schedule.
    let checked_items: Vec<_> = bodies
        .par_iter()
        .map(|&index| check_item(input, own, index))
        .collect();
    for result in checked_items {
        let (checked, found) = result?;
        diags.extend(found);
        match checked {
            Some(thir::Item::Fn(f)) => module.fns.push(f),
            Some(thir::Item::Const(c)) => module.consts.push(c),
            Some(thir::Item::Test(t)) => module.tests.push(t),
            None => {}
        }
    }
    diags.extend(targets::check_module(input, own, &module)?);
    diags.extend(consts::check_module(input, own, &module)?);
    diags.extend(warnings::module_warnings(input, own, &module)?);
    Ok((module, diags))
}

/// Checks one body: a function, a constant's initializer or a test.
/// `None` for an item without a checked body: an erroneous item, or a
/// body with errors.
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
        DefKind::Const => check_const(input, own, def, item, iface_item),
        DefKind::Test => check_test(input, own, def, item),
        // Other kinds have no body.
        _ => Ok((None, Vec::new())),
    }
}

/// The resolution of item `index`'s body.
fn body_res<'a>(input: &'a TypesInput<'_>, index: DefIndex) -> Result<&'a BodyRes, Ice> {
    input
        .resolved
        .bodies
        .get(&index)
        .ok_or_else(|| Ice::new("a body was not resolved"))
}

/// What an entry point returns (D-073, D-448).
#[derive(Clone, Debug)]
enum EntryKind {
    Nil,
    Result(InferTy),
}

impl EntryKind {
    /// The entry kind of the result type `ret`, if it is one.
    fn of(input: &TypesInput<'_>, ret: Ty) -> Option<Self> {
        if ret == Ty::NIL {
            return Some(Self::Nil);
        }
        match input.globals.types.kind(ret) {
            TyKind::Named { def, args } if *def == builtin::RESULT => match args[..] {
                [ok, err] if ok == Ty::NIL => Some(Self::Result(InferTy::Known(err))),
                _ => None,
            },
            _ => None,
        }
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
        ItemSig::Fn(sig) => (
            sig.params.iter().map(|p| p.ty).collect::<Vec<Ty>>(),
            sig.ret,
            sig.generics.clone(),
            true,
        ),
        // The errors were reported with the signature; check the body
        // anyway for its own errors, but never return it.
        // A type parameter whose bound failed to lower is the error type in
        // the body, so uses that need the bound are silent.
        ItemSig::Error => {
            let lowered = lower_fn_sig(input, decl, &mut Vec::new())?;
            let types = &input.globals.types;
            let args = lowered
                .generics
                .params
                .indices()
                .map(|p| {
                    if lowered.bound_errors.contains(&p) {
                        Ty::ERROR
                    } else {
                        types.param(p)
                    }
                })
                .collect::<Vec<Ty>>();
            let erase = |ty| crate::ifaces::tyutil::subst(types, ty, &args);
            (
                lowered
                    .params
                    .iter()
                    .map(|p| erase(p.ty))
                    .collect::<Result<_, Ice>>()?,
                erase(lowered.ret)?,
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
        context: item.attrs.std_context,
    });
    // Every top-level `main` is an entry point (D-137, D-448); a wrong
    // signature is E0050, reported with the signature.
    let entry_kind = if item.name == sym::MAIN && sig_ok {
        EntryKind::of(input, ret)
    } else {
        None
    };
    let body = match decl.body() {
        None => None,
        Some(block) => {
            let res = body_res(input, index)?;
            let origin = match ret_span(decl) {
                Some(span) => (span, "expected because of this return type".to_owned()),
                None => (
                    item.name_span,
                    "the return type is omitted, so it is `Nil`".to_owned(),
                ),
            };
            let mut cx = BodyCx::new(input, own, res, generics.clone())?;
            let body = cx.check_fn_body(block, &params, ret, origin)?;
            diags.append(&mut cx.diags);
            match body {
                Some(body) => Some(body),
                None => return Ok((None, diags)),
            }
        }
    };
    // A function with only `@external(js, …)` stays in THIR with neither a
    // body nor a native external: native code never runs it (the
    // cross-target check rejects such uses), but a constant's initializer,
    // which is not target-checked (§12.6, D-512), may reach it, and the
    // back end gives it a body that reports the effect (E0077, D-533).
    let js_only = item
        .attrs
        .externals
        .iter()
        .any(|e| matches!(e, cheby_resolve::ExternalAttr::Js { .. }));
    if !sig_ok || has_error(&diags) || (body.is_none() && native_external.is_none() && !js_only) {
        // A function with neither a body nor any external was reported by
        // the resolver (E0046).
        return Ok((None, diags));
    }
    let entry = match entry_kind {
        None => None,
        Some(kind) => Some(entry_info(input, own, &generics, kind, |t| match t {
            InferTy::Known(ty) => Ok(*ty),
            _ => Err(Ice::new("an entry point's error type with variables")),
        })?),
    };
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

/// How the entry thunk of an entry point or a test reports its result
/// (D-182, D-140): an `Err` is printed with `Show` when its type
/// satisfies it, else with debug printing.
fn entry_info(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    generics: &Generics,
    kind: EntryKind,
    close: impl Fn(&InferTy) -> Result<Ty, Ice>,
) -> Result<EntryInfo, Ice> {
    Ok(match kind {
        EntryKind::Nil => EntryInfo::Nil,
        EntryKind::Result(err) => {
            let err_ty = close(&err)?;
            let cx = IfaceCx {
                input,
                own,
                generics,
            };
            let show = if err_ty == Ty::NEVER {
                None
            } else {
                ifaces::satisfy(&cx, err_ty, builtin::SHOW)?.ok()
            };
            EntryInfo::Result { err_ty, show }
        }
    })
}

/// A constant's initializer, checked against its declared type (§4.5,
/// C13). Restrictions on the type and cycles are `crate::consts`'s.
fn check_const(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    index: DefIndex,
    item: &ItemDef,
    iface_item: &IfaceItem,
) -> Result<(Option<thir::Item>, Vec<Diagnostic>), Ice> {
    let decl = input
        .tree
        .node(item.node)
        .and_then(ConstDecl::cast)
        .ok_or_else(|| Ice::new("a constant item's node is not a constant declaration"))?;
    let (ty, sig_ok) = match &iface_item.sig {
        ItemSig::Const(sig) => (sig.ty, !contains_error(&input.globals.types, sig.ty)),
        ItemSig::Error => (Ty::ERROR, false),
        _ => return Err(Ice::new("a constant without a constant signature")),
    };
    // A missing initializer or type is a syntax error.
    let Some(value) = decl.value() else {
        return Ok((None, Vec::new()));
    };
    let res = body_res(input, index)?;
    let mut cx = BodyCx::new(input, own, res, Generics::empty())?;
    let origin = decl.ty().map(|t| {
        (
            t.significant_span(),
            "expected because of this type".to_owned(),
        )
    });
    let expected = Expected::new(InferTy::Known(ty), origin);
    let value = cx.check_expr(value, Some(&expected))?;
    let body = cx.finish(value, ty)?;
    let mut deps = std::mem::take(&mut cx.const_deps);
    let diags = std::mem::take(&mut cx.diags);
    let Some(body) = body.filter(|_| sig_ok) else {
        return Ok((None, diags));
    };
    deps.sort_by_cached_key(|&dep| {
        let path = input.modules.get(dep.module).map(|m| m.path.clone());
        (path, input.name_of(own, dep))
    });
    deps.dedup();
    Ok((
        Some(thir::Item::Const(ConstDef {
            def: input.def_id(index),
            span: item.name_span,
            body,
            deps: deps.into(),
        })),
        diags,
    ))
}

/// A `test` body (§4.7, C15): of type `Nil` or `Result<Nil, E>` (D-140),
/// else E0073.
fn check_test(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    index: DefIndex,
    item: &ItemDef,
) -> Result<(Option<thir::Item>, Vec<Diagnostic>), Ice> {
    let decl = input
        .tree
        .node(item.node)
        .and_then(TestDecl::cast)
        .ok_or_else(|| Ice::new("a test item's node is not a test declaration"))?;
    let (Some(block), Some(name)) = (decl.body(), decl.name()) else {
        return Ok((None, Vec::new()));
    };
    let res = body_res(input, index)?;
    // An interpolated test name is a syntax error.
    let name_text = name.plain_value().unwrap_or_default();
    let mut cx = BodyCx::new(input, own, res, Generics::empty())?;
    let value = cx.block(block, None)?;
    let kind = cx.test_result(value)?;
    let body = cx.finish(value, Ty::NIL)?;
    let diags = std::mem::take(&mut cx.diags);
    let (Some(mut body), Some(kind)) = (body, kind) else {
        return Ok((None, diags));
    };
    let mut scratch = Vec::new();
    body.ret = match &kind {
        EntryKind::Nil => Ty::NIL,
        EntryKind::Result(err) => {
            let err = cx.infcx.zonk(err, &mut scratch)?;
            input.globals.types.named(builtin::RESULT, [Ty::NIL, err])
        }
    };
    let generics = Generics::empty();
    let entry = entry_info(input, own, &generics, kind, |t| {
        cx.infcx.zonk(t, &mut Vec::new())
    })?;
    Ok((
        Some(thir::Item::Test(TestDef {
            def: input.def_id(index),
            name: name_text.into(),
            span: name.significant_span(),
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

    /// An expectation without an origin label.
    pub const fn plain(ty: InferTy) -> Self {
        Self { ty, origin: None }
    }
}

/// What introduced a type variable, for E0057's wording.
#[derive(Clone, Debug)]
pub(crate) enum VarNote {
    /// A parameter of a closure or local function, or a `use` binder,
    /// named `name` when it is a plain name.
    Param { name: Option<Name> },
    /// The return type of the local function `name`.
    LocalFnRet { name: Name },
    /// Type parameter `param` of the generic item `callee`, at a use.
    TyArg { callee: DefId, param: Name },
}

/// The state of checking one top-level body, nested closures and local
/// functions included (§3.12.4).
///
/// The members are the contract between the checker's files:
/// `pat.rs`, `case.rs` and `record.rs` use them without editing this file.
pub(crate) struct BodyCx<'a> {
    pub input: &'a TypesInput<'a>,
    pub own: &'a ModuleInterface,
    pub res: &'a BodyRes,
    /// The enclosing top-level item's type parameters: for display, and
    /// the bounds of its `Param` types.
    pub generics: Generics,
    pub infcx: InferCtxt<'a>,
    /// THIR as it is built; types are filled in by [`Self::finish`].
    pub exprs: IndexVec<ExprId, Expr>,
    /// The inference type of each expression.
    pub expr_tys: IndexVec<ExprId, InferTy>,
    /// Type arguments of generic uses (calls, constructors, function
    /// values), zonked into the expression's kind by [`Self::finish`].
    pub inst: Vec<(ExprId, Box<[InferTy]>)>,
    /// The type of each local, once its binding is checked: the resolver's
    /// locals first, then the synthetic ones of [`Self::new_local`].
    pub locals: IndexVec<LocalId, Option<InferTy>>,
    /// The name and span of each synthetic local, in id order after the
    /// resolver's locals.
    pub synthetic: Vec<(Name, Span)>,
    /// The inference type of each pattern ([`Self::new_pat_id`]); the
    /// patterns' `ty` is filled from it by [`Self::finish`].
    pub pat_tys: IndexVec<PatId, InferTy>,
    /// The span of each pattern, for diagnostics about its type.
    pub pat_spans: IndexVec<PatId, Span>,
    /// Constraints resolved at the end of the body (§3.12.4, D-234).
    pub deferred: Vec<Deferred>,
    /// What introduced some of the type variables, for E0057.
    pub var_notes: Vec<(TyVid, VarNote)>,
    /// The local functions whose bodies are being checked, innermost
    /// last: inside its own body, a local function's name is a
    /// [`ExprKind::SelfRef`] (D-110).
    pub local_fns: Vec<LocalId>,
    /// The constants the body reads directly ([`thir::ConstDef::deps`]).
    pub const_deps: Vec<DefId>,
    /// The locals bound so far on the current path through the pattern
    /// being checked: alternatives each start from what was bound before
    /// them (§6.7).
    pub pat_path: Vec<LocalId>,
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
            synthetic: Vec::new(),
            pat_tys: IndexVec::new(),
            pat_spans: IndexVec::new(),
            deferred: Vec::new(),
            var_notes: Vec::new(),
            local_fns: Vec::new(),
            const_deps: Vec::new(),
            pat_path: Vec::new(),
            diags: Vec::new(),
            errored: false,
        })
    }

    fn check_fn_body(
        &mut self,
        block: Block<'_>,
        params: &[Ty],
        ret: Ty,
        origin: (Span, String),
    ) -> Result<Option<Body>, Ice> {
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

    /// The kind of result of a test body `value` (D-140): `Nil` (a body
    /// of type `Never` too, and a body whose type is still unknown), or
    /// `Result<Nil, E>`. Anything else is E0073 and gives `None`
    /// (D-493).
    fn test_result(&mut self, value: ExprId) -> Result<Option<EntryKind>, Ice> {
        let ty = self.ty_of(value)?;
        match self.infcx.resolve(&ty)? {
            InferTy::Known(t) if t == Ty::NIL || t == Ty::NEVER => return Ok(Some(EntryKind::Nil)),
            InferTy::Known(Ty::ERROR) => return Ok(None),
            InferTy::Var(_) if self.infcx.unify(&ty, &InferTy::NIL)?.is_ok() => {
                return Ok(Some(EntryKind::Nil));
            }
            InferTy::Named { def, args } if def == builtin::RESULT => {
                if let [ok, err] = &args[..]
                    && self.infcx.unify(ok, &InferTy::NIL)?.is_ok()
                {
                    return Ok(Some(EntryKind::Result(err.clone())));
                }
            }
            _ => {}
        }
        let shown = self.show(&ty);
        let span = self.tail_span(value)?;
        let mut diag = Diagnostic::new(
            codes::INVALID_TEST_RESULT,
            format!("a test body must have type `Nil` or `Result<Nil, E>`, found `{shown}`"),
        )
        .with_primary(span, format!("this has type `{shown}`"));
        if self.infcx.shallow(&ty)? == InferTy::Known(Ty::BOOL) {
            diag = diag.with_help("to check the condition, write `assert` before it");
        }
        self.report(diag);
        Ok(None)
    }

    /// The span of a block's value: its tail, or the block itself.
    fn tail_span(&self, id: ExprId) -> Result<Span, Ice> {
        let expr = self.exprs.try_get(id, "expression")?;
        match &expr.kind {
            ExprKind::Block {
                tail: Some(tail), ..
            } => self.tail_span(*tail),
            _ => Ok(expr.span),
        }
    }

    // ---- building THIR ----------------------------------------------

    /// Appends an expression of inference type `ty`.
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

    /// Records a diagnostic; an error marks the body as errored.
    pub fn report(&mut self, diag: Diagnostic) {
        if diag.is_error() {
            self.errored = true;
        }
        self.diags.push(diag);
    }

    /// The inference type of expression `id`.
    pub fn ty_of(&self, id: ExprId) -> Result<InferTy, Ice> {
        self.expr_tys.try_get(id, "expression type").cloned()
    }

    /// The span of expression `id`.
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

    /// Sets the type of `local` when its binding is checked.
    pub fn set_local(&mut self, local: LocalId, ty: InferTy) -> Result<(), Ice> {
        let slot = self
            .locals
            .get_mut(local)
            .ok_or_else(|| Ice::new(format!("no local {local:?}")))?;
        *slot = Some(ty);
        Ok(())
    }

    /// A new synthetic local of type `ty`, made by desugaring (a capture's
    /// argument, a pipe's left side), after the resolver's locals.
    pub fn new_local(&mut self, name: Name, ty: InferTy, span: Span) -> Result<LocalId, Ice> {
        let local = self.locals.push(Some(ty))?;
        self.synthetic.push((name, span));
        Ok(local)
    }

    /// A new pattern id whose inference type is `ty`. The pattern's `ty`
    /// stays `Ty::ERROR` until [`Self::finish`] zonks this one into it.
    pub fn new_pat_id(&mut self, ty: InferTy, span: Span) -> Result<PatId, Ice> {
        let id = self.pat_tys.push(ty)?;
        let span_id = self.pat_spans.push(span)?;
        if span_id != id {
            return Err(Ice::new("pattern tables out of step"));
        }
        Ok(id)
    }

    /// A fresh type variable that may become any type.
    pub fn fresh_var(&mut self, span: Span) -> Result<InferTy, Ice> {
        self.infcx.new_var(span, VarKind::General)
    }

    /// A fresh type variable with a note for E0057.
    pub(crate) fn noted_var(&mut self, span: Span, note: VarNote) -> Result<InferTy, Ice> {
        let var = self.fresh_var(span)?;
        if let InferTy::Var(vid) = var {
            self.var_notes.push((vid, note));
        }
        Ok(var)
    }

    /// `t` shallow-resolved, or `None` while it is still a variable: for
    /// the constructs that need a known type at their point of checking
    /// (D-213).
    pub fn known_ty(&mut self, t: &InferTy) -> Result<Option<InferTy>, Ice> {
        Ok(match self.infcx.shallow(t)? {
            InferTy::Var(_) => None,
            known => Some(known),
        })
    }

    /// A type written in the body (an annotation, a turbofish argument),
    /// lowered with the body's path resolutions and the item's generics.
    /// Errors are reported here, and the result is then [`Ty::ERROR`].
    pub fn lower_written_ty(&mut self, ty: ast::Type<'_>) -> Result<InferTy, Ice> {
        let lowering = TyLowering {
            input: self.input,
            paths: &self.res.paths,
        };
        let mut diags = Vec::new();
        let lowered = lowering.lower_in_body(self.own, ty, &mut diags)?;
        for diag in diags {
            self.report(diag);
        }
        if contains_error(&self.input.globals.types, lowered) {
            self.errored = true;
            return Ok(InferTy::ERROR);
        }
        Ok(InferTy::Known(lowered))
    }

    /// The resolution of the path node `node` of the body.
    pub fn path_res(&self, node: cheby_syntax::NodeId) -> Result<Res, Ice> {
        self.res
            .paths
            .get(&node)
            .copied()
            .ok_or_else(|| Ice::new("a path of the body was not resolved"))
    }

    /// `t` as users write it.
    pub fn show(&self, t: &InferTy) -> String {
        let cx = self.input.display_cx(self.own, Some(&self.generics));
        self.infcx.display(t, &cx)
    }

    /// An interned name for a synthetic local.
    pub(crate) fn synthetic_name(&self, text: &str) -> Name {
        self.input.globals.names.intern(text)
    }

    // ---- the end of the body -------------------------------------------

    /// Defaults literals, zonks every type into THIR (patterns, locals and
    /// type arguments included), reports types that stayed unknown
    /// (E0057) and literals that do not fit (E0071), resolves the deferred
    /// constraints and, for a body without errors, runs the
    /// exhaustiveness checks. `None` for a body with an error.
    fn finish(&mut self, value: ExprId, ret: Ty) -> Result<Option<Body>, Ice> {
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
        let mut pat_tys = IndexVec::<PatId, Ty>::new();
        for ty in &self.pat_tys {
            pat_tys.push(self.infcx.zonk(ty, &mut unresolved)?)?;
        }
        let mut missing_pat = false;
        for expr in &mut exprs {
            for (_, pat) in expr.kind.pats_mut() {
                pat.walk_mut(&mut |p| match pat_tys.get(p.id) {
                    Some(&ty) => p.ty = ty,
                    None => missing_pat = true,
                });
            }
        }
        if missing_pat {
            return Err(Ice::new("a pattern without a pattern id"));
        }
        let locals = self.zonk_locals(&mut unresolved)?;
        self.report_unresolved(unresolved)?;
        self.check_literals(&exprs);
        self.resolve_deferred(&mut exprs)?;
        if self.errored || has_error(&self.diags) {
            return Ok(None);
        }
        if exprs.iter().any(|e| matches!(e.kind, ExprKind::Error)) {
            return Err(Ice::new("an error expression in a body without errors"));
        }
        let body = Body {
            params: self.res.params.clone(),
            locals,
            exprs,
            value,
            ret,
        };
        let found = exhaust::check_body(self.input, self.own, &body)?;
        let errored = has_error(&found);
        self.diags.extend(found);
        if errored {
            return Ok(None);
        }
        Ok(Some(body))
    }

    /// E0057 for every type variable nothing determined, at the
    /// expression that introduced it, unless the body already has an
    /// error, which such a type usually follows from
    /// (D-494).
    fn report_unresolved(&mut self, mut unresolved: Vec<TyVid>) -> Result<(), Ice> {
        if unresolved.is_empty() {
            return Ok(());
        }
        unresolved.sort_unstable();
        unresolved.dedup();
        if self.errored {
            return Ok(());
        }
        let mut seen: Vec<Span> = Vec::new();
        for vid in unresolved {
            let Some(span) = self.infcx.origin(vid) else {
                continue;
            };
            if seen.contains(&span) {
                continue;
            }
            seen.push(span);
            let mut note = None;
            for (noted, n) in &self.var_notes {
                if self.infcx.root_of(*noted)? == vid {
                    note = Some(n.clone());
                    break;
                }
            }
            let diag = self.annotation_needed(span, note.as_ref());
            self.report(diag);
        }
        self.errored = true;
        Ok(())
    }

    /// E0057 for a type introduced at `span`.
    fn annotation_needed(&self, span: Span, note: Option<&VarNote>) -> Diagnostic {
        let names = &self.input.globals.names;
        let diag = Diagnostic::new(codes::TYPE_ANNOTATION_NEEDED, "type annotation needed");
        match note {
            Some(VarNote::Param { name: Some(name) }) => {
                let name = names.get(*name);
                diag.with_primary(span, format!("the type of `{name}` is not known"))
                    .with_help(format!(
                        "give the parameter a type annotation: `{name}: <type>`"
                    ))
            }
            Some(VarNote::Param { name: None }) => diag
                .with_primary(span, "the type of this parameter is not known")
                .with_help("give the parameter a type annotation: `<pattern>: <type>`"),
            Some(VarNote::LocalFnRet { name }) => {
                let name = names.get(*name);
                diag.with_primary(span, format!("the return type of `{name}` is not known"))
                    .with_help("write the return type: `-> <type>`")
            }
            Some(VarNote::TyArg { callee, param }) => {
                let callee = self.input.name_of(self.own, *callee);
                let param = names.get(*param);
                diag.with_primary(
                    span,
                    format!("the type argument `{param}` of `{callee}` is not known"),
                )
                .with_help(format!(
                    "give the type arguments after the name: `{callee}::<…>`"
                ))
            }
            None => diag
                .with_primary(span, "the type of this is not known")
                .with_help("annotate the binding that holds it: `let name: <type> = …`"),
        }
    }

    /// E0071 for every integer or float literal, in an expression or a
    /// pattern, whose value does not fit its final type (D-084, §3.3.4).
    /// The JS safe-integer rule is the cross-target check's (C12).
    fn check_literals(&mut self, exprs: &IndexVec<ExprId, Expr>) {
        let mut found = Vec::new();
        for expr in exprs {
            if let ExprKind::Lit(lit) = &expr.kind {
                found.push((lit.clone(), expr.ty, expr.span));
            }
            for (_, pat) in expr.kind.pats() {
                pat.walk(&mut |p| {
                    if let PatKind::Lit(lit) = &p.kind {
                        found.push((lit.clone(), p.ty, p.span));
                    }
                });
            }
        }
        for (lit, ty, span) in found {
            if let Some(diag) = self.literal_range(&lit, ty, span) {
                self.report(diag);
            }
        }
    }

    /// E0071 if `lit` does not fit `ty`.
    fn literal_range(&self, lit: &Lit, ty: Ty, span: Span) -> Option<Diagnostic> {
        let shown = self.show(&InferTy::Known(ty));
        match lit {
            Lit::Int(value) => {
                let (min, max) = int_range(ty)?;
                if (min..=max).contains(value) {
                    return None;
                }
                Some(
                    Diagnostic::new(
                        codes::LITERAL_OUT_OF_RANGE,
                        format!("literal out of range for `{shown}`"),
                    )
                    .with_primary(span, format!("`{value}` does not fit in `{shown}`"))
                    .with_note(format!("`{shown}` holds {min} to {max}")),
                )
            }
            Lit::Float(value) => {
                // A literal fits when it rounds to a finite value of its
                // type, so `3.4028235e38` is the largest `F32` (D-496).
                // Below half an ulp past `f32::MAX` it rounds down to it;
                // at the midpoint it rounds to even, which is the infinity.
                // The `F32` value is rounded once from the decimal text,
                // so a literal just below the midpoint fits even when its
                // `Float` rounding is the midpoint (D-569).
                let fits = if ty == Ty::F32 {
                    value.narrow.is_finite()
                } else if ty == Ty::FLOAT {
                    value.wide.is_finite()
                } else {
                    true
                };
                (!fits).then(|| {
                    Diagnostic::new(
                        codes::LITERAL_OUT_OF_RANGE,
                        format!("literal out of range for `{shown}`"),
                    )
                    .with_primary(span, format!("this is too large for `{shown}`"))
                    .with_note(format!(
                        "the largest finite `{shown}` is about {}",
                        if ty == Ty::F32 { "3.4e38" } else { "1.8e308" }
                    ))
                })
            }
            Lit::Str(_) => None,
        }
    }

    /// Every local with its zonked type: the resolver's, then the
    /// synthetic ones.
    fn zonk_locals(
        &self,
        unresolved: &mut Vec<TyVid>,
    ) -> Result<IndexVec<LocalId, thir::Local>, Ice> {
        let mut locals = IndexVec::new();
        let decls = self
            .res
            .locals
            .iter()
            .map(|decl| (decl.name, decl.span, false))
            .chain(
                self.synthetic
                    .iter()
                    .map(|&(name, span)| (name, span, true)),
            );
        for ((name, span, synthetic), ty) in decls.zip(self.locals.iter()) {
            let ty = match ty {
                Some(t) => self.infcx.zonk(t, unresolved)?,
                None => Ty::ERROR,
            };
            locals.push(thir::Local {
                name,
                ty,
                span,
                synthetic,
            })?;
        }
        if locals.len() != self.locals.len() {
            return Err(Ice::new("local tables out of step"));
        }
        Ok(locals)
    }
}

/// The values of integer type `ty`, `None` for other types.
fn int_range(ty: Ty) -> Option<(i128, i128)> {
    Some(if ty == Ty::INT || ty == Ty::I64 {
        (i128::from(i64::MIN), i128::from(i64::MAX))
    } else if ty == Ty::I8 {
        (i128::from(i8::MIN), i128::from(i8::MAX))
    } else if ty == Ty::I16 {
        (i128::from(i16::MIN), i128::from(i16::MAX))
    } else if ty == Ty::I32 {
        (i128::from(i32::MIN), i128::from(i32::MAX))
    } else if ty == Ty::U8 {
        (0, i128::from(u8::MAX))
    } else if ty == Ty::U16 {
        (0, i128::from(u16::MAX))
    } else if ty == Ty::U32 {
        (0, i128::from(u32::MAX))
    } else if ty == Ty::U64 {
        (0, i128::from(u64::MAX))
    } else {
        return None;
    })
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
        // An error expression keeps no type arguments, and an interface
        // call's callee is chosen at the end of the body.
        ExprKind::Error
        | ExprKind::Call {
            callee: Callee::Iface { .. } | Callee::Dyn { .. },
            ..
        } => Ok(()),
        _ => Err(Ice::new("type arguments for an expression that takes none")),
    }
}
