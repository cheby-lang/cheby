//! Std in the build cache (D-980): std's one code, its masks over every
//! std module, its code records, and its front records.
//!
//! Std is compiled into the global cache on first use, keyed by the build
//! ID, which fixes std's sources (D-957), so no std key holds a source
//! hash. A warm command reads std's front records and interfaces instead
//! of parsing and checking std, and std's code records instead of
//! compiling it; D-450's rule (check only the std modules the imports
//! reach) applies only to a front end that reads no std front record
//! (D-1262).
//!
//! - **One inference unit** (D-968, D-1264): std's masks are
//!   inferred over every std module's code unit, whatever the program
//!   imports, in a front end of std's own ([`whole_std`], once per
//!   process). So std has one code per build ID and settings, and every
//!   build applies those masks to std's units: a build that lowers std
//!   itself (`cheby build`, `CHEBY_DUMP_DIR`, a build without a cache)
//!   takes them by name ([`std_masks_in`]); a JIT host with a cache loads
//!   std's records ([`std_records`]). Package masks never read std's facts:
//!   std names no package item, and a package calls only std's `pub`
//!   functions, which never borrow (D-787, D-812).
//! - **One code per module and settings** (D-958, D-980): each std
//!   module's code record lies under [`std_code_key`], the build ID, the
//!   module's path, `BuildKind::Std` with the command's other settings
//!   ([`std_settings`]) and the positions switch (D-1173), and is shared by
//!   `cheby run`, `cheby test`, every evaluation module and `cheby check`
//!   (D-978's `plain` code is the package's modules without borrowing and
//!   std's one code). `cargo xtask test-std` builds under
//!   `BuildKind::StdSelfTest` and reads none of them
//!   (D-1265).
//! - **The first use** (D-980): a lookup that misses lowers all of std for
//!   its facts and compiles every std module's code unit into its record,
//!   stored behind the command (D-1138), not only the load set's
//!   (D-1267): a later command whose load set holds
//!   another std module then needs no std body, so it parses no std
//!   module. It costs about 0.1 s once per machine, toolchain and settings
//!   (`cargo xtask bench-compile`'s "std first use").
//!
//! `CHEBY_CACHE_LOG` lists std's lookups and parses under their own names,
//! `parse.std <module>`, `front.std <module> hit|miss` and
//! `code.std <module> hit|miss`, beside D-1135's package-only lines
//! (D-1263).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use cheby_codegen_clif::record::CodeRecord;
use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{Ice, ModuleId, PackageId};
use cheby_jit::{BatchCx, BatchUnit, Jit, JitOptions};
use cheby_mir::borrow::{self, BorrowMask};
use cheby_mir::{FnKey, MirModule, MirOptions, UnitKind};
use cheby_package::{Manifest, ModulePath};
use rayon::prelude::*;

use crate::cache::front::FrontRecord;
use crate::cache::{BuildId, BuildKind, CacheLog, Key, KeyKind, Settings, StoreShare};
use crate::frontend::{self, Frontend};
use crate::{Error, Module, Package, parse_file, std_lib};

/// Whether a front end reads std's front records instead of parsing std
/// (D-980); `load_set.rs`'s walk reads a std hit's summaries
/// ([`std_summaries`]).
pub(crate) const FRONT_HITS: bool = true;

/// D-980: std module `module`'s (`std::<path>`) front record key:
/// `KeyKind::Front` over the build ID, the tag `std-f1` and the path; the
/// build ID fixes std's sources (D-957), so no source hash
/// (D-1262).
pub(crate) fn std_front_key(build: BuildId, module: &str) -> Key {
    Key::builder(KeyKind::Front, build)
        .str("std-f1")
        .str(module)
        .finish()
}

