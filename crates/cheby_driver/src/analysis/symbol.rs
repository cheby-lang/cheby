//! `Symbol`: what a name means, made only of strings, so it is the one
//! identity that crosses snapshots (D-1365).
//!
//! Every handle of a run (`DefId`, `ModuleId`, `LocalId`, `FileId`, …) is
//! valid in that run only. A `Symbol` names the same thing in every run
//! where it still exists, and `Snapshot::find` maps it back to a handle of
//! one run. Equality, ordering and hashing are field by field, so two
//! symbols are equal exactly when they name the same thing:
//!
//! - A field is the type and the field name, over all of the type's
//!   variants (D-071), so a field read in one variant's pattern and another
//!   variant's constructor is one symbol.
//! - A record type and its constructor are two symbols, one per namespace.
//!   Rename treats them as one target by their shared name span, not by
//!   symbol equality.
//! - A use through an import alias is the alias's symbol, which lives in
//!   the importing module, never the aliased item's.
//! - A local is its item plus its name and an ordinal over the locals of
//!   that name in the item, in source order. The ordinal can shift across
//!   an edit, so a request that crosses snapshots also compares the token
//!   text (plan §1.3).
//!
//! `cheby_driver` has no serde. `cheby_lsp` mirrors this type for `data`
//! payloads and child output.

use cheby_iface::Namespace;
use cheby_package::ModulePath;

/// What a name means, across snapshots: strings, no ids (D-1365).
///
/// A constructor is `(module, Value, ctor name, Member::None)`. A builtin
/// uses the builtin module's path. A `test` item's name is its string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Symbol {
    /// The module that declares it, or for a member, its item's module.
    /// For [`Member::ImportAlias`], the importing module.
    pub module: ModulePath,
    /// The namespace of the item, or for a module, [`SymbolNs::Module`].
    pub ns: SymbolNs,
    /// The item: `show`, `Shape`. Empty for [`SymbolNs::Module`] and for
    /// [`Member::ImportAlias`].
    pub name: Box<str>,
    /// What inside the item, or [`Member::None`] for the item itself.
    pub member: Member,
}

/// The kind of thing a [`Symbol`] names at the top of its module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolNs {
    /// The module itself. The symbol's `name` is empty.
    Module,
    /// Types, aliases and interfaces (§7.7).
    Type,
    /// Functions, constants and constructors (§7.7).
    Value,
    /// A `test` item, named by its string.
    Test,
}

impl From<Namespace> for SymbolNs {
    fn from(ns: Namespace) -> Self {
        match ns {
            Namespace::Type => Self::Type,
            Namespace::Value => Self::Value,
        }
    }
}

/// What a [`Symbol`] names inside its item.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Member {
    /// The item itself.
    None,
    /// A field of the type `name` (D-071): one symbol over its variants.
    Field(Box<str>),
    /// A function of the interface `name`.
    InterfaceFn(Box<str>),
    /// A type parameter of the item `name`.
    TypeParam(Box<str>),
    /// A local of the item `name`. `ordinal` counts the locals of that name
    /// in the item in source order, from 0, since arms may reuse a name.
    Local {
        /// The local's name as written.
        name: Box<str>,
        /// Which local of that name, in source order.
        ordinal: u32,
    },
    /// An import's `as` name in `module`, which is the importing module.
    ImportAlias(Box<str>),
}

impl Symbol {
    /// The module `module` itself.
    #[must_use]
    pub fn of_module(module: ModulePath) -> Self {
        Self {
            module,
            ns: SymbolNs::Module,
            name: Box::default(),
            member: Member::None,
        }
    }

    /// The item `name` of `module` in `ns`. A `test` item takes
    /// [`SymbolNs::Test`] and its string as `name`.
    #[must_use]
    pub fn item(module: ModulePath, ns: SymbolNs, name: impl Into<Box<str>>) -> Self {
        Self {
            module,
            ns,
            name: name.into(),
            member: Member::None,
        }
    }

    /// The field `field` of the type `ty` declared in `module`.
    #[must_use]
    pub fn field(module: ModulePath, ty: impl Into<Box<str>>, field: impl Into<Box<str>>) -> Self {
        Self::item(module, SymbolNs::Type, ty).with_member(Member::Field(field.into()))
    }

    /// The function `func` of the interface `iface` declared in `module`.
    #[must_use]
    pub fn interface_fn(
        module: ModulePath,
        iface: impl Into<Box<str>>,
        func: impl Into<Box<str>>,
    ) -> Self {
        Self::item(module, SymbolNs::Type, iface).with_member(Member::InterfaceFn(func.into()))
    }

