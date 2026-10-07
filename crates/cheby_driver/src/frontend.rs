//! The build graph and the front-end phases (G1).
//!
//! 1. Module ids: the builtin module is 0, then the package's modules in
//!    discovery order (sorted by file path), then std modules in the order
//!    the worklist reaches them.
//! 2. A worklist over modules in id order collects imports and maps them to
//!    modules, loading std modules on first import (G2).
//! 3. The import graph's topological order, with ties broken by id; each
//!    cycle is E0040, and modules on or above a cycle are not checked.
//! 4. Each module in order: resolution (B), its interface (C2), then its
//!    bodies (C3). A module sees only the interfaces of its transitive
//!    dependencies (D-246). Sequential here; the structure allows running
//!    independent modules in parallel with results stored by id (D-263).
//! 5. Across the whole program, like a linker: every native symbol bound
//!    by an `@external` has one C signature (E0054). The first binding of
//!    each symbol is kept for E0053 when `cheby run` cannot resolve it.
//!
//! Every output is in a deterministic order (D-264).

use std::collections::BTreeMap;
use std::sync::Arc;

use cheby_diag::codes;
use cheby_diag::{Diagnostic, SourceMap, Span};
use cheby_iface::{
    DepView, Globals, Ice, InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleTable, PackageId,
    builtin,
};
use cheby_mir::{ForeignSig, foreign_sig};
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::ResolvedModule;
use cheby_resolve::{
    ImportRequest, ImportTarget, Lookup, ModuleLookup, ResolveInput, StdCatalog, closest_name,
    collect_imports, cycle_diagnostic, resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;
use cheby_types::{TypesInput, build_interface, check_bodies, thir};

use crate::{Error, Package, parse_file, std_lib};

/// The result of the front end over a package without phase-A errors.
pub(crate) struct Frontend {
    pub globals: Globals,
    pub modules: ModuleTable,
    pub set: InterfaceSet,
    /// Per module id: the module's checked bodies, if it was checked.
    checked: Vec<Option<Checked>>,
    /// Checked modules in build order (dependencies first).
    pub order: Vec<ModuleId>,
    /// The first binding of each native symbol, in build order.
    pub natives: BTreeMap<Box<str>, NativeBinding>,
}

/// An `@external(native, "symbol")` binding of a symbol.
#[derive(Clone, Debug)]
pub(crate) struct NativeBinding {
    pub sig: ForeignSig,
    /// The symbol's string literal.
    pub span: Span,
}

/// Records the native symbols bound by `resolved`'s functions, reporting
/// E0054 for a symbol already bound with another C signature (§12.5.1).
fn bind_natives(
    resolved: &ResolvedModule,
    iface: &cheby_iface::ModuleInterface,
    globals: &Globals,
    natives: &mut BTreeMap<Box<str>, NativeBinding>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (index, item) in resolved.items.iter_enumerated() {
        let Some((symbol, span)) = item.attrs.native_symbol() else {
            continue;
        };
        let Some(ItemSig::Fn(sig)) = iface.item(index).map(|i| &i.sig) else {
            continue;
        };
        let sig = foreign_sig(sig, globals);
        match natives.get(symbol) {
            Some(first) if first.sig != sig => diagnostics.push(
                Diagnostic::new(
                    codes::CONFLICTING_NATIVE_SYMBOL,
                    format!(
                        "the native symbol `{symbol}` is bound with two different C signatures"
                    ),
                )
                .with_primary(span, "bound here with another signature")
                .with_secondary(first.span, "first bound here")
                .with_note("every `@external` of one symbol must give it the same C signature"),
            ),
            Some(_) => {}
            None => {
                natives.insert(symbol.into(), NativeBinding { sig, span });
            }
        }
    }
}

/// One checked module.
pub(crate) struct Checked {
    /// The module's transitive dependencies, sorted: what its phases may
    /// read (D-246, D-260).
    pub deps: Vec<ModuleId>,
    pub thir: thir::Module,
}

impl Frontend {
    pub fn checked(&self, module: ModuleId) -> Option<&Checked> {
        usize::try_from(module.as_u32())
            .ok()
            .and_then(|n| self.checked.get(n))
            .and_then(Option::as_ref)
    }

    /// The local module at `path`, if the package has it.
    pub fn local_module(&self, path: &ModulePath) -> Option<ModuleId> {
        self.modules.find(path).filter(|&m| {
            self.modules
                .get(m)
                .is_some_and(|i| i.package == PackageId::LOCAL)
        })
    }

    /// The `main` of `module`, if it has one that type-checked.
    pub fn main_of(&self, module: ModuleId) -> Option<&thir::FnDef> {
        self.checked(module)?
            .thir
            .fns
            .iter()
            .find(|f| f.entry.is_some())
    }
}

/// Maps written import paths to modules, loading std modules on first use.
struct DriverLookup<'a> {
    package: &'a str,
    modules: &'a mut ModuleTable,
    sources: &'a mut SourceMap,
    trees: &'a mut Vec<Option<SyntaxTree>>,
    diagnostics: &'a mut Vec<Diagnostic>,
    /// A std module that failed to parse: a bug in std, never the user's.
    std_error: Option<String>,
}

