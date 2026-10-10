//! Dictionaries, interface calls, `dyn` calls, conversions to `dyn` and
//! type descriptors (D5, D-236, D-453, D-499, D-523, D-524).
//!
//! W3-A fills the dictionary and descriptor parts; the `dyn` parts
//! ([`BodyCx::call_dyn`], [`BodyCx::dyn_coerce`] and `Dict::Dyn`) are
//! W4-A's and still E0038 (plan §4.2).
//!
//! Shapes (D-524, plan §5.4):
//!
//! - A dictionary for interface `I` and type `T` is `Dict { fns, embedded
//!   }` (layout id 9): field 0 is `T`'s descriptor, then one closure per
//!   function of `cheby_iface::interface_fns(I)` (embedded interfaces'
//!   functions first, D-524), then the dictionaries of the interfaces `I`
//!   embeds directly, in written order. A satisfying function without
//!   dictionaries is its static function value (`cb$fnval$…`); one with
//!   dictionaries (conditional conformance, D-064) is a closure of its
//!   value shim holding them, as for a function value (D-453).
//! - A built-in tuple dictionary (D-167) has the same shape; each function
//!   is a closure of the [`FnKey::TupleDictFn`] code holding the element
//!   dictionaries ([`tuple_fn`]).
//! - A type with no type arguments is described by its nominal descriptor
//!   itself (`DescNominal`, layout 12): `cheby_rt_desc_<name>` for a
//!   builtin type, `cb$desc$<module path>$$<Type>` for a user type. Any
//!   other type is a `Desc { args }` (layout 11): the nominal descriptor,
//!   then the descriptors of its arguments (a tuple's elements; a function
//!   type's parameters, then its result). GAP(desc-canonical-form).
//! - A dictionary or descriptor that needs no dictionary of the enclosing
//!   item is a static object of the unit ([`statics`]), named from paths
//!   and printed types (D-523); any other is built with `Alloc` where it
//!   is needed. A type parameter's descriptor is field 0 of the dictionary
//!   of its first bound (D-499).
//! - `Dict::Param` is one of the item's leading dictionary parameters, in
//!   (type parameter, bound) order; a closure that needs one captures it
//!   ([`BodyCx::dict_params_used`]). `Dict::Embedded` of a parameter's
//!   dictionary loads the embedded dictionary's field; of a known type it
//!   is that type's dictionary for the embedded interface.
//! - An interface call loads the function's closure from the dictionary
//!   and calls it with uniform arguments (`Callee::Indirect`, D-453), as a
//!   tail call in tail position (D-015).

mod statics;
#[cfg(test)]
mod tests;
mod tuple;

use std::collections::BTreeSet;

use cheby_diag::Span;
use cheby_iface::{DefId, Generics, Ice, ItemSig, ParamIdx, Ty, TyKind, interface_fns};
use cheby_types::thir::{Callee, Dict, DictFn, ExprId, ExprKind, InterpPart, OpImpl};

pub(super) use statics::{Statics, unit_module};
pub(super) use tuple::tuple_fn;

use crate::ir::{
    Abi, BuiltinDesc, Callee as MirCallee, Const, FnKey, LayoutRef, Local, Operand, Rvalue,
    Terminator,
};
use crate::lower::LowerCx;
use crate::lower::body::BodyCx;
use crate::lower::closure::subtree;
use crate::repr::Repr;

/// The dictionary shape of an interface (D-524).
pub(super) struct IfaceShape {
    /// `interface_fns` order.
    pub(super) fns: Vec<DefId>,
    /// The interfaces it embeds directly, in written order.
    pub(super) embeds: Vec<DefId>,
}

impl IfaceShape {
    /// The shape of `iface`.
    pub(super) fn of(cx: &LowerCx<'_>, iface: DefId) -> Result<Self, Ice> {
        let item = cx.defs.item(iface)?;
        let ItemSig::Interface(def) = &item.sig else {
            return Err(Ice::new(format!("{iface:?} is not an interface")));
        };
        let fns = interface_fns(iface, &|d| cx.defs.item(d).ok())?;
        Ok(Self {
            fns,
            embeds: def.embeds.to_vec(),
        })
    }