    /// The type parameter `param` of the item `item` in `ns` of `module`.
    #[must_use]
    pub fn type_param(
        module: ModulePath,
        ns: SymbolNs,
        item: impl Into<Box<str>>,
        param: impl Into<Box<str>>,
    ) -> Self {
        Self::item(module, ns, item).with_member(Member::TypeParam(param.into()))
    }

    /// The `ordinal`th local called `name` of the item `item` in `ns` of
    /// `module`.
    #[must_use]
    pub fn local(
        module: ModulePath,
        ns: SymbolNs,
        item: impl Into<Box<str>>,
        name: impl Into<Box<str>>,
        ordinal: u32,
    ) -> Self {
        Self::item(module, ns, item).with_member(Member::Local {
            name: name.into(),
            ordinal,
        })
    }

    /// The `as` name `alias` of an import in `importing`, in the namespace
    /// where the alias is bound: [`SymbolNs::Value`] for a module's alias
    /// (D-062), and the item's namespace for an item's, so `Point as P` has
    /// two symbols (D-111). The symbol's `name` is empty: the alias is not
    /// an item of `importing`.
    #[must_use]
    pub fn import_alias(importing: ModulePath, ns: SymbolNs, alias: impl Into<Box<str>>) -> Self {
        Self {
            module: importing,
            ns,
            name: Box::default(),
            member: Member::ImportAlias(alias.into()),
        }
    }

    /// This symbol with `member` in place of its member.
    #[must_use]
    pub fn with_member(self, member: Member) -> Self {
        Self { member, ..self }
    }

