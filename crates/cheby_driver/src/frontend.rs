//! The build graph and the front-end phases (G1).
//!
//! 1. Module ids: the builtin module is 0, then the package's modules in
//!    discovery order (sorted by file path), then std modules in the order
//!    the worklist reaches them.
//! 2. A worklist over modules in id order collects imports and maps them to
//!    modules, loading std modules on first import (G2).
//! 3. The import graph's topological order, with ties broken by id; each
//!    cycle is E0040, and modules on or above a cycle are not checked.
//! 4. Each module: resolution (B), its interface (C2), then its bodies
//!    (C3). A module sees only the interfaces of its transitive
//!    dependencies (D-246). Modules run as tasks on rayon's global pool
//!    (D-263, whose size `-j` sets): a module starts once every module it
//!    imports has published its interface, so independent modules run in
//!    parallel, and a module's bodies check in parallel too
//!    (`cheby_types::check_bodies_with`). Each task stores its results by
//!    module id; they are merged afterwards in the topological order, so
//!    nothing depends on the schedule.
//! 5. Across the whole program, like a linker: every native symbol bound
//!    by an `@external` has one C signature (E0054). The first binding of
//!    each symbol in the topological order is kept for E0053 when
//!    `cheby run` cannot resolve it.
//! 6. For `cheby check` only, the whole-package warnings over every
//!    checked module of the package (D-152, D-266), once, after every
//!    module is checked, and only when every module of the package was
//!    checked (an import cycle leaves some unchecked). `cheby run` skips
//!    them so the edit-run path stays local (D-516).
//!    The caller sorts every diagnostic, warnings included, by position.
//!
//! Every output is in a deterministic order (D-264).
//!
//! With `CHEBY_DUMP_DIR` set, the canonical encoding of every checked
//! module's interface is written to `<dir>/iface/<module path>.iface`
//! (segments joined with `.`), for `cargo xtask check-determinism`
//! (D-551, plan §4.5).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use cheby_diag::codes;
use cheby_diag::{Diagnostic, FileId, SourceMap, Span};
use cheby_iface::{
    DepView, Globals, Ice, InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface,
    ModuleTable, PackageId, builtin, encode,
};
use cheby_mir::ForeignSig;
use cheby_mir::repr::foreign_sig_with_context;
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::ResolvedModule;
use cheby_resolve::{
    ImportRequest, ImportTarget, Lookup, ModuleLookup, ResolveInput, StdCatalog, closest_name,
    collect_imports, cycle_diagnostic, resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;
use cheby_types::{
    PackageModule, Tests, TypesInput, build_interface, check_bodies_with, package_warnings, thir,
};

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
    /// The local package's `targets` (§13.3), which narrow the targets of
    /// every local item (D-190); std items exist on every target.
    pub package_targets: Targets,
    /// The local package's name: the first segment of its module paths.
    pub package_name: String,
    /// The local package's `[const-eval] time-limit` (D-536).
    pub const_eval_time_limit: std::time::Duration,
    /// The local package's `[const-eval] memory-limit` in bytes (D-536).
    pub const_eval_memory_limit: u64,
    /// The local package's `[runtime] fiber-stack` in bytes (D-660): the
    /// default stack of the fibers its runs and tests spawn.
    pub fiber_stack: u64,
    /// The local package's `[runtime] main-stack` in bytes (D-660): the
    /// stack of `main`'s root fiber and of each test.
    pub main_stack: u64,
    /// Per module id: its imports, as written. For the REPL's `:module`,
    /// which puts a module's imports in scope (§13.5.3).
    pub imports: Vec<Vec<ImportRequest>>,
    /// Per module id: the targets of its imports, in the same order.
    pub import_targets: Vec<Vec<ImportTarget>>,
    /// Per module id: the native symbols its functions bind, in item order,
    /// so the REPL's `:reload` can recompute [`Frontend::natives`].
    pub module_natives: Vec<Vec<(Box<str>, NativeBinding)>>,
}

/// An `@external(native, "symbol")` binding of a symbol.
#[derive(Clone, Debug)]
pub(crate) struct NativeBinding {
    pub sig: ForeignSig,
    /// The symbol's string literal.
    pub span: Span,
}

/// The native symbols bound by `resolved`'s functions, in item order.
pub(crate) fn native_bindings(
    resolved: &ResolvedModule,
    iface: &ModuleInterface,
    globals: &Globals,
) -> Vec<(Box<str>, NativeBinding)> {
    let mut out = Vec::new();
    for (index, item) in resolved.items.iter_enumerated() {
        let Some((symbol, span)) = item.attrs.native_symbol() else {
            continue;
        };
        let Some(ItemSig::Fn(sig)) = iface.item(index).map(|i| &i.sig) else {
            continue;
        };
        let sig = foreign_sig_with_context(sig, globals, item.attrs.std_context);
        out.push((symbol.into(), NativeBinding { sig, span }));
    }
    out
}

/// Records one module's native bindings (from [`native_bindings`]),
/// reporting E0054 for a symbol already bound with another C signature
/// (§12.5.1). Modules are merged in the topological order, so the first
/// binding kept is the same on every schedule.
pub(crate) fn bind_natives(
    bindings: Vec<(Box<str>, NativeBinding)>,
    natives: &mut BTreeMap<Box<str>, NativeBinding>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (symbol, NativeBinding { sig, span }) in bindings {
        match natives.get(&symbol) {
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
                natives.insert(symbol, NativeBinding { sig, span });
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
    /// The module's interface (also in [`Frontend::set`] for the package's
    /// modules).
    pub interface: Arc<ModuleInterface>,
    /// The warnings of a module checked by [`check_extra_module`]; empty
    /// for the build's modules, whose diagnostics go to the build's list.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "plan §4.7's REPL-entry contract; the REPL checks entries through cheby_types::repl, so only tests call it"
        )
    )]
    pub warnings: Vec<Diagnostic>,
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

    /// Drops the results [`check_reloaded`] added for `modules`, the
    /// versions of a `:reload` that was rejected: they leave the build
    /// order, so nothing compiles, imports or binds natives through them
    /// (D-625). Their ids and published interfaces stay, unused.
    pub fn forget_reloaded(&mut self, modules: &[ModuleId]) {
        self.order.retain(|m| !modules.contains(m));
        for &module in modules {
            let Ok(at) = usize::try_from(module.as_u32()) else {
                continue;
            };
            if let Some(slot) = self.checked.get_mut(at) {
                *slot = None;
            }
            if let Some(slot) = self.imports.get_mut(at) {
                slot.clear();
            }
            if let Some(slot) = self.import_targets.get_mut(at) {
                slot.clear();
            }
            if let Some(slot) = self.module_natives.get_mut(at) {
                slot.clear();
            }
        }
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

/// Which command the front end runs for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `cheby check`: every check, the whole-package warnings included.
    Check,
    /// Building to run or dump: the whole-package warnings are skipped
    /// (D-266, D-516).
    Build,
    /// Testing the standard library itself (D-479, D-542): like `Build`,
    /// and the `test` blocks of std modules are checked and kept in THIR.
    StdTests,
}

