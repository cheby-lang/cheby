//! The satisfaction engine: finding a type's functions in its module,
//! matching their signatures, and building dictionaries.

use cheby_iface::{
    DefId, DefKind, Ice, IfaceItem, InterfaceDef, ItemSig, ModuleId, ModuleInterface, Name,
    Namespace, ParamIdx, Ty, TyKind, Vis, builtin, interface_fns,
};

use super::tyutil::{match_ty, mentions, params_of, subst};
use super::{IfaceCx, OpFn, Reason, SELF, Unsatisfied, Want, is_builtin_numeric};
use crate::infer::contains_error;
use crate::thir::{Callee, Dict, DictFn, OpImpl};

/// How deep satisfaction may recurse through the bounds of satisfying
/// functions and tuple elements before it gives up cleanly. Each level is
/// a strictly smaller part of the type, so real programs stay far below.
pub(crate) const DEPTH_LIMIT: u32 = 64;

/// A function the type's module must have. `Self` is `ParamIdx` 0.
pub(super) struct Required {
    pub name: Name,
    /// The interface function, or `None` for an operator.
    pub func: Option<DefId>,
    pub params: Box<[Ty]>,
    pub ret: Ty,
}

/// Where a nominal type's functions are looked for.
struct TypeHome<'a> {
    module: &'a ModuleInterface,
    ty_item: &'a IfaceItem,
}

/// The dictionary used where a type contains an error (already reported).
fn placeholder(iface: DefId, ty: Ty) -> Dict {
    Dict::Impl {
        iface,
        ty,
        fns: Box::new([]),
    }
}

/// The rank of a visibility: `priv` < package < `pub`.
const fn rank(vis: Vis) -> u8 {
    match vis {
        Vis::Priv => 0,
        Vis::Package => 1,
        Vis::Pub => 2,
    }
}