    /// Whether this is a local of an item.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self.member, Member::Local { .. })
    }

    /// The symbol this one is a member of: the item for a field, an
    /// interface function, a type parameter or a local, and the importing
    /// module for an import alias. `None` for an item or a module.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        match self.member {
            Member::None => None,
            Member::ImportAlias(_) => Some(Self::of_module(self.module.clone())),
            Member::Field(_)
            | Member::InterfaceFn(_)
            | Member::TypeParam(_)
            | Member::Local { .. } => Some(Self {
                module: self.module.clone(),
                ns: self.ns,
                name: self.name.clone(),
                member: Member::None,
            }),
        }
    }

    /// The name a use of this symbol writes: the member's name, the item's
    /// name, or for a module its last segment.
    #[must_use]
    pub fn text(&self) -> &str {
        match &self.member {
            Member::None => match self.ns {
                SymbolNs::Module => self.module.segments().last().map_or("", String::as_str),
                SymbolNs::Type | SymbolNs::Value | SymbolNs::Test => &self.name,
            },
            Member::Field(name)
            | Member::InterfaceFn(name)
            | Member::TypeParam(name)
            | Member::Local { name, .. }
            | Member::ImportAlias(name) => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};
    use std::hash::{BuildHasher, RandomState};

    use super::*;

    fn path(segments: &[&str]) -> ModulePath {
        let (first, rest) = segments.split_first().expect("a package name");
        rest.iter()
            .fold(ModulePath::root(first), |path, seg| path.child(seg))
    }

    fn shapes() -> ModulePath {
        path(&["app", "shapes"])
    }

    /// One hasher for every call, so equal values hash equal.
    fn hash_of(state: &RandomState, symbol: &Symbol) -> u64 {
        state.hash_one(symbol)
    }

    #[test]
    fn equal_symbols_hash_equal_whatever_their_allocation() {
        let state = RandomState::new();
        let built = [
            Symbol::of_module(shapes()),
            Symbol::item(shapes(), SymbolNs::Value, "area"),
            Symbol::item(shapes(), SymbolNs::Test, "area of a unit square"),
            Symbol::field(shapes(), "Shape", "width"),
            Symbol::interface_fn(shapes(), "Show", "show"),
            Symbol::type_param(shapes(), SymbolNs::Type, "Pair", "a"),
            Symbol::local(shapes(), SymbolNs::Value, "area", "w", 1),
            Symbol::import_alias(shapes(), SymbolNs::Value, "l"),
        ];
        for symbol in &built {
            let again = Symbol {
                module: path(&["app", "shapes"]),
                ns: symbol.ns,
                name: String::from(&*symbol.name).into_boxed_str(),
                member: symbol.member.clone(),
            };
            assert_eq!(*symbol, again);
            assert_eq!(hash_of(&state, symbol), hash_of(&state, &again));
        }
        let set: HashSet<Symbol> = built.iter().cloned().collect();
        assert_eq!(set.len(), built.len(), "every built symbol is distinct");
    }

    #[test]
    fn a_field_is_its_type_and_name_only() {
        // D-071: one symbol over every variant, built from any site.
        let from_pattern = Symbol::field(shapes(), "Shape", "width");
        let from_ctor = Symbol::item(shapes(), SymbolNs::Type, "Shape")
            .with_member(Member::Field("width".into()));
        assert_eq!(from_pattern, from_ctor);
        assert_ne!(from_pattern, Symbol::field(shapes(), "Shape", "height"));
        assert_ne!(from_pattern, Symbol::field(shapes(), "Box", "width"));
        assert_ne!(
            from_pattern,
            Symbol::field(path(&["app", "boxes"]), "Shape", "width")
        );
    }

    #[test]
    fn a_record_type_and_its_constructor_differ_by_namespace() {
        let ty = Symbol::item(shapes(), SymbolNs::Type, "Point");
        let ctor = Symbol::item(shapes(), SymbolNs::Value, "Point");
        assert_ne!(ty, ctor);
        assert_eq!(ty.text(), ctor.text());
    }

    #[test]
    fn locals_differ_by_ordinal_item_and_namespace() {
        let first = Symbol::local(shapes(), SymbolNs::Value, "area", "w", 0);
        assert_ne!(
            first,
            Symbol::local(shapes(), SymbolNs::Value, "area", "w", 1)
        );
        assert_ne!(
            first,
            Symbol::local(shapes(), SymbolNs::Value, "perimeter", "w", 0)
        );
        assert_ne!(
            first,
            Symbol::local(shapes(), SymbolNs::Test, "area", "w", 0)
        );
        assert!(first.is_local());
        assert_eq!(first.text(), "w");
        // A local is never its item's type parameter of the same name.
        assert_ne!(
            Symbol::local(shapes(), SymbolNs::Value, "map", "a", 0),
            Symbol::type_param(shapes(), SymbolNs::Value, "map", "a")
        );
    }

    #[test]
    fn an_alias_is_not_the_aliased_item() {
        let item = Symbol::item(path(&["std", "list"]), SymbolNs::Value, "map");
        let alias = Symbol::import_alias(shapes(), SymbolNs::Value, "map");
        assert_ne!(item, alias);
        assert_eq!(alias.text(), "map");
        assert_eq!(alias.parent(), Some(Symbol::of_module(shapes())));
        assert_ne!(
            alias,
            Symbol::import_alias(shapes(), SymbolNs::Module, "map")
        );
    }

    #[test]
    fn member_kinds_never_collide() {
        let members = [
            Member::None,
            Member::Field("x".into()),
            Member::InterfaceFn("x".into()),
            Member::TypeParam("x".into()),
            Member::Local {
                name: "x".into(),
                ordinal: 0,
            },
            Member::ImportAlias("x".into()),
        ];
        let symbols: BTreeSet<Symbol> = members
            .iter()
            .map(|m| Symbol::item(shapes(), SymbolNs::Type, "T").with_member(m.clone()))
            .collect();
        assert_eq!(symbols.len(), members.len());
    }

    #[test]
    fn order_is_module_then_namespace_then_name_then_member() {
        let mut symbols = [
            Symbol::field(shapes(), "Shape", "a"),
            Symbol::item(shapes(), SymbolNs::Value, "area"),
            Symbol::item(shapes(), SymbolNs::Type, "Shape"),
            Symbol::of_module(shapes()),
            Symbol::item(path(&["app"]), SymbolNs::Value, "main"),
            Symbol::item(shapes(), SymbolNs::Test, "a"),
        ];
        symbols.sort();
        let got: Vec<(String, SymbolNs, &str)> = symbols
            .iter()
            .map(|s| (s.module.to_string(), s.ns, s.text()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("app".to_owned(), SymbolNs::Value, "main"),
                ("app::shapes".to_owned(), SymbolNs::Module, "shapes"),
                ("app::shapes".to_owned(), SymbolNs::Type, "Shape"),
                ("app::shapes".to_owned(), SymbolNs::Type, "a"),
                ("app::shapes".to_owned(), SymbolNs::Value, "area"),
                ("app::shapes".to_owned(), SymbolNs::Test, "a"),
            ]
        );
    }

    #[test]
    fn parent_strips_the_member() {
        let item = Symbol::item(shapes(), SymbolNs::Type, "Shape");
        assert_eq!(item.parent(), None);
        assert_eq!(Symbol::of_module(shapes()).parent(), None);
        for member in [
            Symbol::field(shapes(), "Shape", "width"),
            Symbol::interface_fn(shapes(), "Shape", "show"),
            Symbol::type_param(shapes(), SymbolNs::Type, "Shape", "a"),
            Symbol::local(shapes(), SymbolNs::Type, "Shape", "x", 2),
        ] {
            assert_eq!(member.parent().as_ref(), Some(&item));
        }
    }

    #[test]
    fn namespaces_map_from_the_interface() {
        assert_eq!(SymbolNs::from(Namespace::Type), SymbolNs::Type);
        assert_eq!(SymbolNs::from(Namespace::Value), SymbolNs::Value);
    }
}
