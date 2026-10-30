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
//!    nothing depends on the schedule. A module outside std reads through
//!    a recording view, and the modules it read beyond its imports are its
//!    front end's hidden reads ([`Checked::front_reads`], D-955). A debug
//!    toolchain decodes each interface right after building it and
//!    compares it with the original (D-954).
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
    ModuleTable, PackageId, ReadLog, builtin, encode,
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
use cheby_types::{Tests, TypesInput, build_interface, check_bodies_with, thir};

use crate::cache::front::{self, External, FrontKey, FrontRecord, ImportKey};
use crate::cache::replay::{self, SpanCx, StoredSpan};
use crate::cache::{Hash128, Key};
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
    /// Per module id: the hashes of its source file's bytes, computed once
    /// per command when the file was read, or when `cheby` was built for
    /// std (D-963); `None` for the builtin module and for modules a REPL
    /// added.
    pub source_hashes: Vec<Option<crate::SourceHashes>>,
    /// Whether every checked module has its bodies: false only when a
    /// warm `cheby run` or `cheby test` found an error and skipped checking
    /// its cache hits again, so nothing may lower it ([`run_inner`]).
    pub complete: bool,
    /// The local package's native code (D-940, D-941): its C objects and
    /// its loaded shared library, prepared by [`run_cached`] in `Build`
    /// mode once the package checks; empty in `Check` mode, which compiles
    /// no C (D-532), and for a package without `[native]`.
    pub native: crate::native::PackageNative,
    /// Where borrowing inference writes its records and logs its code
    /// keys (K4b, D-969): the cache's store and lookup log and each
    /// package module's front identity; `None` without a cache.
    pub records: Option<crate::borrowing::RecordSink>,
    /// The command's store and whether it reads it, for what K4d caches
    /// beside code records (the reuse lines, D-965); `None` without a
    /// cache.
    pub cache_store: Option<(crate::cache::StoreShare, bool)>,
    /// Per package module with a front record: its front identity `F2`,
    /// which keys its warm facts (D-1250, [`crate::warm_keys`]).
    pub warm_f2: BTreeMap<ModuleId, Hash128>,
    /// The package hits a warm `cheby run` or `cheby test` did not check
    /// (D-1250): what is read of them instead of their THIR.
    pub warm: BTreeMap<ModuleId, crate::warm_keys::WarmModule>,
    /// Whether something asked for the checked body of a module in
    /// [`Frontend::warm`]: a warm build that then fails starts again with
    /// every module checked ([`Frontend::wanted_body`]).
    pub wanted: std::sync::atomic::AtomicBool,
}

/// An `@external(native, "symbol")` binding of a symbol.
#[derive(Clone, Debug)]
pub(crate) struct NativeBinding {
    pub sig: ForeignSig,
    /// The symbol's string literal.
    pub span: Span,
}

/// The native symbols bound by `resolved`'s functions and external types' drop functions
/// (D-949), in item order.
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
        let sig = match iface.item(index).map(|i| &i.sig) {
            Some(ItemSig::Fn(sig)) => {
                foreign_sig_with_context(sig, globals, item.attrs.std_context)
            }
            // D-949: an external type's drop function is
            // `void f(void *handle)`, checked by E0054 like any binding.
            Some(ItemSig::Type(_)) => ForeignSig {
                params: vec![cheby_mir::repr::CType::Ptr],
                ret: cheby_mir::repr::CType::Void,
            },
            _ => continue,
        };
        out.push((symbol.into(), NativeBinding { sig, span }));
    }
    out
}

