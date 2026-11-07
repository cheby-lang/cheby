//! The module scope (B5, B6, §7.6, §7.7): the module's own items, its
//! imports, and the prelude, per namespace.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefIndex, DefKind, Ice, ImportNames, IndexVec, ModuleId, ModuleInterface, Name,
    Namespace, builtin,
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::paths::PathCx;
use crate::{ImportItemRequest, ImportRequest, ImportTarget, ItemDef, Res, ResolveInput};

/// Where a module-scope name comes from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ScopeSource {
    /// An item of the module.
    Item,
    /// The import with this position in the module's imports.
    Import(usize),
    /// The prelude (§7.6).
    Prelude,
    /// A prelude type re-exported by its home module (D-447).
    HomeReexport,
    /// An item a REPL session's import can no longer bind, because a
    /// `:reload` removed it or made it private (§13.5.3). The name stays
    /// taken, so it hides the prelude as the import did, and
    /// [`ModuleScope::get`] does not find it, so each use is an unknown
    /// name.
    Withdrawn,
}

/// One name of the module scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScopeEntry {
    pub res: Res,
    pub source: ScopeSource,
    /// Where the name is introduced; `None` for the prelude.
    pub span: Option<Span>,
}

/// The names visible everywhere in a module, per namespace. A module's own
/// items and its imports never collide (E0041); both hide the prelude
/// (W0007, D-146, D-212).
#[derive(Clone, Debug, Default)]
pub struct ModuleScope {
    types: FxHashMap<Name, ScopeEntry>,
    values: FxHashMap<Name, ScopeEntry>,
    /// Names with an E0041 between an import and another name, so uses
    /// of them report nothing more.
    conflicts: FxHashSet<Name>,
}

impl ModuleScope {
    #[must_use]
    pub fn get(&self, ns: Namespace, name: Name) -> Option<&ScopeEntry> {
        self.map(ns)
            .get(&name)
            .filter(|entry| entry.source != ScopeSource::Withdrawn)
    }

    /// The names of namespace `ns` that a lookup finds, in no particular
    /// order.
    pub fn names(&self, ns: Namespace) -> impl Iterator<Item = Name> + '_ {
        self.map(ns)
            .iter()
            .filter(|(_, entry)| entry.source != ScopeSource::Withdrawn)
            .map(|(&name, _)| name)
    }

    /// The names of `ns`, in no particular order, so a caller that shows
    /// them sorts them (D-264). As for [`ModuleScope::get`], a withdrawn
    /// name is left out. The prelude and home re-exports are in, and each
    /// entry's `source` tells them apart.
    pub fn iter(&self, ns: Namespace) -> impl Iterator<Item = (Name, &ScopeEntry)> {
        self.map(ns)
            .iter()
            .filter(|(_, entry)| entry.source != ScopeSource::Withdrawn)
            .map(|(&name, entry)| (name, entry))
    }

    /// How the module names what it imports, for text a reader of the
    /// module sees (D-1384). When two imports bind one item or module, the
    /// first one written gives the name, so the result does not depend on
    /// the order of the maps (D-264).
    #[must_use]
    pub fn import_names(&self) -> ImportNames {
        let mut imports: Vec<_> = [Namespace::Type, Namespace::Value]
            .into_iter()
            .flat_map(|ns| self.iter(ns))
            .filter_map(|(name, entry)| match entry.source {
                ScopeSource::Import(position) => Some((
                    position,
                    entry.span.map(|span| span.range.start()),
                    name,
                    entry.res,
                )),
                _ => None,
            })
            .collect();
        imports.sort_by_key(|&(position, start, ..)| (position, start));
        let mut names = ImportNames::default();
        for (_, _, name, res) in imports {
            match res {
                Res::Def(def) => {
                    names.items.entry(def).or_insert(name);
                }
                Res::Module(module) => {
                    names.modules.entry(module).or_insert(name);
                }
                _ => {}
            }
        }
        names
    }

    /// Whether an import of `name` collided with another name (E0041).
    #[must_use]
    pub fn is_conflicted(&self, name: Name) -> bool {
        self.conflicts.contains(&name)
    }

    const fn map(&self, ns: Namespace) -> &FxHashMap<Name, ScopeEntry> {
        match ns {
            Namespace::Type => &self.types,
            Namespace::Value => &self.values,
        }
    }

    const fn map_mut(&mut self, ns: Namespace) -> &mut FxHashMap<Name, ScopeEntry> {
        match ns {
            Namespace::Type => &mut self.types,
            Namespace::Value => &mut self.values,
        }
    }
}

