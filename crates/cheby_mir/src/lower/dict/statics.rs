//! Static dictionaries, descriptors and dictionary closures (D-523,
//! D-524): the objects that need no dictionary of the enclosing item.
//!
//! Names are built from paths and printed types, never from ids
//! (D-264), so the same object gets the same symbol in every unit and
//! batch, and the JIT defines it once (D-523):
//!
//! - a dictionary: `cb$dict$<unit module>$$<interface path>$<type>`;
//! - a closure in a static dictionary (a satisfying function with
//!   dictionaries, or a tuple function): `cb$fnval$<unit
//!   module>$$<dictionary name>$<n>`, `n` being the function's position;
//! - a user type's nominal descriptor: `cb$desc$<module path>$$<Type>`;
//! - the nominal descriptor of `dyn I`: `cb$desc$<module path of
//!   I>$$dyn$<I>`, holding the name `dyn <I>` (empty module path for the
//!   builtin `Show`);
//! - an applied type's descriptor: `cb$desc$<unit module>$$<type>`;
//! - the forwarding dictionary through which a `dyn J` value satisfies
//!   `I` (`J` or an interface `J` embeds): `cb$dict$<unit
//!   module>$$<I path>$dyn <J path>`, whose closures are statics
//!   `<dictionary name>$<n>` of [`FnKey::DynForward`] code.
//!
//! Code generation joins the module path with `$` and ends it with `$$`,
//! and keeps the name as it is, paths included, so the boundary between
//! the two is never ambiguous (D-579).
//!
//! `<type>` is the type printed with full module paths ([`type_text`]):
//! `List<app::shapes::Circle>`, `(Int, String)`, `fn(Int) -> Bool`
//! (D-580).
//!
//! A closure in a static dictionary is a static too, of kind `fnval`, whose
//! captures are static objects: the dictionaries its function needs
//! (D-064) or a tuple's element dictionaries (D-167, D-580).

use cheby_iface::{DefId, Ice, IndexVec, ModuleId, Ty, TyKind, builtin};
use cheby_types::thir::{Dict, DictFn};

use super::{IfaceShape, count, normalize, project};
use crate::ir::{BuiltinDesc, Const, FnKey, LayoutRef, StaticId, StaticKind, StaticObject, StrId};
use crate::lower::{LowerCx, Pending};

/// Builds static objects into a unit's [`Pending`].
pub(in crate::lower) struct Statics<'p, 'cx> {
    pub(in crate::lower) cx: &'p LowerCx<'cx>,
    pub(in crate::lower) pending: &'p mut Pending,
    /// The module path in the symbols of the unit's dictionaries,
    /// dictionary closures and applied descriptors.
    pub(in crate::lower) module: &'p str,
}

/// The module path of the unit being lowered, for the symbols of its
/// static objects.
///
/// # Errors
///
/// An [`Ice`] for the entries unit, which has no module: its thunks pass
/// their item's module instead.
pub(in crate::lower) fn unit_module(cx: &LowerCx<'_>) -> Result<String, Ice> {
    let own = cx
        .defs
        .own
        .ok_or_else(|| Ice::new("a static object outside a module's unit"))?;
    Ok(cx.defs.modules.try_get(own.module)?.path.to_string())
}

/// `ty` printed with full module paths, the same in every module, for
/// static names.
pub(in crate::lower) fn type_text(cx: &LowerCx<'_>, ty: Ty) -> Result<String, Ice> {
    let mut out = String::new();
    write_ty(cx, ty, &mut out)?;
    Ok(out)
}

fn write_ty(cx: &LowerCx<'_>, ty: Ty, out: &mut String) -> Result<(), Ice> {
    match cx.defs.globals.types.kind(ty) {
        TyKind::Named { def, args } => {
            out.push_str(&cx.defs.name(*def)?.path());
            write_list(cx, args, "<", ">", out)?;
        }
        TyKind::Tuple(elems) => write_list(cx, elems, "(", ")", out)?,
        TyKind::Fn { params, ret } => {
            out.push_str("fn");
            if params.is_empty() {
                out.push_str("()");
            }
            write_list(cx, params, "(", ")", out)?;
            out.push_str(" -> ");
            write_ty(cx, *ret, out)?;
        }
        TyKind::Dyn { iface, .. } => {
            out.push_str("dyn ");
            out.push_str(&cx.defs.name(*iface)?.path());
        }
        TyKind::Never => out.push_str("Never"),
        TyKind::Param(_) | TyKind::Error => {
            return Err(Ice::new(format!(
                "a static name for the open type {:?}",
                cx.defs.globals.types.kind(ty)
            )));
        }
    }
    Ok(())
}