/// A module checked on top of a finished [`Frontend`] that is not part of
/// the package: a REPL entry (plan §4.7).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "plan §4.7's REPL-entry contract; the REPL checks entries through cheby_types::repl, so only tests call it"
    )
)]
pub(crate) struct ExtraModule<'a> {
    /// The module's path, as diagnostics and symbols name it.
    pub path: ModulePath,
    /// The module's source, registered in the caller's `SourceMap`.
    pub source: FileId,
    /// The modules of `front` the extra module may read, sorted: its
    /// transitive dependencies (D-246).
    pub deps: Vec<ModuleId>,
    /// Interfaces outside `front` it may also read (earlier REPL entries).
    pub extra_interfaces: Vec<&'a ModuleInterface>,
}

/// Runs one pipeline step (parsing, resolution, interface, bodies) for
/// `module` on top of `front`, as a module of the local package.
/// `Ok(Err(..))` holds every diagnostic of the module when one of them is
/// an error; `Ok(Ok(..))` its checked bodies, its interface and its
/// warnings ([`Checked::warnings`]).
///
/// - Module ids: an interface of `extra_interfaces` keeps its own module
///   id, which must be past `front`'s modules; ids that `front` and the
///   extra interfaces leave free between them are filled with placeholder
///   modules named `<extra:N>`. The module gets the next id after all of
///   them (`thir.module` of the result).
/// - What it may read (D-246): `deps`, every module it imports with its
///   transitive dependencies, the shipped home modules of prelude types
///   (D-447) that `front` loaded, and the extra interfaces.
/// - Imports resolve against `front`'s modules only: `front` must already
///   hold every std module the source imports (a std module it lacks is an
///   [`Ice`]).
/// - E0054 compares the module's native bindings with `front`'s.
///
/// # Errors
///
/// An [`Ice`] if `module.source` is not in `sources`, an id is out of
/// place, or the phases disagree.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "plan §4.7's REPL-entry contract; the REPL checks entries through cheby_types::repl, so only tests call it"
    )
)]
pub(crate) fn check_extra_module(
    front: &Frontend,
    sources: &mut SourceMap,
    module: ExtraModule<'_>,
) -> Result<Result<Checked, Vec<Diagnostic>>, Ice> {
    let text = sources
        .file(module.source)
        .ok_or_else(|| Ice::new("an extra module's source is not in the source map"))?
        .text_arc();
    let (tree, mut diags) = cheby_syntax::parse(module.source, text).into_parts();
    if diags.iter().any(Diagnostic::is_error) {
        diags.extend(cheby_syntax::validate_tokens(&tree));
        return Ok(Err(diags));
    }
    diags.extend(cheby_syntax::validate(&tree));
    if diags.iter().any(Diagnostic::is_error) {
        return Ok(Err(diags));
    }

    let (table, set, id) = extra_table(front, &module)?;

    // Imports, against `front`'s modules.
    let requests = collect_imports(&tree, &front.globals.names);
    let mut lookup = ExtraLookup {
        package: &front.package_name,
        modules: &table,
    };
    let (found, import_diags) = resolve_import_targets(
        &requests,
        PackageId::LOCAL,
        &mut lookup,
        &front.globals.names,
    )?;
    diags.extend(import_diags);
    let deps = extra_deps(front, module, &table, &found, id);

    let known = std_lib::known();
    let input = ResolveInput {
        module: id,
        package: PackageId::LOCAL,
        tree: &tree,
        imports: &requests,
        targets: &found,
        deps: DepView::new(&set, &deps),
        modules: &table,
        globals: &front.globals,
        std: StdCatalog { known: &known },
        package_targets: front.package_targets,
    };
    let (resolved, resolve_diags) = resolve_module(&input)?;
    diags.extend(resolve_diags);
    let types = TypesInput {
        module: id,
        package: PackageId::LOCAL,
        tree: &tree,
        resolved: &resolved,
        deps: DepView::new(&set, &deps),
        modules: &table,
        globals: &front.globals,
    };
    let (iface, iface_diags) = build_interface(&types)?;
    diags.extend(iface_diags);
    let mut natives = front.natives.clone();
    bind_natives(
        native_bindings(&resolved, &iface, &front.globals),
        &mut natives,
        &mut diags,
    );
    let (thir, body_diags) = check_bodies_with(&types, &iface, Tests::SkipStd)?;
    diags.extend(body_diags);
    if diags.iter().any(Diagnostic::is_error) {
        return Ok(Err(diags));
    }
    Ok(Ok(Checked {
        deps,
        thir,
        interface: Arc::new(iface),
        warnings: diags,
    }))
}

/// A new version of a package module that the REPL's `:reload` checks on
/// top of a finished [`Frontend`] (§13.5.3).
pub(crate) struct ReloadedModule<'a> {
    /// Its id, already in `front.modules`, after every module it imports.
    pub id: ModuleId,
    pub tree: &'a SyntaxTree,
    pub imports: Vec<ImportRequest>,
    /// The targets of `imports`, the new versions of reloaded modules
    /// included.
    pub targets: Vec<ImportTarget>,
}