pub(crate) fn build(
    input: &ResolveInput<'_>,
    items: &IndexVec<DefIndex, ItemDef>,
    diags: &mut Vec<Diagnostic>,
) -> Result<ModuleScope, Ice> {
    let mut scope = ModuleScope::default();
    let prelude = Prelude::new(input)?;
    // Own items. Duplicates were reported by item collection; the first
    // declaration wins.
    for (index, item) in items.iter_enumerated() {
        let Some(ns) = item.kind.namespace() else {
            continue;
        };
        if scope.map(ns).contains_key(&item.name) {
            continue;
        }
        let res = Res::Def(DefId::new(input.module, index));
        if let Some(hidden) = prelude.get(ns, item.name) {
            diags.push(prelude.hiding(input, hidden, item.name, item.name_span, false));
        }
        scope.map_mut(ns).insert(
            item.name,
            ScopeEntry {
                res,
                source: ScopeSource::Item,
                span: Some(item.name_span),
            },
        );
    }
    // Module imports bind a name in the value namespace (§7.7); item
    // imports bind the listed items (§7.5.2).
    let lookups = PathCx {
        input,
        items,
        scope: &ModuleScope::default(),
    };
    for (n, (request, target)) in input.imports.iter().zip(input.targets).enumerate() {
        if let Some(list) = &request.items {
            for item in list {
                for binding in item_import(&lookups, request, item, *target, diags)? {
                    let (ns, name, span, res) = binding;
                    // A name that is already taken is E0041 instead.
                    if let Res::Def(def) = res
                        && scope.get(ns, name).is_none()
                        && let Some(hidden) = prelude.get(ns, name)
                        && hidden != def
                    {
                        diags.push(prelude.hiding(input, hidden, name, span, true));
                    }
                    let source = if request.session && res == Res::Err {
                        ScopeSource::Withdrawn
                    } else {
                        ScopeSource::Import(n)
                    };
                    insert_import(&mut scope, input, binding, source, diags);
                }
            }
            continue;
        }
        let Some((name, span)) = request.module_binding() else {
            continue;
        };
        let res = target.module().map_or(Res::Err, Res::Module);
        insert_import(
            &mut scope,
            input,
            (Namespace::Value, name, span, res),
            ScopeSource::Import(n),
            diags,
        );
    }
    add_prelude(input, &prelude, &mut scope)?;
    Ok(scope)
}

/// The prelude (§7.6, D-112): the builtin items with
/// [`builtin::is_prelude`], by namespace and name.
struct Prelude<'a> {
    builtin: &'a ModuleInterface,
}

impl<'a> Prelude<'a> {
    fn new(input: &ResolveInput<'a>) -> Result<Self, Ice> {
        let builtin = input
            .deps
            .get(ModuleId::BUILTIN)
            .ok_or_else(|| Ice::new("the builtin interface is not readable"))?;
        Ok(Self { builtin })
    }

    /// The prelude item named `name` in namespace `ns`, if any.
    fn get(&self, ns: Namespace, name: Name) -> Option<DefId> {
        let index = self.builtin.lookup(ns, name)?;
        let def = DefId::new(ModuleId::BUILTIN, index);
        builtin::is_prelude(def).then_some(def)
    }