/// `open`, the types separated by `, `, then `close`; nothing for no
/// types.
fn write_list(
    cx: &LowerCx<'_>,
    tys: &[Ty],
    open: &str,
    close: &str,
    out: &mut String,
) -> Result<(), Ice> {
    if tys.is_empty() {
        return Ok(());
    }
    out.push_str(open);
    for (n, &t) in tys.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_ty(cx, t, out)?;
    }
    out.push_str(close);
    Ok(())
}

/// The runtime's nominal descriptor of a builtin type (D-523).
fn builtin_desc(def: DefId) -> Option<BuiltinDesc> {
    Some(match def {
        builtin::INT => BuiltinDesc::Int,
        builtin::FLOAT => BuiltinDesc::Float,
        builtin::F32 => BuiltinDesc::F32,
        builtin::I8 => BuiltinDesc::I8,
        builtin::I16 => BuiltinDesc::I16,
        builtin::I32 => BuiltinDesc::I32,
        builtin::I64 => BuiltinDesc::I64,
        builtin::U8 => BuiltinDesc::U8,
        builtin::U16 => BuiltinDesc::U16,
        builtin::U32 => BuiltinDesc::U32,
        builtin::U64 => BuiltinDesc::U64,
        builtin::BOOL => BuiltinDesc::Bool,
        builtin::NIL_TY => BuiltinDesc::Nil,
        builtin::STRING => BuiltinDesc::String,
        builtin::BYTES => BuiltinDesc::Bytes,
        builtin::LIST => BuiltinDesc::List,
        builtin::MAP => BuiltinDesc::Map,
        builtin::SET => BuiltinDesc::Set,
        builtin::OPTION => BuiltinDesc::Option,
        builtin::RESULT => BuiltinDesc::Result,
        builtin::ORDER => BuiltinDesc::Order,
        _ => return None,
    })
}

impl Statics<'_, '_> {
    /// The static dictionary of the closed `dict` (D-524).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `dict` depends on a dictionary parameter or a `dyn`
    /// value, or does not fit its interface.
    pub(in crate::lower) fn dict(&mut self, dict: &Dict) -> Result<Const, Ice> {
        match normalize(self.cx, dict)? {
            Dict::Impl { iface, ty, fns } => self.impl_dict(iface, ty, &fns),
            Dict::Tuple { iface, ty, elems } => self.tuple_dict(iface, ty, &elems),
            Dict::Dyn { iface, ty } => self.dyn_dict(iface, ty),
            other => Err(Ice::new(format!(
                "a static dictionary for {other:?}, which is not closed"
            ))),
        }
    }

    /// The name of the dictionary of `iface` for `ty`.
    fn dict_name(&self, iface: DefId, ty: Ty) -> Result<String, Ice> {
        Ok(format!(
            "{}${}",
            self.cx.defs.name(iface)?.path(),
            type_text(self.cx, ty)?
        ))
    }

    fn impl_dict(&mut self, iface: DefId, ty: Ty, fns: &[DictFn]) -> Result<Const, Ice> {
        let shape = IfaceShape::of(self.cx, iface)?;
        if shape.fns.len() != fns.len() {
            return Err(Ice::new(format!(
                "a dictionary of {iface:?} with {} functions for {}",
                fns.len(),
                shape.fns.len()
            )));
        }
        let name = self.dict_name(iface, ty)?;
        let mut fields = Vec::with_capacity(
            fns.len()
                .saturating_add(shape.embeds.len())
                .saturating_add(1),
        );
        fields.push(self.desc(ty)?);
        for (n, f) in fns.iter().enumerate() {
            self.pending.request_shim(f.def);
            if f.dicts.is_empty() {
                fields.push(Const::FnValue(f.def));
                continue;
            }
            let mut captures = Vec::with_capacity(f.dicts.len().saturating_add(1));
            captures.push(Const::Code(FnKey::ValueShim(f.def)));
            for d in &f.dicts {
                captures.push(self.dict(d)?);
            }
            fields.push(self.closure(&name, n, captures)?);
        }
        for &j in &shape.embeds {
            let sub = Dict::Impl {
                iface: j,
                ty,
                fns: project(self.cx, iface, fns, j)?.into(),
            };
            fields.push(self.dict(&sub)?);
        }
        let module = self.module.to_owned();
        self.add(
            StaticKind::Dict,
            module,
            name,
            shape.layout()?,
            fields,
            IndexVec::new(),
        )
    }

