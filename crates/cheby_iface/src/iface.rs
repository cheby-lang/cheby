//! The interface model of a module (B8): everything dependents may know
//! about it, and nothing about its bodies (D-246).

use cheby_diag::Span;
use cheby_package::{ModulePath, Targets};
use rustc_hash::FxHashMap;

use crate::{
    DefId, DefIndex, DefKind, Generics, Ice, IndexVec, ModuleId, Name, Namespace, PackageId, Ty,
    Vis,
};

/// The signature of an item.
#[derive(Clone, PartialEq, Debug)]
pub enum ItemSig {
    Fn(FnSig),
    Type(TypeDef),
    Ctor(CtorSig),
    Alias(AliasDef),
    Const(ConstSig),
    Interface(InterfaceDef),
    InterfaceFn(InterfaceFnSig),
    Test,
    /// The signature had errors (already reported); uses get `Ty::ERROR`.
    Error,
}

/// A parameter of a function signature.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ParamSig {
    pub name: Name,
    pub ty: Ty,
}

/// A top-level function's signature.
#[derive(Clone, PartialEq, Debug)]
pub struct FnSig {
    pub generics: Generics,
    pub params: Box<[ParamSig]>,
    /// `Ty::NIL` when the return type is omitted (D-083).
    pub ret: Ty,
    /// Indexes of the parameters marked by the std-only attribute
    /// `@std_clamped(name, …)`, sorted (D-298, D-303,
    /// GAP(std-attributes)). Always empty outside `std`.
    pub clamped: Box<[u32]>,
}

/// A type definition.
#[derive(Clone, PartialEq, Debug)]
pub struct TypeDef {
    pub generics: Generics,
    pub kind: TypeDefKind,
    /// `pub exposed` (§4.3.4).
    pub exposed: bool,
    /// For builtin types: the standard-library module that re-exports the
    /// type and holds its functions (§8.2, D-353), such as `std::option`.
    pub home: Option<ModulePath>,
    /// The functions given by the std-only attribute
    /// `@std_hooks(equal_fn, hash_fn, debug_fn)` (D-342,
    /// GAP(std-attributes)). The front end only stores them.
    pub hooks: Option<TypeHooks>,
}

/// The equality, hashing and debug-printing functions of a standard-library
/// type (D-342), named in the type's own module.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TypeHooks {
    pub equal: Name,
    pub hash: Name,
    pub debug: Name,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeDefKind {
    /// A type the compiler and runtime implement.
    Builtin(BuiltinType),
    /// A type with constructors, in declaration order.
    Adt { ctors: Box<[DefIndex]> },
    /// `@external` type (§12.5.3).
    External,
}

/// The builtin types that are not ADTs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuiltinType {
    Int,
    Float,
    String,
    List,
    Never,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    /// `Map<K, V>`, homed in `std::map` (GAP(kernel-types)).
    Map,
    /// `Set<T>`, homed in `std::set` (GAP(kernel-types)).
    Set,
    /// `Bytes`, homed in `std::bytes` (GAP(kernel-types)).
    Bytes,
}

/// A constructor's signature.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CtorSig {
    /// The type it constructs, in the same module.
    pub ty: DefIndex,
    /// Its position among the type's constructors.
    pub tag: u32,
    pub fields: CtorFields,
}

/// A constructor's fields. Field types use the type's `ParamIdx`s.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CtorFields {
    Unit,
    Positional(Box<[Ty]>),
    Named(Box<[(Name, Ty)]>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct AliasDef {
    pub generics: Generics,
    pub target: Ty,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConstSig {
    pub ty: Ty,
}

/// An interface (C8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InterfaceDef {
    /// Embedded interfaces, in written order.
    pub embeds: Box<[crate::DefId]>,
    /// Its functions, in declaration order.
    pub fns: Box<[DefIndex]>,
}

/// A function of an interface. `Self` is `ParamIdx` 0.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InterfaceFnSig {
    pub params: Box<[Ty]>,
    pub ret: Ty,
}

