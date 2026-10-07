//! The builtin module: the prelude's declarations (H1, §7.6, D-112).
//!
//! The spec declares `Bool`, `Option` and the other prelude types in their
//! standard-library modules (§8.2, D-353), but the prelude must exist
//! before any module is checked and without import cycles (D-447). They
//! are compiler-known declarations of
//! [`ModuleId::BUILTIN`](crate::ModuleId::BUILTIN), which has no source and
//! cannot be imported. Each type records its home module, which re-exports
//! it (name resolution adds the re-export) and is where interface
//! satisfaction looks for its functions. Observable behavior matches the
//! spec.

use cheby_package::{ModulePath, Targets};

use crate::{
    BuiltinType, CtorFields, CtorSig, DefId, DefIndex, DefKind, GenericParam, Generics, Globals,
    IfaceItem, IndexVec, InterfaceDef, InterfaceFnSig, ItemSig, ModuleId, ModuleInterface, Name,
    PackageId, ParamIdx, Ty, TypeDef, TypeDefKind, Vis, sym,
};

const fn def(index: u32) -> DefId {
    DefId::new(ModuleId::BUILTIN, DefIndex::from_u32(index))
}

pub const INT: DefId = def(0);
pub const FLOAT: DefId = def(1);
pub const STRING: DefId = def(2);
pub const BOOL: DefId = def(3);
pub const FALSE: DefId = def(4);
pub const TRUE: DefId = def(5);
/// The type `Nil`.
pub const NIL_TY: DefId = def(6);
/// The constructor `Nil`.
pub const NIL: DefId = def(7);
pub const LIST: DefId = def(8);
pub const RESULT: DefId = def(9);
pub const OK: DefId = def(10);
pub const ERR: DefId = def(11);
pub const OPTION: DefId = def(12);
pub const SOME: DefId = def(13);
pub const NONE: DefId = def(14);
pub const ORDER: DefId = def(15);
pub const LESS: DefId = def(16);
pub const EQUAL: DefId = def(17);
pub const GREATER: DefId = def(18);
pub const NEVER: DefId = def(19);
pub const I8: DefId = def(20);
pub const I16: DefId = def(21);
pub const I32: DefId = def(22);
pub const I64: DefId = def(23);
pub const U8: DefId = def(24);
pub const U16: DefId = def(25);
pub const U32: DefId = def(26);
pub const U64: DefId = def(27);
pub const F32: DefId = def(28);
pub const SHOW: DefId = def(29);
/// `Show::show`.
pub const SHOW_FN: DefId = def(30);
/// `Map<K, V>`, not in the prelude: homed in `std::map`
/// (GAP(kernel-types)).
pub const MAP: DefId = def(31);
/// `Set<T>`, not in the prelude: homed in `std::set` (GAP(kernel-types)).
pub const SET: DefId = def(32);
/// `Bytes`, not in the prelude: homed in `std::bytes`
/// (GAP(kernel-types)).
pub const BYTES: DefId = def(33);

/// The number of builtin items.
pub const LEN: u32 = 34;

/// The home module of each builtin type that has one (§8.2, D-353), by
/// item: `(item, last segment under std)`. `Never` has none, and the home
/// of `Show` is not specified (D-447).
pub const HOMES: &[(DefId, &str)] = &[
    (INT, "int"),
    (FLOAT, "float"),
    (STRING, "string"),
    (BOOL, "bool"),
    (NIL_TY, "nil"),
    (LIST, "list"),
    (RESULT, "result"),
    (OPTION, "option"),
    (ORDER, "order"),
    (I8, "i8"),
    (I16, "i16"),
    (I32, "i32"),
    (I64, "i64"),
    (U8, "u8"),
    (U16, "u16"),
    (U32, "u32"),
    (U64, "u64"),
    (F32, "f32"),
    (MAP, "map"),
    (SET, "set"),
    (BYTES, "bytes"),
];

/// The path of a home module, `std::<segment>`.
#[must_use]
pub fn home_path(segment: &str) -> ModulePath {
    ModulePath::root("std").child(segment)
}

/// The builtin items whose home module is `path`. For a home module, name
/// resolution adds these as re-exports, together with their constructors.
#[must_use]
pub fn homed_at(path: &ModulePath) -> Vec<DefId> {
    HOMES
        .iter()
        .filter(|(_, segment)| home_path(segment) == *path)
        .map(|&(def, _)| def)
        .collect()
}