    /// W0007: a declaration or an item import that hides the prelude item
    /// `hidden` of the same namespace (D-146, D-212).
    fn hiding(
        &self,
        input: &ResolveInput<'_>,
        hidden: DefId,
        name: Name,
        span: Span,
        imported: bool,
    ) -> Diagnostic {
        let text = input.globals.names.get(name);
        let item = self.builtin.item(hidden.index);
        let kind = item.map_or("item", |item| match item.kind {
            DefKind::Ctor => "constructor",
            DefKind::Interface => "interface",
            _ => "type",
        });
        let owner = item
            .filter(|item| item.kind == DefKind::Ctor)
            .and_then(|item| item.parent)
            .map_or(hidden, |parent| DefId::new(ModuleId::BUILTIN, parent));
        let home = builtin::HOMES
            .iter()
            .find(|(def, _)| *def == owner)
            .map(|&(_, segment)| segment);
        let (label, help) = if imported {
            (
                "imported here",
                format!("if this is not intended, import it under another name: `{text} as …`"),
            )
        } else {
            (
                "declared here",
                format!("if this is not intended, give the {kind} another name"),
            )
        };
        let note = match home {
            Some(home) => format!(
                "in this module, `{text}` no longer means the prelude {kind}, which stays \
                 reachable as `{home}::{text}` after `import std::{home}`"
            ),
            None => format!("in this module, `{text}` no longer means the prelude {kind}"),
        };
        Diagnostic::new(
            codes::HIDDEN_PRELUDE_NAME,
            format!("`{text}` hides the prelude {kind} `{text}`"),
        )
        .with_primary(span, label)
        .with_note(note)
        .with_help(help)
    }
}

/// The bindings of one item of `import m::{x, Y as Z}`: a `LOWER` name
/// is a function or constant, an `UPPER` name the type or interface and
/// the constructor with that name (D-111). A name that is missing or not
/// visible is reported once, whatever its namespaces.
fn item_import(
    lookups: &PathCx<'_>,
    request: &ImportRequest,
    item: &ImportItemRequest,
    target: ImportTarget,
    diags: &mut Vec<Diagnostic>,
) -> Result<Vec<(Namespace, Name, Span, Res)>, Ice> {
    let input = lookups.input;
    let (bound, bound_span) = item.alias.unwrap_or((item.name, item.span));
    let text = input.globals.names.get(item.name);
    let upper = text.starts_with(|c: char| c.is_ascii_uppercase());
    let namespaces: &[Namespace] = if upper {
        &[Namespace::Type, Namespace::Value]
    } else {
        &[Namespace::Value]
    };
    let failed = || {
        namespaces
            .iter()
            .map(|&ns| (ns, bound, bound_span, Res::Err))
            .collect()
    };
    let Some(module) = target.module() else {
        return Ok(failed());
    };
    let iface = input
        .deps
        .get(module)
        .ok_or_else(|| Ice::new("an imported module's interface is not readable"))?;
    let builtin_iface = input
        .deps
        .get(ModuleId::BUILTIN)
        .ok_or_else(|| Ice::new("the builtin interface is not readable"))?;
    let path = &input.modules.try_get(module)?.path;
    let homed = builtin::homed_at(path);
    let present: Vec<Namespace> = namespaces
        .iter()
        .copied()
        .filter(|&ns| {
            iface.lookup(ns, item.name).is_some()
                || builtin_iface
                    .lookup(ns, item.name)
                    .is_some_and(|index| is_homed(builtin_iface, &homed, index))
        })
        .collect();
    if present.is_empty() {
        // Reports E0042.
        let ns = namespaces.first().copied().unwrap_or(Namespace::Value);
        lookups.in_module(module, ns, (item.name, item.span), request.path_span, diags)?;
        return Ok(failed());
    }
    let mut out = Vec::new();
    let mut reported = false;
    for ns in present {
        let mut found = Vec::new();
        let res = lookups.in_module(
            module,
            ns,
            (item.name, item.span),
            request.path_span,
            &mut found,
        )?;
        // The type and the constructor of one name share their
        // visibility: one error is enough.
        if !reported {
            reported = !found.is_empty();
            diags.extend(found);
        }
        out.push((ns, bound, bound_span, res));
    }
    Ok(out)
}

