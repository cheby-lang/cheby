//! The base of std for the server: every shipped std module checked once
//! per process, with trees and facts kept, and a plain index derived from
//! it (A §7, D-1372).
//!
//! - [`StdBase::build`] runs one synthetic package that imports every
//!   module of [`std_lib::STD_MODULES`], with `Retain::STD`: every module
//!   keeps its tree, its resolution and its facts, and there is no
//!   containment, so a lost body or pass fails the build as a batch run
//!   would. Test bodies stay unchecked (`Tests::SkipStd`), so a `test`
//!   block of std has no facts (D-1370).
//! - [`StdIndex`] is plain data, strings and ranges, so it crosses every
//!   generation (A §1.5). It serves auto-import, `workspace/symbol`,
//!   completion over modules a package run has not loaded, docs, field
//!   docs (R-6) and std constants' initializers (S-20).
//! - [`Snapshot::head_of`] gives the [`TypeHead`] of a type of any
//!   snapshot, which is how a package snapshot's types meet the index.
//! - [`materialize_std`] writes std's sources once per toolchain, read
//!   only, so a location in std is a plain `file://` (A §6.2, D-1369).
//!
//! Only what checked enters the index: staged and planned modules are never
//! loaded, so no unshipped module can appear (G11).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cheby_diag::{Diagnostic, SourceMap, TextRange};
use cheby_iface::{
    CtorFields, DefId, DefKind, DepView, DisplayCx, Globals, Ice, IfaceItem, ItemSig, ModuleId,
    ModuleInterface, PackageId, SigOpts, Ty, TyKind, builtin, item_signature, structure_visible,
};
use cheby_package::{Manifest, ModulePath, Targets};
use cheby_resolve::{ItemDef, field_docs, item_docs};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, ConstDecl, SourceFile, module_doc_text};

use super::snapshot::{BaselineEntry, ModuleView, Parts, Snapshot, SnapshotId};
use super::symbol::Symbol;
use crate::cache::{BuildId, CacheEnv, Hash128, cache_root};
use crate::frontend::{Retain, RunExtras, run_analysis};
use crate::{Error, Module, Package, parse_file, std_lib};

/// The session number of every [`StdBase`] snapshot. Package sessions count
/// up from 0, so they never reach it.
const STD_SESSION: u32 = u32::MAX;

/// The name of the synthetic package that imports all of std.
const BASE_NAME: &str = "std_base";

/// How many variants a type's [`StdItem::signature`] shows before `…`.
const MAX_VARIANTS: usize = 16;

/// The generation of the next [`StdBase`] snapshot, from 1.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// The number of the next temporary file of [`materialize_std_at`], so
/// two calls in one process never write the same one.
static TEMPS: AtomicU64 = AtomicU64::new(0);

/// Every shipped std module, checked once, and its index (A §7).
#[derive(Debug)]
pub struct StdBase {
    snapshot: Arc<Snapshot>,
    index: StdIndex,
}

impl StdBase {
    /// Checks every shipped std module with everything retained, and
    /// derives the index. One per process, on the analysis thread: it takes
    /// about as long as a run that loads all of std.
    ///
    /// # Errors
    ///
    /// An [`Error`] when std does not check, loses a body or a pass, or
    /// keeps no tree for a module: all internal errors.
    pub fn build(globals: Arc<Globals>) -> Result<Arc<Self>, Error> {
        Self::build_with(globals, "")
    }

