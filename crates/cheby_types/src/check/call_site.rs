//! Call-site functions (§4.8): the rules of a function that carries the
//! standard library's `@std_call_site(constructor)`, as the log functions
//! of `std::log` do (D-1628). The attribute is the one source of these
//! rules: no function is known by its name, and the constructor is read
//! from the function's signature in its module's interface
//! ([`cheby_iface::FnSig::call_site`], D-246, D-1630).
//!
//! - Every call of such a function, as a plain call, a pipe stage with or
//!   without a hole, the call of a `use` or the call inside a capture,
//!   becomes a call of the constructor with three more arguments, the
//!   [`ExprKind::CallSite`] parts of the call site (D-1628, D-1631). The
//!   call site is the start of the path that names the function, not the
//!   start of the call expression, which for a pipe is the start of the
//!   chain (D-1629, D-856).
//! - A mention of such a function as a value becomes a closure that calls
//!   the constructor with the place of the mention, as a positional
//!   constructor or an interface function used as a value becomes a
//!   closure (D-500, D-1629).
//! - The swaps are made in the finished body, so the language server's
//!   facts keep the function that is written, as `check::key` does.
//! - A constructor has the generics of the function it stands for, which
//!   `decl::call_site` checks in std's own module, so the recorded type
//!   arguments and dictionaries fit it unchanged.
//! - Such a function never counts toward satisfying an interface, so no
//!   interface call, bound or `dyn` conversion reaches it: `ifaces::solve`
//!   asks [`call_site_fn`] where it picks a type's module function
//!   (D-1630).

use cheby_diag::Span;
use cheby_iface::{
    DefId, DefKind, Ice, ItemSig, ModuleInterface, Name, Namespace, Ty, TyKind, Vis,
};

use super::BodyCx;
use crate::TypesInput;
use crate::decl::call_site::fits;
use crate::infer::InferTy;
use crate::thir::{self, Body, CallSitePart, Callee, ExprId, ExprKind, ParamOrigin, PatKind};

/// A function that carries `@std_call_site(constructor)` (D-1628).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallSiteFn {
    /// The function.
    def: DefId,
    /// The name of the constructor called in its place, a function of the
    /// same module.
    ctor: Name,
}

/// The function `def` with its call-site constructor, when its signature
/// records one (D-1628). It runs for every function a body names, so it
/// reads only the item.
///
/// # Errors
///
/// An [`Ice`] if `def` has no interface item.
pub(crate) fn call_site_fn(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefId,
) -> Result<Option<CallSiteFn>, Ice> {
    let item = input.item(own, def)?;
    if item.kind != DefKind::Fn {
        return Ok(None);
    }
    let ItemSig::Fn(sig) = &item.sig else {
        return Ok(None);
    };
    Ok(sig.call_site.map(|ctor| CallSiteFn { def, ctor }))
}

/// The constructor that stands for `which` (D-1630): from `own` when this
/// module is the function's own, else through the dependencies. `None`
/// when the constructor's signature has errors or does not have the shape
/// the function gives it ([`fits`]): its own module reports both, the
/// first where the signature is written and the second as E0045 at the
/// attribute (`decl::call_site`), so the use is silent and is never
/// rewritten into an ill-typed call.
///
/// # Errors
///
/// An [`Ice`] if the function's module is not readable or has no such
/// function: a mention of `which` needs its module, and a module whose
/// signature records a constructor declares it.
fn call_site_ctor(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    which: CallSiteFn,
) -> Result<Option<DefId>, Ice> {
    let missing = || {
        Ice::new(format!(
            "no call-site constructor `{}` for `{}`",
            input.globals.names.get(which.ctor),
            input.name_of(own, which.def)
        ))
    };
    let ItemSig::Fn(sig) = &input.item(own, which.def)?.sig else {
        return Err(Ice::new(
            "a call-site function without a function signature",
        ));
    };
    let module = which.def.module;
    let iface = if module == input.module {
        own
    } else {
        input.deps.get(module).ok_or_else(missing)?
    };
    // The module's interface records a constructor only when it names a
    // function of the module, so a name a dependency's interface does not
    // hold is a `priv` function that the encoding left out. Its module
    // reports it as E0045, cold or warm, and the use stays silent (D-1630).
    let Some(index) = iface.lookup(Namespace::Value, which.ctor) else {
        return if module == input.module {
            Err(missing())
        } else {
            Ok(None)
        };
    };
    let item = iface.item(index).ok_or_else(missing)?;
    if item.vis == Vis::Priv {
        return Ok(None);
    }
    match &item.sig {
        ItemSig::Fn(ctor) => Ok(fits(ctor, sig).then(|| DefId::new(module, index))),
        ItemSig::Error => Ok(None),
        ItemSig::Type(_)
        | ItemSig::Ctor(_)
        | ItemSig::Alias(_)
        | ItemSig::Const(_)
        | ItemSig::Interface(_)
        | ItemSig::InterfaceFn(_)
        | ItemSig::Test => Err(missing()),
    }
}