    /// `Dict { fns, embedded }`.
    pub(super) fn layout(&self) -> Result<LayoutRef, Ice> {
        Ok(LayoutRef::Dict {
            fns: count(self.fns.len())?,
            embedded: count(self.embeds.len())?,
        })
    }

    /// The field holding the closure of `iface_fn`: by identity, or by
    /// name for a function of an embedded interface whose name another
    /// embedded interface required first (§8.3, D-524).
    pub(super) fn fn_field(&self, cx: &LowerCx<'_>, iface_fn: DefId) -> Result<u32, Ice> {
        if let Some(n) = self.fns.iter().position(|&f| f == iface_fn) {
            return count(n.saturating_add(1));
        }
        let name = cx.defs.item(iface_fn)?.name;
        for (n, &f) in self.fns.iter().enumerate() {
            if cx.defs.item(f)?.name == name {
                return count(n.saturating_add(1));
            }
        }
        Err(Ice::new(format!(
            "{iface_fn:?} is not a function of the dictionary"
        )))
    }

    /// The field holding the dictionary of the `k`th embedded interface.
    pub(super) fn embed_field(&self, k: usize) -> Result<u32, Ice> {
        count(self.fns.len().saturating_add(k).saturating_add(1))
    }
}

/// A field count or index as a `u32`.
pub(super) fn count(n: usize) -> Result<u32, Ice> {
    u32::try_from(n).map_err(|_| Ice::new("a dictionary or descriptor too large"))
}

/// The interface of `dict`; `generics` are the enclosing item's.
pub(super) fn dict_iface(dict: &Dict, generics: Option<&Generics>) -> Result<DefId, Ice> {
    match dict {
        Dict::Impl { iface, .. }
        | Dict::Embedded { iface, .. }
        | Dict::Tuple { iface, .. }
        | Dict::Dyn { iface, .. } => Ok(*iface),
        Dict::Param { param, bound } => bound_iface(generics, *param, *bound),
    }
}

/// The interface of bound `bound` of the type parameter `param`.
fn bound_iface(generics: Option<&Generics>, param: ParamIdx, bound: u32) -> Result<DefId, Ice> {
    generics
        .and_then(|g| g.params.get(param))
        .and_then(|p| p.bounds.get(usize::try_from(bound).ok()?))
        .copied()
        .ok_or_else(|| Ice::new(format!("no bound {bound} of type parameter {param:?}")))
}

/// The position of the dictionary of bound `bound` of `param` among the
/// item's dictionary parameters: every bound of the earlier type
/// parameters, then the earlier bounds of `param` (D-453).
fn flat_index(generics: Option<&Generics>, param: ParamIdx, bound: u32) -> Result<usize, Ice> {
    let g = generics.ok_or_else(|| Ice::new("a dictionary parameter outside a generic item"))?;
    let mut n = 0_usize;
    for (p, gp) in g.params.iter_enumerated() {
        if p == param {
            let b = usize::try_from(bound).map_err(|_| Ice::new("a bound index too large"))?;
            if b >= gp.bounds.len() {
                break;
            }
            return Ok(n.saturating_add(b));
        }
        n = n.saturating_add(gp.bounds.len());
    }
    Err(Ice::new(format!(
        "no bound {bound} of type parameter {param:?}"
    )))
}

/// Calls `f` on every type parameter `ty` mentions, without recursion.
fn ty_params(cx: &LowerCx<'_>, ty: Ty, f: &mut dyn FnMut(ParamIdx)) {
    let mut stack = vec![ty];
    while let Some(t) = stack.pop() {
        match cx.defs.globals.types.kind(t) {
            TyKind::Param(p) => f(*p),
            TyKind::Named { args, .. } | TyKind::Dyn { args, .. } | TyKind::Tuple(args) => {
                stack.extend(args.iter().copied());
            }
            TyKind::Fn { params, ret } => {
                stack.extend(params.iter().copied());
                stack.push(*ret);
            }
            TyKind::Error | TyKind::Never => {}
        }
    }
}