/// Whether builtin item `index` is re-exported by the home module whose
/// homed items are `homed`: one of them, or a constructor of one.
fn is_homed(builtin_iface: &ModuleInterface, homed: &[DefId], index: DefIndex) -> bool {
    let def = DefId::new(ModuleId::BUILTIN, index);
    let owner = match builtin_iface.item(index) {
        Some(item) if item.kind == DefKind::Ctor => {
            item.parent.map(|p| DefId::new(ModuleId::BUILTIN, p))
        }
        Some(_) => Some(def),
        None => None,
    };
    owner.is_some_and(|owner| homed.contains(&owner))
}
/// Adds an import's binding, reporting E0041 if the name is taken.
fn insert_import(
    scope: &mut ModuleScope,
    input: &ResolveInput<'_>,
    (ns, name, span, res): (Namespace, Name, Span, Res),
    source: ScopeSource,
    diags: &mut Vec<Diagnostic>,
) {
    let entry = ScopeEntry {
        res,
        source,
        span: Some(span),
    };
    match scope.map(ns).get(&name) {
        // A failed import was reported; its name must not add noise.
        Some(_) if res == Res::Err => {}
        Some(existing) => {
            let text = input.globals.names.get(name);
            // Reported where the second name is introduced (§7.7). Imports
            // come before the module's items (E0018), so against an item
            // that is the item's declaration.
            let diag = match (existing.source, existing.span) {
                (ScopeSource::Item, Some(declared)) => Diagnostic::new(
                    codes::DUPLICATE_NAME,
                    format!("`{text}` is both imported and declared in this module"),
                )
                .with_primary(declared, "declared here")
                .with_secondary(span, "imported here"),
                (source, first) => {
                    let message = match source {
                        ScopeSource::Import(_) => format!("`{text}` is imported twice"),
                        _ => format!("`{text}` is both imported and declared in this module"),
                    };
                    let mut diag = Diagnostic::new(codes::DUPLICATE_NAME, message)
                        .with_primary(span, "imported here");
                    if let Some(first) = first {
                        diag = diag.with_secondary(first, "first imported here");
                    }
                    diag
                }
            };
            // The imports a REPL session adds to an entry may collide: the
            // newest stands and this report is not the entry's (`repl.rs`),
            // so nothing explains a later use but what the use reports.
            if diag
                .primary_span()
                .is_some_and(|at| at.file == input.tree.file())
            {
                scope.conflicts.insert(name);
            }
            diags.push(diag.with_help("give the import another name with `as`"));
        }
        None => {
            scope.map_mut(ns).insert(name, entry);
        }
    }
}

