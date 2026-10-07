//! Object layouts of constructors, computed from interfaces alone (D-246,
//! D-252, D-454).
//!
//! A module that builds or matches `geo::Point` computes the same offsets
//! as `geo` itself, from `geo`'s interface and never its bodies.

use cheby_iface::{CtorFields, DefId, Ice, ItemSig, ModuleId, TypeDefKind};

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
    /// The constructor's path, for debug printing and diagnostics.
    pub name: Box<str>,
    /// The constructor's position among its type's constructors.
    pub tag: u32,
    /// In declaration order.
    pub fields: Vec<FieldDesc>,
    pub value_fields: u32,
    pub raw_fields: u32,
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
    Ok(LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: defs.name(ctor)?.path().into(),
        tag: sig.tag,
        fields,
        value_fields,
        raw_fields,
    })
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
