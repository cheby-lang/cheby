//! The module scope (B5, B6, §7.6, §7.7): the module's own items, its
//! imports, and the prelude, per namespace.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, ModuleId, Name, Namespace, builtin};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::paths::PathCx;
use crate::{
    ImportItemRequest, ImportRequest, ImportTarget, ItemDef, ItemStatus, Res, ResolveInput,
};

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
/// items and its imports never collide (E0041); both hide the prelude.
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
        self.map(ns).get(&name)
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
    // Own items. Duplicates were reported by item collection; the first
    // declaration wins.
    for (index, item) in items.iter_enumerated() {
        let Some(ns) = item.kind.namespace() else {
            continue;
        };
        let res = match item.status {
            // A body-only hit leaves the signature usable (D-246).
            ItemStatus::Ok | ItemStatus::BodyUnsupported => {
                Res::Def(DefId::new(input.module, index))
            }
            ItemStatus::Unsupported => Res::Err,
        };
        scope.map_mut(ns).entry(item.name).or_insert(ScopeEntry {
            res,
            source: ScopeSource::Item,
            span: Some(item.name_span),
        });
    }
    // Module imports bind a name in the value namespace (§7.7); item
    // imports bind the listed items (§7.5.2).
    let lookups = PathCx {
        input,
        items,
        scope: &ModuleScope::default(),
        gated: std::cell::Cell::new(false),
    };
    for (n, (request, target)) in input.imports.iter().zip(input.targets).enumerate() {
        if let Some(list) = &request.items {
            for item in list {
                for (ns, name, span, res) in item_import(&lookups, request, item, *target, diags)? {
                    insert_import(&mut scope, input, (ns, name, span, res), n, diags);
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
            n,
            diags,
        );
    }
    add_prelude(input, &mut scope)?;
    Ok(scope)
}

/// The bindings of one item of `import m::{x, Y as Z}`: a `LOWER` name
/// is a function or constant, an `UPPER` name the type or interface and
/// the constructor with that name (D-111).
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
    let Some(module) = target.module() else {
        return Ok(namespaces
            .iter()
            .map(|&ns| (ns, bound, bound_span, Res::Err))
            .collect());
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
    let homed = !builtin::homed_at(path).is_empty();
    let present: Vec<Namespace> = namespaces
        .iter()
        .copied()
        .filter(|&ns| {
            iface.lookup(ns, item.name).is_some()
                || (homed && builtin_iface.lookup(ns, item.name).is_some())
        })
        .collect();
    if present.is_empty() {
        // Reports E0042, or E0038 for a partly shipped std module.
        let ns = namespaces.first().copied().unwrap_or(Namespace::Value);
        lookups.in_module(module, ns, (item.name, item.span), request.path_span, diags)?;
        return Ok(namespaces
            .iter()
            .map(|&ns| (ns, bound, bound_span, Res::Err))
            .collect());
    }
    let mut out = Vec::new();
    for ns in present {
        let res =
            lookups.in_module(module, ns, (item.name, item.span), request.path_span, diags)?;
        out.push((ns, bound, bound_span, res));
    }
    Ok(out)
}

/// Adds an import's binding, reporting E0041 if the name is taken.
fn insert_import(
    scope: &mut ModuleScope,
    input: &ResolveInput<'_>,
    (ns, name, span, res): (Namespace, Name, Span, Res),
    n: usize,
    diags: &mut Vec<Diagnostic>,
) {
    let entry = ScopeEntry {
        res,
        source: ScopeSource::Import(n),
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
            diags.push(diag.with_help("give the import another name with `as`"));
            scope.conflicts.insert(name);
        }
        None => {
            scope.map_mut(ns).insert(name, entry);
        }
    }
}

fn add_prelude(input: &ResolveInput<'_>, scope: &mut ModuleScope) -> Result<(), Ice> {
    // The prelude and, in a home module, its re-exports. Both are hidden
    // by own items and imports (D-146: the hiding warning lands later).
    let builtin_iface = input
        .deps
        .get(ModuleId::BUILTIN)
        .ok_or_else(|| Ice::new("the builtin interface is not readable"))?;
    let path = &input.modules.try_get(input.module)?.path;
    let homed = builtin::homed_at(path);
    for (index, item) in builtin_iface.items.iter_enumerated() {
        let Some(ns) = item.kind.namespace() else {
            continue;
        };
        let def = DefId::new(ModuleId::BUILTIN, index);
        let owner = if item.kind == cheby_iface::DefKind::Ctor {
            item.parent.map(|p| DefId::new(ModuleId::BUILTIN, p))
        } else {
            Some(def)
        };
        let source = if owner.is_some_and(|owner| homed.contains(&owner)) {
            ScopeSource::HomeReexport
        } else {
            ScopeSource::Prelude
        };
        scope.map_mut(ns).entry(item.name).or_insert(ScopeEntry {
            res: Res::Def(def),
            source,
            span: None,
        });
    }
    Ok(())
}