/// Resolves and checks a reloaded module (§13.5.3, D-625), as
/// the build checks a package module, publishes its interface, and adds
/// its results to `front` (`checked`, `order`, its imports and native
/// bindings); [`Frontend::natives`] does not change. `display` is the
/// table diagnostics name modules with. Returns its diagnostics, unsorted;
/// when one is an error, the caller must not use the module.
///
/// # Errors
///
/// An [`Ice`] if the module's id is not the next free slot of `front`'s
/// results or a phase fails.
pub(crate) fn check_reloaded(
    front: &mut Frontend,
    display: &ModuleTable,
    module: ReloadedModule<'_>,
) -> Result<Vec<Diagnostic>, Ice> {
    let id = module.id;
    // What it may read: its imports and the shipped home modules of
    // prelude types (D-447), with their transitive dependencies (D-246).
    let mut direct: Vec<ModuleId> = module.targets.iter().filter_map(|t| t.module()).collect();
    for (_, segment) in builtin::HOMES {
        if std_lib::find(segment).is_some()
            && let Some(home) = front.modules.find(&ModulePath::root("std").child(segment))
        {
            direct.push(home);
        }
    }
    let mut deps = Vec::new();
    for dep in direct {
        deps.push(dep);
        if let Some(checked) = front.checked(dep) {
            deps.extend(checked.deps.iter().copied());
        }
    }
    deps.sort_unstable();
    deps.dedup();
    deps.retain(|&m| m != id && m != ModuleId::BUILTIN);
    front.set.reserve_modules(front.modules.len())?;
    let known = std_lib::known();
    let input = ResolveInput {
        module: id,
        package: PackageId::LOCAL,
        tree: module.tree,
        imports: &module.imports,
        targets: &module.targets,
        deps: DepView::new(&front.set, &deps),
        modules: display,
        globals: &front.globals,
        std: StdCatalog { known: &known },
        package_targets: front.package_targets,
    };
    let (resolved, mut diags) = resolve_module(&input)?;
    let types = TypesInput {
        module: id,
        package: PackageId::LOCAL,
        tree: module.tree,
        resolved: &resolved,
        deps: DepView::new(&front.set, &deps),
        modules: display,
        globals: &front.globals,
    };
    let (iface, found) = build_interface(&types)?;
    diags.extend(found);
    let natives = native_bindings(&resolved, &iface, &front.globals);
    let iface = Arc::new(iface);
    front.set.publish(Arc::clone(&iface))?;
    let (thir, found) = check_bodies_with(&types, &iface, Tests::SkipStd)?;
    diags.extend(found);
    let at = index(id)?;
    while front.checked.len() <= at {
        front.checked.push(None);
        front.imports.push(Vec::new());
        front.import_targets.push(Vec::new());
        front.module_natives.push(Vec::new());
    }
    let ok = !diags.iter().any(Diagnostic::is_error);
    if ok {
        if let Some(slot) = front.checked.get_mut(at) {
            *slot = Some(Checked {
                deps,
                thir,
                interface: iface,
                warnings: Vec::new(),
            });
        }
        if let Some(slot) = front.imports.get_mut(at) {
            *slot = module.imports;
        }
        if let Some(slot) = front.import_targets.get_mut(at) {
            *slot = module.targets;
        }
        if let Some(slot) = front.module_natives.get_mut(at) {
            *slot = natives;
        }
        front.order.push(id);
    }
    Ok(diags)
}

/// The module table and the interfaces an extra module sees, and its id
/// (see [`check_extra_module`]).
fn extra_table(
    front: &Frontend,
    module: &ExtraModule<'_>,
) -> Result<(ModuleTable, InterfaceSet, ModuleId), Ice> {
    let mut table = front.modules.clone();
    let mut set = front.set.clone();
    let mut extra: Vec<&ModuleInterface> = module.extra_interfaces.clone();
    extra.sort_by_key(|iface| iface.module);
    for iface in &extra {
        let id = index(iface.module)?;
        if id < table.len() {
            return Err(Ice::new(format!(
                "the extra interface of {:?} reuses a module id",
                iface.module
            )));
        }
        while table.len() <= id {
            let n = table.len();
            table.add(ModuleInfo {
                path: ModulePath::root(&format!("<extra:{n}>")),
                package: PackageId::LOCAL,
                file: None,
            })?;
        }
    }
    let id = table.add(ModuleInfo {
        path: module.path.clone(),
        package: PackageId::LOCAL,
        file: Some(module.source),
    })?;
    set.reserve_modules(table.len())?;
    for iface in extra {
        set.publish(Arc::new(iface.clone()))?;
    }
    Ok((table, set, id))
}

/// What an extra module may read: the caller's `deps`, the extra
/// interfaces, and its imports and the loaded home modules (D-447) with
/// their transitive dependencies; sorted.
fn extra_deps(
    front: &Frontend,
    module: ExtraModule<'_>,
    table: &ModuleTable,
    found: &[ImportTarget],
    id: ModuleId,
) -> Vec<ModuleId> {
    let mut deps: Vec<ModuleId> = module.deps;
    deps.extend(module.extra_interfaces.iter().map(|iface| iface.module));
    let mut direct: Vec<ModuleId> = found.iter().filter_map(|t| t.module()).collect();
    for (_, segment) in builtin::HOMES {
        if let Some(home) = table.find(&ModulePath::root("std").child(segment)) {
            direct.push(home);
        }
    }
    for dep in direct {
        deps.push(dep);
        if let Some(checked) = front.checked(dep) {
            deps.extend(checked.deps.iter().copied());
        }
    }
    deps.sort_unstable();
    deps.dedup();
    deps.retain(|&m| m != id && m != ModuleId::BUILTIN);
    deps
}

/// Maps an extra module's imports to the modules `front` already has.
struct ExtraLookup<'a> {
    package: &'a str,
    modules: &'a ModuleTable,
}