/// Std's front record of `module` (`std::<path>` written as `<path>` or
/// the full path, as the caller logs it), logging
/// `front.std <module> hit|miss`. A hit is a record that decodes and whose
/// interface artifact is in the store with the store's build ID and the
/// record's interface hash. A miss is std's first use ([`first_use`],
/// under the environment's MIR options), whose record of `module` comes
/// back; so a front end with a cache that reads parses no std module
/// (D-980), and `None` only when the command bypasses reads
/// (`CHEBY_DUMP_DIR`, D-965), whose front end parses std as D-450 says.
///
/// # Errors
///
/// An internal error when std's first use fails or `module` is no std
/// module.
pub(crate) fn lookup_std_front(
    cache: &crate::Cache,
    module: &str,
) -> Result<Option<FrontRecord>, Ice> {
    if !cache.reads {
        return Ok(None);
    }
    let at = StdStore::of_cache(cache);
    let build = at.store.build_id();
    let found = at
        .store
        .get_record(&std_front_key(build, module))
        .and_then(|bytes| FrontRecord::decode(&bytes));
    at.log.record("front.std", module, found.is_some());
    if found.is_some() {
        return Ok(found);
    }
    let mut used = first_use_from_env(&at)?;
    used.fronts.remove(module).map(Some).ok_or_else(|| {
        Ice::new(format!(
            "std's first use wrote no front record of `{module}`"
        ))
    })
}

/// The paths of the std modules a std front record's module imports, in
/// source order: what the front end resolves instead of the module's
/// tree (D-980).
///
/// # Errors
///
/// An internal error for an import that did not resolve, which std never
/// has.
pub(crate) fn import_paths(record: &FrontRecord) -> Result<Vec<String>, Ice> {
    record
        .imports
        .iter()
        .map(|import| match import {
            crate::cache::front::ImportKey::Module { path, .. } => Ok(path.clone()),
            crate::cache::front::ImportKey::Unresolved => {
                Err(Ice::new("a std front record with an unresolved import"))
            }
        })
        .collect()
}

/// [`first_use`] under the environment's MIR options, for a front end,
/// which has no build's options. The command reports an invalid option
/// itself; std's code under the defaults is then only stored.
fn first_use_from_env(at: &StdStore<'_>) -> Result<FirstUse, Ice> {
    let mir = std_mir(crate::backend::mir_options_from_env().unwrap_or_default());
    let settings = std_settings(&crate::borrowing::settings(BuildKind::Std, mir));
    first_use(at, &settings, mir)
}

/// A std front record whose interface artifact did not decode (missing or
/// damaged, D-960): std's first use runs again, which writes every std
/// record and blob again (D-1139: a blob that failed to decode is written
/// by the next put), and the front end starts again with std parsed (its
/// second attempt reads no std record).
///
/// # Errors
///
/// As [`first_use`].
pub(crate) fn repair(cache: &crate::Cache) -> Result<(), Ice> {
    first_use_from_env(&StdStore::of_cache(cache)).map(|_| ())
}

/// The reference summaries of std module `module` of `front` that `front`
/// did not check because its front record hit (D-980): the record's, in
/// the walk's form (D-1178), for `load_set.rs`'s walk; `None` for a module
/// `front` checked or one outside std.
///
/// # Errors
///
/// An internal error when such a module's record cannot be read again.
pub(crate) fn std_summaries(
    front: &Frontend,
    module: ModuleId,
) -> Result<Option<Vec<crate::cache::front::ReferenceSummary>>, Ice> {
    let info = front.modules.try_get(module)?;
    if info.package != PackageId::STD || front.checked(module).is_some() {
        return Ok(None);
    }
    let path = info.path.to_string();
    let record = crate::borrowing::sink(front)
        .and_then(crate::borrowing::RecordSink::std_store)
        .and_then(|at| {
            at.store
                .get_record(&std_front_key(at.store.build_id(), &path))
        })
        .and_then(|bytes| FrontRecord::decode(&bytes))
        .ok_or_else(|| {
            Ice::new(format!(
                "std module `{path}` has neither a check nor a front record"
            ))
        })?;
    Ok(Some(record.references))
}

/// D-968, D-980: the settings of std's one code, `BuildKind::Std` with the
/// command's other D-958 settings, shared by `run`, `test`, `plain` and
/// `check`.
pub(crate) fn std_settings(command: &Settings) -> Settings {
    Settings {
        kind: BuildKind::Std,
        ..command.clone()
    }
}

