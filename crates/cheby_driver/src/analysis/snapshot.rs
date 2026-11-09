//! `Snapshot`: one immutable generation of a package's analysis.
//!
//! Everything keyed by a handle of one run (`ModuleId`, `DefId`, `FileId`,
//! `Ty`, `NodeId`, …) lives in the snapshot of that run and is read through
//! it. [`Symbol`] is the only identity that crosses snapshots (D-1365):
//! [`Snapshot::key_of`] names a definition and [`Snapshot::find`] maps the
//! name back into another snapshot.
//!
//! Nothing lowers a snapshot's front end: the field is private and no
//! method hands it to the back end, which is what lets a run with
//! `Retain::LIVE` drop std's bodies (D-1361).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Code, FileId, SourceFile, SourceMap, TextRange};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, Generics, Globals, Ice, InterfaceSet, ItemSig, ModuleId,
    ModuleInterface, ModuleTable, Name, Namespace, PackageId, ParamIdx,
};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::{ImportRequest, ImportTarget, ResolvedModule};
use cheby_syntax::SyntaxTree;
use cheby_types::facts::ModuleFacts;
use cheby_types::satisfaction::QueryInput;
use cheby_types::{TypesInput, thir};

use super::symbol::{Member, Symbol, SymbolNs};
use crate::frontend::{Checked, Frontend};

/// Which generation a snapshot is: the session that made it and its run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId {
    /// The session, unique in the process.
    pub session: u32,
    /// The session's run, from 1.
    pub generation: u64,
}

/// Where a file of a snapshot lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileLocation {
    /// A file of the package, by its absolute path.
    Package(PathBuf),
    /// A std file by its display path, such as `std/list.cheby`.
    Std(String),
    /// A file with no place on disk.
    Synthetic,
}

/// What handlers read about the state of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisStatus {
    /// The files of this run with phase-A errors. Rename reads this.
    pub broken: BTreeSet<PathBuf>,
    /// The imports a cycle cut made unresolved (D-1358): the importing
    /// file and the import's range in it.
    pub cut_imports: Vec<(PathBuf, TextRange)>,
    /// The run used the last good manifest, since the live one gave none
    /// (A §2.7).
    pub manifest_is_last_good: bool,
}

/// One entry of [`Snapshot::baseline`]: a diagnostic of the run as a
/// verify compares it, by code, file and message. `path` is `None` for a
/// diagnostic with no span.
pub type BaselineEntry = (Code, Option<PathBuf>, String);

/// What [`Snapshot::find`] maps a [`Symbol`] to in one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolTarget {
    /// A module.
    Module(ModuleId),
    /// An item: a function, constant, type, alias, interface, constructor,
    /// interface function or test.
    Item(DefId),
    /// A field of the type `ty`, over all its constructors (D-071).
    Field {
        /// The type.
        ty: DefId,
        /// The field's name.
        name: Name,
    },
    /// A type parameter of an item.
    TypeParam {
        /// The item that declares it.
        item: DefId,
        /// Its index in the item's generics.
        param: ParamIdx,
    },
    /// A local of an item with a body. Which binding it is, the index finds
    /// by name and ordinal in the item's body, and a request that crosses
    /// snapshots compares the token text too (plan §1.3).
    Local {
        /// The item whose body declares it.
        item: DefId,
        /// The local's name.
        name: Name,
        /// Which local of that name, in source order.
        ordinal: u32,
    },
    /// An import's `as` name.
    ImportAlias {
        /// The importing module.
        module: ModuleId,
        /// The import, by its index in the module's imports as written.
        import: usize,
        /// The item of `import a::{f as g}` whose alias it is, by its index
        /// in the import's list. `None` for the module's alias, `import a
        /// as b`.
        item: Option<usize>,
    },
}

/// The parts [`super::session::Session`] builds a snapshot from.
pub(crate) struct Parts {
    pub id: SnapshotId,
    pub root: PathBuf,
    pub front: Frontend,
    pub sources: SourceMap,
    /// The editor's version of each open file of the run.
    pub versions: BTreeMap<PathBuf, i32>,
    /// The package's files of the run, by absolute path.
    pub files: BTreeMap<PathBuf, FileId>,
    pub broken: BTreeSet<PathBuf>,
    pub baseline: Vec<BaselineEntry>,
    /// The run's targets, after `--target` narrowed the manifest's.
    pub targets: Targets,
    /// The run's manifest is the last good one, not the live one.
    pub manifest_is_last_good: bool,
    /// The run is of one file in single-file mode (D-1371).
    pub single_file: bool,
}