impl ModuleLookup for ExtraLookup<'_> {
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
            let candidates: Vec<String> = self
                .modules
                .iter()
                .filter(|(_, info)| info.package == PackageId::LOCAL && info.file.is_some())
                .map(|(_, info)| info.path.to_string())
                .collect();
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
            return Err(Ice::new(format!(
                "`{module_path}` is not loaded in the build this module extends"
            )));
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

/// The inputs every module's pipeline step reads, shared by the tasks.
struct BuildInputs<'a> {
    mode: Mode,
    globals: &'a Globals,
    modules: &'a ModuleTable,
    trees: &'a [Option<SyntaxTree>],
    imports: &'a [Vec<ImportRequest>],
    targets: &'a [Vec<ImportTarget>],
    graph: &'a Graph,
    set: &'a InterfaceSet,
    known: &'a [&'a str],
    local_targets: Targets,
}

/// What one module's pipeline step produced.
struct ModuleOutput {
    /// The diagnostics of resolution and of the interface.
    early: Vec<Diagnostic>,
    natives: Vec<(Box<str>, NativeBinding)>,
    /// The diagnostics of the bodies.
    late: Vec<Diagnostic>,
    checked: Checked,
    /// Kept for the whole-package warnings (`cheby check`, local modules).
    resolved: Option<ResolvedModule>,
}

impl BuildInputs<'_> {
    /// Resolves module `id`, builds and publishes its interface, calls
    /// `published` (which lets its dependents start), then checks its
    /// bodies.
    fn check_module(&self, id: ModuleId, published: impl FnOnce()) -> Result<ModuleOutput, Ice> {
        let deps: Vec<ModuleId> = reachable(self.graph, self.graph.successors(id.as_u32()))
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        let info = self.modules.try_get(id)?;
        let tree = get(self.trees, id)?
            .as_ref()
            .ok_or_else(|| Ice::new(format!("module `{}` has no syntax tree", info.path)))?;
        let input = ResolveInput {
            module: id,
            package: info.package,
            tree,
            imports: get(self.imports, id)?,
            targets: get(self.targets, id)?,
            deps: DepView::new(self.set, &deps),
            modules: self.modules,
            globals: self.globals,
            std: StdCatalog { known: self.known },
            package_targets: if info.package == PackageId::LOCAL {
                self.local_targets
            } else {
                Targets::ALL
            },
        };
        let (resolved, mut early) = resolve_module(&input)?;
        let types = TypesInput {
            module: id,
            package: info.package,
            tree,
            resolved: &resolved,
            deps: DepView::new(self.set, &deps),
            modules: self.modules,
            globals: self.globals,
        };
        let (iface, diags) = build_interface(&types)?;
        early.extend(diags);
        let natives = native_bindings(&resolved, &iface, self.globals);
        let iface = Arc::new(iface);
        self.set.publish(Arc::clone(&iface))?;
        published();
        let tests = if self.mode == Mode::StdTests {
            Tests::All
        } else {
            Tests::SkipStd
        };
        let (thir, late) = check_bodies_with(&types, &iface, tests)?;
        let resolved =
            (self.mode == Mode::Check && info.package == PackageId::LOCAL).then_some(resolved);
        Ok(ModuleOutput {
            early,
            natives,
            late,
            checked: Checked {
                deps,
                thir,
                interface: iface,
                warnings: Vec::new(),
            },
            resolved,
        })
    }
}

/// The modules of a build as tasks over the import graph.
struct Schedule<'a> {
    inputs: BuildInputs<'a>,
    /// Per module id: how many modules it imports have not published
    /// their interfaces yet.
    waiting: Vec<AtomicUsize>,
    /// Per module id: the scheduled modules that import it.
    dependents: Vec<Vec<ModuleId>>,
    /// Per module id: what its task produced.
    outputs: Vec<OnceLock<Result<ModuleOutput, Ice>>>,
}

impl Schedule<'_> {
    /// Runs every module of `order` (a topological order without the
    /// modules left out by a cycle) on rayon's global pool and returns the
    /// outputs by module id. A module whose dependency failed with an
    /// [`Ice`] never runs; its slot stays empty.
    fn run(inputs: BuildInputs<'_>, order: &[u32]) -> Vec<Option<Result<ModuleOutput, Ice>>> {
        let len = inputs.modules.len();
        let mut scheduled = vec![false; len];
        for &node in order {
            if let Some(slot) = usize::try_from(node)
                .ok()
                .and_then(|n| scheduled.get_mut(n))
            {
                *slot = node != ModuleId::BUILTIN.as_u32();
            }
        }
        let mut waiting: Vec<usize> = vec![0; len];
        let mut dependents: Vec<Vec<ModuleId>> = vec![Vec::new(); len];
        for &node in order {
            if node == ModuleId::BUILTIN.as_u32() {
                continue;
            }
            for &dep in inputs.graph.successors(node) {
                let is_scheduled = usize::try_from(dep)
                    .ok()
                    .and_then(|d| scheduled.get(d))
                    .copied()
                    .unwrap_or(false);
                if !is_scheduled {
                    continue;
                }
                if let Some(count) = usize::try_from(node).ok().and_then(|n| waiting.get_mut(n)) {
                    *count = count.saturating_add(1);
                }
                if let Some(list) = usize::try_from(dep)
                    .ok()
                    .and_then(|d| dependents.get_mut(d))
                {
                    list.push(ModuleId::from_u32(node));
                }
            }
        }
        let ready: Vec<ModuleId> = order
            .iter()
            .filter(|&&node| node != ModuleId::BUILTIN.as_u32())
            .filter(|&&node| {
                usize::try_from(node)
                    .ok()
                    .and_then(|n| waiting.get(n))
                    .is_some_and(|&w| w == 0)
            })
            .map(|&node| ModuleId::from_u32(node))
            .collect();
        let schedule = Schedule {
            inputs,
            waiting: waiting.into_iter().map(AtomicUsize::new).collect(),
            dependents,
            outputs: (0..len).map(|_| OnceLock::new()).collect(),
        };
        rayon::scope(|scope| {
            for &module in &ready {
                schedule.spawn(scope, module);
            }
        });
        schedule
            .outputs
            .into_iter()
            .map(OnceLock::into_inner)
            .collect()
    }

    fn spawn<'s>(&'s self, scope: &rayon::Scope<'s>, module: ModuleId) {
        scope.spawn(move |scope| {
            let output = self.inputs.check_module(module, || {
                let dependents = usize::try_from(module.as_u32())
                    .ok()
                    .and_then(|m| self.dependents.get(m))
                    .map_or(&[][..], Vec::as_slice);
                for &dependent in dependents {
                    let Some(waiting) = usize::try_from(dependent.as_u32())
                        .ok()
                        .and_then(|d| self.waiting.get(d))
                    else {
                        continue;
                    };
                    // The last interface it waited for: it may start. The
                    // release-acquire pair orders every dependency's
                    // `publish` before the dependent's reads.
                    if waiting.fetch_sub(1, Ordering::AcqRel) == 1 {
                        self.spawn(scope, dependent);
                    }
                }
            });
            if let Some(slot) = usize::try_from(module.as_u32())
                .ok()
                .and_then(|m| self.outputs.get(m))
            {
                // Each module runs once, so its slot is empty here.
                let _ = slot.set(output);
            }
        });
    }
}