/// Whether `ty` mentions no type parameter.
fn ty_closed(cx: &LowerCx<'_>, ty: Ty) -> bool {
    let mut closed = true;
    ty_params(cx, ty, &mut |_| closed = false);
    closed
}

/// Whether `ty` mentions a `dyn` type.
fn ty_mentions_dyn(cx: &LowerCx<'_>, ty: Ty) -> bool {
    let mut stack = vec![ty];
    while let Some(t) = stack.pop() {
        match cx.defs.globals.types.kind(t) {
            TyKind::Dyn { .. } => return true,
            TyKind::Named { args, .. } | TyKind::Tuple(args) => stack.extend(args.iter().copied()),
            TyKind::Fn { params, ret } => {
                stack.extend(params.iter().copied());
                stack.push(*ret);
            }
            TyKind::Param(_) | TyKind::Error | TyKind::Never => {}
        }
    }
    false
}

/// Whether `dict` needs no dictionary of the enclosing item, so it is a
/// static object (D-524).
pub(super) fn dict_closed(cx: &LowerCx<'_>, dict: &Dict) -> bool {
    match dict {
        Dict::Impl { ty, fns, .. } => {
            ty_closed(cx, *ty)
                && fns
                    .iter()
                    .all(|f| f.dicts.iter().all(|d| dict_closed(cx, d)))
        }
        Dict::Param { .. } | Dict::Dyn { .. } => false,
        Dict::Embedded { of, .. } => dict_closed(cx, of),
        Dict::Tuple { ty, elems, .. } => {
            ty_closed(cx, *ty) && elems.iter().all(|d| dict_closed(cx, d))
        }
    }
}

/// Whether `dict` is or holds a `dyn` value's dictionary, or describes a
/// type that mentions `dyn` (W4-A).
pub(super) fn dict_mentions_dyn(cx: &LowerCx<'_>, dict: &Dict) -> bool {
    match dict {
        Dict::Dyn { .. } => true,
        Dict::Impl { ty, fns, .. } => {
            ty_mentions_dyn(cx, *ty)
                || fns
                    .iter()
                    .any(|f| f.dicts.iter().any(|d| dict_mentions_dyn(cx, d)))
        }
        Dict::Param { .. } => false,
        Dict::Embedded { of, .. } => dict_mentions_dyn(cx, of),
        Dict::Tuple { ty, elems, .. } => {
            ty_mentions_dyn(cx, *ty) || elems.iter().any(|d| dict_mentions_dyn(cx, d))
        }
    }
}

/// The satisfying functions of `to`, an interface that `from` embeds
/// (transitively), taken from `fns`, the functions of a dictionary of
/// `from`: each of `to`'s functions by name (§8.3, D-524).
pub(super) fn project(
    cx: &LowerCx<'_>,
    from: DefId,
    fns: &[DictFn],
    to: DefId,
) -> Result<Vec<DictFn>, Ice> {
    let from_fns = interface_fns(from, &|d| cx.defs.item(d).ok())?;
    if from_fns.len() != fns.len() {
        return Err(Ice::new(format!(
            "a dictionary of {from:?} with {} functions for {}",
            fns.len(),
            from_fns.len()
        )));
    }
    let mut out = Vec::new();
    for f in interface_fns(to, &|d| cx.defs.item(d).ok())? {
        let name = cx.defs.item(f)?.name;
        let mut found = None;
        for (n, &g) in from_fns.iter().enumerate() {
            if cx.defs.item(g)?.name == name {
                found = fns.get(n);
                break;
            }
        }
        out.push(
            found
                .ok_or_else(|| Ice::new(format!("{to:?} is not embedded in {from:?}")))?
                .clone(),
        );
    }
    Ok(out)
}

