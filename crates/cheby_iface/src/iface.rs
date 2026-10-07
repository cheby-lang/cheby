//! The interface model of a module (B8): everything dependents may know
//! about it, and nothing about its bodies (D-246).

use cheby_diag::Span;
use cheby_package::{ModulePath, Targets};
use rustc_hash::FxHashMap;

use crate::{
    DefIndex, DefKind, Generics, Ice, IndexVec, ModuleId, Name, Namespace, PackageId, Ty, Vis,
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
    /// The declaration was rejected with E0038; uses of it are silent
    /// errors.
    Unsupported,
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
    /// The targets the function exists on (§12.5, §12.6).
    pub targets: Targets,
    /// The message of `@deprecated`, if any.
    pub deprecated: Option<Box<str>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Globals, ItemSig};

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
            sig: ItemSig::Error,
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