/// One immutable generation: everything keyed by run-local ids lives here.
pub struct Snapshot {
    id: SnapshotId,
    root: PathBuf,
    front: Frontend,
    sources: SourceMap,
    versions: BTreeMap<PathBuf, i32>,
    files: BTreeMap<PathBuf, FileId>,
    /// By file id.
    locations: Vec<FileLocation>,
    /// By file id: the module the file holds.
    modules_by_file: Vec<Option<ModuleId>>,
    /// By module id: the modules that import it directly, sorted.
    importers: Vec<Vec<ModuleId>>,
    /// Every module with an interface, sorted.
    all_ids: Vec<ModuleId>,
    baseline: Vec<BaselineEntry>,
    targets: Targets,
    status: AnalysisStatus,
    single_file: bool,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("modules", &self.front.modules.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// `id` as an index into a per-module list.
fn at(id: ModuleId) -> Option<usize> {
    usize::try_from(id.as_u32()).ok()
}

/// The module id of index `n` of a per-module list.
fn module_id(n: usize) -> Option<ModuleId> {
    u32::try_from(n).ok().map(ModuleId::from_u32)
}

impl Snapshot {
    /// The snapshot of a finished run. Only the session builds one.
    pub(crate) fn new(parts: Parts) -> Self {
        let Parts {
            id,
            root,
            front,
            sources,
            versions,
            files,
            broken,
            baseline,
            targets,
            manifest_is_last_good,
            single_file,
        } = parts;
        let count = sources.files().count();
        let mut locations = vec![FileLocation::Synthetic; count];
        let mut modules_by_file = vec![None; count];
        for (path, &file) in &files {
            if let Some(slot) = usize::try_from(file.index())
                .ok()
                .and_then(|n| locations.get_mut(n))
            {
                *slot = FileLocation::Package(path.clone());
            }
        }
        for (module, info) in front.modules.iter() {
            let Some(file) = info.file else { continue };
            let Some(n) = usize::try_from(file.index()).ok() else {
                continue;
            };
            if let Some(slot) = modules_by_file.get_mut(n) {
                *slot = Some(module);
            }
            if info.package != PackageId::LOCAL
                && let (Some(slot), Some(source)) = (locations.get_mut(n), sources.file(file))
            {
                *slot = FileLocation::Std(source.path().to_owned());
            }
        }
        let mut importers = vec![Vec::new(); front.modules.len()];
        for (from, targets) in front.import_targets.iter().enumerate() {
            let Some(from) = module_id(from) else {
                continue;
            };
            for to in targets.iter().filter_map(|t| t.module()) {
                if let Some(list) = at(to).and_then(|n| importers.get_mut(n)) {
                    list.push(from);
                }
            }
        }
        for list in &mut importers {
            list.sort_unstable();
            list.dedup();
        }
        let all_ids = front
            .modules
            .iter()
            .map(|(module, _)| module)
            .filter(|&module| front.set.get(module).is_some())
            .collect();
        let cut_imports = front
            .cut_imports
            .iter()
            .filter_map(|&(module, span)| {
                let file = front.modules.get(module)?.file?;
                let path = files.iter().find(|&(_, &f)| f == file)?.0.clone();
                Some((path, span.range))
            })
            .collect();
        Self {
            id,
            root,
            front,
            sources,
            versions,
            files,
            locations,
            modules_by_file,
            importers,
            all_ids,
            baseline,
            targets,
            status: AnalysisStatus {
                broken,
                cut_imports,
                manifest_is_last_good,
            },
            single_file,
        }
    }

    /// Which generation this is.
    #[must_use]
    pub const fn id(&self) -> SnapshotId {
        self.id
    }

    /// The package root, absolute.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The run is of one file in single-file mode (D-1371), whose package
    /// and module are the synthetic `script`, a name no answer shows
    /// (D-1477).
    #[must_use]
    pub const fn is_single_file(&self) -> bool {
        self.single_file
    }

    /// The state of the run.
    #[must_use]
    pub const fn status(&self) -> &AnalysisStatus {
        &self.status
    }

    /// The editor's version of `path` in this run. `None` for a file read
    /// from disk.
    #[must_use]
    pub fn version_of(&self, path: &Path) -> Option<i32> {
        self.versions.get(path).copied()
    }

    /// The file of the package at the absolute `path`.
    #[must_use]
    pub fn file_of(&self, path: &Path) -> Option<FileId> {
        self.files.get(path).copied()
    }

    /// Where `file` lives.
    #[must_use]
    pub fn location(&self, file: FileId) -> Option<&FileLocation> {
        self.locations.get(usize::try_from(file.index()).ok()?)
    }

    /// The text of `file` in this run.
    #[must_use]
    pub fn source(&self, file: FileId) -> Option<&SourceFile> {
        self.sources.file(file)
    }

    /// The run's source map.
    #[must_use]
    pub const fn sources(&self) -> &SourceMap {
        &self.sources
    }

    /// The module `file` holds.
    #[must_use]
    pub fn module_at(&self, file: FileId) -> Option<ModuleView<'_>> {
        let n = usize::try_from(file.index()).ok()?;
        self.modules_by_file
            .get(n)
            .copied()
            .flatten()
            .and_then(|id| self.module(id))
    }

    /// The hash of the bytes the module file `file` was read from
    /// (D-963). For a file that was not UTF-8 it is not its lossy text's.
    pub(crate) fn bytes_hash(&self, file: FileId) -> Option<crate::cache::Hash128> {
        let id = self.module_at(file)?.id();
        self.front.source_hashes(id).map(|h| h.sip128)
    }

    /// The module `id`.
    #[must_use]
    pub fn module(&self, id: ModuleId) -> Option<ModuleView<'_>> {
        self.front
            .modules
            .get(id)
            .map(|_| ModuleView { snapshot: self, id })
    }