/// One item of a module's interface.
#[derive(Clone, PartialEq, Debug)]
pub struct IfaceItem {
    pub name: Name,
    pub kind: DefKind,
    pub vis: Vis,
    /// A constructor's type, an interface function's interface.
    pub parent: Option<DefIndex>,
    /// The name's span. In memory only, never hashed or serialized. `None`
    /// for builtin items, which have no source.
    pub span: Option<Span>,
    /// The targets the item exists on: its `@target`, narrowed by the
    /// module's `@!target`, and for a function without a body the targets
    /// of its `@external`s (§12.5, §12.6, D-141, D-461). The source of
    /// truth for every item kind. Constructors and interface functions
    /// carry their parent's targets.
    pub targets: Targets,
    /// The message of `@deprecated("…")`, if any (D-200).
    pub deprecated: Option<Box<str>>,
    pub sig: ItemSig,
}

/// A module's interface: every item, the private ones included, with
/// lookups by name.
///
/// Private items are here so the module's own later phases, codegen naming
/// and the REPL can find them, but they are left out of the hash and the
/// encoding ([`crate::hash`]), so editing a private signature never changes
/// dependents' inputs (D-249).
///
/// The item list only grows: the REPL appends a redefinition as a new item
/// and the name maps point to the newest one ([`ModuleInterface::push`]).
/// Do not change `items` directly; the name maps would not follow.
#[derive(Clone, Debug)]
pub struct ModuleInterface {
    pub module: ModuleId,
    pub package: PackageId,
    pub items: IndexVec<DefIndex, IfaceItem>,
    types: FxHashMap<Name, DefIndex>,
    values: FxHashMap<Name, DefIndex>,
    pub hash: Option<crate::InterfaceHash>,
}

impl ModuleInterface {
    /// An interface with no items.
    #[must_use]
    pub fn empty(module: ModuleId, package: PackageId) -> Self {
        Self {
            module,
            package,
            items: IndexVec::new(),
            types: FxHashMap::default(),
            values: FxHashMap::default(),
            hash: None,
        }
    }

    /// The interface of `module` with these items. When two items share a
    /// name in one namespace (already reported as E0041), the first one
    /// wins, as in name resolution.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if an item's parent is not an earlier item.
    pub fn new(
        module: ModuleId,
        package: PackageId,
        items: IndexVec<DefIndex, IfaceItem>,
    ) -> Result<Self, Ice> {
        let mut iface = Self::empty(module, package);
        for (index, item) in items.iter_enumerated() {
            Self::check_parent(index, item)?;
            if let Some(ns) = item.kind.namespace() {
                iface.map_mut(ns).entry(item.name).or_insert(index);
            }
        }
        iface.items = items;
        Ok(iface)
    }

    /// Appends an item, which becomes the one its name refers to (REPL
    /// redefinitions, D-043). Returns its index.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the item's parent is not an earlier item, or the
    /// module is full.
    pub fn push(&mut self, item: IfaceItem) -> Result<DefIndex, Ice> {
        let index = self
            .items
            .next_index()
            .ok_or_else(|| Ice::new("too many items in one module"))?;
        Self::check_parent(index, &item)?;
        let ns = item.kind.namespace();
        let name = item.name;
        self.items.push(item)?;
        if let Some(ns) = ns {
            self.map_mut(ns).insert(name, index);
        }
        Ok(index)
    }

    fn check_parent(index: DefIndex, item: &IfaceItem) -> Result<(), Ice> {
        match item.parent {
            Some(parent) if parent >= index => {
                Err(Ice::new("an item's parent must come before the item"))
            }
            _ => Ok(()),
        }
    }

    const fn map_mut(&mut self, ns: Namespace) -> &mut FxHashMap<Name, DefIndex> {
        match ns {
            Namespace::Type => &mut self.types,
            Namespace::Value => &mut self.values,
        }
    }

    #[must_use]
    pub fn item(&self, index: DefIndex) -> Option<&IfaceItem> {
        self.items.get(index)
    }