/// Every home module path, in [`HOMES`] order.
pub fn home_paths() -> impl Iterator<Item = ModulePath> {
    HOMES.iter().map(|(_, segment)| home_path(segment))
}

/// The number of builtin items in the prelude (§7.6): the first
/// `PRELUDE_LEN` items. The items after them are builtin but reachable only
/// through their home modules (GAP(kernel-types)).
pub const PRELUDE_LEN: u32 = 31;

/// Whether `def` is one of the builtin items.
#[must_use]
pub fn is_builtin(def: DefId) -> bool {
    def.module == ModuleId::BUILTIN && def.index.as_u32() < LEN
}

/// Whether `def` is a builtin item of the prelude (§7.6), which name
/// resolution adds to every module's scope. `Map`, `Set` and `Bytes` are
/// builtin but not in the prelude (GAP(kernel-types)).
#[must_use]
pub fn is_prelude(def: DefId) -> bool {
    def.module == ModuleId::BUILTIN && def.index.as_u32() < PRELUDE_LEN
}

/// The interface of the builtin module. All items are `pub`. The prelude
/// (§7.6) is exactly its first [`PRELUDE_LEN`] items: the prelude types,
/// their constructors and `Show`. `Map`, `Set` and `Bytes` follow, reachable
/// only through their home modules (GAP(kernel-types)).
#[must_use]
pub fn interface(globals: &Globals) -> ModuleInterface {
    let types = &globals.types;
    let t0 = types.param(ParamIdx::from_u32(0));
    let t1 = types.param(ParamIdx::from_u32(1));
    let mut items = Vec::new();
    let home = |segment: &str| Some(home_path(segment));
    let builtin = |items: &mut Vec<IfaceItem>, name: Name, kind: BuiltinType, segment| {
        let params: &[Name] = match kind {
            BuiltinType::List | BuiltinType::Set => &[sym::T],
            BuiltinType::Map => &[sym::K, sym::V],
            _ => &[],
        };
        items.push(ty_item(
            name,
            generics(params),
            TypeDefKind::Builtin(kind),
            segment,
        ));
    };
    builtin(&mut items, sym::INT, BuiltinType::Int, home("int"));
    builtin(&mut items, sym::FLOAT, BuiltinType::Float, home("float"));
    builtin(&mut items, sym::STRING, BuiltinType::String, home("string"));
    adt(
        &mut items,
        sym::BOOL,
        &[],
        &[(sym::FALSE, None), (sym::TRUE, None)],
        home("bool"),
    );
    adt(&mut items, sym::NIL, &[], &[(sym::NIL, None)], home("nil"));
    builtin(&mut items, sym::LIST, BuiltinType::List, home("list"));
    adt(
        &mut items,
        sym::RESULT,
        &[sym::T, sym::E],
        &[(sym::OK, Some(t0)), (sym::ERR, Some(t1))],
        home("result"),
    );
    adt(
        &mut items,
        sym::OPTION,
        &[sym::T],
        &[(sym::SOME, Some(t0)), (sym::NONE, None)],
        home("option"),
    );
    adt(
        &mut items,
        sym::ORDER,
        &[],
        &[(sym::LESS, None), (sym::EQUAL, None), (sym::GREATER, None)],
        home("order"),
    );
    builtin(&mut items, sym::NEVER, BuiltinType::Never, None);
    let sized = [
        (sym::I8, BuiltinType::I8, "i8"),
        (sym::I16, BuiltinType::I16, "i16"),
        (sym::I32, BuiltinType::I32, "i32"),
        (sym::I64, BuiltinType::I64, "i64"),
        (sym::U8, BuiltinType::U8, "u8"),
        (sym::U16, BuiltinType::U16, "u16"),
        (sym::U32, BuiltinType::U32, "u32"),
        (sym::U64, BuiltinType::U64, "u64"),
        (sym::F32, BuiltinType::F32, "f32"),
    ];
    for (name, kind, segment) in sized {
        builtin(&mut items, name, kind, home(segment));
    }
    items.push(item(
        sym::SHOW,
        DefKind::Interface,
        None,
        ItemSig::Interface(InterfaceDef {
            embeds: Box::new([]),
            fns: Box::new([SHOW_FN.index]),
        }),
    ));
    items.push(item(
        sym::SHOW_FN,
        DefKind::InterfaceFn,
        Some(SHOW.index),
        ItemSig::InterfaceFn(InterfaceFnSig {
            params: Box::new([t0]),
            ret: Ty::STRING,
        }),
    ));
    builtin(&mut items, sym::MAP, BuiltinType::Map, home("map"));
    builtin(&mut items, sym::SET, BuiltinType::Set, home("set"));
    builtin(&mut items, sym::BYTES, BuiltinType::Bytes, home("bytes"));
    // The table above is well formed (a unit test pins it), so neither
    // step can fail; an empty interface is the harmless fallback.
    IndexVec::from_raw(items)
        .and_then(|items| ModuleInterface::new(ModuleId::BUILTIN, PackageId::STD, items))
        .unwrap_or_else(|_| ModuleInterface::empty(ModuleId::BUILTIN, PackageId::STD))
}