impl DriverLookup<'_> {
    fn local_paths(&self) -> Vec<String> {
        self.modules
            .iter()
            .filter(|(_, info)| info.package == PackageId::LOCAL)
            .map(|(_, info)| info.path.to_string())
            .collect()
    }

    fn load_std(&mut self, path: &str, module_path: ModulePath) -> Result<ModuleId, Ice> {
        let source =
            std_lib::find(path).ok_or_else(|| Ice::new("loading an unknown std module"))?;
        let display = std_lib::display_path(path);
        let parsed = parse_file(
            self.sources,
            &display,
            source.source.as_bytes().to_vec(),
            self.diagnostics,
        )
        .map_err(|e| Ice::new(e.message().to_owned()))?;
        if parsed.has_syntax_errors {
            self.std_error = Some(display);
        }
        let id = self.modules.add(ModuleInfo {
            path: module_path,
            package: PackageId::STD,
            file: Some(parsed.file),
        })?;
        let index = usize::try_from(id.as_u32()).map_err(|_| Ice::new("module id overflow"))?;
        if self.trees.len() != index {
            return Err(Ice::new("module ids and trees are out of step"));
        }
        self.trees.push(parsed.syntax);
        Ok(id)
    }
}

impl ModuleLookup for DriverLookup<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("an import with an empty path"));
        };
        let module_path = rest
            .iter()
            .fold(ModulePath::root(first), |p, segment| p.child(segment));
        if first == self.package {
            if rest == ["main"] {
                return Ok(Lookup::RootByMainSegment);
            }
            if let Some(id) = self.modules.find(&module_path) {
                return Ok(Lookup::Found(id));
            }
            let text = module_path.to_string();
            let candidates = self.local_paths();
            let closest =
                closest_name(&text, candidates.iter().map(String::as_str)).map(str::to_owned);
            return Ok(Lookup::NoModule {
                package: PackageId::LOCAL,
                closest,
            });
        }
        if first != "std" {
            return Ok(Lookup::NoPackage);
        }
        if let Some(id) = self.modules.find(&module_path) {
            return Ok(Lookup::Found(id));
        }
        let under_std = rest.join("::");
        if std_lib::find(&under_std).is_some() {
            return self.load_std(&under_std, module_path).map(Lookup::Found);
        }
        if std_lib::is_planned(&under_std) {
            return Ok(Lookup::Planned);
        }
        let closest = closest_name(&under_std, std_lib::known()).map(|p| format!("std::{p}"));
        Ok(Lookup::NoModule {
            package: PackageId::STD,
            closest,
        })
    }
}