/// Std module `module`'s code record key (`KeyKind::Code`), logged as
/// `code.std <module> hit|miss`, with positions on (D-1173):
/// [`std_code_key_at`].
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "plan §2.3's contract; tests call it")
)]
pub(crate) fn std_code_key(build: BuildId, module: &str, settings: &Settings) -> Key {
    std_code_key_at(build, module, settings, true)
}

/// Std module `module`'s code record key under `KeyKind::Code`: the build
/// ID, the tag `std-k1`, the module's path, `settings` (whose kind is
/// `std`) and the positions switch (D-1173), each field length-prefixed.
/// Std's masks are inferred over every std module (D-968), so they are a
/// function of the build ID and the settings, and no mask, callee or
/// identity enters the key (D-1264).
pub(crate) fn std_code_key_at(
    build: BuildId,
    module: &str,
    settings: &Settings,
    positions: bool,
) -> Key {
    Key::builder(KeyKind::Code, build)
        .str("std-k1")
        .str(module)
        .settings(settings)
        .bytes(&[u8::from(positions)])
        .finish()
}

/// The MIR options of std's one code in a build with `mir`: borrowing on
/// (D-968; it still needs `CHEBY_RC=perceus`, [`MirOptions::borrows`]),
/// whatever the build's own kind (`plain` included, D-978), and no reuse
/// sites (std records none, D-717).
pub(crate) const fn std_mir(mir: MirOptions) -> MirOptions {
    MirOptions {
        borrow: true,
        record_sites: false,
        ..mir
    }
}

/// Std lowered whole in a front end of its own, its masks inferred over
/// every std module as one inference unit (D-968, D-980): what a first use
/// computes, and what a build that lowers std itself takes its masks from.
pub(crate) struct WholeStd {
    /// Every std module's code unit, lowered whole (D-966), without masks
    /// and not counted, in build order.
    units: Vec<MirModule>,
    /// The masks written into them under borrowing, by def of std's own
    /// front end.
    by_def: borrow::Masks,
    /// The same masks of std's functions that borrow something, by module
    /// path and item, sorted.
    named: Vec<(String, String, BorrowMask)>,
    /// Every std module's path, in build order.
    modules: Vec<String>,
    /// Every std module's front record but its blobs, in build order.
    fronts: Vec<StdFront>,
}

/// What a std module's front record holds (D-1262), made
/// from std's own front end: everything but the blobs' names, since the
/// interface artifact holds the store's build ID.
struct StdFront {
    /// The module's path.
    path: String,
    /// Its interface hash.
    interface: crate::cache::Hash128,
    /// Its interface's canonical encoding (D-953), and each encoded item's
    /// span in its own file.
    encoded: Vec<u8>,
    spans: Vec<Option<(u32, u32)>>,
    /// Its imports: std modules, each with its interface hash.
    imports: Vec<crate::cache::front::ImportKey>,
    /// Its native bindings, in item order.
    externals: Vec<crate::cache::front::External>,
    /// Its reference summaries in the walk's form (D-1178,
    /// `load_set::summarize`), which a command walks instead of std's THIR.
    references: Vec<crate::cache::front::ReferenceSummary>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "read by tests only"))]
impl WholeStd {
    /// The masks of std's functions that borrow something, by module path
    /// and item, sorted.
    pub(crate) fn named(&self) -> &[(String, String, BorrowMask)] {
        &self.named
    }

    /// Every std module's path, in build order: what a first use parsed.
    pub(crate) fn modules(&self) -> &[String] {
        &self.modules
    }
}

/// [`WholeStd`], computed once per process.
static WHOLE: OnceLock<Result<Arc<WholeStd>, String>> = OnceLock::new();