/// Writes the canonical encoding of every checked module's interface to
/// `<dir>/iface/<module path>.iface` when `CHEBY_DUMP_DIR` is set (D-551).
fn dump_interfaces(front: &Frontend) -> Result<(), Error> {
    let Some(dir) = std::env::var_os("CHEBY_DUMP_DIR") else {
        return Ok(());
    };
    let dir = Path::new(&dir).join("iface");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::new(format!("cannot create {}: {e}", dir.display())))?;
    for &id in &front.order {
        let Some(checked) = front.checked(id) else {
            continue;
        };
        let info = front.modules.try_get(id)?;
        let bytes = encode(
            &checked.interface,
            &DepView::new(&front.set, &checked.deps),
            &front.modules,
            &front.globals,
        )?;
        let name = format!("{}.iface", info.path.segments().join("."));
        let file = dir.join(name);
        std::fs::write(&file, bytes)
            .map_err(|e| Error::new(format!("cannot write {}: {e}", file.display())))?;
    }
    Ok(())
}

/// Reads the package's module files and parses and validates them in
/// parallel on rayon's global pool (D-263, I2), each exactly as
/// `crate::parse_file` does. Files enter `sources` in the order of
/// `files`, and each file's diagnostics are pushed in that order, so file
/// ids and output never depend on the schedule. For `load_package`.
///
/// # Errors
///
/// An [`Error`] if a file cannot be read or is too large for the source
/// map.
pub(crate) fn read_modules(
    root: &Path,
    files: Vec<cheby_package::ModuleFile>,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<crate::Module>, Error> {
    use cheby_diag::{TextRange, TextSize};
    use rayon::prelude::*;

    let bytes: Vec<std::io::Result<Vec<u8>>> = files
        .par_iter()
        .map(|file| std::fs::read(root.join(&file.file)))
        .collect();
    // Into the source map in order: (file, text to parse or the E0001
    // diagnostic of a file that is not UTF-8).
    let mut added: Vec<(FileId, Result<Arc<str>, Diagnostic>)> = Vec::with_capacity(files.len());
    for (file, bytes) in files.iter().zip(bytes) {
        let bytes = bytes.map_err(|e| Error::new(format!("{}: {e}", file.display)))?;
        let too_large = |_| Error::from(cheby_package::PackageError::too_large(&file.display));
        match String::from_utf8(bytes) {
            Ok(text) => {
                let text: Arc<str> = Arc::from(text);
                let id = sources
                    .add_file(file.display.as_str(), Arc::clone(&text))
                    .map_err(too_large)?;
                added.push((id, Ok(text)));
            }
            Err(e) => {
                let at = e.utf8_error().valid_up_to();
                let lossy = String::from_utf8_lossy(e.as_bytes()).into_owned();
                let id = sources
                    .add_file(file.display.as_str(), lossy)
                    .map_err(too_large)?;
                // The valid prefix is the same in the lossy text, and the
                // bad bytes became one U+FFFD there.
                let start = TextSize::try_from(at).unwrap_or(TextSize::MAX);
                let range = TextRange::at(
                    start,
                    TextSize::try_from('\u{fffd}'.len_utf8()).unwrap_or(1),
                );
                let diag = Diagnostic::new(codes::INVALID_UTF8, "source file is not valid UTF-8")
                    .with_primary(Span::new(id, range), "this byte is not valid UTF-8")
                    .with_help("save the file as UTF-8");
                added.push((id, Err(diag)));
            }
        }
    }
    let parsed: Vec<(crate::ParsedFile, Vec<Diagnostic>)> = added
        .into_par_iter()
        .map(|(file, text)| match text {
            Err(diag) => (
                crate::ParsedFile {
                    file,
                    syntax: None,
                    has_syntax_errors: true,
                },
                vec![diag],
            ),
            Ok(text) => {
                let (tree, mut diags) = cheby_syntax::parse(file, text).into_parts();
                let has_syntax_errors = diags.iter().any(Diagnostic::is_error);
                // As in `parse_file`: on a tree with grammar errors only the
                // token-level checks run (D-425).
                if has_syntax_errors {
                    diags.extend(cheby_syntax::validate_tokens(&tree));
                } else {
                    diags.extend(cheby_syntax::validate(&tree));
                }
                (
                    crate::ParsedFile {
                        file,
                        syntax: Some(tree),
                        has_syntax_errors,
                    },
                    diags,
                )
            }
        })
        .collect();
    let mut modules = Vec::with_capacity(files.len());
    for (file, (source, diags)) in files.into_iter().zip(parsed) {
        diagnostics.extend(diags);
        modules.push(crate::Module {
            path: file.path,
            source,
        });
    }
    Ok(modules)
}

/// Runs name resolution and type checking over `package`, which has no
/// phase-A errors. Diagnostics are pushed unsorted.
pub(crate) fn run(
    package: Package,
    mode: Mode,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Frontend, Error> {
    let front = run_inner(package, mode, sources, diagnostics)?;
    dump_interfaces(&front)?;
    Ok(front)
}

#[expect(
    clippy::too_many_lines,
    reason = "the front-end phases in order, each a few lines"
)]
fn run_inner(
    package: Package,
    mode: Mode,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Frontend, Ice> {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut trees: Vec<Option<SyntaxTree>> = vec![None];
    let package_name = package.manifest.name.clone();
    let local_targets = package.manifest.targets;
    let const_eval_time_limit = package.manifest.const_eval_time_limit;
    let const_eval_memory_limit = package.manifest.const_eval_memory_limit;
    let fiber_stack = package.manifest.fiber_stack;
    let main_stack = package.manifest.main_stack;
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
    set.reserve_modules(modules.len())?;
    let mut outputs = Schedule::run(
        BuildInputs {
            mode,
            globals: &globals,
            modules: &modules,
            trees: &trees,
            imports: &imports,
            targets: &targets,
            graph: &graph,
            set: &set,
            known: &known,
            local_targets,
        },
        &order,
    );

    // The results, merged in the topological order (D-264).
    let mut checked: Vec<Option<Checked>> = (0..modules.len()).map(|_| None).collect();
    let mut module_natives: Vec<Vec<(Box<str>, NativeBinding)>> =
        (0..modules.len()).map(|_| Vec::new()).collect();
    let mut built = Vec::new();
    let mut natives = BTreeMap::new();
    // The resolutions of the package's modules, for the whole-package
    // warnings of `cheby check`.
    let mut local_resolved: Vec<(ModuleId, ResolvedModule)> = Vec::new();
    for node in order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let output = outputs
            .get_mut(index(id)?)
            .and_then(Option::take)
            .ok_or_else(|| Ice::new(format!("module {id:?} was never checked")))??;
        diagnostics.extend(output.early);
        if let Some(slot) = module_natives.get_mut(index(id)?) {
            slot.clone_from(&output.natives);
        }
        bind_natives(output.natives, &mut natives, diagnostics);
        diagnostics.extend(output.late);
        if let Some(slot) = checked.get_mut(index(id)?) {
            *slot = Some(output.checked);
        }
        built.push(id);
        if let Some(resolved) = output.resolved {
            local_resolved.push((id, resolved));
        }
    }
    // The whole-package pass needs every module of the package: a module
    // on or above an import cycle was not checked, and its uses are
    // unknown (E0040 is reported).
    let every_local_checked = modules
        .iter()
        .filter(|(_, info)| info.package == PackageId::LOCAL)
        .all(|(id, _)| local_resolved.iter().any(|(checked, _)| *checked == id));
    if mode == Mode::Check && every_local_checked {
        let mut package_modules = Vec::with_capacity(local_resolved.len());
        for (id, resolved) in &local_resolved {
            let module = get(&checked, *id)?
                .as_ref()
                .ok_or_else(|| Ice::new("a checked module without its bodies"))?;
            let tree = get(&trees, *id)?
                .as_ref()
                .ok_or_else(|| Ice::new("a checked module without its syntax tree"))?;
            let own = set
                .get(*id)
                .ok_or_else(|| Ice::new("a checked module without its interface"))?;
            package_modules.push(PackageModule {
                input: TypesInput {
                    module: *id,
                    package: PackageId::LOCAL,
                    tree,
                    resolved,
                    deps: DepView::new(&set, &module.deps),
                    modules: &modules,
                    globals: &globals,
                },
                own,
                thir: &module.thir,
            });
        }
        diagnostics.extend(package_warnings(&package_modules)?);
    }
    Ok(Frontend {
        globals,
        modules,
        set,
        checked,
        order: built,
        natives,
        package_targets: local_targets,
        package_name,
        const_eval_time_limit,
        const_eval_memory_limit,
        fiber_stack,
        main_stack,
        imports,
        import_targets: targets,
        module_natives,
    })
}