fn generics(names: &[Name]) -> Generics {
    let mut params = IndexVec::new();
    for &name in names {
        // At most two parameters: always fits.
        let _ = params.push(GenericParam {
            name,
            bounds: Box::new([]),
        });
    }
    Generics { params }
}

const fn item(name: Name, kind: DefKind, parent: Option<DefIndex>, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        name,
        kind,
        vis: Vis::Pub,
        parent,
        span: None,
        targets: Targets::ALL,
        deprecated: None,
        sig,
    }
}

fn ty_item(
    name: Name,
    generics: Generics,
    kind: TypeDefKind,
    home: Option<ModulePath>,
) -> IfaceItem {
    let exposed = matches!(kind, TypeDefKind::Adt { .. });
    item(
        name,
        DefKind::Type,
        None,
        ItemSig::Type(TypeDef {
            generics,
            kind,
            exposed,
            home,
            hooks: None,
        }),
    )
}

/// Pushes an ADT and its constructors. A constructor has one positional
/// field of the given type, or none.
fn adt(
    items: &mut Vec<IfaceItem>,
    name: Name,
    params: &[Name],
    ctors: &[(Name, Option<Ty>)],
    home: Option<ModulePath>,
) {
    let ty_index = DefIndex::from_u32(u32::try_from(items.len()).unwrap_or(u32::MAX));
    let first = ty_index.as_u32().saturating_add(1);
    let ctor_indexes: Box<[DefIndex]> = (0..u32::try_from(ctors.len()).unwrap_or(0))
        .map(|n| DefIndex::from_u32(first.saturating_add(n)))
        .collect();
    items.push(ty_item(
        name,
        generics(params),
        TypeDefKind::Adt {
            ctors: ctor_indexes,
        },
        home,
    ));
    for (tag, &(ctor, field)) in (0_u32..).zip(ctors) {
        let fields = field.map_or(CtorFields::Unit, |ty| {
            CtorFields::Positional(Box::new([ty]))
        });
        items.push(item(
            ctor,
            DefKind::Ctor,
            Some(ty_index),
            ItemSig::Ctor(CtorSig {
                ty: ty_index,
                tag,
                fields,
            }),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Namespace;

    #[test]
    fn table_is_pinned() {
        let globals = Globals::new();
        let iface = interface(&globals);
        assert_eq!(iface.items.len(), usize::try_from(LEN).unwrap());
        let expected: &[(DefId, &str, DefKind)] = &[
            (INT, "Int", DefKind::Type),
            (FLOAT, "Float", DefKind::Type),
            (STRING, "String", DefKind::Type),
            (BOOL, "Bool", DefKind::Type),
            (FALSE, "False", DefKind::Ctor),
            (TRUE, "True", DefKind::Ctor),
            (NIL_TY, "Nil", DefKind::Type),
            (NIL, "Nil", DefKind::Ctor),
            (LIST, "List", DefKind::Type),
            (RESULT, "Result", DefKind::Type),
            (OK, "Ok", DefKind::Ctor),
            (ERR, "Err", DefKind::Ctor),
            (OPTION, "Option", DefKind::Type),
            (SOME, "Some", DefKind::Ctor),
            (NONE, "None", DefKind::Ctor),
            (ORDER, "Order", DefKind::Type),
            (LESS, "Less", DefKind::Ctor),
            (EQUAL, "Equal", DefKind::Ctor),
            (GREATER, "Greater", DefKind::Ctor),
            (NEVER, "Never", DefKind::Type),
            (I8, "I8", DefKind::Type),
            (I16, "I16", DefKind::Type),
            (I32, "I32", DefKind::Type),
            (I64, "I64", DefKind::Type),
            (U8, "U8", DefKind::Type),
            (U16, "U16", DefKind::Type),
            (U32, "U32", DefKind::Type),
            (U64, "U64", DefKind::Type),
            (F32, "F32", DefKind::Type),
            (SHOW, "Show", DefKind::Interface),
            (SHOW_FN, "show", DefKind::InterfaceFn),
            (MAP, "Map", DefKind::Type),
            (SET, "Set", DefKind::Type),
            (BYTES, "Bytes", DefKind::Type),
        ];
        assert_eq!(expected.len(), iface.items.len());
        for &(def, name, kind) in expected {
            let item = iface.item(def.index).unwrap();
            assert_eq!(globals.names.get(item.name), name, "{def:?}");
            assert_eq!(item.kind, kind, "{name}");
            assert_eq!(item.vis, Vis::Pub);
            if let Some(ns) = kind.namespace() {
                assert_eq!(iface.lookup(ns, item.name), Some(def.index), "{name}");
            }
        }
        // `Nil` is in both namespaces.
        assert_eq!(iface.lookup(Namespace::Type, sym::NIL), Some(NIL_TY.index));
        assert_eq!(iface.lookup(Namespace::Value, sym::NIL), Some(NIL.index));
        // Constructor order follows docs/stdlib.
        let ItemSig::Ctor(none) = &iface.item(NONE.index).unwrap().sig else {
            panic!("None is a constructor");
        };
        assert_eq!((none.ty, none.tag), (OPTION.index, 1));
        let ItemSig::Type(option) = &iface.item(OPTION.index).unwrap().sig else {
            panic!("Option is a type");
        };
        assert_eq!(
            option.kind,
            TypeDefKind::Adt {
                ctors: Box::new([SOME.index, NONE.index])
            }
        );
        assert_eq!(option.generics.len(), 1);
        assert!(option.exposed);
        assert_eq!(option.home, Some(home_path("option")));
    }

    #[test]
    fn kernel_types_are_builtin_but_not_prelude() {
        let globals = Globals::new();
        let iface = interface(&globals);
        assert_eq!(PRELUDE_LEN, 31);
        assert_eq!(LEN, 34);
        for index in 0..LEN {
            let def = def(index);
            assert!(is_builtin(def));
            assert_eq!(is_prelude(def), index < PRELUDE_LEN, "{def:?}");
        }
        assert!(!is_prelude(def(LEN)));
        assert!(!is_prelude(DefId::new(
            ModuleId::from_u32(1),
            DefIndex::from_u32(0)
        )));
        let expect = [
            (MAP, BuiltinType::Map, &["K", "V"][..], "map"),
            (SET, BuiltinType::Set, &["T"][..], "set"),
            (BYTES, BuiltinType::Bytes, &[][..], "bytes"),
        ];
        for (def, kind, params, home) in expect {
            let ItemSig::Type(ty) = &iface.item(def.index).unwrap().sig else {
                panic!("{def:?} is a type");
            };
            assert_eq!(ty.kind, TypeDefKind::Builtin(kind));
            let names: Vec<&str> = ty
                .generics
                .params
                .iter()
                .map(|p| globals.names.get(p.name))
                .collect();
            assert_eq!(names, params);
            assert!(!ty.exposed);
            assert_eq!(ty.home, Some(home_path(home)));
            assert_eq!(homed_at(&home_path(home)), [def]);
        }
    }

    #[test]
    fn homes() {
        assert_eq!(homed_at(&home_path("option")), [OPTION]);
        assert_eq!(homed_at(&home_path("io")), []);
        assert_eq!(home_paths().count(), HOMES.len());
        assert!(is_builtin(SHOW_FN));
        assert!(!is_builtin(def(LEN)));
    }
}