/// Std lowered whole and inferred as one unit ([`WholeStd`]), computed on
/// the first call of the process.
///
/// # Errors
///
/// An internal error when std does not check, lower or infer.
pub(crate) fn whole_std() -> Result<Arc<WholeStd>, Ice> {
    WHOLE
        .get_or_init(|| {
            compute_whole_std()
                .map(Arc::new)
                .map_err(|e| e.message().to_owned())
        })
        .clone()
        .map_err(Ice::new)
}

/// The package of std's own front end: one module that imports every
/// shipped std module, like the std self-test's (D-542), so the front end
/// checks all of std (D-968), and nothing of the package is stored.
fn whole_std_package(
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Package, Error> {
    const NAME: &str = "std_whole";
    let manifest_text = format!("name = \"{NAME}\"\nversion = \"0.0.0\"\n");
    let manifest_file = sources
        .add_file("cheby.toml", manifest_text.clone())
        .map_err(|_| Error::from(Ice::new("std's whole-std manifest")))?;
    let manifest = Manifest::parse(manifest_file, &manifest_text, diagnostics)
        .ok_or_else(|| Error::from(Ice::new("std's whole-std manifest does not parse")))?;
    let mut text = String::from("//! Imports every shipped std module (D-968, D-980).\n\n");
    for module in std_lib::STD_MODULES {
        let _ = writeln!(text, "import std::{}", module.path);
    }
    let hashes = crate::SourceHashes::of(text.as_bytes());
    let source = parse_file(sources, "src/main.cheby", text.into_bytes(), diagnostics)?;
    if source.has_syntax_errors {
        return Err(Ice::new("std's whole-std root module does not parse").into());
    }
    Ok(Package {
        root: PathBuf::new(),
        manifest,
        modules: vec![Module {
            path: ModulePath::root(NAME),
            source,
            hashes,
            parsed: true,
            syntax_diagnostics: Vec::new(),
        }],
    })
}

/// Checks every std module, lowers each whole and infers std's masks over
/// all of them (D-968).
fn compute_whole_std() -> Result<WholeStd, Error> {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = whole_std_package(&mut sources, &mut diagnostics)?;
    let front = frontend::run(
        package,
        frontend::Mode::Build,
        &mut sources,
        &mut diagnostics,
    )?;
    if diagnostics.iter().any(Diagnostic::is_error) {
        return Err(Ice::new("the standard library does not check").into());
    }
    let std: Vec<ModuleId> = std_modules(&front);
    let mut units = crate::backend::lower_whole(&front, &sources, &std)?;
    let mut masked = units.clone();
    let inferred = crate::borrowing::infer(&front, &mut masked, &|unit, facts| {
        crate::backend::unit_masks_of(&front, unit, facts)
    })?;
    let by_def = inferred.masks;
    let mut named = Vec::new();
    for unit in &masked {
        for f in &unit.functions {
            if let FnKey::Item(def) = f.key
                && !f.borrow.is_owned()
            {
                let name = unit.name(def)?;
                named.push((name.module.to_string(), name.item.to_string(), f.borrow));
            }
        }
    }
    named.sort();
    // The units keep no mask: a build applies them only under borrowing.
    units.retain(|unit| matches!(unit.unit, UnitKind::Code(_)));
    let modules = std
        .iter()
        .map(|&m| front.modules.try_get(m).map(|info| info.path.to_string()))
        .collect::<Result<Vec<String>, Ice>>()?;
    let fronts = std
        .iter()
        .map(|&m| std_front_of(&front, m))
        .collect::<Result<Vec<StdFront>, Ice>>()?;
    Ok(WholeStd {
        units,
        by_def,
        named,
        modules,
        fronts,
    })
}

/// Std module `module`'s front record parts, from `front`, std's own front
/// end, which checked it from source.
fn std_front_of(front: &Frontend, module: ModuleId) -> Result<StdFront, Ice> {
    use crate::cache::Hash128;
    use crate::cache::front::{External, ImportKey};
    let info = front.modules.try_get(module)?;
    let path = info.path.to_string();
    let checked = front
        .checked(module)
        .ok_or_else(|| Ice::new(format!("std module `{path}` was not checked")))?;
    let file = info
        .file
        .ok_or_else(|| Ice::new(format!("std module `{path}` has no file")))?;
    let iface = &checked.interface;
    let hash = iface
        .hash
        .ok_or_else(|| Ice::new(format!("std module `{path}` has no interface hash")))?;
    let view = cheby_iface::DepView::new(&front.set, &checked.deps);
    let encoded = cheby_iface::encode(iface, &view, &front.modules, &front.globals)?;
    let spans = cheby_iface::encoded_order(iface, &front.globals)?
        .into_iter()
        .map(|index| {
            iface
                .item(index)
                .and_then(|item| item.span)
                .filter(|span| span.file == file)
                .map(|span| (span.range.start(), span.range.len()))
        })
        .collect();
    let at = usize::try_from(module.as_u32()).map_err(|_| Ice::new("module id overflow"))?;
    let mut imports = Vec::new();
    for target in front.import_targets.get(at).map_or(&[][..], Vec::as_slice) {
        imports.push(match target.module() {
            Some(m) => ImportKey::Module {
                path: front.modules.try_get(m)?.path.to_string(),
                hash: front
                    .set
                    .get(m)
                    .and_then(|i| i.hash)
                    .map(|h| Hash128(h.0))
                    .ok_or_else(|| {
                        Ice::new(format!("std's `{path}` imports a module without a hash"))
                    })?,
            },
            None => ImportKey::Unresolved,
        });
    }
    let mut externals = Vec::new();
    for (symbol, binding) in front.module_natives.get(at).map_or(&[][..], Vec::as_slice) {
        if binding.span.file != file {
            return Err(Ice::new(format!(
                "std's `{path}` binds `{symbol}` outside its file"
            )));
        }
        externals.push(External {
            symbol: symbol.to_string(),
            sig: binding.sig.clone(),
            span: (binding.span.range.start(), binding.span.range.len()),
        });
    }
    // The names a decoded interface holds: every item but the `priv` ones
    // (D-953), named as summaries name them.
    let visible: BTreeSet<String> = cheby_iface::encoded_order(iface, &front.globals)?
        .into_iter()
        .filter_map(|index| iface.item(index))
        .map(|item| {
            let name = front.globals.names.get(item.name);
            match item.parent.and_then(|p| iface.item(p)) {
                Some(parent) => format!("{}::{name}", front.globals.names.get(parent.name)),
                None => name.to_owned(),
            }
        })
        .collect();
    let references = walkable(&crate::load_set::summarize(front, module)?, &path, &visible);
    Ok(StdFront {
        path,
        interface: Hash128(hash.0),
        encoded,
        spans,
        imports,
        externals,
        references,
    })
}

/// Std module `path`'s summaries `entries` (the walk's form) as a command
/// walks them over decoded interfaces (D-1262): a decoded
/// interface holds no `priv` item (D-953), so a mention of one of the
/// module's own `priv` items (`visible` lacks its name; `priv` is module
/// private, so no other module mentions it) is replaced by that item's
/// mentions, recursively, in the order the walk meets them, each kept once.
/// The `priv` items' own entries stay, for the load set's closure.
fn walkable(
    entries: &[crate::cache::front::ReferenceSummary],
    path: &str,
    visible: &BTreeSet<String>,
) -> Vec<crate::cache::front::ReferenceSummary> {
    use crate::cache::front::{ItemRef, MentionerKind};
    let hidden: BTreeMap<String, Vec<ItemRef>> = entries
        .iter()
        .filter(|e| matches!(e.kind, MentionerKind::Function | MentionerKind::Constant))
        .filter(|e| !visible.contains(&e.item))
        .map(|e| (e.item.clone(), e.mentions.clone()))
        .collect();
    let expand = |mentions: &[ItemRef]| -> Vec<ItemRef> {
        let mut out = Vec::new();
        let mut kept = BTreeSet::new();
        let mut entered = BTreeSet::new();
        let mut stack: Vec<std::slice::Iter<'_, ItemRef>> = vec![mentions.iter()];
        while let Some(top) = stack.last_mut() {
            let Some(mention) = top.next() else {
                stack.pop();
                continue;
            };
            if mention.module == path && !visible.contains(&mention.item) {
                if let Some(inner) = hidden.get(&mention.item)
                    && entered.insert(mention.item.clone())
                {
                    stack.push(inner.iter());
                }
            } else if kept.insert(mention.clone()) {
                out.push(mention.clone());
            }
        }
        out
    };
    entries
        .iter()
        .map(|entry| crate::cache::front::ReferenceSummary {
            item: entry.item.clone(),
            kind: entry.kind,
            mentions: expand(&entry.mentions),
        })
        .collect()
}

/// The std modules of `front`, in build order.
fn std_modules(front: &Frontend) -> Vec<ModuleId> {
    front
        .order
        .iter()
        .copied()
        .filter(|&m| {
            m != ModuleId::BUILTIN
                && front
                    .modules
                    .get(m)
                    .is_some_and(|info| info.package == PackageId::STD)
        })
        .collect()
}

/// Std's whole masks (D-968) by def of `front`, for the build's std units
/// that `front` lowers itself (`cheby build`, `CHEBY_DUMP_DIR`, a build
/// without a cache), found by name; a name `front` cannot see is skipped,
/// since only the units it lowers take masks from this.
///
/// # Errors
///
/// [`whole_std`]'s errors, and an internal error when a name cannot be
/// read.
pub(crate) fn std_masks_in(front: &Frontend) -> Result<borrow::Masks, Ice> {
    let whole = whole_std()?;
    let names: Vec<(&str, &str)> = whole
        .named
        .iter()
        .map(|(module, item, _)| (module.as_str(), item.as_str()))
        .collect();
    let found = crate::borrowing::with_build_defs(front, |defs| borrow::find_defs(&names, defs))?;
    let mut masks = borrow::Masks::new();
    for ((_, _, mask), def) in whole.named.iter().zip(found) {
        if let Some(def) = def {
            masks.insert(def, *mask);
        }
    }
    Ok(masks)
}

/// Where std's records are read and written: the store, the lookup log
/// and whether lookups read (`CHEBY_DUMP_DIR` bypasses reads, D-965).
#[derive(Clone)]
pub(crate) struct StdStore<'a> {
    pub store: StoreShare,
    pub log: &'a CacheLog,
    pub reads: bool,
}