    /// The package's modules, in id order (display order).
    pub fn local_modules(&self) -> impl Iterator<Item = ModuleView<'_>> {
        self.all_modules().filter(|m| m.is_local())
    }

    /// Every module of the run, the builtin module and std included, in id
    /// order.
    pub fn all_modules(&self) -> impl Iterator<Item = ModuleView<'_>> {
        self.front
            .modules
            .iter()
            .map(move |(id, _)| ModuleView { snapshot: self, id })
    }

    /// Every module with an interface, sorted: the view of a query
    /// ([`Snapshot::query_input`]).
    #[must_use]
    pub fn all_ids(&self) -> &[ModuleId] {
        &self.all_ids
    }

    /// The modules that import `id` directly, sorted.
    #[must_use]
    pub fn importers(&self, id: ModuleId) -> &[ModuleId] {
        at(id)
            .and_then(|n| self.importers.get(n))
            .map_or(&[], Vec::as_slice)
    }

    /// The run's interners.
    #[must_use]
    pub fn globals(&self) -> &Globals {
        &self.front.globals
    }

    /// What a run beside this one shares with it: the interners and the
    /// targets, after `--target` (plan §1.6). `verify` runs on them.
    pub(crate) fn run_context(&self) -> (Arc<Globals>, Targets) {
        (Arc::clone(&self.front.globals), self.targets)
    }

    /// Every module of the run, by id and by path.
    #[must_use]
    pub const fn table(&self) -> &ModuleTable {
        &self.front.modules
    }

    /// Every interface of the run.
    #[must_use]
    pub const fn interfaces(&self) -> &InterfaceSet {
        &self.front.set
    }

    /// The run's diagnostics as a verify compares them: a sorted multiset
    /// of code, file and message (plan §1.6).
    #[must_use]
    pub fn baseline(&self) -> &[BaselineEntry] {
        &self.baseline
    }

    /// The modules the run lost to an internal error, with the error
    /// (D-1362). Each module that imports one and never ran is missing too,
    /// with no entry here.
    #[must_use]
    pub fn lost(&self) -> &[(ModuleId, Ice)] {
        &self.front.lost
    }

    /// Whether the run lost a body with no diagnostic anywhere: what ends a
    /// batch run as an internal error (D-464).
    #[must_use]
    pub const fn silent_drop(&self) -> bool {
        self.front.silent_drop
    }

    /// What a satisfaction query reads (D-1386): `home`'s module, package
    /// and interface, with `tree` and `resolved` of `from`, over a view of
    /// every loaded module. For queries that never feed a check. `None`
    /// when `home` has no interface, `from` keeps no tree or resolution, or
    /// `from` is a view of another snapshot, whose tree and resolution
    /// would mix with this run's (A §1.5).
    #[must_use]
    pub fn query_input<'s>(
        &'s self,
        home: ModuleId,
        from: ModuleView<'s>,
    ) -> Option<QueryInput<'s>> {
        if !std::ptr::eq(from.snapshot, self) {
            return None;
        }
        let info = self.front.modules.get(home)?;
        let own = self.front.set.get(home)?;
        Some(QueryInput {
            input: TypesInput {
                module: home,
                package: info.package,
                tree: from.tree()?,
                resolved: from.resolved()?,
                deps: DepView::new(&self.front.set, &self.all_ids),
                modules: &self.front.modules,
                globals: &self.front.globals,
            },
            own,
        })
    }

    /// The stable name of the item `def` (D-1365). `None` for an item the
    /// run does not have, and for one its name no longer refers to.
    #[must_use]
    pub fn key_of(&self, def: DefId) -> Option<Symbol> {
        let module = self.front.modules.get(def.module)?.path.clone();
        let iface = self.front.set.get(def.module)?;
        let item = iface.item(def.index)?;
        if iface.is_hidden(def.index) || !iface.is_current(def.index) {
            return None;
        }
        let names = &self.front.globals.names;
        let name = names.get(item.name);
        Some(match item.kind {
            DefKind::Fn | DefKind::Const | DefKind::Ctor => {
                Symbol::item(module, SymbolNs::Value, name)
            }
            DefKind::Type | DefKind::Alias | DefKind::Interface => {
                Symbol::item(module, SymbolNs::Type, name)
            }
            DefKind::Test => Symbol::item(module, SymbolNs::Test, name),
            DefKind::InterfaceFn => {
                let parent = iface.item(item.parent?)?;
                Symbol::interface_fn(module, names.get(parent.name), name)
            }
        })
    }

    /// The handle `symbol` names in this snapshot (plan §1.3). Items with a
    /// namespace go through the interface's lookup; a test, an interface
    /// function, a field and a type parameter through a scan of the
    /// interface. `None` when the symbol no longer resolves.
    #[must_use]
    pub fn find(&self, symbol: &Symbol) -> Option<SymbolTarget> {
        let module = self.find_module(&symbol.module)?;
        let names = &self.front.globals.names;
        match (&symbol.member, symbol.ns) {
            (Member::None, SymbolNs::Module) => {
                return symbol
                    .name
                    .is_empty()
                    .then_some(SymbolTarget::Module(module));
            }
            (_, SymbolNs::Module) => return None,
            (Member::ImportAlias(alias), ns) => return self.find_alias(module, ns, alias),
            _ => {}
        }
        let iface = self.front.set.get(module)?;
        let index = match symbol.ns {
            SymbolNs::Type => iface.lookup(Namespace::Type, names.lookup(&symbol.name)?)?,
            SymbolNs::Value => iface.lookup(Namespace::Value, names.lookup(&symbol.name)?)?,
            SymbolNs::Test => {
                let name = names.lookup(&symbol.name)?;
                scan(iface, |index, item| {
                    item.kind == DefKind::Test && item.name == name && iface.is_current(index)
                })?
            }
            SymbolNs::Module => return None,
        };
        let item = DefId::new(module, index);
        match &symbol.member {
            Member::None => Some(SymbolTarget::Item(item)),
            Member::InterfaceFn(func) => {
                let func = names.lookup(func)?;
                let found = scan(iface, |_, f| {
                    f.kind == DefKind::InterfaceFn && f.parent == Some(index) && f.name == func
                })?;
                Some(SymbolTarget::Item(DefId::new(module, found)))
            }
            Member::Field(field) => {
                let name = names.lookup(field)?;
                has_field(iface, index, name).then_some(SymbolTarget::Field { ty: item, name })
            }
            Member::TypeParam(param) => {
                let name = names.lookup(param)?;
                let generics = sig_generics(&iface.item(index)?.sig)?;
                let param = generics
                    .params
                    .iter_enumerated()
                    .find(|(_, p)| p.name == name)
                    .map(|(param, _)| param)?;
                Some(SymbolTarget::TypeParam { item, param })
            }
            Member::Local { name, ordinal } => {
                let name = names.lookup(name)?;
                let resolved = self.module(module)?.resolved()?;
                resolved
                    .bodies
                    .contains_key(&index)
                    .then_some(SymbolTarget::Local {
                        item,
                        name,
                        ordinal: *ordinal,
                    })
            }
            Member::ImportAlias(_) => None,
        }
    }

    /// The import of `module` that binds `alias` in `ns` (D-1365). A
    /// module's alias is a value (D-062). An item's alias is bound in each
    /// namespace where the imported module has the item, so `Point as P`
    /// binds a type and a constructor (D-111). `None` on a namespace that
    /// does not match, or an item import whose target failed.
    fn find_alias(&self, module: ModuleId, ns: SymbolNs, alias: &str) -> Option<SymbolTarget> {
        let n = at(module)?;
        let alias = self.front.globals.names.lookup(alias)?;
        let requests = self.front.imports.get(n)?;
        let targets = self.front.import_targets.get(n);
        requests.iter().enumerate().find_map(|(import, request)| {
            if request.items.is_none() && request.alias.is_some_and(|(name, _)| name == alias) {
                return (ns == SymbolNs::Value).then_some(SymbolTarget::ImportAlias {
                    module,
                    import,
                    item: None,
                });
            }
            let (item, found) = request
                .items
                .iter()
                .flatten()
                .enumerate()
                .find(|(_, item)| item.alias.is_some_and(|(name, _)| name == alias))?;
            let target = targets
                .and_then(|t| t.get(import))
                .and_then(|t| t.module())?;
            let iface = self.front.set.get(target)?;
            let space = match ns {
                SymbolNs::Value => Namespace::Value,
                SymbolNs::Type => Namespace::Type,
                SymbolNs::Module | SymbolNs::Test => return None,
            };
            iface
                .lookup(space, found.name)
                .map(|_| SymbolTarget::ImportAlias {
                    module,
                    import,
                    item: Some(item),
                })
        })
    }

    /// The module at `path`, the builtin module's included, which the
    /// table's own lookup never finds.
    fn find_module(&self, path: &ModulePath) -> Option<ModuleId> {
        self.front.modules.find(path).or_else(|| {
            let builtin = self.front.modules.get(ModuleId::BUILTIN)?;
            (builtin.path == *path).then_some(ModuleId::BUILTIN)
        })
    }

    /// The checked parts of module `id`.
    fn checked(&self, id: ModuleId) -> Option<&Checked> {
        self.front.checked_quiet(id)
    }

    /// Whether the run kept module `id`'s tree, and so its bodies.
    fn retained(&self, id: ModuleId) -> bool {
        at(id)
            .and_then(|n| self.front.trees.get(n))
            .is_some_and(Option::is_some)
    }
}