impl<'a> IfaceCx<'a> {
    /// The interface item `def`, from this module or a dependency.
    pub(super) fn item(&self, def: DefId) -> Result<&'a IfaceItem, Ice> {
        self.input.item(self.own, def)
    }

    fn read(&self, def: DefId) -> Option<&'a IfaceItem> {
        self.input.item(self.own, def).ok()
    }

    /// Every function of `iface`, in dictionary order.
    pub(super) fn interface_fns(&self, iface: DefId) -> Result<Vec<DefId>, Ice> {
        interface_fns(iface, &|def| self.read(def))
    }

    fn interface_def(&self, iface: DefId) -> Result<Option<&'a InterfaceDef>, Ice> {
        Ok(match &self.item(iface)?.sig {
            ItemSig::Interface(def) => Some(def),
            _ => None,
        })
    }

    /// Whether `outer` embeds `inner`, directly or through other embedded
    /// interfaces.
    pub(super) fn embeds(&self, outer: DefId, inner: DefId) -> Result<bool, Ice> {
        let mut stack = vec![outer];
        let mut seen = vec![outer];
        while let Some(next) = stack.pop() {
            let Some(def) = self.interface_def(next)? else {
                continue;
            };
            for &embed in &def.embeds {
                if embed == inner {
                    return Ok(true);
                }
                if !seen.contains(&embed) {
                    seen.push(embed);
                    stack.push(embed);
                }
            }
        }
        Ok(false)
    }

    /// The bounds of the enclosing item's type parameter `param`.
    pub(super) fn bounds(&self, param: ParamIdx) -> Result<&'a [DefId], Ice> {
        self.generics
            .params
            .get(param)
            .map(|p| &*p.bounds)
            .ok_or_else(|| Ice::new(format!("type parameter {param:?} not in scope")))
    }

    /// The interface `name` of `std::ops`, if this module can read it.
    pub(super) fn ops_iface(&self, name: &str) -> Option<DefId> {
        let module = self.input.modules.find(&builtin::home_path("ops"))?;
        let iface = if module == self.input.module {
            self.own
        } else {
            self.input.deps.get(module)?
        };
        let name = self.input.globals.names.lookup(name)?;
        let index = iface.lookup(Namespace::Type, name)?;
        let item = iface.item(index)?;
        (item.kind == DefKind::Interface).then(|| DefId::new(module, index))
    }

    /// Whether `iface` is `Compare` of `std::ops`, which tuples satisfy by
    /// the built-in rule (D-167).
    fn is_ops_compare(&self, iface: DefId) -> Result<bool, Ice> {
        let path_ok = self
            .input
            .modules
            .get(iface.module)
            .is_some_and(|info| info.path == builtin::home_path("ops"));
        Ok(path_ok && self.input.globals.names.get(self.item(iface)?.name) == "Compare")
    }

    /// The function a type's module must have for interface function `f`,
    /// or `None` if `f`'s signature had an error.
    fn required_of(&self, f: DefId) -> Result<Option<Required>, Ice> {
        let item = self.item(f)?;
        Ok(match &item.sig {
            ItemSig::InterfaceFn(sig) => Some(Required {
                name: item.name,
                func: Some(f),
                params: sig.params.clone(),
                ret: sig.ret,
            }),
            _ => None,
        })
    }

    /// The function behind operator `op`, as a requirement.
    fn required_op(&self, op: OpFn) -> Required {
        let types = &self.input.globals.types;
        let this = types.param(SELF);
        let (params, ret): (Box<[Ty]>, Ty) = match op {
            OpFn::Neg => (Box::new([this]), this),
            OpFn::Compare => (Box::new([this, this]), Ty::ORDER),
            OpFn::Add | OpFn::Sub | OpFn::Mul | OpFn::Div => (Box::new([this, this]), this),
        };
        Required {
            name: self.input.globals.names.intern(op.fn_name()),
            func: None,
            params,
            ret,
        }
    }

    /// The module whose functions count for nominal type `def` (§8.2):
    /// its declaring module, or the home module of a builtin type (D-447),
    /// which must be readable here.
    fn type_home(&self, def: DefId) -> Result<Result<TypeHome<'a>, Reason>, Ice> {
        let ty_item = self.item(def)?;
        if def.module == ModuleId::BUILTIN {
            let Some(&(_, home)) = builtin::HOMES.iter().find(|(d, _)| *d == def) else {
                return Ok(Err(Reason::NoModule));
            };
            let module = match self.input.modules.find(&builtin::home_path(home)) {
                Some(id) if id == self.input.module => Some(self.own),
                Some(id) => self.input.deps.get(id),
                None => None,
            };
            return Ok(match module {
                Some(module) => Ok(TypeHome { module, ty_item }),
                // GAP(home-not-in-deps): a std module that does not import
                // the home module cannot use its functions.
                None => Err(Reason::HomeNotInDeps { home }),
            });
        }
        let module = if def.module == self.input.module {
            self.own
        } else {
            self.input
                .deps
                .get(def.module)
                .ok_or_else(|| Ice::new(format!("the module of {def:?} is not readable")))?
        };
        Ok(Ok(TypeHome { module, ty_item }))
    }

    /// The function of `home` that meets `req` for `Self` = `ty`, with its
    /// type arguments and the dictionaries of its own bounds (D-064).
    fn find_fn(
        &self,
        home: &TypeHome<'a>,
        ty: Ty,
        req: &Required,
        depth: u32,
    ) -> Result<Result<DictFn, Reason>, Ice> {
        let types = &self.input.globals.types;
        let self_args = [ty];
        let want_params = req
            .params
            .iter()
            .map(|&p| subst(types, p, &self_args))
            .collect::<Result<Vec<Ty>, Ice>>()?;
        let want_ret = subst(types, req.ret, &self_args)?;
        let required = types.func(want_params.clone(), want_ret);
        let module = home.module;
        let missing = Reason::Missing {
            name: req.name,
            func: req.func,
            required,
            module: module.module,
        };
        let Some(index) = module.lookup(Namespace::Value, req.name) else {
            return Ok(Err(missing));
        };
        let item = module
            .item(index)
            .ok_or_else(|| Ice::new("a looked-up item is missing"))?;
        if item.kind != DefKind::Fn {
            return Ok(Err(missing));
        }
        let found = DefId::new(module.module, index);
        let silent = DictFn {
            def: found,
            ty_args: Box::new([]),
            dicts: Box::new([]),
        };
        // An error already reported in the function's signature.
        let ItemSig::Fn(sig) = &item.sig else {
            return Ok(Ok(silent));
        };
        let actual = types.func(
            sig.params.iter().map(|p| p.ty).collect::<Box<[Ty]>>(),
            sig.ret,
        );
        let mismatch = Reason::Signature {
            found,
            func: req.func,
            required,
            actual,
        };
        let Some(k) = req.params.iter().position(|&p| mentions(types, p, SELF)) else {
            // An interface function without `Self` in a parameter: E0066
            // at its declaration.
            return Ok(Ok(silent));
        };
        let (Some(pattern), Some(&target)) = (sig.params.get(k), want_params.get(k)) else {
            return Ok(Err(mismatch));
        };
        if sig.params.len() != want_params.len() {
            return Ok(Err(mismatch));
        }
        let mut bound = vec![None; sig.generics.len()];
        if !match_ty(types, pattern.ty, target, &mut bound)? {
            return Ok(Err(mismatch));
        }
        let Some(ty_args) = bound.into_iter().collect::<Option<Vec<Ty>>>() else {
            // A type parameter the `Self` parameter does not determine.
            return Ok(Err(mismatch));
        };
        for (param, &want) in sig.params.iter().zip(&want_params) {
            if subst(types, param.ty, &ty_args)? != want {
                return Ok(Err(mismatch));
            }
        }
        if subst(types, sig.ret, &ty_args)? != want_ret {
            return Ok(Err(mismatch));
        }
        if rank(item.vis) < rank(home.ty_item.vis) {
            return Ok(Err(Reason::LessVisible {
                found,
                func: req.func,
                type_vis: home.ty_item.vis,
            }));
        }
        let mut dicts = Vec::new();
        for (param, generic) in sig.generics.params.iter_enumerated() {
            let arg = ty_args
                .get(usize::try_from(param.as_u32()).unwrap_or(usize::MAX))
                .copied()
                .ok_or_else(|| Ice::new("a type argument is missing"))?;
            for &b in &generic.bounds {
                match self.satisfy_ty(arg, b, depth.saturating_add(1))? {
                    Ok(dict) => dicts.push(dict),
                    Err(inner) => {
                        return Ok(Err(Reason::Bound {
                            found,
                            param,
                            inner: Box::new(inner),
                        }));
                    }
                }
            }
        }
        Ok(Ok(DictFn {
            def: found,
            ty_args: ty_args.into(),
            dicts: dicts.into(),
        }))
    }

    /// The satisfying functions of nominal type `ty` (declared as `def`)
    /// for the interface functions `fns`, in that order.
    fn impl_fns(
        &self,
        def: DefId,
        ty: Ty,
        fns: &[DefId],
        depth: u32,
    ) -> Result<Result<Vec<DictFn>, Reason>, Ice> {
        let home = match self.type_home(def)? {
            Ok(home) => home,
            Err(reason) => return Ok(Err(reason)),
        };
        let mut out = Vec::with_capacity(fns.len());
        for &f in fns {
            let Some(req) = self.required_of(f)? else {
                // The interface function's signature had an error.
                out.push(DictFn {
                    def: f,
                    ty_args: Box::new([]),
                    dicts: Box::new([]),
                });
                continue;
            };
            match self.find_fn(&home, ty, &req, depth)? {
                Ok(dict_fn) => out.push(dict_fn),
                Err(reason) => return Ok(Err(reason)),
            }
        }
        Ok(Ok(out))
    }

    /// The dictionary of the enclosing item's type parameter `param` for
    /// `iface`: its bound, or a bound that embeds `iface`.
    fn param_dict(&self, param: ParamIdx, iface: DefId) -> Result<Option<Dict>, Ice> {
        let bounds = self.bounds(param)?;
        let index = |i: usize| u32::try_from(i).map_err(|_| Ice::new("too many bounds"));
        for (i, &b) in bounds.iter().enumerate() {
            if b == iface {
                return Ok(Some(Dict::Param {
                    param,
                    bound: index(i)?,
                }));
            }
        }
        for (i, &b) in bounds.iter().enumerate() {
            if self.embeds(b, iface)? {
                return Ok(Some(Dict::Embedded {
                    of: Box::new(Dict::Param {
                        param,
                        bound: index(i)?,
                    }),
                    iface,
                }));
            }
        }
        Ok(None)
    }

    /// The first type parameter `ty` mentions that has no bound, which
    /// therefore has no descriptor (D-236).
    fn unbounded_param(&self, ty: Ty) -> Result<Option<ParamIdx>, Ice> {
        for param in params_of(&self.input.globals.types, ty) {
            if self.bounds(param)?.is_empty() {
                return Ok(Some(param));
            }
        }
        Ok(None)
    }

    /// `dict`, a dictionary built for `ty`, if `ty`'s descriptor can be
    /// built here (GAP(bound-needs-descriptor), D-236).
    fn with_descriptor(
        &self,
        ty: Ty,
        want: Want,
        dict: Dict,
    ) -> Result<Result<Dict, Unsatisfied>, Ice> {
        Ok(match self.unbounded_param(ty)? {
            Some(param) => Err(Unsatisfied {
                ty,
                want,
                reason: Reason::Descriptor { param },
            }),
            None => Ok(dict),
        })
    }

    /// Whether `ty` satisfies `iface`, with its dictionary (§8.2).
    pub(super) fn satisfy_ty(
        &self,
        ty: Ty,
        iface: DefId,
        depth: u32,
    ) -> Result<Result<Dict, Unsatisfied>, Ice> {
        let types = &self.input.globals.types;
        let want = Want::Iface(iface);
        let fail = |reason| Ok(Err(Unsatisfied { ty, want, reason }));
        if depth > DEPTH_LIMIT {
            return fail(Reason::TooDeep);
        }
        if contains_error(types, ty) {
            return Ok(Ok(placeholder(iface, ty)));
        }
        let fns = self.interface_fns(iface)?;
        let empty = Dict::Impl {
            iface,
            ty,
            fns: Box::new([]),
        };
        match types.kind(ty) {
            TyKind::Dyn { iface: has, .. } => {
                if *has == iface || self.embeds(*has, iface)? {
                    Ok(Ok(Dict::Dyn { iface, ty }))
                } else if fns.is_empty() {
                    // Any value converts to `dyn` of an empty interface; a
                    // `dyn` value keeps its own descriptor (§8.6.1).
                    Ok(Ok(empty))
                } else {
                    fail(Reason::Dyn { has: *has })
                }
            }
            TyKind::Param(param) => {
                if let Some(dict) = self.param_dict(*param, iface)? {
                    Ok(Ok(dict))
                } else if fns.is_empty() {
                    self.with_descriptor(ty, want, empty)
                } else {
                    fail(Reason::Param { param: *param })
                }
            }
            // Every type satisfies an empty interface (D-166).
            _ if fns.is_empty() => self.with_descriptor(ty, want, empty),
            TyKind::Tuple(elems) => {
                if iface != builtin::SHOW && !self.is_ops_compare(iface)? {
                    return fail(Reason::NoModule);
                }
                let mut dicts = Vec::with_capacity(elems.len());
                for (index, &elem) in elems.iter().enumerate() {
                    match self.satisfy_ty(elem, iface, depth.saturating_add(1))? {
                        Ok(dict) => dicts.push(dict),
                        Err(inner) => {
                            return fail(Reason::Element {
                                index,
                                inner: Box::new(inner),
                            });
                        }
                    }
                }
                Ok(Ok(Dict::Tuple {
                    iface,
                    ty,
                    elems: dicts.into(),
                }))
            }
            TyKind::Named { def, .. } => match self.impl_fns(*def, ty, &fns, depth)? {
                Ok(fns) => self.with_descriptor(
                    ty,
                    want,
                    Dict::Impl {
                        iface,
                        ty,
                        fns: fns.into(),
                    },
                ),
                Err(reason) => fail(reason),
            },
            TyKind::Fn { .. } | TyKind::Never | TyKind::Error => fail(Reason::NoModule),
        }
    }

    /// The parent interface of interface function `iface_fn`.
    fn parent_iface(&self, iface_fn: DefId) -> Result<DefId, Ice> {
        let item = self.item(iface_fn)?;
        match (item.kind, item.parent) {
            (DefKind::InterfaceFn, Some(parent)) => Ok(DefId::new(iface_fn.module, parent)),
            _ => Err(Ice::new(format!(
                "{iface_fn:?} is not an interface function"
            ))),
        }
    }

    /// See [`super::iface_call`].
    pub(super) fn iface_call(
        &self,
        iface_fn: DefId,
        self_ty: Ty,
    ) -> Result<Result<Callee, Unsatisfied>, Ice> {
        let types = &self.input.globals.types;
        let iface = self.parent_iface(iface_fn)?;
        let fail = |reason| {
            Ok(Err(Unsatisfied {
                ty: self_ty,
                want: Want::Iface(iface),
                reason,
            }))
        };
        if contains_error(types, self_ty) {
            return Ok(Ok(Callee::Iface {
                iface_fn,
                dict: placeholder(iface, self_ty),
            }));
        }
        match types.kind(self_ty) {
            TyKind::Dyn { iface: has, .. } => {
                if *has == iface || self.embeds(*has, iface)? {
                    Ok(Ok(Callee::Dyn { iface_fn }))
                } else {
                    fail(Reason::Dyn { has: *has })
                }
            }
            TyKind::Named { def, .. } => {
                let fns = self.interface_fns(iface)?;
                let dict_fns = match self.impl_fns(*def, self_ty, &fns, 0)? {
                    Ok(dict_fns) => dict_fns,
                    Err(reason) => return fail(reason),
                };
                let name = self.item(iface_fn)?.name;
                for (f, dict_fn) in fns.iter().zip(dict_fns) {
                    if self.item(*f)?.name == name {
                        return Ok(Ok(Callee::Fn {
                            def: dict_fn.def,
                            ty_args: dict_fn.ty_args,
                            dicts: dict_fn.dicts,
                        }));
                    }
                }
                Err(Ice::new("an interface function is not among its functions"))
            }
            _ => Ok(self
                .satisfy_ty(self_ty, iface, 0)?
                .map(|dict| Callee::Iface { iface_fn, dict })),
        }
    }

    /// See [`super::operator`] and [`super::negation`].
    pub(super) fn op_impl(&self, op: OpFn, ty: Ty) -> Result<Result<OpImpl, Unsatisfied>, Ice> {
        let types = &self.input.globals.types;
        let fail = |reason| {
            Ok(Err(Unsatisfied {
                ty,
                want: Want::Op(op),
                reason,
            }))
        };
        if contains_error(types, ty) || is_builtin_numeric(ty) {
            return Ok(Ok(OpImpl::Builtin));
        }
        let req = self.required_op(op);
        match types.kind(ty) {
            TyKind::Named { def, .. } => {
                let home = match self.type_home(*def)? {
                    Ok(home) => home,
                    Err(reason) => return fail(reason),
                };
                match self.find_fn(&home, ty, &req, 0)? {
                    Ok(f) => Ok(Ok(OpImpl::Call(Callee::Fn {
                        def: f.def,
                        ty_args: f.ty_args,
                        dicts: f.dicts,
                    }))),
                    Err(reason) => fail(reason),
                }
            }
            TyKind::Param(param) => {
                for &bound in self.bounds(*param)? {
                    for f in self.interface_fns(bound)? {
                        let item = self.item(f)?;
                        let matches = matches!(
                            &item.sig,
                            ItemSig::InterfaceFn(sig)
                                if item.name == req.name && sig.params == req.params && sig.ret == req.ret
                        );
                        if !matches {
                            continue;
                        }
                        let declaring = self.parent_iface(f)?;
                        let dict = self
                            .param_dict(*param, declaring)?
                            .ok_or_else(|| Ice::new("a bound's function without its dictionary"))?;
                        return Ok(Ok(OpImpl::Call(Callee::Iface { iface_fn: f, dict })));
                    }
                }
                fail(Reason::Param { param: *param })
            }
            TyKind::Tuple(_) if op == OpFn::Compare => {
                let Some(compare) = self.ops_iface("Compare") else {
                    return fail(Reason::HomeNotInDeps { home: "ops" });
                };
                let dict = match self.satisfy_ty(ty, compare, 0)? {
                    Ok(dict) => dict,
                    Err(err) => return fail(err.reason),
                };
                let iface_fn = self
                    .interface_fns(compare)?
                    .into_iter()
                    .find(|&f| self.read(f).is_some_and(|item| item.name == req.name))
                    .ok_or_else(|| Ice::new("`Compare` without `compare`"))?;
                Ok(Ok(OpImpl::Call(Callee::Iface { iface_fn, dict })))
            }
            _ => fail(Reason::NoModule),
        }
    }
}