impl<'a> StdStore<'a> {
    /// `cache`'s store and log.
    pub(crate) fn of_cache(cache: &'a crate::Cache) -> Self {
        Self {
            store: cache.store.share(),
            log: &cache.log,
            reads: cache.reads,
        }
    }
}

/// What std's first use wrote ([`first_use`]): every std module's code
/// record and front record, by module path.
pub(crate) struct FirstUse {
    pub code: BTreeMap<String, CodeRecord>,
    pub fronts: BTreeMap<String, FrontRecord>,
}

/// Std's code records of `modules` (std module paths) under `settings`
/// (`BuildKind::Std`) and `mir` (std's, [`std_mir`]), each looked up and
/// logged as `code.std <module> hit|miss`. When one misses, std's first
/// use runs ([`first_use`]): every std module's record is compiled and
/// stored, and the records of `modules` come from it.
///
/// # Errors
///
/// An internal error when std cannot be compiled.
pub(crate) fn std_records(
    at: &StdStore<'_>,
    modules: &[String],
    settings: &Settings,
    mir: MirOptions,
) -> Result<BTreeMap<String, CodeRecord>, Ice> {
    let build = at.store.build_id();
    let mut found = BTreeMap::new();
    let mut missed = false;
    for module in modules {
        let key = std_code_key_at(build, module, settings, mir.positions);
        let record = if at.reads {
            at.store
                .get_record(&key)
                .and_then(|bytes| CodeRecord::decode(&bytes).ok())
        } else {
            None
        };
        at.log.record("code.std", module, record.is_some());
        match record {
            Some(record) => {
                found.insert(module.clone(), record);
            }
            None => missed = true,
        }
    }
    if !missed {
        return Ok(found);
    }
    let mut all = first_use(at, settings, mir)?;
    let mut out = BTreeMap::new();
    for module in modules {
        let record = match found.remove(module) {
            Some(record) => record,
            None => all
                .code
                .remove(module)
                .ok_or_else(|| Ice::new(format!("std's first use compiled no `{module}`")))?,
        };
        out.insert(module.clone(), record);
    }
    Ok(out)
}