    fn tuple_dict(&mut self, iface: DefId, ty: Ty, elems: &[Dict]) -> Result<Const, Ice> {
        let shape = IfaceShape::of(self.cx, iface)?;
        if !shape.embeds.is_empty() {
            return Err(Ice::new(format!(
                "a tuple dictionary of {iface:?}, which embeds other interfaces"
            )));
        }
        let name = self.dict_name(iface, ty)?;
        let arity = count(elems.len())?;
        let mut elem_dicts = Vec::with_capacity(elems.len());
        for d in elems {
            elem_dicts.push(self.dict(d)?);
        }
        let mut fields = Vec::with_capacity(shape.fns.len().saturating_add(1));
        fields.push(self.desc(ty)?);
        for (n, &f) in shape.fns.iter().enumerate() {
            self.pending.request_tuple_fn(f, arity);
            let mut captures = Vec::with_capacity(elem_dicts.len().saturating_add(1));
            captures.push(Const::Code(FnKey::TupleDictFn { iface_fn: f, arity }));
            captures.extend(elem_dicts.iter().cloned());
            fields.push(self.closure(&name, n, captures)?);
        }
        let module = self.module.to_owned();
        self.add(
            StaticKind::Dict,
            module,
            name,
            shape.layout()?,
            fields,
            IndexVec::new(),
        )
    }

    /// The forwarding dictionary through which a `dyn J` value (`ty`)
    /// satisfies `iface`, `J` itself or an interface `J` embeds (§8.6,
    /// D-502): the descriptor of the type `dyn J`, then for each function
    /// a static closure of its forwarder ([`super::dyn_forward`]), then
    /// the forwarding dictionaries of `iface`'s embedded interfaces for
    /// the same `dyn J`.
    fn dyn_dict(&mut self, iface: DefId, ty: Ty) -> Result<Const, Ice> {
        let TyKind::Dyn {
            iface: dyn_iface, ..
        } = self.cx.defs.globals.types.kind(ty)
        else {
            return Err(Ice::new(format!(
                "a `dyn` dictionary of {iface:?} for the type {:?}",
                self.cx.defs.globals.types.kind(ty)
            )));
        };
        let dyn_iface = *dyn_iface;
        let shape = IfaceShape::of(self.cx, iface)?;
        let name = self.dict_name(iface, ty)?;
        let mut fields = Vec::with_capacity(
            shape
                .fns
                .len()
                .saturating_add(shape.embeds.len())
                .saturating_add(1),
        );
        fields.push(self.desc(ty)?);
        for (n, &f) in shape.fns.iter().enumerate() {
            self.pending.request_dyn_forward(f, dyn_iface);
            let code = Const::Code(FnKey::DynForward {
                iface_fn: f,
                dyn_iface,
            });
            fields.push(self.closure(&name, n, vec![code])?);
        }
        for &j in &shape.embeds {
            fields.push(self.dict(&Dict::Dyn { iface: j, ty })?);
        }
        let module = self.module.to_owned();
        self.add(
            StaticKind::Dict,
            module,
            name,
            shape.layout()?,
            fields,
            IndexVec::new(),
        )
    }

    /// The static closure of the `n`th function of the dictionary `dict`:
    /// its code word, then its captures, all constants.
    fn closure(&mut self, dict: &str, n: usize, fields: Vec<Const>) -> Result<Const, Ice> {
        let layout = LayoutRef::Closure(count(fields.len().saturating_sub(1))?);
        let module = self.module.to_owned();
        self.add(
            StaticKind::FnValue,
            module,
            format!("{dict}${n}"),
            layout,
            fields,
            IndexVec::new(),
        )
    }