/// D-944: E0055 for each of `bindings`, a module's outside std, that names
/// a symbol the runtime exports (`cheby_runtime::symbols()`), which only
/// the standard library may bind; returns the other bindings, so a
/// reserved one never meets std's binding in E0054. Resolution reports
/// the reserved prefixes itself; the driver passes the runtime's names, so
/// `cheby_resolve` does not depend on the runtime.
pub(crate) fn reserve_exports(
    bindings: Vec<(Box<str>, NativeBinding)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(Box<str>, NativeBinding)> {
    let runtime = cheby_runtime::symbols();
    let exported = |symbol: &str| runtime.iter().any(|(name, _)| *name == symbol);
    bindings
        .into_iter()
        .filter(|(symbol, binding)| {
            match cheby_resolve::runtime_export_reserved(symbol, binding.span, &exported) {
                Some(diagnostic) => {
                    diagnostics.push(diagnostic);
                    false
                }
                None => true,
            }
        })
        .collect()
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
    /// The front end's hidden reads (D-955, `cache.md` §3.3's `R`): the
    /// modules whose interfaces resolution, the interface and the bodies
    /// read through the module's view that are neither the module's
    /// direct imports, nor std modules (the build ID fixes std), nor the
    /// builtin module; sorted by id. Empty for std modules, which record
    /// nothing, and for REPL entries and reloads, which are never cached
    /// (D-1124, D-964).
    pub front_reads: Vec<ModuleId>,
    pub thir: thir::Module,
    /// The native drop function of each external type the module declares
    /// (D-949), from its resolved items
    /// (`cheby_resolve::ResolvedModule::external_drops`): what lowering's
    /// `ExternalDesc::drop` gets (`crate::native::attach_drops`).
    pub drops: Vec<(cheby_iface::DefIndex, Box<str>)>,
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
        let found = self.checked_quiet(module);
        if found.is_none() && self.warm.contains_key(&module) {
            self.wanted.store(true, Ordering::Relaxed);
        }
        found
    }

    /// [`Frontend::checked`], without noting a warm hit's body as wanted:
    /// for callers that skip a module without one.
    pub fn checked_quiet(&self, module: ModuleId) -> Option<&Checked> {
        usize::try_from(module.as_u32())
            .ok()
            .and_then(|n| self.checked.get(n))
            .and_then(Option::as_ref)
    }

    /// Whether the build asked for the checked body of a warm hit (D-1250):
    /// the command starts again with every module checked.
    pub fn wanted_body(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// The tests of `module`: its checked ones, or a warm hit's from its
    /// warm facts (D-1250), whose bodies are empty and never lowered.
    pub fn tests_of(&self, module: ModuleId) -> Option<&[thir::TestDef]> {
        match self.warm.get(&module) {
            Some(warm) => Some(&warm.tests),
            None => self.checked_quiet(module).map(|c| c.thir.tests.as_slice()),
        }
    }

    /// The summaries of warm hit `module` in the walk's form
    /// (`load_set.rs`), `None` for a module that was checked.
    pub fn warm_summaries(
        &self,
        module: ModuleId,
    ) -> Option<&[crate::cache::front::ReferenceSummary]> {
        self.warm.get(&module).map(|w| w.summaries.as_slice())
    }

    /// The hashes of `module`'s source computed when it was read (D-963).
    pub fn source_hashes(&self, module: ModuleId) -> Option<crate::SourceHashes> {
        usize::try_from(module.as_u32())
            .ok()
            .and_then(|n| self.source_hashes.get(n))
            .copied()
            .flatten()
    }

    /// The package libraries a JIT of this build resolves native symbols
    /// in, in build order (D-941): the local package's, if it has native
    /// code.
    pub fn native_libraries(&self) -> Vec<Arc<cheby_jit::NativeLibrary>> {
        self.native.libraries()
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
        // A warm hit's `main` comes from its warm facts (D-1250).
        if let Some(warm) = self.warm.get(&module) {
            return warm.main.as_ref();
        }
        self.checked_quiet(module)?
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
    /// K6: the command's cache, where std's front records are looked up
    /// first when `std_hits_ok` (D-980), and std's parses logged.
    std_cache: Option<&'a crate::Cache>,
    std_hits_ok: bool,
    /// K6: the std modules whose front record hit, which are not parsed.
    std_hits: &'a mut BTreeMap<ModuleId, FrontRecord>,
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
        // K6: std's front record first (D-980); its file joins the source
        // map unparsed, for spans and D-538's seeds.
        if let Some(cache) = self.std_cache {
            let full = module_path.to_string();
            if self.std_hits_ok
                && let Some(record) = crate::std_cache::lookup_std_front(cache, &full)?
            {
                let file = self
                    .sources
                    .add_file(display, source.source)
                    .map_err(|_| Ice::new("a std file is too large"))?;
                let id = self.modules.add(ModuleInfo {
                    path: module_path,
                    package: PackageId::STD,
                    file: Some(file),
                })?;
                if self.trees.len() != index(id)? {
                    return Err(Ice::new("module ids and trees are out of step"));
                }
                self.trees.push(None);
                self.std_hits.insert(id, record);
                return Ok(id);
            }
            cache.log.parsed_std(&full);
        }
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

/// The hidden reads among `read`, the modules a recording view of
/// `module` handed out (D-955): those that are not `module` itself, a
/// direct import (`imports`), a std module, the builtin module or one of
/// `known` (an earlier phase's hidden reads). Sorted by id, as `read` is.
pub(crate) fn hidden_reads(
    modules: &ModuleTable,
    module: ModuleId,
    read: Vec<ModuleId>,
    imports: &[ImportTarget],
    known: &[ModuleId],
) -> Vec<ModuleId> {
    let mut hidden = read;
    hidden.retain(|&m| {
        m != module
            && m != ModuleId::BUILTIN
            && !imports.iter().any(|t| t.module() == Some(m))
            && !known.contains(&m)
            && modules
                .get(m)
                .is_some_and(|info| info.package != PackageId::STD)
    });
    hidden
}

/// A module's native bindings as its record stores them, or `None` when
/// one's symbol literal is not in the module's own `file`.
fn externals(natives: &[(Box<str>, NativeBinding)], file: FileId) -> Option<Vec<External>> {
    natives
        .iter()
        .map(|(symbol, binding)| {
            (binding.span.file == file).then(|| External {
                symbol: symbol.to_string(),
                sig: binding.sig.clone(),
                span: (binding.span.range.start(), binding.span.range.len()),
            })
        })
        .collect()
}

/// The build-time source hashes of the std module at `path` (D-963).
fn std_source_hashes(path: &ModulePath) -> Option<crate::SourceHashes> {
    let segments = path.segments();
    let (first, rest) = segments.split_first()?;
    if first.as_str() != "std" {
        return None;
    }
    let under: Vec<&str> = rest.iter().map(String::as_str).collect();
    std_lib::source_hashes(&under.join("::"))
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
    let bindings = reserve_exports(
        native_bindings(&resolved, &iface, &front.globals),
        &mut diags,
    );
    bind_natives(bindings, &mut natives, &mut diags);
    let (thir, body_diags) = check_bodies_with(&types, &iface, Tests::SkipStd)?;
    diags.extend(body_diags);
    if diags.iter().any(Diagnostic::is_error) {
        return Ok(Err(diags));
    }
    Ok(Ok(Checked {
        deps,
        // D-1124: REPL entries are never cached
        // (D-964), so their views record nothing.
        front_reads: Vec::new(),
        thir,
        drops: resolved.external_drops(),
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
                // D-1124: a reload is never cached
                // (D-964), so its views record nothing.
                front_reads: Vec::new(),
                thir,
                drops: resolved.external_drops(),
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

/// Why a front end with the cache stopped without a result.
pub(crate) enum Stop {
    /// An internal compiler error.
    Ice(Ice),
    /// Something the package index or a hit vouched for turned out wrong
    /// (a module the index let go unparsed lies on an import cycle, has a
    /// grammar error or an import that no longer resolves; a hit's
    /// interface that a checked module needs cannot be decoded): the
    /// command starts its front end again, parsing every module and
    /// decoding each hit's interface at its lookup (D-1133).
    Restart,
}

impl From<Ice> for Stop {
    fn from(ice: Ice) -> Self {
        Self::Ice(ice)
    }
}

/// The lines of the three lists a front record's diagnostics blob holds:
/// phase A's, then the front end's early and late ones.
const DIAGNOSTIC_LISTS: usize = 3;

/// The syntax tree of a module parsed after loading, with its phase-A
/// diagnostics, once; `None` for one with a grammar error.
type LateTree = OnceLock<Option<(SyntaxTree, Vec<Diagnostic>)>>;

/// A file read by [`read_modules`]: its id, and its text to parse, `None`
/// when it is not parsed, or the E0001 of a file that is not UTF-8.
type ToParse = (FileId, Result<Option<Arc<str>>, Diagnostic>);

/// How a front end uses the cache (D-955, D-962).
#[derive(Clone, Copy)]
pub(crate) struct CacheUse<'a> {
    pub cache: &'a crate::Cache,
    /// The package index this command read, if any, and its bytes.
    pub index: Option<&'a (crate::cache::index::PackageIndex, Vec<u8>)>,
    /// Where this command's index goes (the root's absolute path and the
    /// family), unless the cache is bypassed for reads and writes.
    pub index_key: Key,
    /// Whether modules the index vouches for may stay unparsed and hits'
    /// interfaces be decoded only when a checked module needs them; false
    /// after a [`Stop::Restart`].
    pub lazy: bool,
    /// The build a `Mode::Build` front end feeds, for `cheby run` and
    /// `cheby test` (D-1250): it checks only what its warm build needs
    /// ([`crate::warm_keys`]); `None` for every other command.
    pub build: Option<crate::warm_keys::WarmBuild<'a>>,
}

/// The inputs every module's pipeline step reads, shared by the tasks.
struct BuildInputs<'a> {
    mode: Mode,
    globals: &'a Globals,
    modules: &'a ModuleTable,
    sources: &'a SourceMap,
    trees: &'a [Option<SyntaxTree>],
    /// Per module id: the phase-A diagnostics of a package module parsed
    /// when the package was loaded (already reported then).
    syntax: &'a [Option<Vec<Diagnostic>>],
    imports: &'a [Vec<ImportRequest>],
    targets: &'a [Vec<ImportTarget>],
    graph: &'a Graph,
    set: &'a InterfaceSet,
    known: &'a [&'a str],
    local_targets: Targets,
    /// Per module id: whether it is checked from source even when its
    /// lookup hits: in `Build` mode every module, in `Check` mode those
    /// constant evaluation needs (D-1134).
    must_check: &'a [bool],
    /// What the tasks of one pass share, filled as they run.
    shared: &'a Shared,
    /// Per module id: the syntax tree of a module parsed after loading,
    /// with its phase-A diagnostics; `None` for one with a grammar error.
    late_trees: &'a [LateTree],
    /// The package modules checked from source, by path, once per check
    /// (the driver's count, never printed).
    counted: &'a std::sync::Mutex<Vec<String>>,
    /// The cache, which the package's modules are looked up in and written
    /// to (D-955); `None` checks every module from source and writes
    /// nothing.
    cache: Option<FrontCx<'a>>,
}

/// What the tasks of a front end fill as they run, by module id.
struct Shared {
    /// The interface hash each module published: a hit's from its record,
    /// so that dependents' keys need no decoded interface.
    hashes: Vec<OnceLock<Option<cheby_iface::InterfaceHash>>>,
    /// The front record of each hit.
    records: Vec<OnceLock<FrontRecord>>,
    /// Whether a hit's interface was decoded and published (D-954): done
    /// once, when a checked module first needs it.
    decoded: Vec<OnceLock<bool>>,
    /// The front identity of each package hit, which keys its warm facts
    /// (D-1250, [`crate::warm_keys`]).
    f2: Vec<OnceLock<Hash128>>,
    /// The warm facts of each package hit, read once when its interface is
    /// decoded (D-1250).
    facts: Vec<OnceLock<front::WarmFacts>>,
}

impl Shared {
    fn new(len: usize) -> Self {
        Self {
            hashes: (0..len).map(|_| OnceLock::new()).collect(),
            records: (0..len).map(|_| OnceLock::new()).collect(),
            decoded: (0..len).map(|_| OnceLock::new()).collect(),
            f2: (0..len).map(|_| OnceLock::new()).collect(),
            facts: (0..len).map(|_| OnceLock::new()).collect(),
        }
    }

    fn hash(&self, module: ModuleId) -> Option<cheby_iface::InterfaceHash> {
        self.hashes
            .get(index(module).ok()?)
            .and_then(OnceLock::get)
            .copied()
            .flatten()
    }
}

/// What the front end needs to read and write front records.
#[derive(Clone, Copy)]
struct FrontCx<'a> {
    cache: &'a crate::Cache,
    use_: CacheUse<'a>,
    /// The package's name.
    package: &'a str,
    /// Per module id: its source hashes (D-963).
    hashes: &'a [Option<crate::SourceHashes>],
    /// Per module id: its source file's length in bytes, for replayed
    /// spans.
    text_lens: &'a [u32],
    /// Per module id: whether it was left unparsed because the index
    /// vouched for its source.
    unparsed: &'a [bool],
    /// Which pass of [`run_inner`] this is.
    pass: Pass,
}

/// The two passes of a build with a cache ([`run_inner`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    /// Looks every package module up, logs the lookup, and writes the
    /// record of a miss.
    Warm,
    /// `cheby check`'s error path: checks again from source, on fresh
    /// interfaces, a module whose check against decoded interfaces
    /// reported an error, with the modules it read that were not checked
    /// on fresh interfaces (D-1136); looks nothing up
    /// and writes nothing.
    Recheck,
}

/// A module's summaries, computed from its check (D-965, D-967).
#[derive(Clone, Debug, Default)]
struct Summaries {
    references: Vec<front::ReferenceSummary>,
    uses: front::UseSummary,
    /// Whether every body checked (`cheby_types::ModuleRefs::complete`).
    complete: bool,
}

/// What one module's pipeline step produced.
struct ModuleOutput {
    /// The phase-A diagnostics of a module that was not parsed when the
    /// package was loaded: replayed for a hit, or from parsing it later.
    syntax: Vec<Diagnostic>,
    /// The diagnostics of resolution and of the interface.
    early: Vec<Diagnostic>,
    natives: Vec<(Box<str>, NativeBinding)>,
    /// The diagnostics of the bodies.
    late: Vec<Diagnostic>,
    /// The interface hash the module published.
    hash: Option<cheby_iface::InterfaceHash>,
    /// The checked bodies; `None` on a cache hit that was not checked.
    checked: Option<Checked>,
    /// The summaries of a package module checked from source.
    summaries: Option<Summaries>,
    /// The front record found when the lookup hit.
    hit: Option<FrontRecord>,
    /// The module's entry for this command's package index: a hit's, or
    /// that of the record just written.
    entry: Option<crate::cache::index::IndexEntry>,
}

/// A front record found by a lookup, with the keys it lies under.
struct Found {
    f1: Key,
    f2: Key,
    record: FrontRecord,
}

/// What a lookup saw of a module's read-set hint, so that writing the
/// module's record need not read the hint again.
enum HintSeen {
    /// The hint was not looked at.
    Unknown,
    /// The hint's paths; `None` when there is no hint, which reads as the
    /// empty set (D-1137).
    Seen(Option<Vec<String>>),
    /// The hint is malformed: it is written again.
    Bad,
}

impl BuildInputs<'_> {
    /// Runs package or std module `id`'s step: a package module is looked
    /// up in the cache first (D-955). A hit that need not be checked
    /// publishes its interface hash from its record, and its interface only
    /// when a checked module needs it (D-954); its diagnostics and externals
    /// are replayed (D-965). Any other module is checked from source
    /// ([`BuildInputs::check_source`]); a hit that is checked anyway prints
    /// its replayed diagnostics and is not written again, a miss is
    /// written. `published` lets the module's dependents start.
    fn check_module(&self, id: ModuleId, published: impl FnOnce()) -> Result<ModuleOutput, Stop> {
        let deps: Vec<ModuleId> = reachable(self.graph, self.graph.successors(id.as_u32()))
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        let info = self.modules.try_get(id)?;
        // K6: a std module whose front record hit (D-980).
        if info.package == PackageId::STD
            && let Some(cx) = self.cache
            && cx.pass == Pass::Warm
            && let Some(record) = self.shared.records.get(index(id)?).and_then(OnceLock::get)
        {
            return self.std_hit(cx, id, record, published);
        }
        // Other std modules are checked from source, as D-450 says.
        let cx = match self.cache {
            Some(cx) if info.package == PackageId::LOCAL && cx.pass == Pass::Warm => cx,
            _ => return self.check_source(id, deps, published),
        };
        let key = self.front_key(cx, id)?;
        let (found, hint) = match &key {
            Some((f1, _)) if cx.cache.reads => self.find(cx, id, &deps, *f1)?,
            _ => (None, HintSeen::Unknown),
        };
        let must_check = self.must_check.get(index(id)?).copied().unwrap_or(true);
        let mut found = found;
        if !must_check && let Some(hit) = &found {
            if let Some(output) = self.hit(cx, id, hit)? {
                published();
                return Ok(output);
            }
            // Its replay or its interface failed: a miss, written again.
            found = None;
        }
        let mut output = self.check_source(id, deps, published)?;
        if let Some(found) = found
            && let Some(lists) = self.replay(cx, id, &found.record)?
        {
            // A hit checked anyway prints what the cache replays, which
            // must be what the check found (D-1131).
            let [syntax, early, late] = lists;
            if cfg!(debug_assertions)
                && (early != output.early
                    || late != output.late
                    || output.hash.map(|h| Hash128(h.0)) != Some(found.record.interface))
            {
                return Err(Stop::Ice(Ice::new(format!(
                    "module `{}` checked from source gives other diagnostics or another interface hash than the cache's (D-954)",
                    info.path
                ))));
            }
            output.early = early;
            output.late = late;
            if get(cx.unparsed, id).copied().unwrap_or(false) {
                output.syntax = syntax;
            }
            output.entry = self.entry(cx, id, found.f1, found.f2, found.record.clone())?;
            output.hit = Some(found.record);
            return Ok(output);
        }
        if let Some((f1, imports)) = key {
            output.entry = self.write_front(cx, id, f1, imports, &output, hint)?;
        }
        Ok(output)
    }

    /// K6: std module `id`, whose front record `record` hit (D-980): neither
    /// parsed nor checked. Its natives come from the record, its interface
    /// is decoded now (D-954) and its hash published; std reports no
    /// diagnostics.
    fn std_hit(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        record: &FrontRecord,
        published: impl FnOnce(),
    ) -> Result<ModuleOutput, Stop> {
        let info = self.modules.try_get(id)?;
        let file = info
            .file
            .ok_or_else(|| Ice::new(format!("std module `{}` has no file", info.path)))?;
        let span_cx = self.span_cx(cx, id, file)?;
        let mut natives = Vec::with_capacity(record.externals.len());
        for external in &record.externals {
            let span = span_cx
                .resolve(&StoredSpan::Own {
                    start: external.span.0,
                    len: external.span.1,
                })
                .ok_or_else(|| Ice::new(format!("a native span of std's `{}`", info.path)))?;
            natives.push((
                external.symbol.clone().into_boxed_str(),
                NativeBinding {
                    sig: external.sig.clone(),
                    span,
                },
            ));
        }
        if !self.ensure_interface(id)? {
            // A damaged or missing artifact (D-960): written again, and
            // the front end starts again, parsing std.
            crate::std_cache::repair(cx.cache)?;
            return Err(Stop::Restart);
        }
        let hash = cheby_iface::InterfaceHash(record.interface.0);
        if let Some(slot) = self.shared.hashes.get(index(id)?) {
            let _ = slot.set(Some(hash));
        }
        published();
        Ok(ModuleOutput {
            syntax: Vec::new(),
            early: Vec::new(),
            natives,
            late: Vec::new(),
            hash: Some(hash),
            checked: None,
            summaries: None,
            hit: None,
            entry: None,
        })
    }

    /// The syntax tree of module `id`: parsed when the package was loaded,
    /// or now, once, for a module the package index let go unparsed. A
    /// grammar error in a module the index vouched for is a
    /// [`Stop::Restart`].
    fn tree(&self, id: ModuleId) -> Result<(&SyntaxTree, Option<&[Diagnostic]>), Stop> {
        let info = self.modules.try_get(id)?;
        if let Some(tree) = get(self.trees, id)?.as_ref() {
            return Ok((tree, None));
        }
        let file = info
            .file
            .ok_or_else(|| Ice::new(format!("module `{}` has no file", info.path)))?;
        let slot = self
            .late_trees
            .get(index(id)?)
            .ok_or_else(|| Ice::new("no late tree slot"))?;
        let parsed = slot.get_or_init(|| {
            let text = self.sources.file(file)?.text_arc();
            let (tree, mut diags) = cheby_syntax::parse(file, text).into_parts();
            if diags.iter().any(Diagnostic::is_error) {
                return None;
            }
            diags.extend(cheby_syntax::validate(&tree));
            (!diags.iter().any(Diagnostic::is_error)).then_some((tree, diags))
        });
        match parsed {
            Some((tree, diags)) => Ok((tree, Some(diags))),
            None => Err(Stop::Restart),
        }
    }

    /// A hit that is not checked (D-954, D-965): its record's interface
    /// hash is published for its dependents' keys, its diagnostics and
    /// externals are replayed, and its interface is decoded only when a
    /// checked module needs it ([`BuildInputs::ensure_interface`]), or now
    /// when the front end is not lazy. `None` when the replay or the
    /// decoding fails: the module is a miss.
    fn hit(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        found: &Found,
    ) -> Result<Option<ModuleOutput>, Stop> {
        let at = index(id)?;
        let record = &found.record;
        let slot = self
            .shared
            .records
            .get(at)
            .ok_or_else(|| Ice::new("no record slot"))?;
        let _ = slot.set(record.clone());
        if let Some(slot) = self.shared.f2.get(at) {
            let _ = slot.set(found.f2.hash());
        }
        // A hit that turns into a miss is checked from source, which
        // publishes its interface: so nothing of it is published until its
        // replay and its externals have succeeded (D-960: a missing blob
        // reads as a miss). Its replay never decodes its own interface,
        // since a span into its own file is stored as such.
        let Some([syntax, early, late]) = self.replay(cx, id, record)? else {
            return Ok(None);
        };
        let info = self.modules.try_get(id)?;
        let Some(file) = info.file else {
            return Ok(None);
        };
        let span_cx = self.span_cx(cx, id, file)?;
        let mut natives = Vec::with_capacity(record.externals.len());
        for external in &record.externals {
            let Some(span) = span_cx.resolve(&StoredSpan::Own {
                start: external.span.0,
                len: external.span.1,
            }) else {
                return Ok(None);
            };
            natives.push((
                external.symbol.clone().into_boxed_str(),
                NativeBinding {
                    sig: external.sig.clone(),
                    span,
                },
            ));
        }
        // Not lazy (after a restart), or a warm `cheby run` or `cheby
        // test`, whose back end names every hit's items (D-1250): its
        // interface is decoded now, last.
        if (!cx.use_.lazy || self.mode == Mode::Build) && !self.ensure_interface(id)? {
            return Ok(None);
        }
        let hash = cheby_iface::InterfaceHash(record.interface.0);
        if let Some(slot) = self.shared.hashes.get(at) {
            let _ = slot.set(Some(hash));
        }
        Ok(Some(ModuleOutput {
            syntax: if get(cx.unparsed, id).copied().unwrap_or(false) {
                syntax
            } else {
                Vec::new()
            },
            early,
            natives,
            late,
            hash: Some(hash),
            checked: None,
            summaries: None,
            hit: Some(record.clone()),
            entry: self.entry(cx, id, found.f1, found.f2, record.clone())?,
        }))
    }

    /// The replayed diagnostics of `record` (D-965), as phase A's, the
    /// early and the late lists; the interfaces their spans into other
    /// modules name are decoded first. `None` when the blob is missing or
    /// malformed or a span no longer resolves.
    fn replay(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        record: &FrontRecord,
    ) -> Result<Option<[Vec<Diagnostic>; 3]>, Stop> {
        if record.diagnostics == Hash128::of(&replay::empty_blob(DIAGNOSTIC_LISTS)) {
            return Ok(Some([Vec::new(), Vec::new(), Vec::new()]));
        }
        let Some(bytes) = cx.cache.store.get_blob(record.diagnostics).ok().flatten() else {
            return Ok(None);
        };
        let Some(named) = replay::item_modules(&bytes, DIAGNOSTIC_LISTS) else {
            return Ok(None);
        };
        for path in named {
            let Some(m) = crate::parse_module_path(&path).and_then(|p| self.modules.find(&p))
            else {
                return Ok(None);
            };
            if !self.ensure_interface(m)? {
                return Ok(None);
            }
        }
        let Some(file) = self.modules.try_get(id)?.file else {
            return Ok(None);
        };
        let span_cx = self.span_cx(cx, id, file)?;
        let Some(lists) = replay::decode_diagnostics(&bytes, DIAGNOSTIC_LISTS, &span_cx) else {
            return Ok(None);
        };
        Ok(<[Vec<Diagnostic>; 3]>::try_from(lists).ok())
    }

    /// The span context of package module `id`, whose file is `file`.
    fn span_cx(&self, cx: FrontCx<'_>, id: ModuleId, file: FileId) -> Result<SpanCx<'_>, Ice> {
        Ok(SpanCx {
            own: file,
            own_len: cx.text_lens.get(index(id)?).copied().unwrap_or(0),
            modules: self.modules,
            set: self.set,
            globals: self.globals,
        })
    }

    /// Whether module `module` has an interface in the set, after decoding
    /// it from its artifact if it is a hit that has none yet (D-954): once,
    /// with the interfaces of its dependencies decoded first. False when it
    /// cannot be decoded.
    fn ensure_interface(&self, module: ModuleId) -> Result<bool, Ice> {
        if self.set.get(module).is_some() {
            return Ok(true);
        }
        let Some(slot) = self.shared.decoded.get(index(module)?) else {
            return Ok(false);
        };
        Ok(*slot.get_or_init(|| self.decode_hit(module).unwrap_or(false)))
    }

    /// Decodes hit `module`'s interface from its artifact against its
    /// dependencies' interfaces, applies its spans and publishes it.
    fn decode_hit(&self, module: ModuleId) -> Result<bool, Ice> {
        let Some(cx) = self.cache else {
            return Ok(false);
        };
        let Some(record) = self
            .shared
            .records
            .get(index(module)?)
            .and_then(OnceLock::get)
        else {
            return Ok(false);
        };
        let deps: Vec<ModuleId> = reachable(self.graph, self.graph.successors(module.as_u32()))
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        for &dep in &deps {
            if !self.ensure_interface(dep)? {
                return Ok(false);
            }
        }
        let info = self.modules.try_get(module)?;
        let Some(file) = info.file else {
            return Ok(false);
        };
        let store = &cx.cache.store;
        let Some(blob) = store.get_blob(record.artifact).ok().flatten() else {
            return Ok(false);
        };
        let Ok(artifact) = cheby_iface::read_artifact(&blob) else {
            return Ok(false);
        };
        if artifact.build_id != store.build_id().0 || Hash128(artifact.hash.0) != record.interface {
            return Ok(false);
        }
        let view = DepView::new(self.set, &deps);
        let Ok(mut iface) = cheby_iface::decode_interface(
            module,
            info.package,
            artifact.h,
            &view,
            self.modules,
            self.globals,
        ) else {
            return Ok(false);
        };
        if cheby_iface::apply_spans(&mut iface, file, &artifact.spans).is_err() {
            return Ok(false);
        }
        // D-1250: a warm `cheby run` or `cheby test` adds the hit's private
        // items and tests, which no encoding holds (D-953, D-1121), from its
        // warm facts, after its items, so its own summaries and its tests
        // name defs as a cold build's do; the facts are kept for its warm
        // module.
        if self.mode == Mode::Build
            && let Some(&f2) = self.shared.f2.get(index(module)?).and_then(OnceLock::get)
            && let Some(facts) =
                crate::warm_keys::add_hidden(&cx.cache.store, f2, file, self.globals, &mut iface)?
            && let Some(slot) = self.shared.facts.get(index(module)?)
        {
            let _ = slot.set(facts);
        }
        Ok(self.set.publish(Arc::new(iface)).is_ok() || self.set.get(module).is_some())
    }

    /// Module `id` checked from source: resolution, its interface (then
    /// `published`), its bodies, and for a package module its hidden reads
    /// and its summaries. Every package module it reads must have an
    /// interface: a hit's is decoded first (D-954), and one that cannot be
    /// is a [`Stop::Restart`].
    #[expect(
        clippy::too_many_lines,
        reason = "the module's phases in order, each a few lines"
    )]
    fn check_source(
        &self,
        id: ModuleId,
        deps: Vec<ModuleId>,
        published: impl FnOnce(),
    ) -> Result<ModuleOutput, Stop> {
        let info = self.modules.try_get(id)?;
        for &dep in &deps {
            if !self.ensure_interface(dep)? {
                return Err(Stop::Restart);
            }
        }
        let (tree, late_syntax) = self.tree(id)?;
        // A module parsed after loading takes its imports from its tree
        // now; they must be the ones the package index gave (D-962).
        let late_requests;
        let requests: &[ImportRequest] = if late_syntax.is_some() {
            late_requests = collect_imports(tree, &self.globals.names);
            let targets = get(self.targets, id)?;
            let same = late_requests.len() == targets.len()
                && late_requests.iter().zip(targets).all(|(request, target)| {
                    let written: Vec<&str> = request
                        .path
                        .iter()
                        .map(|&(name, _)| self.globals.names.get(name))
                        .collect();
                    target
                        .module()
                        .and_then(|m| self.modules.get(m))
                        .is_some_and(|m| m.path.to_string() == written.join("::"))
                });
            if !same {
                return Err(Stop::Restart);
            }
            &late_requests
        } else {
            get(self.imports, id)?
        };
        if info.package == PackageId::LOCAL
            && let Ok(mut checked) = self.counted.lock()
        {
            checked.push(info.path.to_string());
        }
        // Every phase reads other modules through one view, which records
        // what it hands out (D-955); std records nothing, since the build
        // ID fixes it.
        let log = ReadLog::new();
        let view = if info.package == PackageId::STD {
            DepView::new(self.set, &deps)
        } else {
            DepView::recording(self.set, &deps, &log)
        };
        let input = ResolveInput {
            module: id,
            package: info.package,
            tree,
            imports: requests,
            targets: get(self.targets, id)?,
            deps: view,
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
            deps: view,
            modules: self.modules,
            globals: self.globals,
        };
        let (iface, diags) = build_interface(&types)?;
        early.extend(diags);
        // D-954: a debug toolchain decodes every interface it encodes and
        // compares it with the original. A module with an error in its
        // declarations is never cached (D-964), so it is not checked
        // (D-1125).
        if cfg!(debug_assertions) && !early.iter().any(Diagnostic::is_error) {
            cheby_iface::check_roundtrip(&iface, &view.unrecorded(), self.modules, self.globals)?;
        }
        let natives = native_bindings(&resolved, &iface, self.globals);
        let iface = Arc::new(iface);
        self.set.publish(Arc::clone(&iface))?;
        if let Some(slot) = self.shared.hashes.get(index(id)?) {
            let _ = slot.set(iface.hash);
        }
        published();
        let tests = if self.mode == Mode::StdTests {
            Tests::All
        } else {
            Tests::SkipStd
        };
        let (thir, late) = check_bodies_with(&types, &iface, tests)?;
        let front_reads =
            hidden_reads(self.modules, id, log.modules(), get(self.targets, id)?, &[]);
        // The summaries read the interfaces again without recording: the
        // reads are taken above.
        let summaries = if info.package == PackageId::LOCAL
            && (self.mode == Mode::Check || self.cache.is_some())
        {
            let quiet = TypesInput {
                deps: DepView::new(self.set, &deps),
                ..types
            };
            Some(self.summaries(&quiet, &iface, &thir)?)
        } else {
            None
        };
        Ok(ModuleOutput {
            syntax: late_syntax.map(<[Diagnostic]>::to_vec).unwrap_or_default(),
            early,
            natives,
            late,
            hash: iface.hash,
            checked: Some(Checked {
                deps,
                front_reads,
                thir,
                drops: resolved.external_drops(),
                interface: iface,
                warnings: Vec::new(),
            }),
            summaries,
            hit: None,
            entry: None,
        })
    }

    /// The reference and use summaries of a package module checked from
    /// source (D-965, D-967): every item by its module's path and its
    /// name, sorted, so they hold no process-local id (D-264).
    #[expect(
        clippy::too_many_lines,
        reason = "the two summaries' items named one by one"
    )]
    fn summaries(
        &self,
        input: &TypesInput<'_>,
        own: &ModuleInterface,
        thir: &thir::Module,
    ) -> Result<Summaries, Ice> {
        let refs = cheby_types::module_refs(input, own, thir)?;
        let name_of = |def: cheby_iface::DefId| -> Option<front::ItemRef> {
            let module = self.modules.get(def.module)?;
            let iface = if def.module == input.module {
                own
            } else {
                self.set.get(def.module)?
            };
            let item = iface.item(def.index)?;
            let name = self.globals.names.get(item.name);
            let item = match item.parent.and_then(|p| iface.item(p)) {
                Some(parent) => format!("{}::{name}", self.globals.names.get(parent.name)),
                None => name.to_owned(),
            };
            Some(front::ItemRef {
                module: module.path.to_string(),
                item,
            })
        };
        let own_name = |index: cheby_iface::DefIndex| -> String {
            input
                .resolved
                .item(index)
                .map(|item| self.globals.names.get(item.name).to_owned())
                .unwrap_or_default()
        };
        let mentioned = |def: cheby_iface::DefId| {
            let kind = if def.module == input.module {
                input.resolved.item(def.index).map(|i| i.kind)
            } else {
                self.set
                    .get(def.module)
                    .and_then(|iface| iface.item(def.index))
                    .map(|i| i.kind)
            };
            matches!(
                kind,
                Some(
                    cheby_iface::DefKind::Fn
                        | cheby_iface::DefKind::Const
                        | cheby_iface::DefKind::Ctor
                )
            )
        };
        let mut by_item: BTreeMap<(String, front::MentionerKind), Vec<front::ItemRef>> =
            BTreeMap::new();
        for (owner, kind) in thir
            .fns
            .iter()
            .map(|f| (f.def.index, front::MentionerKind::Function))
            .chain(
                thir.consts
                    .iter()
                    .map(|c| (c.def.index, front::MentionerKind::Constant)),
            )
            .chain(
                thir.tests
                    .iter()
                    .map(|t| (t.def.index, front::MentionerKind::Test)),
            )
        {
            by_item.entry((own_name(owner), kind)).or_default();
        }
        let owner_kind =
            |owner: cheby_iface::DefIndex| match input.resolved.item(owner).map(|i| i.kind) {
                Some(cheby_iface::DefKind::Const) => front::MentionerKind::Constant,
                Some(cheby_iface::DefKind::Test) => front::MentionerKind::Test,
                _ => front::MentionerKind::Function,
            };
        for &(owner, def) in &refs.mentions {
            if mentioned(def)
                && let Some(item) = name_of(def)
            {
                by_item
                    .entry((own_name(owner), owner_kind(owner)))
                    .or_default()
                    .push(item);
            }
        }
        for &(owner, def) in &refs.hooks {
            if let Some(item) = name_of(def) {
                by_item
                    .entry((own_name(owner), front::MentionerKind::Hook))
                    .or_default()
                    .push(item);
            }
        }
        let references = by_item
            .into_iter()
            .map(|((item, kind), mut mentions)| {
                mentions.sort();
                mentions.dedup();
                front::ReferenceSummary {
                    item,
                    kind,
                    mentions,
                }
            })
            .collect();
        let mut uses: Vec<front::ItemRef> = refs.uses.iter().filter_map(|&d| name_of(d)).collect();
        uses.sort();
        uses.dedup();
        let candidates = refs
            .candidates
            .iter()
            .map(|c| front::UnusedCandidate {
                item: self.globals.names.get(c.name).to_owned(),
                kind: c.kind.to_owned(),
                span: (c.span.range.start(), c.span.range.len()),
            })
            .collect();
        Ok(Summaries {
            references,
            uses: front::UseSummary { uses, candidates },
            complete: refs.complete,
        })
    }

    /// The front key `F1` of package module `id` and its imports (D-955),
    /// or `None` when an import's interface has no hash, so the module is
    /// neither looked up nor written.
    fn front_key(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
    ) -> Result<Option<(Key, Vec<ImportKey>)>, Ice> {
        let Some(source) = cx.hashes.get(index(id)?).copied().flatten() else {
            return Ok(None);
        };
        let mut imports = Vec::new();
        for target in get(self.targets, id)? {
            match target.module() {
                Some(m) => {
                    let Some(hash) = self.shared.hash(m) else {
                        return Ok(None);
                    };
                    imports.push(ImportKey::Module {
                        path: self.modules.try_get(m)?.path.to_string(),
                        hash: Hash128(hash.0),
                    });
                }
                None => imports.push(ImportKey::Unresolved),
            }
        }
        let key = FrontKey {
            package: cx.package.to_owned(),
            targets: self.local_targets,
            module: self.modules.try_get(id)?.path.to_string(),
            source: source.sip128,
            imports,
        };
        Ok(Some((key.key(cx.cache.store.build_id()), key.imports)))
    }

    /// The two-step lookup of package module `id` under its front key
    /// `f1` (D-955): its hidden reads, whose interface hashes extend `f1`
    /// to `F2`, the key of its front record. The package index answers
    /// first (D-962): when its entry for the module has this `F1`, its
    /// reads are the hint's and, when `F2` matches too, its record is the
    /// record, so no file is opened. Otherwise the hint under `F1` names
    /// the reads (a missing hint is the empty set,
    /// D-1137) and the record is read under `F2`. Also
    /// returns what was seen of the hint. A miss is `None`.
    fn find(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        deps: &[ModuleId],
        f1: Key,
    ) -> Result<(Option<Found>, HintSeen), Ice> {
        let store = &cx.cache.store;
        let build = store.build_id();
        let path = self.modules.try_get(id)?.path.to_string();
        let entry = cx
            .use_
            .index
            .and_then(|(index, _)| index.get(&path))
            .filter(|entry| entry.f1 == f1.hash());
        let (paths, hint) = if let Some(entry) = entry {
            let paths = entry.record.reads.clone();
            let seen = HintSeen::Seen((!paths.is_empty()).then(|| paths.clone()));
            (paths, seen)
        } else {
            match store.get_hint(&front::reads_key(f1, build)).ok().flatten() {
                None => (Vec::new(), HintSeen::Seen(None)),
                Some(bytes) => match front::decode_hint(&bytes) {
                    Some(paths) => (paths.clone(), HintSeen::Seen(Some(paths))),
                    None => return Ok((None, HintSeen::Bad)),
                },
            }
        };
        let Some(reads) = self.read_hashes(deps, &paths) else {
            return Ok((None, hint));
        };
        let f2 = front::front_identity(f1, build, &reads);
        if let Some(entry) = entry.filter(|entry| entry.f2 == f2.hash()) {
            return Ok((
                Some(Found {
                    f1,
                    f2,
                    record: entry.record.clone(),
                }),
                hint,
            ));
        }
        let record = store
            .get_record(&f2)
            .ok()
            .flatten()
            .and_then(|bytes| FrontRecord::decode(&bytes))
            .filter(|record| record.reads == paths);
        Ok((record.map(|record| Found { f1, f2, record }), hint))
    }

    /// The paths of hidden reads with the interface hashes their modules
    /// published, or `None` when one is not among `deps` or has no hash.
    fn read_hashes(&self, deps: &[ModuleId], paths: &[String]) -> Option<Vec<(String, Hash128)>> {
        let mut reads = Vec::with_capacity(paths.len());
        for path in paths {
            let read = crate::parse_module_path(path).and_then(|p| self.modules.find(&p))?;
            if !deps.contains(&read) {
                return None;
            }
            let hash = self.shared.hash(read)?;
            reads.push((path.clone(), Hash128(hash.0)));
        }
        Some(reads)
    }

    /// The imports of package module `id` as written, each as its segments
    /// joined by `::`: from its syntax tree's requests, or from the
    /// package index for a module that was not parsed when loaded.
    fn written_imports(&self, cx: FrontCx<'_>, id: ModuleId) -> Result<Option<Vec<String>>, Ice> {
        if get(cx.unparsed, id).copied().unwrap_or(false) {
            let path = self.modules.try_get(id)?.path.to_string();
            return Ok(cx
                .use_
                .index
                .and_then(|(index, _)| index.get(&path))
                .map(|entry| entry.written.clone()));
        }
        Ok(Some(
            get(self.imports, id)?
                .iter()
                .map(|request| {
                    request
                        .path
                        .iter()
                        .map(|&(name, _)| self.globals.names.get(name))
                        .collect::<Vec<_>>()
                        .join("::")
                })
                .collect(),
        ))
    }

    /// The package index's entry of package module `id`, whose record
    /// `record` lies under `f2` (D-1133).
    fn entry(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        f1: Key,
        f2: Key,
        record: FrontRecord,
    ) -> Result<Option<crate::cache::index::IndexEntry>, Ice> {
        let Some(source) = cx.hashes.get(index(id)?).copied().flatten() else {
            return Ok(None);
        };
        let Some(written) = self.written_imports(cx, id)? else {
            return Ok(None);
        };
        Ok(Some(crate::cache::index::IndexEntry {
            path: self.modules.try_get(id)?.path.to_string(),
            source: source.sip128,
            written,
            f1: f1.hash(),
            f2: f2.hash(),
            record,
        }))
    }

    /// Writes the front record of package module `id`, checked from source
    /// as `output`, with its interface artifact, its diagnostics and its
    /// summaries, under the key its reads give, and writes its hint when
    /// its reads differ from what the lookup saw (`hint`, so the hint is
    /// not read again); no hint is written for empty reads unless it
    /// replaces a non-empty one (D-955, D-956, D-1137).
    /// Returns the module's entry for the package index. Nothing is written
    /// for a module with an error (D-964). A write that fails is skipped:
    /// the entry is a miss next time, and no output depends on it (D-268,
    /// D-1129).
    #[expect(
        clippy::too_many_lines,
        reason = "the record's parts, then its hint, in order"
    )]
    fn write_front(
        &self,
        cx: FrontCx<'_>,
        id: ModuleId,
        f1: Key,
        imports: Vec<ImportKey>,
        output: &ModuleOutput,
        hint: HintSeen,
    ) -> Result<Option<crate::cache::index::IndexEntry>, Ice> {
        if output
            .early
            .iter()
            .chain(&output.late)
            .any(Diagnostic::is_error)
        {
            return Ok(None);
        }
        let (Some(checked), Some(summaries)) = (&output.checked, &output.summaries) else {
            return Ok(None);
        };
        let info = self.modules.try_get(id)?;
        let (Some(file), Some(hash)) = (info.file, checked.interface.hash) else {
            return Ok(None);
        };
        // Phase A's diagnostics: those of the parse when the package was
        // loaded, or of the parse after it.
        let syntax: &[Diagnostic] = match get(self.syntax, id)?.as_deref() {
            Some(found) => found,
            None => &output.syntax,
        };
        let store = &cx.cache.store;
        let build = store.build_id();
        let view = DepView::new(self.set, &checked.deps);
        let h = encode(&checked.interface, &view, self.modules, self.globals)?;
        let spans: Vec<Option<(u32, u32)>> =
            cheby_iface::encoded_order(&checked.interface, self.globals)?
                .into_iter()
                .map(|index| {
                    checked
                        .interface
                        .item(index)
                        .and_then(|item| item.span)
                        .filter(|span| span.file == file)
                        .map(|span| (span.range.start(), span.range.len()))
                })
                .collect();
        let artifact = cheby_iface::write_artifact(build.0, &h, &spans);
        let span_cx = SpanCx {
            own: file,
            own_len: cx.text_lens.get(index(id)?).copied().unwrap_or(0),
            modules: self.modules,
            set: self.set,
            globals: self.globals,
        };
        let Some(diagnostics) =
            replay::encode_diagnostics(&[syntax, &output.early, &output.late], &span_cx)
        else {
            // D-965: a span into another module is stored only as an item.
            if cfg!(debug_assertions) {
                return Err(Ice::new(format!(
                    "a diagnostic of `{}` points into another module, but not at one of its items, so it cannot be cached (D-965)",
                    info.path
                )));
            }
            return Ok(None);
        };
        let Some(externals) = externals(&output.natives, file) else {
            return Ok(None);
        };
        let mut reads = Vec::with_capacity(checked.front_reads.len());
        for &read in &checked.front_reads {
            let Some(read_hash) = self.shared.hash(read) else {
                return Ok(None);
            };
            reads.push((
                self.modules.try_get(read)?.path.to_string(),
                Hash128(read_hash.0),
            ));
        }
        reads.sort();
        let (Ok(artifact), Ok(diagnostics)) =
            (store.put_blob(&artifact), store.put_blob(&diagnostics))
        else {
            return Ok(None);
        };
        let record = FrontRecord {
            interface: Hash128(hash.0),
            artifact,
            diagnostics,
            imports,
            externals,
            reads: reads.iter().map(|(path, _)| path.clone()).collect(),
            references: summaries.references.clone(),
            uses: summaries.uses.clone(),
        };
        let f2 = front::front_identity(f1, build, &reads);
        if store.put_record(&f2, &record.encode()).is_err() {
            return Ok(None);
        }
        let hint_key = front::reads_key(f1, build);
        // What the hint holds: `Some(None)` when there is none, `None` when
        // it is malformed and must be written whatever the reads are.
        let current: Option<Option<Vec<String>>> = match hint {
            HintSeen::Seen(paths) => Some(paths),
            HintSeen::Bad => None,
            HintSeen::Unknown => match store.get_hint(&hint_key).ok().flatten() {
                None => Some(None),
                Some(bytes) => front::decode_hint(&bytes).map(Some),
            },
        };
        let write = match current {
            None => true,
            Some(None) => !record.reads.is_empty(),
            Some(Some(paths)) => paths != record.reads,
        };
        if write {
            let _ = store.put_hint(&hint_key, &front::encode_hint(&record.reads));
        }
        self.entry(cx, id, f1, f2, record)
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
    outputs: Vec<OnceLock<Result<ModuleOutput, Stop>>>,
}