/// Std's first use (D-980), whichever command makes it
/// (D-1266): every std module checked, lowered whole
/// and inferred as one unit ([`whole_std`]), counted with `mir`, compiled
/// alone into its code record and stored under [`std_code_key_at`], and
/// its front record stored under [`std_front_key`] with its interface
/// artifact (D-1262), all behind the command (D-1138);
/// `parse.std <module>` is logged for each std module. A write that fails
/// is skipped: a later command misses (D-1129).
///
/// # Errors
///
/// An internal error when std does not compile.
pub(crate) fn first_use(
    at: &StdStore<'_>,
    settings: &Settings,
    mir: MirOptions,
) -> Result<FirstUse, Ice> {
    use crate::cache::Hash128;
    let whole = whole_std()?;
    for module in &whole.modules {
        at.log.parsed_std(module);
    }
    let code = compile_whole(&whole, mir)?;
    let build = at.store.build_id();
    for (module, record) in &code {
        let key = std_code_key_at(build, module, settings, mir.positions);
        let _ = at.store.put_record(&key, &record.encode());
    }
    // Std reports no diagnostics: every record names the blob of the front
    // end's three empty lists (`frontend.rs`'s `DIAGNOSTIC_LISTS`).
    let empty = crate::cache::replay::empty_blob(3);
    let _ = at.store.put_blob(&empty);
    let mut fronts = BTreeMap::new();
    for front in &whole.fronts {
        let artifact = cheby_iface::write_artifact(build.0, &front.encoded, &front.spans);
        let _ = at.store.put_blob(&artifact);
        let record = FrontRecord {
            interface: front.interface,
            artifact: Hash128::of(&artifact),
            diagnostics: Hash128::of(&empty),
            imports: front.imports.clone(),
            externals: front.externals.clone(),
            reads: Vec::new(),
            references: front.references.clone(),
            uses: crate::cache::front::UseSummary::default(),
        };
        let _ = at
            .store
            .put_record(&std_front_key(build, &front.path), &record.encode());
        fronts.insert(front.path.clone(), record);
    }
    Ok(FirstUse { code, fronts })
}