/// The first item of `iface` that `wanted` accepts.
fn scan(
    iface: &ModuleInterface,
    wanted: impl Fn(DefIndex, &cheby_iface::IfaceItem) -> bool,
) -> Option<DefIndex> {
    iface
        .items
        .iter_enumerated()
        .find(|&(index, item)| wanted(index, item))
        .map(|(index, _)| index)
}

/// Whether a constructor of the type `ty` has a field named `name`.
fn has_field(iface: &ModuleInterface, ty: DefIndex, name: Name) -> bool {
    iface.items.iter().any(|item| {
        item.kind == DefKind::Ctor
            && item.parent == Some(ty)
            && matches!(
                &item.sig,
                ItemSig::Ctor(ctor) if matches!(
                    &ctor.fields,
                    cheby_iface::CtorFields::Named(fields) if fields.iter().any(|&(n, _)| n == name)
                )
            )
    })
}

/// The type parameters an item's signature declares.
const fn sig_generics(sig: &ItemSig) -> Option<&Generics> {
    match sig {
        ItemSig::Fn(f) => Some(&f.generics),
        ItemSig::Type(t) => Some(&t.generics),
        ItemSig::Alias(a) => Some(&a.generics),
        ItemSig::Ctor(_)
        | ItemSig::Const(_)
        | ItemSig::Interface(_)
        | ItemSig::InterfaceFn(_)
        | ItemSig::Test
        | ItemSig::Error => None,
    }
}