    /// The static descriptor of the closed type `ty` (D-524).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a type that mentions a type parameter or `Never`.
    pub(in crate::lower) fn desc(&mut self, ty: Ty) -> Result<Const, Ice> {
        let (head, args): (Const, Vec<Ty>) = match self.cx.defs.globals.types.kind(ty) {
            TyKind::Named { def, args } if args.is_empty() => return self.nominal(*def),
            // Interfaces have no type parameters (D-165), so `dyn I` is a
            // nominal type of its own.
            TyKind::Dyn { iface, .. } => return self.dyn_nominal(*iface),
            TyKind::Named { def, args } => (self.nominal(*def)?, args.to_vec()),
            TyKind::Tuple(elems) => (Const::BuiltinDesc(BuiltinDesc::Tuple), elems.to_vec()),
            TyKind::Fn { params, ret } => {
                let mut args = params.to_vec();
                args.push(*ret);
                (Const::BuiltinDesc(BuiltinDesc::Fn), args)
            }
            other => {
                return Err(Ice::new(format!("a static descriptor of {other:?}")));
            }
        };
        let name = type_text(self.cx, ty)?;
        let mut fields = Vec::with_capacity(args.len().saturating_add(1));
        fields.push(head);
        for &a in &args {
            fields.push(self.desc(a)?);
        }
        let layout = LayoutRef::Desc {
            args: count(args.len())?,
        };
        let module = self.module.to_owned();
        self.add(
            StaticKind::Desc,
            module,
            name,
            layout,
            fields,
            IndexVec::new(),
        )
    }

    /// The nominal descriptor of the type `def`: the runtime's for a
    /// builtin type, else `cb$desc$<module path>$$<Type>` holding the
    /// type's name and module path (D-523, D-524).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for `Never` or a def that is not readable.
    pub(in crate::lower) fn nominal(&mut self, def: DefId) -> Result<Const, Ice> {
        if let Some(b) = builtin_desc(def) {
            return Ok(Const::BuiltinDesc(b));
        }
        if def.module == ModuleId::BUILTIN {
            return Err(Ice::new(format!(
                "the builtin type {def:?} has no descriptor"
            )));
        }
        let name = self.cx.defs.name(def)?;
        let mut strings: IndexVec<StrId, Box<str>> = IndexVec::new();
        let type_name = strings.push(name.item.clone())?;
        let module_path = strings.push(name.module.clone())?;
        self.add(
            StaticKind::Desc,
            name.module.to_string(),
            name.item.to_string(),
            LayoutRef::DescNominal,
            vec![Const::Str(type_name), Const::Str(module_path)],
            strings,
        )
    }

    /// The nominal descriptor of the type `dyn iface`:
    /// `cb$desc$<module path of iface>$$dyn$<iface>` (D-523, D-579),
    /// holding the printed name `dyn <iface>` and the interface's module
    /// path. One object per JIT (D-561), so `dyn I` is one nominal type
    /// (D-582). The builtin `Show` has an empty module path, giving
    /// `cb$desc$$$dyn$Show` (D-603): the first `$$` still
    /// ends the module path (D-579).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a def that is not readable.
    pub(in crate::lower) fn dyn_nominal(&mut self, iface: DefId) -> Result<Const, Ice> {
        let name = self.cx.defs.name(iface)?;
        let mut strings: IndexVec<StrId, Box<str>> = IndexVec::new();
        let type_name = strings.push(format!("dyn {}", name.item).into())?;
        let module_path = strings.push(name.module.clone())?;
        self.add(
            StaticKind::Desc,
            name.module.to_string(),
            format!("dyn${}", name.item),
            LayoutRef::DescNominal,
            vec![Const::Str(type_name), Const::Str(module_path)],
            strings,
        )
    }

    /// Adds a static object to the unit (the same symbol twice is one
    /// object) and returns it as a constant.
    fn add(
        &mut self,
        kind: StaticKind,
        module: String,
        name: String,
        layout: LayoutRef,
        fields: Vec<Const>,
        strings: IndexVec<StrId, Box<str>>,
    ) -> Result<Const, Ice> {
        let id = self.pending.add_static(StaticObject {
            id: StaticId::from_u32(0),
            kind,
            module: module.into(),
            name: name.into(),
            layout,
            fields: fields.into(),
            strings,
        })?;
        Ok(Const::Static(id))
    }
}