/// Every unit of `whole` with its masks when `mir` borrows, counted with
/// `mir` (D-718), compiled alone into its code record in a JIT of its own
/// (D-1217) that is dropped once the records are taken: by module path.
fn compile_whole(whole: &WholeStd, mir: MirOptions) -> Result<BTreeMap<String, CodeRecord>, Ice> {
    let mut units = whole.units.clone();
    let counted: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| {
            if mir.borrows() {
                borrow::apply(unit, &whole.by_def)?;
            }
            cheby_mir::count_lowered_with(unit, &mir)
        })
        .collect();
    counted.into_iter().collect::<Result<(), Ice>>()?;
    let codegen = crate::backend::codegen_options(mir, false);
    let options = JitOptions {
        stub_unresolved: true,
        ..JitOptions::from_env()
    };
    let mut jit = Jit::configured(codegen, options).map_err(|e| Ice::new(e.to_string()))?;
    let batch: Vec<BatchUnit<'_>> = units.iter().map(BatchUnit::Mir).collect();
    let records = jit
        .compile_units(&batch, &BatchCx { test_ordinals: &[] })
        .map_err(|e| Ice::new(e.to_string()))?;
    let mut out = BTreeMap::new();
    for (unit, record) in units.iter().zip(records) {
        let record =
            record.ok_or_else(|| Ice::new(format!("std's `{}` gave no record", unit.path)))?;
        out.insert(unit.path.to_string(), record);
    }
    drop(jit);
    Ok(out)
}