/// One module of a [`Snapshot`].
#[derive(Clone, Copy)]
pub struct ModuleView<'s> {
    snapshot: &'s Snapshot,
    id: ModuleId,
}

impl std::fmt::Debug for ModuleView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleView")
            .field("id", &self.id)
            .field("path", &self.path().map(ToString::to_string))
            .finish()
    }
}

impl<'s> ModuleView<'s> {
    /// The module's id in this snapshot.
    #[must_use]
    pub const fn id(self) -> ModuleId {
        self.id
    }

    /// The module's path, such as `app::shapes` or `std::list`.
    #[must_use]
    pub fn path(self) -> Option<&'s ModulePath> {
        self.snapshot
            .front
            .modules
            .get(self.id)
            .map(|info| &info.path)
    }

    /// The module's file; `None` for the builtin module.
    #[must_use]
    pub fn file(self) -> Option<FileId> {
        self.snapshot.front.modules.get(self.id)?.file
    }

    /// Whether the module belongs to the package.
    #[must_use]
    pub fn is_local(self) -> bool {
        self.snapshot
            .front
            .modules
            .get(self.id)
            .is_some_and(|info| info.package == PackageId::LOCAL)
    }

    /// Whether the module's file has phase-A errors in this run.
    #[must_use]
    pub fn is_broken(self) -> bool {
        self.file()
            .and_then(|file| self.snapshot.location(file))
            .is_some_and(|location| match location {
                FileLocation::Package(path) => self.snapshot.status.broken.contains(path),
                FileLocation::Std(_) | FileLocation::Synthetic => false,
            })
    }

    /// The module's syntax tree. `None` for std in a run that keeps local
    /// trees only.
    #[must_use]
    pub fn tree(self) -> Option<&'s SyntaxTree> {
        at(self.id)
            .and_then(|n| self.snapshot.front.trees.get(n))
            .and_then(Option::as_ref)
    }

    /// The module's resolution. `None` where [`ModuleView::tree`] is, and
    /// for a module that was not checked: lost, behind a lost module, or on
    /// an import cycle no cut opened.
    #[must_use]
    pub fn resolved(self) -> Option<&'s ResolvedModule> {
        self.snapshot.checked(self.id)?.resolved.as_ref()
    }

    /// The module's checked bodies. `None` for a module that was not
    /// checked, and for std in a run that keeps no std body.
    #[must_use]
    pub fn thir(self) -> Option<&'s thir::Module> {
        if !self.snapshot.retained(self.id) {
            return None;
        }
        self.snapshot.checked(self.id).map(|c| &c.thir)
    }

    /// The facts of the module's bodies (D-1380), with what the run lost.
    #[must_use]
    pub fn facts(self) -> Option<&'s ModuleFacts> {
        self.snapshot.checked(self.id)?.facts.as_ref()
    }

    /// The module's interface.
    #[must_use]
    pub fn interface(self) -> Option<&'s ModuleInterface> {
        self.snapshot.front.set.get(self.id)
    }

    /// The targets of the module's imports, in written order.
    #[must_use]
    pub fn imports(self) -> &'s [ImportTarget] {
        at(self.id)
            .and_then(|n| self.snapshot.front.import_targets.get(n))
            .map_or(&[], Vec::as_slice)
    }

    /// The module's imports as written.
    #[must_use]
    pub fn import_requests(self) -> &'s [ImportRequest] {
        at(self.id)
            .and_then(|n| self.snapshot.front.imports.get(n))
            .map_or(&[], Vec::as_slice)
    }

    /// The module's transitive dependencies, sorted: what its check read
    /// (D-246). Empty for a module that was not checked.
    #[must_use]
    pub fn deps(self) -> &'s [ModuleId] {
        self.snapshot
            .checked(self.id)
            .map_or(&[], |c| c.deps.as_slice())
    }

    /// The view of the interfaces the module's check read (D-246, D-447).
    #[must_use]
    pub fn dep_view(self) -> DepView<'s> {
        DepView::new(&self.snapshot.front.set, self.deps())
    }

    /// The modules that import this one directly, sorted.
    #[must_use]
    pub fn importers(self) -> &'s [ModuleId] {
        self.snapshot.importers(self.id)
    }
}