/// `dict` with every `Embedded` of a known type's dictionary replaced by
/// that type's dictionary of the embedded interface. What is left of
/// `Embedded` reaches into a dictionary parameter's (or a tuple's).
pub(super) fn normalize(cx: &LowerCx<'_>, dict: &Dict) -> Result<Dict, Ice> {
    let Dict::Embedded { of, iface } = dict else {
        return Ok(dict.clone());
    };
    match normalize(cx, of)? {
        Dict::Impl {
            iface: from,
            ty,
            fns,
        } => Ok(Dict::Impl {
            iface: *iface,
            ty,
            fns: project(cx, from, &fns, *iface)?.into(),
        }),
        other => Ok(Dict::Embedded {
            of: Box::new(other),
            iface: *iface,
        }),
    }
}

/// The fields to load, in order, to reach the dictionary of `to` from a
/// dictionary of `from`: depth first through the embedded interfaces in
/// written order. `None` if `from` does not embed `to`.
fn embed_path(
    cx: &LowerCx<'_>,
    from: DefId,
    to: DefId,
) -> Result<Option<Vec<(LayoutRef, u32)>>, Ice> {
    if from == to {
        return Ok(Some(Vec::new()));
    }
    let shape = IfaceShape::of(cx, from)?;
    for (k, &j) in shape.embeds.iter().enumerate() {
        if let Some(mut rest) = embed_path(cx, j, to)? {
            rest.insert(0, (shape.layout()?, shape.embed_field(k)?));
            return Ok(Some(rest));
        }
    }
    Ok(None)
}

/// The dictionaries a callee holds.
fn callee_dicts(c: &Callee) -> Vec<&Dict> {
    match c {
        Callee::Fn { dicts, .. } => dicts.iter().collect(),
        Callee::Iface { dict, .. } => vec![dict],
        _ => Vec::new(),
    }
}