    /// The item named `name` in namespace `ns`, of any visibility.
    #[must_use]
    pub fn lookup(&self, ns: Namespace, name: Name) -> Option<DefIndex> {
        match ns {
            Namespace::Type => self.types.get(&name),
            Namespace::Value => self.values.get(&name),
        }
        .copied()
    }

    /// Whether item `index` is the one its name currently refers to. Items
    /// replaced by a REPL redefinition, and items in no namespace whose
    /// parent was replaced, are stale.
    #[must_use]
    pub fn is_current(&self, index: DefIndex) -> bool {
        let Some(item) = self.items.get(index) else {
            return false;
        };
        match (item.kind.namespace(), item.parent) {
            (Some(ns), _) if item.kind != DefKind::Ctor => {
                self.lookup(ns, item.name) == Some(index)
            }
            (_, Some(parent)) => self.is_current(parent),
            (_, None) => true,
        }
    }

    /// Whether item `index` is visible from another module of package
    /// `from` (§7.3): `pub` always, package-visible if `from` is this
    /// module's package, `priv` never.
    #[must_use]
    pub fn visible_from(&self, index: DefIndex, from: PackageId) -> bool {
        self.items.get(index).is_some_and(|item| match item.vis {
            Vis::Pub => true,
            Vis::Package => from == self.package,
            Vis::Priv => false,
        })
    }
}

/// Every function of interface `iface`: the functions of its embedded
/// interfaces first, in embedding order and depth first, then its own in
/// declaration order, deduplicated by name with the first occurrence kept
/// (§8.3). This order is the dictionary layout everywhere: THIR's
/// `Dict::Impl` and MIR.
///
/// `item` reads an item of any module the caller may read. An interface
/// reached twice (a diamond, or a cycle already reported as E0066) is
/// visited once. An embedded item whose signature is not an interface
/// (an error already reported) contributes nothing.
///
/// # Errors
///
/// An [`Ice`] if `iface` or one of its functions cannot be read, or
/// `iface` is not an interface.
pub fn interface_fns<'a>(
    iface: DefId,
    item: &dyn Fn(DefId) -> Option<&'a IfaceItem>,
) -> Result<Vec<DefId>, Ice> {
    let root = item(iface).ok_or_else(|| Ice::new(format!("interface {iface:?} unreadable")))?;
    if root.kind != DefKind::Interface {
        return Err(Ice::new(format!("{iface:?} is not an interface")));
    }
    let mut out = Vec::new();
    let mut names: Vec<Name> = Vec::new();
    let mut visited: Vec<DefId> = Vec::new();
    collect_interface_fns(iface, item, &mut visited, &mut names, &mut out)?;
    Ok(out)
}

fn collect_interface_fns<'a>(
    iface: DefId,
    item: &dyn Fn(DefId) -> Option<&'a IfaceItem>,
    visited: &mut Vec<DefId>,
    names: &mut Vec<Name>,
    out: &mut Vec<DefId>,
) -> Result<(), Ice> {
    if visited.contains(&iface) {
        return Ok(());
    }
    visited.push(iface);
    let Some(def) = item(iface) else {
        return Err(Ice::new(format!("interface {iface:?} unreadable")));
    };
    let ItemSig::Interface(def) = &def.sig else {
        return Ok(());
    };
    for &embed in &def.embeds {
        collect_interface_fns(embed, item, visited, names, out)?;
    }
    for &index in &def.fns {
        let f = DefId::new(iface.module, index);
        let f_item =
            item(f).ok_or_else(|| Ice::new(format!("interface function {f:?} unreadable")))?;
        if !names.contains(&f_item.name) {
            names.push(f_item.name);
            out.push(f);
        }
    }
    Ok(())
}