    /// [`StdBase::build`] with `extra` appended to the synthetic root
    /// module, so tests can put an internal error in the run.
    pub(super) fn build_with(globals: Arc<Globals>, extra: &str) -> Result<Arc<Self>, Error> {
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let package = base_package(&mut sources, &mut diagnostics, extra)?;
        let targets = package.manifest.targets;
        let extras = RunExtras {
            globals,
            retain: Retain::STD,
            cut_cycles: None,
        };
        let front = run_analysis(package, &mut sources, &mut diagnostics, extras)?;
        sources.sort(&mut diagnostics);
        if let Some(error) = diagnostics.iter().find(|d| d.is_error()) {
            return Err(Ice::new(format!(
                "the standard library does not check: {}: {}",
                error.code, error.message
            ))
            .into());
        }
        let baseline = baseline(&sources, &diagnostics);
        let snapshot = Snapshot::new(Parts {
            id: SnapshotId {
                session: STD_SESSION,
                generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
            },
            root: PathBuf::new(),
            front,
            sources,
            versions: BTreeMap::new(),
            files: BTreeMap::new(),
            broken: BTreeSet::new(),
            baseline,
            targets,
            manifest_is_last_good: false,
            single_file: false,
        });
        let index = StdIndex::of(&snapshot)?;
        Ok(Arc::new(Self {
            snapshot: Arc::new(snapshot),
            index,
        }))
    }

    /// The index of every item a package may name in std.
    #[must_use]
    pub const fn index(&self) -> &StdIndex {
        &self.index
    }

    /// The snapshot of the run, for requests on an opened std document
    /// only (A §6.3, D-1370). Its root is empty, and every std file's
    /// location is [`super::FileLocation::Std`].
    #[must_use]
    pub const fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }
}

/// The index of std: plain data, strings and ranges, so it crosses every
/// generation (A §1.5, D-1372).
#[derive(Debug, Default)]
pub struct StdIndex {
    /// Every shipped module, sorted by path.
    pub modules: Vec<StdModuleEntry>,
    /// Every item a package may name, module by module in the order of
    /// [`StdIndex::modules`], each module's in declaration order, then the
    /// builtin items homed there.
    pub items: Vec<StdItem>,
    by_name: BTreeMap<String, Vec<u32>>,
    by_param: BTreeMap<TypeHead, Vec<(u32, u8)>>,
    by_key: BTreeMap<Symbol, u32>,
}

/// One std module of [`StdIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdModuleEntry {
    /// The module's path, such as `std::json::decode`.
    pub path: String,
    /// The last segment, which `import` binds.
    pub binding: String,
    /// Its display path, such as `std/json/decode.cheby` (D-450), which
    /// joins onto [`materialize_std`]'s directory.
    pub file: String,
    /// Its `//!` comment, as Markdown.
    pub doc: Option<String>,
}

/// One item of [`StdIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdItem {
    /// Its stable name (D-1365). A builtin item homed in a std module, such
    /// as `List`, keeps the builtin module's path, as everywhere.
    pub key: Symbol,
    /// The module a package imports to name it, such as `std::list`: the
    /// declaring module, or the home of a builtin item.
    pub module: String,
    /// What it is.
    pub kind: DefKind,
    /// Its declaration without a body, as the item printer writes it:
    /// `pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U>`. A type
    /// shows its constructors only when a package may see them.
    pub signature: String,
    /// Its doc comment, as Markdown. A builtin item, which has no source,
    /// has its home module's `//!` comment (F §6.3).
    pub doc: Option<String>,
    /// The message of its `@deprecated`.
    pub deprecated: Option<String>,
    /// The targets it exists on.
    pub targets: Targets,
    /// The display path of [`StdItem::module`]'s file, such as
    /// `std/list.cheby`.
    pub file: String,
    /// The range of its name in `file`. `None` for a builtin item, which
    /// has no declaration.
    pub name_range: Option<TextRange>,
    /// The heads of a function's or a constructor's parameters, or an
    /// interface function's, in order. Empty for other items.
    pub params: Vec<TypeHead>,
    /// The head of a function's return type, a constant's type, or the
    /// type a constructor builds. [`TypeHead::Unknown`] for other items.
    pub ret: TypeHead,
    /// A type's named fields, over all its constructors, in first-seen
    /// order, when a package may see them (R-6).
    pub fields: Vec<StdField>,
    /// A constant's initializer as written, for hover (S-20).
    pub initializer: Option<String>,
}