#[cfg(test)]
mod tests {
    //! The whole-package warnings (W0005) and when they run.

    use std::path::PathBuf;

    use crate::{Prepared, RunOptions, check, prepare_run};

    /// A package `app` in a fresh temporary directory with these modules
    /// (paths under `src/`).
    fn package(name: &str, modules: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cheby_frontend_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (path, text) in modules {
            let path = dir.join("src").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// `code: message` of every diagnostic of `cheby check`, in output
    /// order.
    fn checked(dir: &std::path::Path) -> Vec<String> {
        check(dir)
            .unwrap()
            .diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect()
    }

    const MAIN: &str = "//! Root.\n\nimport app::shapes\n\n\
                        fn main() {\n  let _ = shapes::area(shapes::Square(2))\n}\n";

    const SHAPES: &str = "//! Shapes.\n\n\
        type Shape {\n  Square(Int),\n  Dot,\n}\n\n\
        fn area(shape: Shape) -> Int {\n  case shape {\n    Square(side) => side * side\n    Dot => 0\n  }\n}\n\n\
        fn perimeter(shape: Shape) -> Int {\n  case shape {\n    Square(side) => 4 * side\n    Dot => 0\n  }\n}\n\n\
        fn only_itself(n: Int) -> Int {\n  only_itself(n)\n}\n\n\
        fn used_by_test() -> Int {\n  1\n}\n\n\
        type Unused {\n  Unused,\n}\n\n\
        pub fn public() -> Int {\n  1\n}\n\n\
        test \"used by a test\" {\n  assert used_by_test() == 1\n}\n";

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn package_visible_items_that_nothing_uses_warn() {
        let dir = package("unused", &[("main.cheby", MAIN), ("shapes.cheby", SHAPES)]);
        assert_eq!(
            checked(&dir),
            [
                "W0005: the function `perimeter` is never used in this package",
                "W0005: the function `only_itself` is never used in this package",
                "W0005: the type `Unused` is never used in this package",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn cheby_run_skips_the_package_pass() {
        let dir = package("run", &[("main.cheby", MAIN), ("shapes.cheby", SHAPES)]);
        let output = match prepare_run(&dir, RunOptions::default()).unwrap() {
            Prepared::Ready(program) => program.output().diagnostics.clone(),
            Prepared::CompileFailed(out) => out.diagnostics,
        };
        assert!(
            output.iter().all(|d| d.code.to_string() != "W0005"),
            "{output:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn module_local_warnings_also_come_in_cheby_run() {
        let main = "//! Root.\n\nfn main() {\n  let unused = 1\n}\n";
        let dir = package("local_in_run", &[("main.cheby", main)]);
        let output = match prepare_run(&dir, RunOptions::default()).unwrap() {
            Prepared::Ready(program) => program.output().diagnostics.clone(),
            Prepared::CompileFailed(out) => out.diagnostics,
        };
        let codes: Vec<String> = output.iter().map(|d| d.code.to_string()).collect();
        assert!(codes.contains(&"W0002".to_owned()), "{codes:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn uses_across_modules_count() {
        let util = "//! Util.\n\nfn helper() -> Int {\n  1\n}\n\nconst limit: Int = 2\n\n\
                    type Point {\n  Point(Int),\n}\n";
        let main = "//! Root.\n\nimport app::util\n\n\
                    fn main() {\n  let util::Point(n) = util::Point(util::helper())\n  \
                    let _ = n + util::limit\n}\n";
        let dir = package("across", &[("main.cheby", main), ("util.cheby", util)]);
        assert_eq!(checked(&dir), Vec::<String>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn modules_left_unchecked_by_a_cycle_stop_the_package_pass() {
        let a = "//! A.\n\nimport app::b\n\nfn f() -> Int {\n  b::g()\n}\n";
        let b = "//! B.\n\nimport app::a\n\nfn g() -> Int {\n  a::f()\n}\n";
        let main = "//! Root.\n\nfn main() {\n  Nil\n}\n\nfn lonely() -> Int {\n  1\n}\n";
        let dir = package(
            "cycle",
            &[("main.cheby", main), ("a.cheby", a), ("b.cheby", b)],
        );
        let found = checked(&dir);
        assert!(found.iter().any(|d| d.starts_with("E0040")), "{found:?}");
        assert!(found.iter().all(|d| !d.starts_with("W0005")), "{found:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The front end of a package with these modules, built for running.
    fn front(name: &str, modules: &[(&str, &str)]) -> (super::Frontend, cheby_diag::SourceMap) {
        let dir = package(name, modules);
        let mut sources = cheby_diag::SourceMap::new();
        let mut diagnostics = Vec::new();
        let package = crate::load_package(&dir, &mut sources, &mut diagnostics)
            .unwrap()
            .unwrap();
        let front =
            super::run(package, super::Mode::Build, &mut sources, &mut diagnostics).unwrap();
        assert!(diagnostics.iter().all(|d| !d.is_error()), "{diagnostics:?}");
        std::fs::remove_dir_all(&dir).unwrap();
        (front, sources)
    }

    const UTIL: &str = "//! Util.\n\npub fn double(n: Int) -> Int {\n  n * 2\n}\n";
    const ROOT: &str =
        "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::double(1)\n}\n";

    /// Checks `text` as an extra module `app::<name>` on top of `front`.
    fn extra(
        front: &super::Frontend,
        sources: &mut cheby_diag::SourceMap,
        name: &str,
        text: &str,
        extra_interfaces: Vec<&cheby_iface::ModuleInterface>,
    ) -> Result<super::Checked, Vec<cheby_diag::Diagnostic>> {
        let source = sources.add_file(format!("<{name}>"), text).unwrap();
        super::check_extra_module(
            front,
            sources,
            super::ExtraModule {
                path: cheby_package::ModulePath::root("app").child(name),
                source,
                deps: Vec::new(),
                extra_interfaces,
            },
        )
        .unwrap()
    }

    #[test]
    fn an_extra_module_reads_the_package_and_std() {
        let (front, mut sources) = front("extra_ok", &[("main.cheby", ROOT), ("util.cheby", UTIL)]);
        let text = "import app::util\nimport std::list\n\n\
                    fn entry() -> Int {\n  list::length([util::double(2)])\n}\n";
        let checked = extra(&front, &mut sources, "repl_1", text, Vec::new()).unwrap();
        assert_eq!(
            checked.thir.module.as_u32(),
            u32::try_from(front.modules.len()).unwrap()
        );
        assert_eq!(checked.thir.fns.len(), 1);
        assert!(checked.warnings.is_empty(), "{:?}", checked.warnings);
        let util = front
            .local_module(&cheby_package::ModulePath::root("app").child("util"))
            .unwrap();
        assert!(checked.deps.contains(&util), "{:?}", checked.deps);
    }

    #[test]
    fn an_extra_module_reports_errors_and_keeps_warnings() {
        let (front, mut sources) = front(
            "extra_errors",
            &[("main.cheby", ROOT), ("util.cheby", UTIL)],
        );
        let bad = "import app::util\n\nfn entry() -> String {\n  util::double(2)\n}\n";
        let diags = extra(&front, &mut sources, "repl_1", bad, Vec::new())
            .err()
            .unwrap();
        assert!(
            diags.iter().any(cheby_diag::Diagnostic::is_error),
            "{diags:?}"
        );
        let unused = "fn entry() -> Int {\n  let unused = 1\n  2\n}\n";
        let checked = extra(&front, &mut sources, "repl_2", unused, Vec::new()).unwrap();
        let codes: Vec<String> = checked
            .warnings
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        assert_eq!(codes, ["W0002"]);
        let syntax = "fn entry( -> Int {\n  1\n}\n";
        assert!(extra(&front, &mut sources, "repl_3", syntax, Vec::new()).is_err());
    }

    #[test]
    fn extra_modules_stack_their_interfaces() {
        let (front, mut sources) =
            front("extra_stack", &[("main.cheby", ROOT), ("util.cheby", UTIL)]);
        let first = extra(
            &front,
            &mut sources,
            "repl_1",
            "fn one() -> Int {\n  1\n}\n",
            Vec::new(),
        )
        .unwrap();
        let second = extra(
            &front,
            &mut sources,
            "repl_2",
            "fn two() -> Int {\n  2\n}\n",
            vec![first.interface.as_ref()],
        )
        .unwrap();
        assert_eq!(second.thir.module.as_u32(), first.thir.module.as_u32() + 1);
        assert!(second.deps.contains(&first.thir.module));
    }

    #[test]
    fn reading_modules_in_parallel_matches_parse_file() {
        let mut files: Vec<(String, Vec<u8>)> = (0..40)
            .map(|n| {
                (
                    format!("m{n:02}.cheby"),
                    format!("//! M{n}.\n\nfn f{n}() -> Int {{\n  {n}\n}}\n").into_bytes(),
                )
            })
            .collect();
        files.push(("bad_syntax.cheby".to_owned(), b"fn ( {\n".to_vec()));
        files.push((
            "bad_utf8.cheby".to_owned(),
            b"//! Bad.\n\nfn f() -> Int {\n  \xff1\n}\n".to_vec(),
        ));
        let dir = package("read_modules", &[]);
        for (name, bytes) in &files {
            std::fs::write(dir.join("src").join(name), bytes).unwrap();
        }
        let mut sink = Vec::new();
        let discovered = cheby_package::discover(&dir, "app", &mut sink).unwrap();
        // One at a time, through `parse_file`.
        let mut seq_sources = cheby_diag::SourceMap::new();
        let mut seq_diags = Vec::new();
        let mut seq = Vec::new();
        for file in &discovered {
            let bytes = std::fs::read(dir.join(&file.file)).unwrap();
            let parsed =
                crate::parse_file(&mut seq_sources, &file.display, bytes, &mut seq_diags).unwrap();
            seq.push((
                parsed.file,
                parsed.has_syntax_errors,
                parsed.syntax.is_some(),
            ));
        }
        // In parallel.
        let mut par_sources = cheby_diag::SourceMap::new();
        let mut par_diags = Vec::new();
        let modules =
            super::read_modules(&dir, discovered, &mut par_sources, &mut par_diags).unwrap();
        let par: Vec<_> = modules
            .iter()
            .map(|m| {
                (
                    m.source.file,
                    m.source.has_syntax_errors,
                    m.source.syntax.is_some(),
                )
            })
            .collect();
        assert_eq!(par, seq);
        let render = |diags: &[cheby_diag::Diagnostic], sources: &cheby_diag::SourceMap| {
            diags
                .iter()
                .map(|d| cheby_diag::render(d, sources))
                .collect::<Vec<String>>()
        };
        assert_eq!(
            render(&par_diags, &par_sources),
            render(&seq_diags, &seq_sources)
        );
        assert!(par_diags.iter().any(|d| d.code.to_string() == "E0001"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The interfaces, THIR and diagnostics of one front-end run, as text.
    fn snapshot(dir: &std::path::Path) -> String {
        use std::fmt::Write as _;
        let mut sources = cheby_diag::SourceMap::new();
        let mut diagnostics = Vec::new();
        let package = crate::load_package(dir, &mut sources, &mut diagnostics)
            .unwrap()
            .unwrap();
        let front =
            super::run(package, super::Mode::Check, &mut sources, &mut diagnostics).unwrap();
        sources.sort(&mut diagnostics);
        let mut out = String::new();
        for d in &diagnostics {
            writeln!(out, "{}", cheby_diag::render(d, &sources)).unwrap();
        }
        for &id in &front.order {
            let checked = front.checked(id).unwrap();
            let bytes = cheby_iface::encode(
                &checked.interface,
                &cheby_iface::DepView::new(&front.set, &checked.deps),
                &front.modules,
                &front.globals,
            )
            .unwrap();
            let path = &front.modules.get(id).unwrap().path;
            writeln!(out, "{path}: {} bytes, {bytes:?}", bytes.len()).unwrap();
            let names: Vec<&str> = checked
                .thir
                .fns
                .iter()
                .map(|f| {
                    let item = checked.interface.item(f.def.index).unwrap();
                    front.globals.names.get(item.name)
                })
                .collect();
            writeln!(out, "  fns {names:?}").unwrap();
        }
        for (symbol, binding) in &front.natives {
            writeln!(out, "native {symbol} {:?}", binding.span).unwrap();
        }
        out
    }

    #[test]
    fn parallel_runs_give_identical_results() {
        // Many independent modules, so the pool runs them concurrently and
        // interns names and types in a different order on each run.
        use std::fmt::Write as _;
        let mut modules = vec![("main.cheby".to_owned(), {
            let mut main = "//! Root.\n\n".to_owned();
            for n in 0..24 {
                writeln!(main, "import app::m{n}").unwrap();
            }
            main.push_str("\nfn main() {\n");
            for n in 0..24 {
                writeln!(main, "  let _ = m{n}::f{n}(1)").unwrap();
            }
            main.push_str("}\n");
            main
        })];
        for n in 0..24 {
            modules.push((
                format!("m{n}.cheby"),
                format!(
                    "//! M{n}.\n\nimport std::list\n\n\
                     type T{n} {{\n  A{n}(Int),\n  B{n},\n}}\n\n\
                     pub fn f{n}(x: Int) -> Int {{\n  case A{n}(x) {{\n    A{n}(y) => list::length([y, {n}])\n    B{n} => 0\n  }}\n}}\n\n\
                     fn unused{n}() -> String {{\n  \"{n}\"\n}}\n"
                ),
            ));
        }
        let borrowed: Vec<(&str, &str)> = modules
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let dir = package("parallel", &borrowed);
        let first = snapshot(&dir);
        for _ in 0..4 {
            assert_eq!(snapshot(&dir), first);
        }
        assert!(first.contains("W0005"), "{first}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