/// The dictionaries an expression holds directly.
fn expr_dicts(kind: &ExprKind) -> Vec<&Dict> {
    let callee = callee_dicts;
    match kind {
        ExprKind::Call { callee: c, .. }
        | ExprKind::Binary {
            imp: OpImpl::Call(c),
            ..
        }
        | ExprKind::Neg {
            imp: OpImpl::Call(c),
            ..
        } => callee(c),
        ExprKind::FnRef { dicts, .. } => dicts.iter().collect(),
        ExprKind::DynCoerce { dict, .. } => vec![dict],
        ExprKind::Interpolate { parts } => parts
            .iter()
            .flat_map(|p| match p {
                InterpPart::Value { show: Some(c), .. } => callee(c),
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

impl BodyCx<'_, '_> {
    /// The dictionary object for `dict`, used by the call at `span`, as
    /// an owned uniform word: a static object when it needs no
    /// dictionary of the enclosing item, built here otherwise.
    pub(super) fn dict(&mut self, dict: &Dict, span: Span) -> Result<Option<Operand>, Ice> {
        if dict_mentions_dyn(self.cx, dict) {
            // W4-A: dictionaries carried by `dyn` values and descriptors
            // of `dyn` types.
            return self.unsupported(span, "interface dictionaries of `dyn` types", Ty::ERROR);
        }
        self.dict_op(dict).map(Some)
    }

    /// [`BodyCx::dict`] without the `dyn` check.
    fn dict_op(&mut self, dict: &Dict) -> Result<Operand, Ice> {
        if dict_closed(self.cx, dict) {
            let module = unit_module(self.cx)?;
            let c = self.statics(&module).dict(dict)?;
            return Ok(Operand::Const(c));
        }
        match normalize(self.cx, dict)? {
            Dict::Param { param, bound } => self.dict_param(param, bound).map(Operand::Local),
            Dict::Impl { iface, ty, fns } => self.build_impl(iface, ty, &fns),
            Dict::Tuple { iface, ty, elems } => self.build_tuple(iface, ty, &elems),
            Dict::Embedded { of, iface } => {
                let from = dict_iface(&of, self.generics)?;
                let path = embed_path(self.cx, from, iface)?
                    .ok_or_else(|| Ice::new(format!("{iface:?} is not embedded in {from:?}")))?;
                let base = self.dict_op(&of)?;
                let mut cur = self.dict_local(base)?;
                for (layout, field) in path {
                    cur = self.define(
                        Repr::Value,
                        Rvalue::Field {
                            base: cur,
                            layout,
                            field,
                        },
                    )?;
                }
                Ok(Operand::Local(cur))
            }
            Dict::Dyn { .. } => Err(Ice::new("a `dyn` dictionary reached MIR lowering")),
        }
    }

    /// The static-object builder for this unit, whose dictionaries and
    /// applied descriptors carry `module` in their symbols.
    fn statics<'s>(&'s mut self, module: &'s str) -> Statics<'s, 's> {
        Statics {
            cx: self.cx,
            pending: &mut *self.pending,
            module,
        }
    }

    /// `op` in a local of its own (a constant is moved into one).
    fn dict_local(&mut self, op: Operand) -> Result<Local, Ice> {
        match op {
            Operand::Local(l) => Ok(l),
            op @ Operand::Const(_) => self.define(Repr::Value, Rvalue::Use(op)),
        }
    }

    /// The dictionary parameter of bound `bound` of `param`.
    fn dict_param(&mut self, param: ParamIdx, bound: u32) -> Result<Local, Ice> {
        let n = flat_index(self.generics, param, bound)?;
        self.dict_param_at(n)
    }

    /// The `n`th dictionary parameter of the enclosing item (D-453).
    pub(super) fn dict_param_at(&self, n: usize) -> Result<Local, Ice> {
        self.dict_params
            .get(n)
            .copied()
            .flatten()
            .ok_or_else(|| Ice::new(format!("dictionary parameter {n} is not available here")))
    }

    /// The dictionary parameters of the enclosing item that the closure
    /// or local function with body `body` needs, by index, sorted: those
    /// its dictionaries name, and the first bound of every type parameter
    /// whose descriptor they need (D-499).
    pub(super) fn dict_params_used(&self, body: ExprId) -> Result<Vec<usize>, Ice> {
        let mut used = BTreeSet::new();
        for id in subtree(self.body, body)? {
            let e = self
                .body
                .expr(id)
                .ok_or_else(|| Ice::new(format!("no THIR expression {id:?}")))?;
            for d in expr_dicts(&e.kind) {
                self.dict_uses(d, &mut used)?;
            }
        }
        Ok(used.into_iter().collect())
    }

    /// Adds the dictionary parameters `dict` needs to `out`.
    fn dict_uses(&self, dict: &Dict, out: &mut BTreeSet<usize>) -> Result<(), Ice> {
        let mut stack = vec![dict];
        while let Some(d) = stack.pop() {
            let ty = match d {
                Dict::Param { param, bound } => {
                    out.insert(flat_index(self.generics, *param, *bound)?);
                    continue;
                }
                Dict::Impl { ty, fns, .. } => {
                    stack.extend(fns.iter().flat_map(|f| f.dicts.iter()));
                    *ty
                }
                Dict::Embedded { of, .. } => {
                    stack.push(of);
                    continue;
                }
                Dict::Tuple { ty, elems, .. } => {
                    stack.extend(elems.iter());
                    *ty
                }
                Dict::Dyn { ty, .. } => *ty,
            };
            let mut params = Vec::new();
            ty_params(self.cx, ty, &mut |p| params.push(p));
            for p in params {
                // A descriptor comes from the first bound (D-499); a
                // parameter without one never needs a descriptor (D-502).
                if let Ok(n) = flat_index(self.generics, p, 0) {
                    out.insert(n);
                }
            }
        }
        Ok(())
    }

    /// A dictionary of `iface` for `ty` built at run time from `fns`.
    fn build_impl(&mut self, iface: DefId, ty: Ty, fns: &[DictFn]) -> Result<Operand, Ice> {
        let shape = IfaceShape::of(self.cx, iface)?;
        if shape.fns.len() != fns.len() {
            return Err(Ice::new(format!(
                "a dictionary of {iface:?} with {} functions for {}",
                fns.len(),
                shape.fns.len()
            )));
        }
        let mut fields = Vec::with_capacity(
            fns.len()
                .saturating_add(shape.embeds.len())
                .saturating_add(1),
        );
        fields.push(self.desc(ty)?);
        for f in fns {
            let mut subs = Vec::with_capacity(f.dicts.len());
            for d in &f.dicts {
                subs.push(self.dict_op(d)?);
            }
            fields.push(self.fn_value(f.def, subs)?);
        }
        for &j in &shape.embeds {
            let sub = Dict::Impl {
                iface: j,
                ty,
                fns: project(self.cx, iface, fns, j)?.into(),
            };
            fields.push(self.dict_op(&sub)?);
        }
        let layout = shape.layout()?;
        Ok(Operand::Local(
            self.define(Repr::Value, Rvalue::Alloc { layout, fields })?,
        ))
    }

    /// A built-in tuple dictionary of `iface` (`Show` or `Compare`) for
    /// the tuple type `ty`, built at run time from the element
    /// dictionaries `elems` (D-167).
    fn build_tuple(&mut self, iface: DefId, ty: Ty, elems: &[Dict]) -> Result<Operand, Ice> {
        let shape = IfaceShape::of(self.cx, iface)?;
        if !shape.embeds.is_empty() {
            return Err(Ice::new(format!(
                "a tuple dictionary of {iface:?}, which embeds other interfaces"
            )));
        }
        let arity = count(elems.len())?;
        let mut elem_ops = Vec::with_capacity(elems.len());
        for d in elems {
            elem_ops.push(self.dict_op(d)?);
        }
        let mut fields = Vec::with_capacity(shape.fns.len().saturating_add(1));
        fields.push(self.desc(ty)?);
        for &f in &shape.fns {
            self.pending.request_tuple_fn(f, arity);
            let mut captures = Vec::with_capacity(elem_ops.len().saturating_add(1));
            captures.push(Operand::Const(Const::Code(FnKey::TupleDictFn {
                iface_fn: f,
                arity,
            })));
            captures.extend(elem_ops.iter().cloned());
            let closure = self.define(
                Repr::Value,
                Rvalue::Alloc {
                    layout: LayoutRef::Closure(arity),
                    fields: captures,
                },
            )?;
            fields.push(Operand::Local(closure));
        }
        let layout = shape.layout()?;
        Ok(Operand::Local(
            self.define(Repr::Value, Rvalue::Alloc { layout, fields })?,
        ))
    }

    /// The descriptor of `ty` as an owned uniform word: static when `ty`
    /// mentions no type parameter, built here otherwise (D-236, D-524).
    pub(super) fn desc(&mut self, ty: Ty) -> Result<Operand, Ice> {
        if ty_closed(self.cx, ty) {
            let module = unit_module(self.cx)?;
            return self.statics(&module).desc(ty).map(Operand::Const);
        }
        let (head, args): (Const, Vec<Ty>) = match self.cx.defs.globals.types.kind(ty) {
            TyKind::Param(p) => {
                // The first bound's dictionary carries it (D-499).
                let iface = bound_iface(self.generics, *p, 0).map_err(|_| {
                    Ice::new(format!(
                        "the descriptor of type parameter {p:?}, which has no bound (D-502)"
                    ))
                })?;
                let layout = IfaceShape::of(self.cx, iface)?.layout()?;
                let dict = self.dict_param(*p, 0)?;
                let field = self.define(
                    Repr::Value,
                    Rvalue::Field {
                        base: dict,
                        layout,
                        field: 0,
                    },
                )?;
                return Ok(Operand::Local(field));
            }
            TyKind::Named { def, args } => {
                let module = unit_module(self.cx)?;
                let head = self.statics(&module).nominal(*def)?;
                (head, args.to_vec())
            }
            TyKind::Tuple(elems) => (Const::BuiltinDesc(BuiltinDesc::Tuple), elems.to_vec()),
            TyKind::Fn { params, ret } => {
                let mut args = params.to_vec();
                args.push(*ret);
                (Const::BuiltinDesc(BuiltinDesc::Fn), args)
            }
            other => {
                return Err(Ice::new(format!("a descriptor of the type {other:?}")));
            }
        };
        let mut fields = Vec::with_capacity(args.len().saturating_add(1));
        fields.push(Operand::Const(head));
        for a in &args {
            fields.push(self.desc(*a)?);
        }
        let layout = LayoutRef::Desc {
            args: count(args.len())?,
        };
        Ok(Operand::Local(
            self.define(Repr::Value, Rvalue::Alloc { layout, fields })?,
        ))
    }

    /// The field of `dict`'s closure for `iface_fn`, read from the
    /// dictionary object `d`.
    fn iface_closure(&mut self, dict: &Dict, d: Operand, iface_fn: DefId) -> Result<Local, Ice> {
        let iface = dict_iface(dict, self.generics)?;
        let shape = IfaceShape::of(self.cx, iface)?;
        let field = shape.fn_field(self.cx, iface_fn)?;
        let base = self.dict_local(d)?;
        self.define(
            Repr::Value,
            Rvalue::Field {
                base,
                layout: shape.layout()?,
                field,
            },
        )
    }

    /// Calls the closure `f` with the uniform `args`: a tail call in tail
    /// position (D-015), else the result converted from its uniform word
    /// to `ret_ty`'s representation.
    fn call_closure(
        &mut self,
        f: Local,
        args: Vec<Operand>,
        ret_ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let callee = MirCallee::Indirect(f);
        if tail && self.b.abi() == Abi::Cheby {
            self.terminate(Terminator::TailCall { callee, args })?;
            return Ok(None);
        }
        let result = self.define(Repr::Value, Rvalue::Call { callee, args })?;
        self.from_uniform(Operand::Local(result), ret_ty).map(Some)
    }

    /// A call of the interface function `iface_fn` through `dict` (D-173):
    /// the dictionary, then the arguments left to right (§5.15.1), each
    /// as a uniform word, then an indirect call of the function's
    /// closure (D-453).
    pub(super) fn call_iface(
        &mut self,
        iface_fn: DefId,
        dict: &Dict,
        args: &[ExprId],
        ret_ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let Some(d) = self.dict(dict, span)? else {
            return Ok(None);
        };
        if self.is_poisoned(&d) {
            return self.placeholder(self.repr_of(ret_ty)).map(Some);
        }
        let mut ops = Vec::with_capacity(args.len());
        for &arg in args {
            let Some(op) = self.expr(arg, false)? else {
                return Ok(None);
            };
            let ty = self.ty(arg)?;
            ops.push(self.to_uniform(op, ty)?);
        }
        let f = self.iface_closure(dict, d, iface_fn)?;
        self.call_closure(f, ops, ret_ty, tail)
    }

    /// A call of the interface function `iface_fn` through `dict` on
    /// arguments already evaluated, each in the representation of its
    /// type (`repr_of(ty)`), giving a value of type `ret_ty`. Operators
    /// implemented by interface functions use it (`scalar.rs`): their
    /// operands are evaluated first, so a failed `assert` can print them
    /// (D-528).
    pub(super) fn call_iface_ops(
        &mut self,
        iface_fn: DefId,
        dict: &Dict,
        args: Vec<(Operand, Ty)>,
        ret_ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let Some(d) = self.dict(dict, span)? else {
            return Ok(None);
        };
        if self.is_poisoned(&d) {
            return self.placeholder(self.repr_of(ret_ty)).map(Some);
        }
        let mut ops = Vec::with_capacity(args.len());
        for (op, ty) in args {
            ops.push(self.to_uniform(op, ty)?);
        }
        let f = self.iface_closure(dict, d, iface_fn)?;
        self.call_closure(f, ops, ret_ty, tail)
    }

    /// A call of the interface function `iface_fn` on the `dyn` value
    /// `args[0]` (D-117).
    pub(super) fn call_dyn(
        &mut self,
        _iface_fn: DefId,
        _args: &[ExprId],
        ret_ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "calls through `dyn` values", ret_ty)
    }

    /// `value` converted to `dyn iface`, of type `ty` (C10).
    pub(super) fn dyn_coerce(
        &mut self,
        _value: ExprId,
        _iface: DefId,
        _dict: &Dict,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "conversions to `dyn`", ty)
    }
}