/// One named field of a std type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdField {
    /// The field's name.
    pub name: String,
    /// Its doc comment: the first of its constructors' with text.
    pub doc: Option<String>,
}

/// A type's outermost shape, by name, so that it compares across
/// generations.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeHead {
    /// A nominal type declared in a module, such as `std::json::Json`.
    Named {
        /// The declaring module's path.
        module: String,
        /// The type's name.
        name: String,
    },
    /// A builtin type, such as `List` or `Int`, wherever it is homed.
    Builtin(String),
    /// A tuple of this many elements.
    Tuple(u32),
    /// A function type with this many parameters.
    Function(u32),
    /// `dyn I<…>`, by the interface.
    Dyn {
        /// The interface's module path.
        module: String,
        /// The interface's name.
        name: String,
    },
    /// A type parameter.
    Param,
    /// An error type, `Never`, or a handle the snapshot cannot read.
    Unknown,
}

impl StdIndex {
    /// The items named `name`.
    pub fn named(&self, name: &str) -> impl Iterator<Item = &StdItem> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|&n| self.at(n))
    }

    /// The functions with a parameter of head `head`, each with the
    /// parameter's position.
    pub fn accepting(&self, head: &TypeHead) -> impl Iterator<Item = (&StdItem, u8)> {
        self.by_param
            .get(head)
            .into_iter()
            .flatten()
            .filter_map(|&(n, position)| Some((self.at(n)?, position)))
    }

    /// The items whose name starts with `text`, by name.
    pub fn prefix<'a>(&'a self, text: &'a str) -> impl Iterator<Item = &'a StdItem> {
        self.by_name
            .range::<str, _>((std::ops::Bound::Included(text), std::ops::Bound::Unbounded))
            .take_while(move |(name, _)| name.starts_with(text))
            .flat_map(|(_, list)| list)
            .filter_map(|&n| self.at(n))
    }

    /// The item `key` names.
    #[must_use]
    pub fn get(&self, key: &Symbol) -> Option<&StdItem> {
        self.by_key.get(key).and_then(|&n| self.at(n))
    }

    /// The module at `path`, such as `std::list`.
    #[must_use]
    pub fn module(&self, path: &str) -> Option<&StdModuleEntry> {
        self.modules.iter().find(|m| m.path == path)
    }

    /// Item number `n`.
    fn at(&self, n: u32) -> Option<&StdItem> {
        self.items.get(usize::try_from(n).ok()?)
    }

    /// Adds `item` to the lists and the lookups.
    fn push(&mut self, item: StdItem) -> Result<(), Ice> {
        let n = u32::try_from(self.items.len()).map_err(|_| Ice::new("too many std items"))?;
        self.by_name
            .entry(item.key.text().to_owned())
            .or_default()
            .push(n);
        if item.kind == DefKind::Fn {
            for (position, head) in item.params.iter().enumerate() {
                let Ok(position) = u8::try_from(position) else {
                    break;
                };
                let list = self.by_param.entry(head.clone()).or_default();
                if !list.iter().any(|&(m, _)| m == n) {
                    list.push((n, position));
                }
            }
        }
        self.by_key.insert(item.key.clone(), n);
        self.items.push(item);
        Ok(())
    }

    /// The index of `snapshot`'s std modules.
    fn of(snapshot: &Snapshot) -> Result<Self, Error> {
        let viewer = snapshot
            .local_modules()
            .next()
            .map(ModuleView::id)
            .ok_or_else(|| Ice::new("std's base has no root module"))?;
        let deps = DepView::new(snapshot.interfaces(), snapshot.all_ids());
        let reader = Reader {
            snapshot,
            deps: &deps,
            viewer,
        };
        let mut modules: Vec<ModuleView<'_>> = snapshot
            .all_modules()
            .filter(|m| {
                m.id() != ModuleId::BUILTIN
                    && snapshot
                        .table()
                        .get(m.id())
                        .is_some_and(|info| info.package == PackageId::STD)
            })
            .collect();
        modules.sort_by_key(|m| m.path().map(ToString::to_string));
        let mut index = Self::default();
        for module in modules {
            reader.module(module, &mut index)?;
        }
        Ok(index)
    }
}