fn add_prelude(
    input: &ResolveInput<'_>,
    prelude: &Prelude<'_>,
    scope: &mut ModuleScope,
) -> Result<(), Ice> {
    // The prelude and, in a home module, the builtin items it re-exports,
    // which include the kernel types `Map`, `Set` and `Bytes` that are not
    // in the prelude (D-447, D-481). Both are hidden by own
    // items and imports (D-146, W0007).
    let builtin_iface = prelude.builtin;
    let path = &input.modules.try_get(input.module)?.path;
    let homed = builtin::homed_at(path);
    for (index, item) in builtin_iface.items.iter_enumerated() {
        let Some(ns) = item.kind.namespace() else {
            continue;
        };
        let def = DefId::new(ModuleId::BUILTIN, index);
        let source = if is_homed(builtin_iface, &homed, index) {
            ScopeSource::HomeReexport
        } else if builtin::is_prelude(def) {
            ScopeSource::Prelude
        } else {
            continue;
        };
        scope.map_mut(ns).entry(item.name).or_insert(ScopeEntry {
            res: Res::Def(def),
            source,
            span: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::FileId;
    use cheby_iface::{
        CtorFields, CtorSig, DepView, DisplayCx, Globals, IfaceItem, InterfaceSet, ItemSig,
        ModuleId, ModuleInfo, ModuleTable, PackageId, TypeDef, TypeDefKind, Vis, display,
    };
    use cheby_package::{ModulePath, Targets};

    use super::*;
    use crate::{ResolvedModule, StdCatalog, collect_imports};

    #[test]
    fn iter_leaves_out_what_get_cannot_find() {
        let globals = Globals::new();
        let shown = globals.names.intern("shown");
        let withdrawn = globals.names.intern("withdrawn");
        let entry = |source| ScopeEntry {
            res: Res::Module(ModuleId::BUILTIN),
            source,
            span: None,
        };
        let mut scope = ModuleScope::default();
        scope.values.insert(shown, entry(ScopeSource::Import(0)));
        scope
            .values
            .insert(withdrawn, entry(ScopeSource::Withdrawn));
        let values: Vec<Name> = scope.iter(Namespace::Value).map(|(n, _)| n).collect();
        assert_eq!(values, [shown]);
        assert_eq!(scope.iter(Namespace::Type).count(), 0);
    }

    /// `a::b`, with the type `T` and its constructor `T`, and the type `V`.
    struct Fixture {
        globals: Globals,
        modules: ModuleTable,
        set: InterfaceSet,
        dep: ModuleId,
        app: ModuleId,
    }

    const T: u32 = 0;
    const T_CTOR: u32 = 1;
    const V: u32 = 2;

    fn fixture() -> Fixture {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let mut add = |path: ModulePath| {
            modules
                .add(ModuleInfo {
                    path,
                    package: PackageId::LOCAL,
                    file: None,
                })
                .unwrap()
        };
        let dep = add(ModulePath::root("a").child("b"));
        let app = add(ModulePath::root("app"));
        let item = |name: &str, kind, parent, sig| IfaceItem {
            name: globals.names.intern(name),
            kind,
            vis: Vis::Pub,
            parent,
            span: None,
            targets: Targets::ALL,
            deprecated: None,
            sig,
        };
        let ty = |ctors: &[u32]| {
            ItemSig::Type(TypeDef {
                generics: cheby_iface::Generics::empty(),
                kind: TypeDefKind::Adt {
                    ctors: ctors.iter().copied().map(DefIndex::from_u32).collect(),
                },
                exposed: true,
                home: None,
                hooks: None,
            })
        };
        let ctor = ItemSig::Ctor(CtorSig {
            ty: DefIndex::from_u32(T),
            tag: 0,
            fields: CtorFields::Unit,
        });
        let items = vec![
            item("T", DefKind::Type, None, ty(&[T_CTOR])),
            item("T", DefKind::Ctor, Some(DefIndex::from_u32(T)), ctor),
            item("V", DefKind::Type, None, ty(&[])),
        ];
        let iface = ModuleInterface::new(dep, PackageId::LOCAL, IndexVec::from_raw(items).unwrap())
            .unwrap();
        let mut set = InterfaceSet::new(builtin::interface(&globals));
        set.insert(iface.into()).unwrap();
        Fixture {
            globals,
            modules,
            set,
            dep,
            app,
        }
    }

    impl Fixture {
        fn resolve(&self, text: &str) -> ResolvedModule {
            let file = FileId::new(self.app.as_u32());
            let (tree, errors) = cheby_syntax::parse(file, Arc::from(text)).into_parts();
            assert!(errors.is_empty(), "{errors:?}");
            let imports = collect_imports(&tree, &self.globals.names);
            let targets = vec![ImportTarget::Module(self.dep); imports.len()];
            let allowed = [self.dep];
            let (module, diags) = crate::resolve_module(&ResolveInput {
                module: self.app,
                package: PackageId::LOCAL,
                tree: &tree,
                imports: &imports,
                targets: &targets,
                deps: DepView::new(&self.set, &allowed),
                modules: &self.modules,
                globals: &self.globals,
                std: StdCatalog::default(),
                package_targets: Targets::ALL,
            })
            .unwrap();
            assert!(diags.is_empty(), "{diags:?}");
            module
        }

        fn def(&self, index: u32) -> DefId {
            DefId::new(self.dep, DefIndex::from_u32(index))
        }

        fn show(&self, index: u32, imports: &ImportNames) -> String {
            let allowed = [self.dep];
            let cx = DisplayCx {
                globals: &self.globals,
                modules: &self.modules,
                own: None,
                deps: &DepView::new(&self.set, &allowed),
                generics: None,
                imports: Some(imports),
            };
            display(self.globals.types.named(self.def(index), []), &cx)
        }
    }

    #[test]
    fn a_module_alias_is_a_module_name() {
        let f = fixture();
        let scope = f.resolve("import a::b as c\n").scope;
        let names = scope.import_names();
        assert_eq!(names.items.len(), 0);
        let bound = names.modules.get(&f.dep).copied().unwrap();
        assert_eq!(f.globals.names.get(bound), "c");
        assert_eq!(f.show(T, &names), "c::T");
    }

    #[test]
    fn a_plain_import_binds_its_last_segment() {
        let f = fixture();
        let names = f.resolve("import a::b\n").scope.import_names();
        assert_eq!(f.show(V, &names), "b::V");
    }

    #[test]
    fn an_item_alias_binds_the_type_and_its_constructor() {
        let f = fixture();
        let names = f.resolve("import a::b::{T as U}\n").scope.import_names();
        assert_eq!(f.show(T, &names), "U");
        let ctor = names.items.get(&f.def(T_CTOR)).copied().unwrap();
        assert_eq!(f.globals.names.get(ctor), "U");
        // Nothing binds the module itself.
        assert_eq!(names.modules.len(), 0);
        assert_eq!(f.show(V, &names), "b::V");
    }

    #[test]
    fn the_first_import_of_an_item_gives_its_name() {
        // One fixture, so each order has the same `Name`s and so the same
        // hash order, which at most one of them can agree with.
        let f = fixture();
        for (text, first) in [
            (
                "import a::b::{V as Zed, V as Abc}\nimport a::b::{V as Mid}\n",
                "Zed",
            ),
            (
                "import a::b::{V as Abc, V as Zed}\nimport a::b::{V as Mid}\n",
                "Abc",
            ),
            (
                "import a::b::{V as Mid}\nimport a::b::{V as Zed, V as Abc}\n",
                "Mid",
            ),
        ] {
            let names = f.resolve(text).scope.import_names();
            assert_eq!(f.show(V, &names), first, "{text}");
        }
    }

    #[test]
    fn own_items_and_the_prelude_are_not_imports() {
        let f = fixture();
        let names = f.resolve("type Own {\n  Own,\n}\n").scope.import_names();
        assert_eq!(names.items.len(), 0);
        assert_eq!(names.modules.len(), 0);
    }

    #[test]
    fn a_withdrawn_or_failed_import_names_nothing() {
        let globals = Globals::new();
        let entry = |res, source| ScopeEntry {
            res,
            source,
            span: None,
        };
        let module = ModuleId::from_u32(3);
        let mut scope = ModuleScope::default();
        scope.values.insert(
            globals.names.intern("gone"),
            entry(Res::Module(module), ScopeSource::Withdrawn),
        );
        scope.values.insert(
            globals.names.intern("failed"),
            entry(Res::Err, ScopeSource::Import(0)),
        );
        let names = scope.import_names();
        assert_eq!(names.modules.len(), 0);
        assert_eq!(names.items.len(), 0);
    }
}
