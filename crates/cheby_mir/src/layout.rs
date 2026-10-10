//! Object layouts of constructors, computed from interfaces alone (D-246,
//! D-252, D-454).
//!
//! A module that builds or matches `geo::Point` computes the same offsets
//! as `geo` itself, from `geo`'s interface and never its bodies.

use std::hash::Hasher as _;

use cheby_iface::{CtorFields, DefId, Ice, ItemSig, ModuleId, TypeDefKind};
use siphasher::sip::SipHasher13;

use crate::defs::Defs;
use crate::ir::LayoutRef;
use crate::repr::{Repr, repr_of};

/// The size of the object header and of every field slot, in bytes
/// (D-279).
const WORD: i32 = 8;

/// One field of a layout.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FieldDesc {
    /// The field's name for a record constructor, `None` for a positional
    /// one.
    pub name: Option<Box<str>>,
    /// How the field is stored: [`Repr::Value`] fields are scanned and
    /// counted; the others are raw.
    pub repr: Repr,
    /// Byte offset from the start of the object.
    pub offset: i32,
}

/// A constructor's layout: fields in declaration order with their offsets.
/// Value fields come first in memory, then raw fields, so freeing needs
/// only the scan count (`value_fields`). Shared by codegen and the runtime
/// registration (D-454), which equality, hashing and debug printing
/// read in declaration order (D-252, D-233).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct LayoutDesc {
    pub layout: LayoutRef,
    /// The constructor's path, for diagnostics and symbols.
    pub name: Box<str>,
    /// The constructor's position among its type's constructors.
    pub tag: u32,
    /// In declaration order.
    pub fields: Vec<FieldDesc>,
    pub value_fields: u32,
    pub raw_fields: u32,
    /// The type the constructor builds.
    pub type_def: DefId,
    /// The type's name as D-463 prints it inside its own module (`Shape`),
    /// for debug printing.
    pub type_name: Box<str>,
    /// The constructor's name (`Circle`), for debug printing (§3.13).
    pub ctor_name: Box<str>,
    /// Whether the constructor has named fields (`Point { x: 1 }`).
    pub record: bool,
    /// The type's `@std_hooks(equal, hash, debug)` functions (D-482).
    /// Always `None` for now; W3-B fills it.
    pub hooks: Option<[DefId; 3]>,
    /// [`type_key`] of the type's module path and name, which the runtime
    /// hashes instead of a layout id (D-264, plan §4.3.2).
    pub type_key: u64,
}

/// The stable key of a type for hashing: SipHash-1-3 with a zero key of
/// the module path's UTF-8 bytes (`std::io`, empty for the builtin
/// module), one `0xff` byte (which UTF-8 never contains), then the type
/// name's bytes (`Shape`). It depends only on paths and names, never on
/// layout ids or build order (D-264).
#[must_use]
pub fn type_key(module_path: &str, type_name: &str) -> u64 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    h.write(module_path.as_bytes());
    h.write(&[0xff]);
    h.write(type_name.as_bytes());
    h.finish()
}

/// The layout of constructor `ctor`. A field whose declared type is `Int`,
/// `Float`, `Bool` or a sized number is raw; every other field, a type
/// parameter included, is a uniform value.
///
/// # Errors
///
/// An [`Ice`] if `ctor` is not a readable constructor, or the object is
/// too large to address.
pub fn ctor_layout(ctor: DefId, defs: &Defs<'_>) -> Result<LayoutDesc, Ice> {
    let item = defs.item(ctor)?;
    let ItemSig::Ctor(sig) = &item.sig else {
        return Err(Ice::new(format!("{ctor:?} is not a constructor")));
    };
    let names = &defs.globals.names;
    let declared: Vec<(Option<Box<str>>, Repr)> = match &sig.fields {
        CtorFields::Unit => Vec::new(),
        CtorFields::Positional(tys) => tys
            .iter()
            .map(|&ty| (None, repr_of(ty, defs.globals)))
            .collect(),
        CtorFields::Named(fields) => fields
            .iter()
            .map(|&(name, ty)| (Some(Box::from(names.get(name))), repr_of(ty, defs.globals)))
            .collect(),
    };
    let value_count = declared.iter().filter(|(_, r)| r.is_counted()).count();
    let mut next_value: i32 = WORD;
    let mut next_raw: i32 = i32::try_from(value_count)
        .ok()
        .and_then(|n| n.checked_mul(WORD))
        .and_then(|n| n.checked_add(WORD))
        .ok_or_else(|| Ice::new("constructor too large"))?;
    let mut fields = Vec::with_capacity(declared.len());
    for (name, repr) in declared {
        let slot = if repr.is_counted() {
            &mut next_value
        } else {
            &mut next_raw
        };
        let offset = *slot;
        *slot = slot
            .checked_add(WORD)
            .ok_or_else(|| Ice::new("constructor too large"))?;
        fields.push(FieldDesc { name, repr, offset });
    }
    let value_fields = u32::try_from(value_count).map_err(|_| Ice::new("constructor too large"))?;
    let raw_fields = u32::try_from(fields.len().saturating_sub(value_count))
        .map_err(|_| Ice::new("constructor too large"))?;
    let type_def = DefId::new(ctor.module, sig.ty);
    let type_path = defs.name(type_def)?;
    let ctor_path = defs.name(ctor)?;
    Ok(LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: ctor_path.path().into(),
        tag: sig.tag,
        fields,
        value_fields,
        raw_fields,
        type_def,
        type_key: type_key(&type_path.module, &type_path.item),
        type_name: type_path.item,
        ctor_name: ctor_path.item,
        record: matches!(sig.fields, CtorFields::Named(_)),
        hooks: None,
    })
}

/// The layouts of every constructor of the ADT `ty`, in tag order, read
/// from the interface of `ty`'s module (D-246).
///
/// # Errors
///
/// An [`Ice`] if `ty` is not a readable ADT.
pub fn type_layouts(ty: DefId, defs: &Defs<'_>) -> Result<Vec<LayoutDesc>, Ice> {
    let item = defs.item(ty)?;
    let ItemSig::Type(t) = &item.sig else {
        return Err(Ice::new(format!("{ty:?} is not a type")));
    };
    let TypeDefKind::Adt { ctors } = &t.kind else {
        return Err(Ice::new(format!("{ty:?} is not a type with constructors")));
    };
    ctors
        .iter()
        .map(|&index| ctor_layout(DefId::new(ty.module, index), defs))
        .collect()
}

/// The layouts of the constructors module `module` declares, in
/// `DefIndex` order. Builtin types' constructors have fixed runtime
/// layouts and are not listed.
///
/// # Errors
///
/// An [`Ice`] if the module's interface is inconsistent.
pub fn module_layouts(module: ModuleId, defs: &Defs<'_>) -> Result<Vec<LayoutDesc>, Ice> {
    let Some(own) = defs.own.filter(|own| own.module == module) else {
        return Err(Ice::new("module layouts need the module's own interface"));
    };
    let mut out = Vec::new();
    for (index, item) in own.items.iter_enumerated() {
        let ItemSig::Ctor(sig) = &item.sig else {
            continue;
        };
        let ty = own
            .item(sig.ty)
            .ok_or_else(|| Ice::new("constructor without its type"))?;
        if matches!(&ty.sig, ItemSig::Type(t) if matches!(t.kind, TypeDefKind::Adt { .. })) {
            out.push(ctor_layout(DefId::new(module, index), defs)?);
        }
    }
    Ok(out)
}