/// What the index reads one std module with.
struct Reader<'s> {
    snapshot: &'s Snapshot,
    deps: &'s DepView<'s>,
    /// The base's root module: a module of another package, as a package
    /// that imports std is.
    viewer: ModuleId,
}

/// Where an item of std is declared: its module's tree and its
/// resolution. `None` for a builtin item.
type Source<'s> = Option<(&'s SyntaxTree, &'s ItemDef)>;

impl Reader<'_> {
    /// Adds `module` and its items to `index`.
    fn module(&self, module: ModuleView<'_>, index: &mut StdIndex) -> Result<(), Error> {
        let path = module
            .path()
            .ok_or_else(|| Ice::new("a std module with no path"))?;
        let (Some(tree), Some(resolved), Some(iface)) =
            (module.tree(), module.resolved(), module.interface())
        else {
            return Err(Ice::new(format!("std's base kept no tree for `{path}`")).into());
        };
        let file = module
            .file()
            .and_then(|file| self.snapshot.source(file))
            .map(|source| source.path().to_owned())
            .ok_or_else(|| Ice::new(format!("std's base has no file for `{path}`")))?;
        let doc = SourceFile::cast(tree.root())
            .map(|root| module_doc_text(root.module_docs()))
            .and_then(with_text);
        let entry = StdModuleEntry {
            path: path.to_string(),
            binding: path.segments().last().cloned().unwrap_or_default(),
            file,
            doc: doc.clone(),
        };
        for (at, item) in iface.items.iter_enumerated() {
            if !self.wanted(iface, at, item) {
                continue;
            }
            let def = DefId::new(module.id(), at);
            let source = resolved.item(at).map(|item_def| (tree, item_def));
            if let Some(entry) = self.item(def, item, &entry, source) {
                index.push(entry)?;
            }
        }
        if let Some(builtins) = self.snapshot.interfaces().get(ModuleId::BUILTIN) {
            for homed in builtin::homed_at(path) {
                let members = builtins.items.iter_enumerated().filter(|(at, item)| {
                    *at == homed.index
                        || (item.kind == DefKind::Ctor && item.parent == Some(homed.index))
                });
                for (at, item) in members {
                    let def = DefId::new(ModuleId::BUILTIN, at);
                    if !self.wanted(builtins, at, item) {
                        continue;
                    }
                    if let Some(mut entry) = self.item(def, item, &entry, None) {
                        entry.doc.clone_from(&doc);
                        index.push(entry)?;
                    }
                }
            }
        }
        index.modules.push(entry);
        Ok(())
    }

    /// Whether a package may name item `at` of `iface`: current, visible
    /// from another package, not a test, and for a constructor or an
    /// interface function, a member of something it may see into.
    fn wanted(&self, iface: &ModuleInterface, at: cheby_iface::DefIndex, item: &IfaceItem) -> bool {
        if iface.is_hidden(at)
            || !iface.is_current(at)
            || item.kind == DefKind::Test
            || !iface.visible_from(at, PackageId::LOCAL)
        {
            return false;
        }
        match (item.kind, item.parent) {
            (DefKind::Ctor, Some(parent)) => iface
                .item(parent)
                .is_some_and(|ty| self.structure(iface, ty)),
            (DefKind::InterfaceFn, Some(parent)) => iface.visible_from(parent, PackageId::LOCAL),
            _ => true,
        }
    }

    /// Whether a package may see the constructors and fields of `ty`, a
    /// type of `iface`.
    fn structure(&self, iface: &ModuleInterface, ty: &IfaceItem) -> bool {
        structure_visible(
            ty,
            iface.module,
            iface.package,
            self.viewer,
            PackageId::LOCAL,
        )
    }

    /// The entry of `def`, an item of std or a builtin item homed at
    /// `home`. `source` is the declaring module's tree and the item's
    /// resolution, `None` for a builtin item. `None` for an item with no
    /// stable name.
    fn item(
        &self,
        def: DefId,
        item: &IfaceItem,
        home: &StdModuleEntry,
        source: Source<'_>,
    ) -> Option<StdItem> {
        let key = self.snapshot.key_of(def)?;
        let iface = self.snapshot.interfaces().get(def.module)?;
        let cx = DisplayCx {
            globals: self.snapshot.globals(),
            modules: self.snapshot.table(),
            own: Some(iface),
            deps: self.deps,
            generics: None,
            imports: None,
        };
        let opts = SigOpts {
            structure: self.structure(iface, item),
            max_variants: MAX_VARIANTS,
        };
        let (params, ret) = self.heads(def, item);
        let fields = match item.kind {
            DefKind::Type if opts.structure => self.fields(iface, def, source),
            _ => Vec::new(),
        };
        let initializer = match (item.kind, source) {
            (DefKind::Const, Some((tree, item_def))) => initializer(tree, item_def),
            _ => None,
        };
        let name_range = source.and_then(|(tree, _)| {
            item.span
                .filter(|span| span.file == tree.file())
                .map(|span| span.range)
        });
        Some(StdItem {
            key,
            module: home.path.clone(),
            kind: item.kind,
            signature: item_signature(def, item, &cx, opts),
            doc: source.and_then(|(tree, item_def)| item_docs(tree, item_def)),
            deprecated: item.deprecated.as_deref().map(str::to_owned),
            targets: item.targets,
            file: home.file.clone(),
            name_range,
            params,
            ret,
            fields,
            initializer,
        })
    }

    /// The heads of `item`'s parameters and of its result.
    fn heads(&self, def: DefId, item: &IfaceItem) -> (Vec<TypeHead>, TypeHead) {
        let head = |ty: Ty| self.snapshot.head_of(ty);
        match &item.sig {
            ItemSig::Fn(sig) => (
                sig.params.iter().map(|p| head(p.ty)).collect(),
                head(sig.ret),
            ),
            ItemSig::InterfaceFn(sig) => (
                sig.params.iter().copied().map(head).collect(),
                head(sig.ret),
            ),
            ItemSig::Const(sig) => (Vec::new(), head(sig.ty)),
            ItemSig::Ctor(ctor) => {
                let params = match &ctor.fields {
                    CtorFields::Unit => Vec::new(),
                    CtorFields::Positional(tys) => tys.iter().copied().map(head).collect(),
                    CtorFields::Named(fields) => fields.iter().map(|&(_, ty)| head(ty)).collect(),
                };
                let ty = DefId::new(def.module, ctor.ty);
                (params, self.snapshot.def_head(ty, false))
            }
            ItemSig::Type(_)
            | ItemSig::Alias(_)
            | ItemSig::Interface(_)
            | ItemSig::Test
            | ItemSig::Error => (Vec::new(), TypeHead::Unknown),
        }
    }

    /// The named fields of the type `def`, with their docs.
    fn fields(&self, iface: &ModuleInterface, def: DefId, source: Source<'_>) -> Vec<StdField> {
        let names = &self.snapshot.globals().names;
        let mut seen: Vec<&str> = Vec::new();
        for ctor in &iface.items {
            if ctor.kind != DefKind::Ctor || ctor.parent != Some(def.index) {
                continue;
            }
            if let ItemSig::Ctor(sig) = &ctor.sig
                && let CtorFields::Named(fields) = &sig.fields
            {
                for &(name, _) in fields {
                    let name = names.get(name);
                    if !seen.contains(&name) {
                        seen.push(name);
                    }
                }
            }
        }
        seen.into_iter()
            .map(|name| StdField {
                name: name.to_owned(),
                doc: source.and_then(|(tree, item_def)| field_docs(tree, item_def, name)),
            })
            .collect()
    }
}