/// Whether the structure of a type, its constructors, patterns, fields and
/// record update, is usable from module `from_module` of package
/// `from_package` (§4.3.4, D-048, D-151):
///
/// | Declaration          | Structure usable              |
/// | -------------------- | ----------------------------- |
/// | `priv type T`        | in its own module only        |
/// | `type T`             | in any module of its package  |
/// | `pub type T`         | in any module of its package  |
/// | `pub exposed type T` | everywhere                    |
///
/// `type_item` is the type's item, declared in `type_module` of
/// `type_package`. An item that is not a type definition is treated as not
/// `exposed`.
#[must_use]
pub fn structure_visible(
    type_item: &IfaceItem,
    type_module: ModuleId,
    type_package: PackageId,
    from_module: ModuleId,
    from_package: PackageId,
) -> bool {
    let exposed = matches!(&type_item.sig, ItemSig::Type(def) if def.exposed);
    match type_item.vis {
        Vis::Priv => from_module == type_module,
        Vis::Package => from_package == type_package,
        Vis::Pub => exposed || from_package == type_package,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Globals, InterfaceDef, ItemSig};

    fn item(
        globals: &Globals,
        name: &str,
        kind: DefKind,
        vis: Vis,
        parent: Option<u32>,
    ) -> IfaceItem {
        IfaceItem {
            name: globals.names.intern(name),
            kind,
            vis,
            parent: parent.map(DefIndex::from_u32),
            span: None,
            targets: Targets::ALL,
            deprecated: None,
            sig: ItemSig::Error,
        }
    }

    fn interface(globals: &Globals, name: &str, embeds: &[DefId], fns: &[u32]) -> IfaceItem {
        IfaceItem {
            sig: ItemSig::Interface(InterfaceDef {
                embeds: embeds.into(),
                fns: fns.iter().copied().map(DefIndex::from_u32).collect(),
            }),
            ..item(globals, name, DefKind::Interface, Vis::Pub, None)
        }
    }

    fn iface_fn(globals: &Globals, name: &str, parent: u32) -> IfaceItem {
        item(globals, name, DefKind::InterfaceFn, Vis::Pub, Some(parent))
    }

    /// A diamond: `Num: Add + Sub`, both embedding `Base`, and `Sub` and
    /// `Num` repeating names.
    #[test]
    fn interface_fns_order_and_deduplication() {
        let g = Globals::new();
        let m = ModuleId::from_u32(2);
        let d = |n: u32| DefId::new(m, DefIndex::from_u32(n));
        let items = IndexVec::from_raw(vec![
            interface(&g, "Base", &[], &[1]),             // 0
            iface_fn(&g, "describe", 0),                  // 1
            interface(&g, "Add", &[d(0)], &[3]),          // 2
            iface_fn(&g, "add", 2),                       // 3
            interface(&g, "Sub", &[d(0)], &[5, 6]),       // 4
            iface_fn(&g, "sub", 4),                       // 5
            iface_fn(&g, "add", 4),                       // 6: same name as Add's
            interface(&g, "Num", &[d(2), d(4)], &[8, 9]), // 7
            iface_fn(&g, "zero_like", 7),                 // 8
            iface_fn(&g, "describe", 7),                  // 9: same name as Base's
            interface(&g, "Loop", &[d(10)], &[]),         // 10: cyclic, already E0066
        ])
        .unwrap();
        let iface = ModuleInterface::new(m, PackageId::LOCAL, items).unwrap();
        let read = |def: DefId| (def.module == m).then(|| iface.item(def.index)).flatten();
        assert_eq!(
            interface_fns(d(7), &read).unwrap(),
            [d(1), d(3), d(5), d(8)]
        );
        assert_eq!(interface_fns(d(4), &read).unwrap(), [d(1), d(5), d(6)]);
        assert_eq!(interface_fns(d(0), &read).unwrap(), [d(1)]);
        assert_eq!(interface_fns(d(10), &read).unwrap(), []);
        // Not an interface, or not readable.
        assert!(interface_fns(d(1), &read).is_err());
        assert!(
            interface_fns(
                DefId::new(ModuleId::from_u32(9), DefIndex::from_u32(0)),
                &read
            )
            .is_err()
        );
    }

    /// Every row of the table in §4.3.4.
    #[test]
    fn structure_visibility_follows_the_opacity_table() {
        let g = Globals::new();
        let ty = |vis: Vis, exposed: bool| IfaceItem {
            sig: ItemSig::Type(crate::TypeDef {
                generics: Generics::empty(),
                kind: crate::TypeDefKind::Adt {
                    ctors: Box::new([]),
                },
                exposed,
                home: None,
                hooks: None,
            }),
            ..item(&g, "T", DefKind::Type, vis, None)
        };
        let home = ModuleId::from_u32(3);
        let sibling = ModuleId::from_u32(4);
        let foreign = ModuleId::from_u32(5);
        let local = PackageId::LOCAL;
        let other = PackageId::STD;
        // (declaration, same module, sibling in the package, other package)
        let rows = [
            (ty(Vis::Priv, false), true, false, false),
            (ty(Vis::Package, false), true, true, false),
            (ty(Vis::Pub, false), true, true, false),
            (ty(Vis::Pub, true), true, true, true),
        ];
        for (item, own, in_package, outside) in rows {
            let vis = (
                item.vis,
                matches!(&item.sig, ItemSig::Type(def) if def.exposed),
            );
            assert_eq!(
                structure_visible(&item, home, local, home, local),
                own,
                "{vis:?}"
            );
            assert_eq!(
                structure_visible(&item, home, local, sibling, local),
                in_package,
                "{vis:?}"
            );
            assert_eq!(
                structure_visible(&item, home, local, foreign, other),
                outside,
                "{vis:?}"
            );
        }
    }

    #[test]
    fn lookups_visibility_and_redefinition() {
        let g = Globals::new();
        let items = IndexVec::from_raw(vec![
            item(&g, "Shape", DefKind::Type, Vis::Pub, None),
            item(&g, "Shape", DefKind::Ctor, Vis::Pub, Some(0)),
            item(&g, "helper", DefKind::Fn, Vis::Priv, None),
            item(&g, "area", DefKind::Fn, Vis::Package, None),
            item(&g, "area", DefKind::Fn, Vis::Pub, None),
        ])
        .unwrap();
        let mut iface =
            ModuleInterface::new(ModuleId::from_u32(3), PackageId::LOCAL, items).unwrap();
        let shape = g.names.intern("Shape");
        let area = g.names.intern("area");
        assert_eq!(
            iface.lookup(Namespace::Type, shape),
            Some(DefIndex::from_u32(0))
        );
        assert_eq!(
            iface.lookup(Namespace::Value, shape),
            Some(DefIndex::from_u32(1))
        );
        // A duplicate (E0041) keeps the first declaration.
        assert_eq!(
            iface.lookup(Namespace::Value, area),
            Some(DefIndex::from_u32(3))
        );
        assert!(iface.visible_from(DefIndex::from_u32(3), PackageId::LOCAL));
        assert!(!iface.visible_from(DefIndex::from_u32(3), PackageId::STD));
        assert!(!iface.visible_from(DefIndex::from_u32(2), PackageId::LOCAL));
        assert!(iface.visible_from(DefIndex::from_u32(0), PackageId::STD));
        // A REPL redefinition appends and takes the name over.
        let new_shape = iface
            .push(item(&g, "Shape", DefKind::Type, Vis::Pub, None))
            .unwrap();
        assert_eq!(new_shape, DefIndex::from_u32(5));
        assert_eq!(iface.lookup(Namespace::Type, shape), Some(new_shape));
        assert!(!iface.is_current(DefIndex::from_u32(0)));
        // The old constructor's type is stale, so the constructor is too.
        assert!(!iface.is_current(DefIndex::from_u32(1)));
        assert!(iface.is_current(new_shape));
        // A parent must come first.
        assert!(
            iface
                .push(item(&g, "X", DefKind::Ctor, Vis::Pub, Some(9)))
                .is_err()
        );
    }
}