impl BodyCx<'_> {
    /// Notes that `id`, a call of `def` or `def` as a value, names a
    /// call-site function at `site`, the start of the path that names it
    /// (D-1629), when `def` carries `@std_call_site`. The swap waits for
    /// the finished body ([`Self::replace_call_sites`]).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` has no interface item.
    pub(crate) fn note_call_site(&mut self, id: ExprId, def: DefId, site: Span) -> Result<(), Ice> {
        if call_site_fn(self.input, self.own, def)?.is_some() {
            self.call_sites.push((id, site));
        }
        Ok(())
    }

    /// Replaces, in the finished `body`, every use [`Self::note_call_site`]
    /// noted: a call calls the constructor with the three parts of its call
    /// site appended, and a function value becomes a closure that does so
    /// for its parameters (D-1628, D-1629). `false`, with `body` partly
    /// rewritten, when a constructor is unusable ([`call_site_ctor`]): the
    /// item then has no body, as one whose signature has an error, and the
    /// function's own module reports why.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a noted expression is not a call of a call-site
    /// function or such a function as a value, or if an item is unreadable.
    pub(crate) fn replace_call_sites(&mut self, body: &mut Body) -> Result<bool, Ice> {
        for (id, site) in std::mem::take(&mut self.call_sites) {
            let expr = body.exprs.try_get(id, "expression")?;
            let (ExprKind::Call {
                callee: Callee::Fn { def, .. },
                ..
            }
            | ExprKind::FnRef { def, .. }) = &expr.kind
            else {
                return Err(Ice::new(
                    "a call site that is neither a call nor a function",
                ));
            };
            let (def, ty) = (*def, expr.ty);
            let Some(which) = call_site_fn(self.input, self.own, def)? else {
                return Err(Ice::new(
                    "a call site of a function without `@std_call_site`",
                ));
            };
            let Some(ctor) = call_site_ctor(self.input, self.own, which)? else {
                return Ok(false);
            };
            let parts = site_parts(body, site)?;
            if matches!(
                body.exprs.try_get(id, "expression")?.kind,
                ExprKind::FnRef { .. }
            ) {
                self.value_closure(body, id, ctor, ty, site, parts)?;
                continue;
            }
            let call = body
                .exprs
                .get_mut(id)
                .ok_or_else(|| Ice::new("a call site disappeared"))?;
            let ExprKind::Call {
                callee: Callee::Fn { def, .. },
                args,
            } = &mut call.kind
            else {
                return Err(Ice::new("a call site changed its kind"));
            };
            *def = ctor;
            let mut all = std::mem::take(args).into_vec();
            all.extend(parts);
            *args = all.into();
        }
        Ok(true)
    }

    /// Turns the function value `id` of type `ty` into the closure
    /// `fn(a, …) { ctor(a, …, path, line, column) }`, whose last three
    /// arguments are `parts`, the place of the mention (D-1629). The
    /// value's type arguments and dictionaries become the call's.
    fn value_closure(
        &mut self,
        body: &mut Body,
        id: ExprId,
        ctor: DefId,
        ty: Ty,
        site: Span,
        parts: [ExprId; 3],
    ) -> Result<(), Ice> {
        let ExprKind::FnRef { ty_args, dicts, .. } = &body.exprs.try_get(id, "expression")?.kind
        else {
            return Err(Ice::new("a call-site function value changed its kind"));
        };
        let (ty_args, dicts) = (ty_args.clone(), dicts.clone());
        let TyKind::Fn { params, ret } = self.input.globals.types.kind(ty) else {
            return Err(Ice::new("a function value whose type is not a function"));
        };
        let (params, ret) = (params.clone(), *ret);
        let name = self.synthetic_name("%param");
        let mut thir_params = Vec::with_capacity(params.len());
        let mut args = Vec::with_capacity(params.len().saturating_add(3));
        for &param in &params {
            let local = body.locals.push(thir::Local {
                name,
                ty: param,
                span: site,
                synthetic: true,
            })?;
            thir_params.push(thir::Param {
                pat: thir::Pat {
                    id: self.new_pat_id(InferTy::Known(param), site)?,
                    kind: PatKind::Bind(local),
                    ty: param,
                    span: site,
                },
                span: site,
                origin: ParamOrigin::Closure,
            });
            args.push(body.exprs.push(thir::Expr {
                kind: ExprKind::Local(local),
                ty: param,
                span: site,
            })?);
        }
        args.extend(parts);
        let call = body.exprs.push(thir::Expr {
            kind: ExprKind::Call {
                callee: Callee::Fn {
                    def: ctor,
                    ty_args,
                    dicts,
                },
                args: args.into(),
            },
            ty: ret,
            span: site,
        })?;
        let value = body
            .exprs
            .get_mut(id)
            .ok_or_else(|| Ice::new("a call-site function value disappeared"))?;
        value.kind = ExprKind::Closure {
            params: thir_params.into(),
            body: call,
        };
        Ok(())
    }
}

/// The three parts of the place `site`, as expressions of `body`.
fn site_parts(body: &mut Body, site: Span) -> Result<[ExprId; 3], Ice> {
    let mut part = |part, ty| {
        body.exprs.push(thir::Expr {
            kind: ExprKind::CallSite(part),
            ty,
            span: site,
        })
    };
    Ok([
        part(CallSitePart::Path, Ty::STRING)?,
        part(CallSitePart::Line, Ty::INT)?,
        part(CallSitePart::Column, Ty::INT)?,
    ])
}

#[cfg(test)]
mod tests;