/// The initializer of the constant `item`, as written.
fn initializer(tree: &SyntaxTree, item: &ItemDef) -> Option<String> {
    let node = tree.node(item.node)?;
    let decl = node.descendants().find_map(ConstDecl::cast)?;
    let value = decl.value()?;
    value
        .significant_span()
        .range
        .slice(tree.text())
        .map(str::to_owned)
}

/// `text`, unless nothing but white space is in it.
fn with_text(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

impl Snapshot {
    /// The head of `ty`, for looking up in [`StdIndex`]. Read only with a
    /// type of this snapshot (A §1.5).
    #[must_use]
    pub fn head_of(&self, ty: Ty) -> TypeHead {
        match self.globals().types.kind(ty) {
            TyKind::Named { def, .. } => self.def_head(*def, false),
            TyKind::Dyn { iface, .. } => self.def_head(*iface, true),
            TyKind::Tuple(elems) => {
                u32::try_from(elems.len()).map_or(TypeHead::Unknown, TypeHead::Tuple)
            }
            TyKind::Fn { params, .. } => {
                u32::try_from(params.len()).map_or(TypeHead::Unknown, TypeHead::Function)
            }
            TyKind::Param(_) => TypeHead::Param,
            TyKind::Error | TyKind::Never => TypeHead::Unknown,
        }
    }

    /// The head of the type or interface `def`: [`TypeHead::Dyn`] when
    /// `dyn_` is set.
    fn def_head(&self, def: DefId, dyn_: bool) -> TypeHead {
        let Some(item) = self
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
        else {
            return TypeHead::Unknown;
        };
        let name = self.globals().names.get(item.name).to_owned();
        if def.module == ModuleId::BUILTIN && !dyn_ {
            return TypeHead::Builtin(name);
        }
        let Some(module) = self.table().get(def.module) else {
            return TypeHead::Unknown;
        };
        let module = module.path.to_string();
        if dyn_ {
            TypeHead::Dyn { module, name }
        } else {
            TypeHead::Named { module, name }
        }
    }
}

/// The synthetic package of the base: one root module that imports every
/// shipped std module, as `whole_std_package` does for the build cache.
fn base_package(
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    extra: &str,
) -> Result<Package, Error> {
    let manifest_text = format!("name = \"{BASE_NAME}\"\nversion = \"0.0.0\"\n");
    let manifest_file = sources
        .add_file(cheby_package::MANIFEST_FILE, manifest_text.clone())
        .map_err(|_| Error::from(Ice::new("std's base manifest")))?;
    let manifest = Manifest::parse(manifest_file, &manifest_text, diagnostics)
        .ok_or_else(|| Error::from(Ice::new("std's base manifest does not parse")))?;
    let mut text = String::from("//! Imports every shipped std module (D-1372).\n\n");
    for module in std_lib::STD_MODULES {
        let _ = writeln!(text, "import std::{}", module.path);
    }
    text.push_str(extra);
    let hashes = crate::SourceHashes::of(text.as_bytes());
    let source = parse_file(sources, "src/main.cheby", text.into_bytes(), diagnostics)?;
    if source.has_syntax_errors {
        return Err(Ice::new("std's base root module does not parse").into());
    }
    Ok(Package {
        root: PathBuf::new(),
        manifest,
        modules: vec![Module {
            path: ModulePath::root(BASE_NAME),
            source,
            hashes,
            parsed: true,
            syntax_diagnostics: Vec::new(),
        }],
    })
}

/// The run's diagnostics as a sorted multiset of code, file and message.
fn baseline(sources: &SourceMap, diagnostics: &[Diagnostic]) -> Vec<BaselineEntry> {
    let mut entries: Vec<BaselineEntry> = diagnostics
        .iter()
        .map(|d| {
            let path = d
                .primary_span()
                .and_then(|span| sources.file(span.file))
                .map(|source| PathBuf::from(source.path()));
            (d.code, path, d.message.clone())
        })
        .collect();
    entries.sort();
    entries
}

/// The directory of std's sources for this toolchain, written on first use
/// (A §6.2, D-1369): `<cache root>/lsp/std-src/<build id>`, whose
/// `std/<path>.cheby` files are read only. A [`super::FileLocation::Std`]
/// display path joins straight onto it. When the cache root cannot be
/// used, the directory is `cheby-std-<build id>` in the temporary
/// directory, written the same way.
///
/// # Errors
///
/// An [`Error`] when neither directory can be written.
pub fn materialize_std(build: BuildId) -> Result<PathBuf, Error> {
    let hex = build.to_hex();
    let mut why = Vec::new();
    match cache_root(&CacheEnv::from_process()) {
        Ok(Some(root)) => {
            let dir = root.join("lsp").join("std-src").join(&hex);
            match materialize_std_at(&dir) {
                Ok(()) => return Ok(dir),
                Err(error) => why.push(error.message().to_owned()),
            }
        }
        Ok(None) => why.push("no cache root is set".to_owned()),
        Err(error) => why.push(error.to_string()),
    }
    let dir = std::env::temp_dir().join(format!("cheby-std-{hex}"));
    match materialize_std_at(&dir) {
        Ok(()) => Ok(dir),
        Err(error) => {
            why.push(error.message().to_owned());
            Err(Error::new(format!(
                "cannot write std's sources: {}",
                why.join("; ")
            )))
        }
    }
}

/// Writes every shipped std module under `dir`, at its display path, read
/// only. A file already there with the same length and hash is kept, so
/// this costs one read of each file after the first time.
///
/// # Errors
///
/// An [`Error`] naming the first file or directory that cannot be written.
pub(crate) fn materialize_std_at(dir: &Path) -> Result<(), Error> {
    let io = |path: &Path, error: std::io::Error| {
        Error::new(format!("cannot write `{}`: {error}", path.display()))
    };
    for module in std_lib::STD_MODULES {
        let path = dir.join(std_lib::display_path(module.path));
        if holds(&path, module.source) {
            continue;
        }
        let parent = path.parent().unwrap_or(dir);
        private_dir(parent).map_err(|e| io(parent, e))?;
        let mut name = path.file_name().unwrap_or_default().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TEMPS.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = path.with_file_name(name);
        // A read-only leftover of a dead process whose pid came back.
        let _ = set_read_only(&tmp, false);
        let _ = std::fs::remove_file(&tmp);
        write_read_only(&tmp, module.source).map_err(|e| io(&tmp, e))?;
        if path.exists() {
            // A changed file: read only, which stops a rename over it on
            // Windows.
            let _ = set_read_only(&path, false);
            let _ = std::fs::remove_file(&path);
        }
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = set_read_only(&tmp, false);
            let _ = std::fs::remove_file(&tmp);
            // Another server may have written it in between.
            if !holds(&path, module.source) {
                return Err(io(&path, error));
            }
        }
    }
    Ok(())
}

/// Whether the file at `path` holds exactly `text`.
fn holds(path: &Path, text: &str) -> bool {
    let same_length = std::fs::metadata(path)
        .is_ok_and(|m| u64::try_from(text.len()).is_ok_and(|len| m.len() == len));
    same_length
        && std::fs::read(path)
            .is_ok_and(|bytes| Hash128::of(&bytes) == Hash128::of(text.as_bytes()))
}

/// Writes `text` to a new file at `path` and makes it read only.
fn write_read_only(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    set_read_only(path, true)
}

/// Sets or clears the read-only flag of `path`.
fn set_read_only(path: &Path, read_only: bool) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(if read_only { 0o444 } else { 0o644 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(read_only);
    std::fs::set_permissions(path, permissions)
}

/// Creates `dir` and its missing parents, owner-only on Unix, as the
/// build cache's directories are.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A base is built on the analysis thread and read by the request pool.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<StdBase>();
    shared::<StdIndex>();
};