fn index(module: ModuleId) -> Result<usize, Ice> {
    usize::try_from(module.as_u32()).map_err(|_| Ice::new("module id overflow"))
}

fn get<T>(v: &[T], module: ModuleId) -> Result<&T, Ice> {
    v.get(index(module)?)
        .ok_or_else(|| Ice::new(format!("no data for module {module:?}")))
}

/// Runs name resolution and type checking over `package`, which has no
/// phase-A errors. Diagnostics are pushed unsorted.
pub(crate) fn run(
    package: Package,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Frontend, Error> {
    run_inner(package, sources, diagnostics).map_err(Error::from)
}

#[expect(
    clippy::too_many_lines,
    reason = "the front-end phases in order, each a few lines"
)]
fn run_inner(
    package: Package,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Frontend, Ice> {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut trees: Vec<Option<SyntaxTree>> = vec![None];
    let package_name = package.manifest.name.clone();
    let local_targets = package.manifest.targets;
    for module in package.modules {
        modules.add(ModuleInfo {
            path: module.path,
            package: PackageId::LOCAL,
            file: Some(module.source.file),
        })?;
        trees.push(module.source.syntax);
    }

    // Imports, module by module in id order; std modules join the table
    // and the worklist as they are reached (B3).
    let mut imports: Vec<Vec<ImportRequest>> = vec![Vec::new()];
    let mut targets: Vec<Vec<ImportTarget>> = vec![Vec::new()];
    let mut implicit: Vec<Vec<ModuleId>> = vec![Vec::new()];
    let mut next = 1_usize;
    let mut std_error = None;
    while next < modules.len() {
        let id = ModuleId::from_u32(u32::try_from(next).map_err(|_| Ice::new("too many modules"))?);
        let info = modules.try_get(id)?.clone();
        let tree = trees
            .get(next)
            .and_then(Option::as_ref)
            .ok_or_else(|| Ice::new(format!("module `{}` has no syntax tree", info.path)))?;
        let requests = collect_imports(tree, &globals.names);
        let mut lookup = DriverLookup {
            package: &package_name,
            modules: &mut modules,
            sources,
            trees: &mut trees,
            diagnostics,
            std_error: None,
        };
        let (found, diags) =
            resolve_import_targets(&requests, info.package, &mut lookup, &globals.names)?;
        // A package module depends on every shipped home module of a
        // prelude type (D-447).
        let mut homes = Vec::new();
        if info.package == PackageId::LOCAL {
            for (_, segment) in builtin::HOMES {
                if std_lib::find(segment).is_some()
                    && let Lookup::Found(home) =
                        lookup.lookup(PackageId::LOCAL, &["std", segment])?
                    && !homes.contains(&home)
                {
                    homes.push(home);
                }
            }
        }
        if let Some(path) = lookup.std_error.take() {
            std_error.get_or_insert(path);
        }
        diagnostics.extend(diags);
        imports.push(requests);
        targets.push(found);
        implicit.push(homes);
        next = next.saturating_add(1);
    }
    if let Some(path) = std_error {
        return Err(Ice::new(format!(
            "the standard library file `{path}` has syntax errors"
        )));
    }

    // The graph and the order (G1).
    let mut graph = Graph::new(modules.len());
    for (from, list) in targets.iter().enumerate() {
        let from = u32::try_from(from).map_err(|_| Ice::new("too many modules"))?;
        for to in list.iter().filter_map(|t| t.module()) {
            graph.add_edge(from, to.as_u32());
        }
    }
    for (from, list) in implicit.iter().enumerate() {
        let from = u32::try_from(from).map_err(|_| Ice::new("too many modules"))?;
        for to in list {
            graph.add_edge(from, to.as_u32());
        }
    }
    let order = match topological_order(&graph) {
        Ok(order) => order,
        Err(cycles) => {
            let mut on_cycle: Vec<u32> = Vec::new();
            for cycle in &cycles {
                let mut with_spans = Vec::with_capacity(cycle.nodes.len());
                for (n, &node) in cycle.nodes.iter().enumerate() {
                    let from = ModuleId::from_u32(node);
                    let to = cycle
                        .nodes
                        .get(n.saturating_add(1))
                        .or_else(|| cycle.nodes.first())
                        .copied()
                        .ok_or_else(|| Ice::new("an empty cycle"))?;
                    let k = get(&targets, from)?
                        .iter()
                        .position(|t| t.module() == Some(ModuleId::from_u32(to)))
                        .ok_or_else(|| {
                            Ice::new("an import cycle through an implicit std dependency")
                        })?;
                    let span: Span = get(&imports, from)?
                        .get(k)
                        .ok_or_else(|| Ice::new("an import target without its request"))?
                        .span;
                    with_spans.push((from, span));
                }
                diagnostics.push(cycle_diagnostic(&with_spans, &modules)?);
                on_cycle.extend(&cycle.nodes);
            }
            // Modules on a cycle, and modules that depend on one, are not
            // checked; the others are.
            let blocked: Vec<bool> = (0..graph.len())
                .map(|n| {
                    let node = u32::try_from(n).unwrap_or(u32::MAX);
                    reachable(&graph, &[node])
                        .iter()
                        .any(|m| on_cycle.contains(m))
                })
                .collect();
            let mut acyclic = Graph::new(graph.len());
            for (n, &is_blocked) in blocked.iter().enumerate() {
                let node = u32::try_from(n).map_err(|_| Ice::new("too many modules"))?;
                if !is_blocked {
                    for &to in graph.successors(node) {
                        acyclic.add_edge(node, to);
                    }
                }
            }
            topological_order(&acyclic)
                .map_err(|_| Ice::new("cycles remain after removing them"))?
                .into_iter()
                .filter(|&n| {
                    usize::try_from(n)
                        .ok()
                        .and_then(|n| blocked.get(n))
                        .is_some_and(|b| !b)
                })
                .collect()
        }
    };

    // Resolution, interfaces and bodies, in order (B4-B7, C).
    let known = std_lib::known();
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut checked: Vec<Option<Checked>> = (0..modules.len()).map(|_| None).collect();
    let mut built = Vec::new();
    let mut natives = BTreeMap::new();
    for node in order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let deps: Vec<ModuleId> = reachable(&graph, graph.successors(node))
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        let info = modules.try_get(id)?.clone();
        let tree = get(&trees, id)?
            .as_ref()
            .ok_or_else(|| Ice::new(format!("module `{}` has no syntax tree", info.path)))?;
        let input = ResolveInput {
            module: id,
            package: info.package,
            tree,
            imports: get(&imports, id)?,
            targets: get(&targets, id)?,
            deps: DepView::new(&set, &deps),
            modules: &modules,
            globals: &globals,
            std: StdCatalog {
                known: &known,
                partial: std_lib::PARTIAL_STD_MODULES,
            },
            package_targets: if info.package == PackageId::LOCAL {
                local_targets
            } else {
                Targets::ALL
            },
        };
        let (resolved, diags) = resolve_module(&input)?;
        diagnostics.extend(diags);
        let types = TypesInput {
            module: id,
            package: info.package,
            tree,
            resolved: &resolved,
            deps: DepView::new(&set, &deps),
            modules: &modules,
            globals: &globals,
        };
        let (iface, diags) = build_interface(&types)?;
        diagnostics.extend(diags);
        bind_natives(&resolved, &iface, &globals, &mut natives, diagnostics);
        let (thir, diags) = check_bodies(&types, &iface)?;
        diagnostics.extend(diags);
        set.insert(Arc::new(iface))?;
        if let Some(slot) = checked.get_mut(index(id)?) {
            *slot = Some(Checked { deps, thir });
        }
        built.push(id);
    }
    Ok(Frontend {
        globals,
        modules,
        set,
        checked,
        order: built,
        natives,
    })
}