impl Schedule<'_> {
    /// Runs every module of `order` (a topological order without the
    /// modules left out by a cycle) on rayon's global pool and returns the
    /// outputs by module id. A module whose dependency failed with an
    /// [`Ice`] never runs; its slot stays empty.
    fn run(inputs: BuildInputs<'_>, order: &[u32]) -> Vec<Option<Result<ModuleOutput, Stop>>> {
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
/// A file whose module path and source hash `skip` accepts (the package
/// index vouches for it, D-962) is read, hashed and added to `sources`,
/// but not parsed ([`crate::Module::parsed`]): the front end parses it
/// only if something needs its tree.
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
    skip: &(dyn Fn(&ModulePath, Hash128) -> bool + Sync),
) -> Result<Vec<crate::Module>, Error> {
    use cheby_diag::{TextRange, TextSize};
    use rayon::prelude::*;

    // Each file is read once and hashed in the same pass, on the worker
    // pool (D-963, D-1130).
    let bytes: Vec<std::io::Result<(Vec<u8>, crate::SourceHashes)>> = files
        .par_iter()
        .map(|file| {
            std::fs::read(root.join(&file.file)).map(|bytes| {
                let hashes = crate::SourceHashes::of(&bytes);
                (bytes, hashes)
            })
        })
        .collect();
    // Into the source map in order: (file, text to parse, `None` for a file
    // that is not parsed, or the E0001 diagnostic of a file that is not
    // UTF-8).
    let mut added: Vec<ToParse> = Vec::with_capacity(files.len());
    let mut hashes = Vec::with_capacity(files.len());
    for (file, bytes) in files.iter().zip(bytes) {
        let (bytes, hash) = bytes.map_err(|e| Error::new(format!("{}: {e}", file.display)))?;
        hashes.push(hash);
        let too_large = |_| Error::from(cheby_package::PackageError::too_large(&file.display));
        match String::from_utf8(bytes) {
            Ok(text) => {
                let text: Arc<str> = Arc::from(text);
                let id = sources
                    .add_file(file.display.as_str(), Arc::clone(&text))
                    .map_err(too_large)?;
                let text = (!skip(&file.path, hash.sip128)).then_some(text);
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
            Ok(None) => (
                crate::ParsedFile {
                    file,
                    syntax: None,
                    has_syntax_errors: false,
                },
                Vec::new(),
            ),
            Ok(Some(text)) => {
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
    for ((file, (source, diags)), hashes) in files.into_iter().zip(parsed).zip(hashes) {
        let parsed = source.syntax.is_some() || source.has_syntax_errors;
        diagnostics.extend(diags.iter().cloned());
        modules.push(crate::Module {
            path: file.path,
            source,
            hashes,
            parsed,
            syntax_diagnostics: diags,
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
    match run_cached(package, mode, sources, diagnostics, None)? {
        Some(front) => Ok(front),
        None => Err(Ice::new("a front end without the cache asked to start again").into()),
    }
}

/// [`run`] with the cache (D-955, D-962, D-965): each package module is
/// looked up by its front key and read set, through the package index
/// first; see [`run_inner`] for which modules are checked. `None` when
/// the front end must start again ([`Stop::Restart`]): the caller loads
/// the package again, parsing every module, and runs it with
/// [`CacheUse::lazy`] false.
pub(crate) fn run_cached(
    package: Package,
    mode: Mode,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    cache: Option<CacheUse<'_>>,
) -> Result<Option<Frontend>, Error> {
    let native = package
        .manifest
        .native
        .clone()
        .map(|table| (package.root.clone(), package.manifest.name.clone(), table));
    let mut front = match run_inner(package, mode, sources, diagnostics, cache) {
        Ok(front) => front,
        Err(Stop::Restart) => return Ok(None),
        Err(Stop::Ice(ice)) => return Err(ice.into()),
    };
    dump_interfaces(&front)?;
    // D-1250: what a later warm `cheby run` or `cheby test` reads of each
    // module checked here instead of checking it again; only once the
    // package checks, as the native step below (D-964): a front end with
    // an error may lack the interfaces the facts name, and the command
    // must print its diagnostics, not an internal error (D-268).
    if mode == Mode::Build
        && cache.is_some_and(|use_| use_.build.is_some())
        && front.complete
        && !diagnostics.iter().any(Diagnostic::is_error)
    {
        crate::warm_keys::store_facts(&front)?;
    }
    // D-941: the commands that run or build code compile the package's C
    // and load its library once the package checks; `cheby check` does
    // neither (D-532).
    if mode != Mode::Check
        && front.complete
        && !diagnostics.iter().any(Diagnostic::is_error)
        && let Some((root, name, table)) = native
    {
        front.native = crate::native::prepare(&root, &name, &table, cache.map(|use_| use_.cache))?;
    }
    Ok(Some(front))
}

/// The front end's phases. With a cache, every package module is looked
/// up (the warm pass), through the package index first (D-962), and
/// output never depends on what the cache held (D-268).
///
/// - `Build` for a warm `cheby run` or `cheby test` (D-1250, K4d,
///   [`crate::warm_keys`]): a module is parsed and checked only when it
///   misses ([`CacheUse::build`]'s misses: edited, its body read, or its
///   constants' module set); every other hit is neither parsed nor
///   checked, its interface is decoded at its lookup with its private
///   items and tests added from its warm facts, its diagnostics and
///   externals are replayed, and the back end loads its records; misses are
///   checked against decoded interfaces and lowered, and every module built
///   is in [`Frontend::order`].
/// - `Build` otherwise (`cheby build`, `--reuse-report`, a command started
///   again): every module is checked once, from source, on fresh
///   interfaces, hit or miss; a hit prints its replayed diagnostics, its
///   artifact is not decoded and nothing is rewritten for it, and a miss is
///   written (D-1132).
/// - `Check`: a hit is neither parsed nor checked. Its interface hash comes
///   from its record, its interface is decoded only when a checked module
///   needs it (D-954), and its diagnostics, externals and use summary are
///   replayed (D-965). A miss is checked against the interfaces at hand,
///   decoded or fresh. Constant evaluation reads bodies, so the package
///   modules holding constants, and every package module they import,
///   transitively, are checked from source on fresh interfaces whether
///   they hit or not (D-1134): decoded
///   interfaces number items in encoded order
///   (D-1121) and are never lowered with bodies
///   checked from source. A module whose check against a decoded
///   interface reports an error is checked again on fresh interfaces,
///   with the modules it reads that were not checked on fresh interfaces,
///   and prints that check (D-1136): a decoded
///   interface holds no `priv` item, which E0043 names (D-953).
///
/// The whole-package pass of `cheby check` (W0005) runs over every
/// module's use summary, and E0054 over every module's natives in build
/// order, a hit's from its record (D-965). Debug toolchains compare a hit
/// that is checked with its replayed diagnostics and interface hash
/// (D-1131).
#[expect(
    clippy::too_many_lines,
    reason = "the front-end phases in order, each a few lines"
)]
fn run_inner(
    package: Package,
    mode: Mode,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    cache: Option<CacheUse<'_>>,
) -> Result<Frontend, Stop> {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut trees: Vec<Option<SyntaxTree>> = vec![None];
    let mut syntax: Vec<Option<Vec<Diagnostic>>> = vec![None];
    let mut unparsed: Vec<bool> = vec![false];
    let package_name = package.manifest.name.clone();
    let local_targets = package.manifest.targets;
    let const_eval_time_limit = package.manifest.const_eval_time_limit;
    let const_eval_memory_limit = package.manifest.const_eval_memory_limit;
    let fiber_stack = package.manifest.fiber_stack;
    let main_stack = package.manifest.main_stack;
    let mut source_hashes: Vec<Option<crate::SourceHashes>> = vec![None];
    for module in package.modules {
        modules.add(ModuleInfo {
            path: module.path,
            package: PackageId::LOCAL,
            file: Some(module.source.file),
        })?;
        trees.push(module.source.syntax);
        unparsed.push(!module.parsed);
        syntax.push(module.parsed.then_some(module.syntax_diagnostics));
        source_hashes.push(Some(module.hashes));
    }
    let package_index = cache.and_then(|c| c.index).map(|(found, _)| found);

    // Imports, module by module in id order; std modules join the table
    // and the worklist as they are reached (B3).
    let mut imports: Vec<Vec<ImportRequest>> = vec![Vec::new()];
    let mut targets: Vec<Vec<ImportTarget>> = vec![Vec::new()];
    let mut implicit: Vec<Vec<ModuleId>> = vec![Vec::new()];
    let mut next = 1_usize;
    let mut std_error = None;
    // K6: std's front records are read instead of parsing std (D-980), in
    // the first attempt of a command that does not lower std itself
    // (`cheby build` does until N9).
    let std_cache = cache.map(|use_| use_.cache);
    let std_hits_ok = crate::std_cache::FRONT_HITS
        && cache.is_some_and(|use_| {
            use_.lazy && use_.cache.reads && (mode == Mode::Check || use_.build.is_some())
        });
    let mut std_hits: BTreeMap<ModuleId, FrontRecord> = BTreeMap::new();
    while next < modules.len() {
        let id = ModuleId::from_u32(u32::try_from(next).map_err(|_| Ice::new("too many modules"))?);
        let info = modules.try_get(id)?.clone();
        let is_unparsed = unparsed.get(next).copied().unwrap_or(false);
        // K6: a std hit's imports, from its record.
        let std_imports = std_hits.get(&id).map(crate::std_cache::import_paths);
        let requests = if is_unparsed || std_imports.is_some() {
            Vec::new()
        } else {
            let tree = trees
                .get(next)
                .and_then(Option::as_ref)
                .ok_or_else(|| Ice::new(format!("module `{}` has no syntax tree", info.path)))?;
            collect_imports(tree, &globals.names)
        };
        let mut lookup = DriverLookup {
            package: &package_name,
            modules: &mut modules,
            sources,
            trees: &mut trees,
            diagnostics,
            std_error: None,
            std_cache,
            std_hits_ok,
            std_hits: &mut std_hits,
        };
        let (found, diags) = if let Some(paths) = std_imports {
            let mut found = Vec::new();
            for path in paths? {
                let segments: Vec<&str> = path.split("::").collect();
                let Lookup::Found(module) = lookup.lookup(info.package, &segments)? else {
                    return Err(Ice::new(format!("std's `{}` imports `{path}`", info.path)).into());
                };
                found.push(ImportTarget::Module(module));
            }
            (found, Vec::new())
        } else if is_unparsed {
            // The imports as written come from the package index, and each
            // must resolve again, against the package's current files, to
            // the module of the same path (D-962).
            let written = package_index
                .and_then(|found| found.get(&info.path.to_string()))
                .map(|entry| entry.written.clone())
                .ok_or(Stop::Restart)?;
            let mut found = Vec::with_capacity(written.len());
            for path in &written {
                let segments: Vec<&str> = path.split("::").collect();
                let Lookup::Found(module) = lookup.lookup(info.package, &segments)? else {
                    return Err(Stop::Restart);
                };
                if lookup.modules.try_get(module)?.path.to_string() != *path {
                    return Err(Stop::Restart);
                }
                found.push(ImportTarget::Module(module));
            }
            (found, Vec::new())
        } else {
            resolve_import_targets(&requests, info.package, &mut lookup, &globals.names)?
        };
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
        ))
        .into());
    }
    // Std modules' hashes were computed when `cheby` was built (D-963).
    for (id, info) in modules.iter().skip(source_hashes.len()) {
        if index(id)? != source_hashes.len() {
            return Err(Ice::new("module ids and source hashes are out of step").into());
        }
        source_hashes.push(std_source_hashes(&info.path));
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
            // E0040 points at imports as written, which a module the index
            // let go unparsed does not have, and a module on or above a
            // cycle is never checked, so its phase-A diagnostics would not
            // be replayed: an import cycle parses every module.
            if unparsed.iter().any(|&u| u) {
                return Err(Stop::Restart);
            }
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
    let len = modules.len();
    let text_lens: Vec<u32> = modules
        .iter()
        .map(|(_, info)| {
            info.file
                .and_then(|f| sources.file(f))
                .map_or(0, |f| u32::try_from(f.text().len()).unwrap_or(u32::MAX))
        })
        .collect();
    unparsed.resize(len, false);
    syntax.resize(len, None);
    // D-1250: a warm `cheby run` or `cheby test` knows before its check
    // which modules miss (K4d).
    let misses = match cache {
        Some(use_) if mode == Mode::Build => crate::warm_keys::misses_before_check(
            use_,
            &crate::warm_keys::FrontState {
                modules: &modules,
                graph: &graph,
                source_hashes: &source_hashes,
                globals: &globals,
                set: &set,
                limits: crate::cache::consts::ConstLimits {
                    time_limit: const_eval_time_limit,
                    memory_limit: const_eval_memory_limit,
                    targets: local_targets,
                },
                package: &package_name,
            },
        )?,
        _ => None,
    };
    let must_check = must_check(
        mode,
        cache,
        &modules,
        &trees,
        &unparsed,
        &graph,
        misses.as_ref(),
    )?;
    let late_trees: Vec<LateTree> = (0..len).map(|_| OnceLock::new()).collect();
    let counted = std::sync::Mutex::new(Vec::new());
    let shared = Shared::new(len);
    // K6: each std hit's record, which `check_module` takes (D-980).
    for (&id, record) in &std_hits {
        if let Some(slot) = shared.records.get(index(id)?) {
            let _ = slot.set(record.clone());
        }
    }
    let sources: &SourceMap = sources;
    let front_cx = |pass| {
        cache.map(|use_| FrontCx {
            cache: use_.cache,
            use_,
            package: &package_name,
            hashes: &source_hashes,
            text_lens: &text_lens,
            unparsed: &unparsed,
            pass,
        })
    };
    let mut outputs = Schedule::run(
        BuildInputs {
            mode,
            globals: &globals,
            modules: &modules,
            sources,
            trees: &trees,
            syntax: &syntax,
            late_trees: &late_trees,
            imports: &imports,
            targets: &targets,
            graph: &graph,
            set: &set,
            known: &known,
            local_targets,
            must_check: &must_check,
            shared: &shared,
            counted: &counted,
            cache: front_cx(Pass::Warm),
        },
        &order,
    );
    if let Some(stop) = first_failure(&mut outputs) {
        return Err(stop);
    }

    // The results, in the topological order (D-264).
    let is_local = |id: ModuleId| {
        modules
            .get(id)
            .is_some_and(|info| info.package == PackageId::LOCAL)
    };
    let mut checked: Vec<Option<Checked>> = (0..len).map(|_| None).collect();
    let mut module_natives: Vec<Vec<(Box<str>, NativeBinding)>> =
        (0..len).map(|_| Vec::new()).collect();
    let mut built = Vec::new();
    // Per module: what it prints, its early and late diagnostics.
    let mut printed: Vec<(Vec<Diagnostic>, Vec<Diagnostic>)> =
        (0..len).map(|_| (Vec::new(), Vec::new())).collect();
    // Phase A's diagnostics of the modules not parsed when the package was
    // loaded.
    let mut late_syntax: Vec<Diagnostic> = Vec::new();
    // Per package module: its use summary and whether its bodies all
    // checked, for the whole-package pass (D-965).
    let mut uses: Vec<Option<(front::UseSummary, bool)>> = (0..len).map(|_| None).collect();
    let mut hits = vec![false; len];
    // Per module: whether it was checked from source with every package
    // module it reads checked from source too (its THIR and interface are
    // fresh, so constant evaluation may lower them).
    let mut fresh = vec![false; len];
    let mut entries = Vec::new();
    let mut warm_f2: BTreeMap<ModuleId, Hash128> = BTreeMap::new();
    for &node in &order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let at = index(id)?;
        let output = outputs
            .get_mut(at)
            .and_then(Option::take)
            .ok_or_else(|| Ice::new(format!("module {id:?} was never checked")))?
            .map_err(|_| Ice::new(format!("module {id:?} failed")))?;
        late_syntax.extend(output.syntax);
        let is_fresh = output.checked.as_ref().is_some_and(|c| {
            c.deps.iter().all(|&d| {
                !is_local(d)
                    || fresh
                        .get(index(d).unwrap_or(usize::MAX))
                        .copied()
                        .unwrap_or(false)
            })
        });
        // K6: a std hit is never lowered (its code comes from its record),
        // so it stays in the order later phases walk (D-980).
        let is_fresh = is_fresh || std_hits.contains_key(&id);
        if let Some(slot) = fresh.get_mut(at) {
            *slot = is_fresh;
        }
        if let Some(slot) = hits.get_mut(at) {
            *slot = output.hit.is_some();
        }
        if let Some(slot) = uses.get_mut(at) {
            *slot = match (&output.summaries, &output.hit) {
                (Some(s), _) => Some((s.uses.clone(), s.complete)),
                (None, Some(record)) => Some((record.uses.clone(), true)),
                (None, None) => None,
            };
        }
        // The front identity that keys the module's warm facts (D-1250).
        if let Some(entry) = &output.entry
            && is_local(id)
        {
            warm_f2.insert(id, entry.f2);
        }
        entries.extend(output.entry);
        if let Some(slot) = printed.get_mut(at) {
            *slot = (output.early, output.late);
        }
        if let Some(slot) = module_natives.get_mut(at) {
            *slot = output.natives;
        }
        if let Some(slot) = checked.get_mut(at) {
            *slot = output.checked;
        }
        built.push(id);
    }
    // `cheby check`'s error path (D-1136): a module
    // whose check against a decoded interface reported an error, and the
    // package modules it reads that were not checked on fresh interfaces,
    // are checked again from source on fresh interfaces. A hit among them
    // prints its replayed diagnostics, the others their new check.
    let has_error =
        |p: &(Vec<Diagnostic>, Vec<Diagnostic>)| p.0.iter().chain(&p.1).any(Diagnostic::is_error);
    let mut again = vec![false; len];
    for &id in &built {
        let at = index(id)?;
        if !printed.get(at).is_some_and(has_error) {
            continue;
        }
        let Some(c) = get(&checked, id)?.as_ref() else {
            continue;
        };
        // K6: a decoded std interface holds no `priv` item either (D-953),
        // so a module with an error that read a std hit is checked again,
        // with those std modules checked from source (D-980's miss path).
        let read_std_hit = c.deps.iter().any(|d| std_hits.contains_key(d));
        if fresh.get(at).copied().unwrap_or(true) && !read_std_hit {
            continue;
        }
        if let Some(slot) = again.get_mut(at) {
            *slot = true;
        }
        for &dep in &c.deps {
            let d = index(dep)?;
            let std_hit = std_hits.contains_key(&dep);
            if (std_hit || (is_local(dep) && !fresh.get(d).copied().unwrap_or(false)))
                && let Some(slot) = again.get_mut(d)
            {
                if std_hit
                    && !*slot
                    && let Some(cache) = std_cache
                {
                    cache
                        .log
                        .parsed_std(&modules.try_get(dep)?.path.to_string());
                }
                *slot = true;
            }
        }
    }
    if again.iter().any(|&a| a) {
        let mut fresh_set = InterfaceSet::new(builtin::interface(&globals));
        fresh_set.reserve_modules(len)?;
        for &id in &built {
            let at = index(id)?;
            if !again.get(at).copied().unwrap_or(false)
                && fresh.get(at).copied().unwrap_or(false)
                && let Some(c) = get(&checked, id)?
            {
                fresh_set.publish(Arc::clone(&c.interface))?;
            }
        }
        let again_order: Vec<u32> = order
            .iter()
            .copied()
            .filter(|&n| {
                usize::try_from(n)
                    .ok()
                    .and_then(|n| again.get(n))
                    .copied()
                    .unwrap_or(false)
            })
            .collect();
        let recheck_shared = Shared::new(len);
        let mut outputs = Schedule::run(
            BuildInputs {
                mode,
                globals: &globals,
                modules: &modules,
                sources,
                trees: &trees,
                syntax: &syntax,
                late_trees: &late_trees,
                imports: &imports,
                targets: &targets,
                graph: &graph,
                set: &fresh_set,
                known: &known,
                local_targets,
                must_check: &must_check,
                shared: &recheck_shared,
                counted: &counted,
                cache: front_cx(Pass::Recheck),
            },
            &again_order,
        );
        if let Some(stop) = first_failure(&mut outputs) {
            return Err(stop);
        }
        for &node in &again_order {
            let id = ModuleId::from_u32(node);
            let at = index(id)?;
            let output = outputs
                .get_mut(at)
                .and_then(Option::take)
                .ok_or_else(|| Ice::new(format!("module {id:?} was never checked again")))?
                .map_err(|_| Ice::new(format!("module {id:?} failed")))?;
            let slot = printed
                .get_mut(at)
                .ok_or_else(|| Ice::new("no warm result"))?;
            if hits.get(at).copied().unwrap_or(false) {
                // A hit prints its replayed diagnostics, which must be the
                // fresh ones (D-1131).
                if cfg!(debug_assertions) && (slot.0 != output.early || slot.1 != output.late) {
                    return Err(Stop::Ice(Ice::new(format!(
                        "module `{}` checked from source gives other diagnostics than the cache's (D-954)",
                        modules.try_get(id)?.path
                    ))));
                }
            } else {
                *slot = (output.early, output.late);
                if let Some(natives) = module_natives.get_mut(at) {
                    *natives = output.natives;
                }
            }
            if let (Some(slot), Some(s)) = (uses.get_mut(at), output.summaries) {
                *slot = Some((s.uses, s.complete));
            }
            if let Some(slot) = checked.get_mut(at) {
                *slot = output.checked;
            }
            if let Some(slot) = fresh.get_mut(at) {
                *slot = true;
            }
        }
        set = fresh_set;
    }

    // The diagnostics in build order, with E0055 for the runtime's names
    // bound outside std (D-944) and E0054 over every module's native
    // bindings, a hit's from its record (D-965).
    diagnostics.extend(late_syntax);
    let mut natives = BTreeMap::new();
    for &id in &built {
        let at = index(id)?;
        let (early, late) = printed.get_mut(at).map(std::mem::take).unwrap_or_default();
        diagnostics.extend(early);
        let mut bindings = get(&module_natives, id)?.clone();
        if modules.try_get(id)?.package != PackageId::STD {
            bindings = reserve_exports(bindings, diagnostics);
        }
        bind_natives(bindings, &mut natives, diagnostics);
        diagnostics.extend(late);
    }
    // The whole-package pass (W0005, D-516) over every module's use
    // summary (D-965): it needs every module of the package, and a module
    // on or above an import cycle was not checked (E0040 is reported).
    let every_local_built = modules
        .iter()
        .filter(|(_, info)| info.package == PackageId::LOCAL)
        .all(|(id, _)| {
            index(id)
                .ok()
                .and_then(|at| uses.get(at))
                .is_some_and(Option::is_some)
        });
    if mode == Mode::Check && every_local_built {
        diagnostics.extend(unused_package_items(&modules, &built, &uses)?);
    }
    // The modules later phases may lower: std's and the package's checked
    // on fresh interfaces (D-1134).
    let lowerable: Vec<ModuleId> = built
        .iter()
        .copied()
        .filter(|&id| {
            index(id)
                .ok()
                .and_then(|at| fresh.get(at))
                .copied()
                .unwrap_or(false)
        })
        .collect();
    // D-1250: a warm `cheby run` or `cheby test` lowers what it checked (a
    // miss checked against decoded hits included) and loads the rest from
    // records, so every module it built is in the order; a hit's warm facts
    // stand in for its THIR. Facts that are missing or do not decode start
    // the front end again, checking every module.
    let mut warm = BTreeMap::new();
    let lowerable = match (&misses, cache) {
        (Some(_), Some(_)) => {
            let naming = crate::warm_keys::Naming {
                modules: &modules,
                set: &set,
                globals: &globals,
            };
            for &id in &built {
                if !is_local(id) || get(&checked, id)?.is_some() {
                    continue;
                }
                let file = modules.try_get(id)?.file;
                let facts = shared
                    .facts
                    .get(index(id)?)
                    .and_then(OnceLock::get)
                    .cloned();
                let (Some(facts), Some(file)) = (facts, file) else {
                    return Err(Stop::Restart);
                };
                let module =
                    crate::warm_keys::warm_module(facts, id, file, &naming).ok_or(Stop::Restart)?;
                warm.insert(id, module);
            }
            built.clone()
        }
        _ => lowerable,
    };
    let records = cache.map(|use_| crate::borrowing::RecordSink::new(use_.cache, &entries));
    if let Some(use_) = cache {
        finish_cache(
            use_,
            &modules,
            &built,
            &hits,
            &unparsed,
            &late_trees,
            entries,
            counted,
        )?;
    }
    Ok(Frontend {
        globals,
        modules,
        set,
        checked,
        order: lowerable,
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
        source_hashes,
        complete: true,
        native: crate::native::PackageNative::default(),
        records,
        cache_store: cache.map(|use_| (use_.cache.store.share(), use_.cache.reads)),
        warm_f2,
        warm,
        wanted: std::sync::atomic::AtomicBool::new(false),
    })
}

/// The first failure among a schedule's outputs: a [`Stop::Restart`] if
/// any, else the first internal error.
fn first_failure(outputs: &mut [Option<Result<ModuleOutput, Stop>>]) -> Option<Stop> {
    let mut ice = None;
    for slot in outputs.iter_mut() {
        if matches!(slot, Some(Err(_)))
            && let Some(Err(stop)) = slot.take()
        {
            match stop {
                Stop::Restart => return Some(Stop::Restart),
                Stop::Ice(found) => {
                    ice.get_or_insert(found);
                }
            }
        }
    }
    ice.map(Stop::Ice)
}

/// Which modules are checked from source even when their lookup hits:
/// every one in `StdTests` mode and without a cache; in `Build` mode with
/// a cache and `misses` (D-1250, K4d), the package modules of `misses`
/// (code, tests, body reads, constant modules) and every std module, and
/// without `misses` every module; in
/// `Check` mode the package modules constant evaluation needs, those that
/// hold a constant (D-459, D-534) and every package module they import,
/// transitively, which holds every module their reference summaries reach
/// (D-967; D-1134). Whether a module holds a
/// constant is a fact of its source: its tree's, or for a module the index
/// let go unparsed, its record's summary.
fn must_check(
    mode: Mode,
    cache: Option<CacheUse<'_>>,
    modules: &ModuleTable,
    trees: &[Option<SyntaxTree>],
    unparsed: &[bool],
    graph: &Graph,
    misses: Option<&crate::warm_keys::BuildMisses>,
) -> Result<Vec<bool>, Stop> {
    let len = modules.len();
    if let (Mode::Build, Some(_), Some(misses)) = (mode, cache, misses) {
        return Ok(modules
            .iter()
            .map(|(id, info)| info.package != PackageId::LOCAL || misses.checks(id))
            .collect());
    }
    let Some(use_) = cache.filter(|_| mode == Mode::Check) else {
        return Ok(vec![true; len]);
    };
    let mut holders = Vec::new();
    for (id, info) in modules.iter() {
        if info.package != PackageId::LOCAL {
            continue;
        }
        let at = index(id)?;
        let holds = if unparsed.get(at).copied().unwrap_or(false) {
            use_.index
                .and_then(|(index, _)| index.get(&info.path.to_string()))
                .map(|entry| entry.record.holds_constant())
                .ok_or(Stop::Restart)?
        } else {
            trees
                .get(at)
                .and_then(Option::as_ref)
                .and_then(|tree| {
                    <cheby_syntax::ast::SourceFile<'_> as cheby_syntax::ast::AstNode<'_>>::cast(
                        tree.root(),
                    )
                })
                .is_some_and(|file| {
                    file.items()
                        .any(|item| matches!(item, cheby_syntax::ast::Item::Const(_)))
                })
        };
        if holds {
            holders.push(id.as_u32());
        }
    }
    let mut out = vec![false; len];
    for node in reachable(graph, &holders) {
        let id = ModuleId::from_u32(node);
        if modules
            .get(id)
            .is_some_and(|info| info.package == PackageId::LOCAL)
            && let Some(slot) = out.get_mut(index(id)?)
        {
            *slot = true;
        }
    }
    Ok(out)
}

/// W0005 over every package module's use summary (D-516, D-965): each
/// package-visible candidate that no module of the package uses. Nothing
/// is reported when a module's bodies did not all check, since they could
/// hold the only uses.
fn unused_package_items(
    modules: &ModuleTable,
    built: &[ModuleId],
    uses: &[Option<(front::UseSummary, bool)>],
) -> Result<Vec<Diagnostic>, Ice> {
    let local: Vec<ModuleId> = built
        .iter()
        .copied()
        .filter(|&id| {
            modules
                .get(id)
                .is_some_and(|info| info.package == PackageId::LOCAL)
        })
        .collect();
    let mut summaries = Vec::with_capacity(local.len());
    for &id in &local {
        match get(uses, id)? {
            Some((summary, true)) => summaries.push((id, summary)),
            _ => return Ok(Vec::new()),
        }
    }
    let used_anywhere: std::collections::BTreeSet<&front::ItemRef> =
        summaries.iter().flat_map(|(_, s)| &s.uses).collect();
    let mut out = Vec::new();
    for (id, summary) in summaries {
        let info = modules.try_get(id)?;
        let Some(file) = info.file else {
            continue;
        };
        let module = info.path.to_string();
        for c in &summary.candidates {
            let item = front::ItemRef {
                module: module.clone(),
                item: c.item.clone(),
            };
            if !used_anywhere.contains(&item) {
                let span = Span::new(file, cheby_diag::TextRange::at(c.span.0, c.span.1));
                out.push(cheby_types::unused_package_item(&c.kind, &c.item, span));
            }
        }
    }
    Ok(out)
}

/// The end of a front end's use of the cache: the package index, written
/// when its bytes differ from the one read (D-1133); the
/// log's lines, one hit or miss per package module and one `parse` per
/// package module parsed (D-973, D-1135); and the driver's
/// counts of parses and checks.
#[expect(
    clippy::too_many_arguments,
    reason = "the front end's state at its end"
)]
fn finish_cache(
    use_: CacheUse<'_>,
    modules: &ModuleTable,
    built: &[ModuleId],
    hits: &[bool],
    unparsed: &[bool],
    late_trees: &[LateTree],
    mut entries: Vec<crate::cache::index::IndexEntry>,
    counted: std::sync::Mutex<Vec<String>>,
) -> Result<(), Ice> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    let bytes = crate::cache::index::PackageIndex { modules: entries }.encode();
    if use_.index.map(|(_, read)| read.as_slice()) != Some(bytes.as_slice()) {
        let _ = use_.cache.store.put_hint(&use_.index_key, &bytes);
    }
    for &id in built {
        let info = modules.try_get(id)?;
        if info.package == PackageId::LOCAL {
            let hit = hits.get(index(id)?).copied().unwrap_or(false);
            use_.cache.log.record("front", &info.path.to_string(), hit);
        }
    }
    let mut parsed = Vec::new();
    for (id, info) in modules.iter() {
        if info.package != PackageId::LOCAL {
            continue;
        }
        let at = index(id)?;
        let late = late_trees.get(at).is_some_and(|slot| slot.get().is_some());
        if !unparsed.get(at).copied().unwrap_or(false) || late {
            let path = info.path.to_string();
            use_.cache.log.parsed(&path);
            parsed.push(path);
        }
    }
    let checked = counted
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use_.cache.count(parsed, checked);
    Ok(())
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
        let modules = super::read_modules(
            &dir,
            discovered,
            &mut par_sources,
            &mut par_diags,
            &|_, _| false,
        )
        .unwrap();
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
