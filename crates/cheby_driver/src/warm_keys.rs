//! What a warm `cheby run` or `cheby test` knows before its check (K4d,
//! D-1250): which package modules it parses and checks from source. Every
//! other module the package index vouches for (D-962) is a hit: it is not
//! parsed or checked, its interface is decoded, its replayed diagnostics
//! are printed, and the back end loads its code and tests records (D-1221)
//! and reads its [`WarmFacts`] instead of its THIR: its summaries in the
//! walk's form and its `main`'s entry ([`WarmModule`]).
//!
//! The key functions K4c computes with before lowering (D-1221, D-1222),
//! `borrowing.rs`'s [`plan`], [`stored_masks`], [`with_build_defs`] and
//! [`unit_keys`], are generic over a [`KeyView`]: a [`Frontend`] after the
//! check, or a [`WarmView`] before it, so a key has one computation. No
//! command computes a key before its check yet (D-1256).
//! Which modules miss is decided as [`keys_before_check`] says
//! (D-1256).
//!
//! [`plan`]: crate::borrowing::plan
//! [`stored_masks`]: crate::borrowing::stored_masks
//! [`with_build_defs`]: crate::borrowing::with_build_defs
//! [`unit_keys`]: crate::borrowing::unit_keys

use std::collections::BTreeSet;

use cheby_iface::{
    DefId, DefKind, DepView, Globals, Ice, IfaceItem, InterfaceSet, ModuleId, ModuleInterface,
    ModuleTable, PackageId, ParamIdx, Ty, TyKind, Vis,
};
use cheby_mir::{Defs, MirOptions};
use cheby_package::graph::{Graph, reachable};
use cheby_types::thir;

use crate::borrowing::RecordSink;
use crate::cache::Hash128;
use crate::cache::front::{
    ReferenceSummary, WarmFacts, WarmItem, WarmMain, WarmTest, warm_facts_key,
};
use crate::cache::index::IndexEntry;
use crate::frontend::{CacheUse, Frontend};

/// What a warm front end knows before the check (D-1250): the package
/// index's entries that vouch for a module (its source hash equals the
/// file's, D-962), each with `f1`, `f2` and its front record (reference
/// summary, interface hash, lowering hint key); no checked body and no
/// THIR.
pub(crate) struct WarmView<'a> {
    /// The command's cache.
    pub cache: CacheUse<'a>,
    /// Every module of the build, std's included, as the front end's
    /// worklist found them.
    pub modules: &'a ModuleTable,
    /// The import graph over `modules`' ids.
    pub graph: &'a Graph,
    /// Per module id: the index entry that vouches for it, or `None`
    /// (parsed and checked anyway).
    pub vouched: &'a [Option<&'a IndexEntry>],
    /// The record sink over the vouched entries, which the key functions
    /// read the store and log through.
    pub records: Option<&'a RecordSink>,
    /// The build's globals.
    pub globals: &'a Globals,
    /// The interfaces at hand before the check: the builtin module's only
    /// (no module is checked or decoded yet).
    pub set: &'a InterfaceSet,
}

/// What the key functions read, implemented for [`Frontend`] (after the
/// check, K4c's path) and for [`WarmView`] (before it): the record sink,
/// the modules' paths and packages, and the definitions callees resolve to
/// (decoded interfaces for a view, checked ones for a [`Frontend`]).
pub(crate) trait KeyView: Sync {
    /// The command's record sink, `None` without a cache.
    fn records(&self) -> Option<&RecordSink>;

    /// Every module of the build.
    fn module_table(&self) -> &ModuleTable;

    /// Calls `f` with the interfaces of the whole build, which name every
    /// def a unit, a facts record or a masks record can hold.
    fn with_defs<R>(&self, f: impl FnOnce(&Defs<'_>) -> R) -> R;
}

impl KeyView for Frontend {
    fn records(&self) -> Option<&RecordSink> {
        self.records.as_ref()
    }

    fn module_table(&self) -> &ModuleTable {
        &self.modules
    }

    fn with_defs<R>(&self, f: impl FnOnce(&Defs<'_>) -> R) -> R {
        with_set(&self.modules, &self.set, &self.globals, f)
    }
}

impl KeyView for WarmView<'_> {
    fn records(&self) -> Option<&RecordSink> {
        self.records
    }

    fn module_table(&self) -> &ModuleTable {
        self.modules
    }

    fn with_defs<R>(&self, f: impl FnOnce(&Defs<'_>) -> R) -> R {
        with_set(self.modules, self.set, self.globals, f)
    }
}

/// Calls `f` with the interfaces of `set` over every module of `modules`,
/// without recording: naming a fact reads nothing lowering did not (D-955).
fn with_set<R>(
    modules: &ModuleTable,
    set: &InterfaceSet,
    globals: &Globals,
    f: impl FnOnce(&Defs<'_>) -> R,
) -> R {
    let all: Vec<ModuleId> = modules.iter().map(|(id, _)| id).collect();
    let view = DepView::new(set, &all);
    f(&Defs {
        own: None,
        deps: &view,
        modules,
        globals,
    })
}

/// What a warm build knows before the check (K4d).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BuildMisses {
    /// The package modules whose code record misses.
    pub code: BTreeSet<ModuleId>,
    /// The package modules whose tests record misses (`cheby test`).
    pub tests: BTreeSet<ModuleId>,
    /// Modules whose checked body a miss's lowering needs (D-1250's first
    /// case). The lowering hint names interface reads (D-955), which are
    /// decoded, never checked.
    pub body_reads: BTreeSet<ModuleId>,
    /// Modules holding a constant whose entry misses (K5b), and every
    /// module they import, transitively (D-1134).
    pub constant_modules: BTreeSet<ModuleId>,
}

impl BuildMisses {
    /// Whether `module` is checked from source: one of the sets holds it.
    pub(crate) fn checks(&self, module: ModuleId) -> bool {
        self.code.contains(&module)
            || self.tests.contains(&module)
            || self.body_reads.contains(&module)
            || self.constant_modules.contains(&module)
    }
}

/// What the front end holds when it decides which modules it checks
/// (`frontend.rs`'s `run_inner`, before the check).
pub(crate) struct FrontState<'a> {
    /// Every module, std's included.
    pub modules: &'a ModuleTable,
    /// The import graph.
    pub graph: &'a Graph,
    /// Per module id: its source hashes (D-963).
    pub source_hashes: &'a [Option<crate::SourceHashes>],
    /// The build's globals.
    pub globals: &'a Globals,
    /// The front end's interfaces, empty but the builtin module's.
    pub set: &'a InterfaceSet,
    /// The package's `[const-eval]` limits, which constants' keys hold
    /// (D-977).
    pub limits: crate::cache::consts::ConstLimits,
    /// The package's name: its root module's path.
    pub package: &'a str,
}

/// The misses of a warm `cheby run` or `cheby test` before its check
/// (D-1250): `None` when the command computes none and checks every module
/// (another command, no package index, `--reuse-report`, or a front end
/// started again).
///
/// # Errors
///
/// As [`keys_before_check`].
pub(crate) fn misses_before_check(
    use_: CacheUse<'_>,
    state: &FrontState<'_>,
) -> Result<Option<BuildMisses>, Ice> {
    let (Some(build), Some((index, _))) = (use_.build, use_.index) else {
        return Ok(None);
    };
    if !use_.lazy || !use_.cache.reads || build.reuse {
        return Ok(None);
    }
    let vouched: Vec<Option<&IndexEntry>> = state
        .modules
        .iter()
        .map(|(id, info)| {
            if info.package != PackageId::LOCAL {
                return None;
            }
            let hash = state
                .source_hashes
                .get(usize::try_from(id.as_u32()).ok()?)?
                .as_ref()?
                .sip128;
            index
                .get(&info.path.to_string())
                .filter(|entry| entry.source == hash)
        })
        .collect();
    let sink = RecordSink::new(use_.cache, vouched.iter().flatten().copied());
    let view = WarmView {
        cache: use_,
        modules: state.modules,
        graph: state.graph,
        vouched: &vouched,
        records: Some(&sink),
        globals: state.globals,
        set: state.set,
    };
    keys_before_check(&view, &build, &state.limits, state.package).map(Some)
}

/// D-1250 (refines D-1132): which package modules a warm build checks,
/// from `view` alone; with K5b's `consts::misses_before_check_from`, which
/// constants miss.
///
/// D-1256: a code key names its callees' slots and
/// its masks by def (D-1193, D-1191), which only interfaces give, and
/// before the check no interface exists, std's included (a decoded package
/// interface refers to std's). So the keys themselves are computed after
/// the check, by K4c's path (`backend.rs`'s `lower_through_cache`), and the
/// warm check is optimistic: a module the index does not vouch for (edited
/// or new) is a code miss, and a vouched module is a hit unless what the
/// back end will read of it without its body is missing. Its body is read
/// (it is checked) when its warm facts are missing or its `main`'s entry
/// cannot be named from decoded interfaces (the entries unit's lowering
/// reads `main`'s entry, D-1250's first case; the warm facts hold it), under
/// `cheby test` when a test's entry cannot be named so (the entries unit
/// names each selected test by its def and entry; a decoded interface holds
/// no test, D-1121, so the warm facts' tests are added to it), when a constant of it
/// misses (K5b) or it is imported, transitively, by a module whose constant
/// misses (D-1134). `--reuse-report`, which reads a record only with its
/// reuse lines (`reuse_replay.rs`), checks every module as before
/// ([`misses_before_check`] gives no misses). A hit the back end must lower
/// after all (an edit that only a reader's lowering reads, D-955; a body
/// edit that changes a package-visible function's mask, D-812; a change of
/// D-958's settings or of `CHEBY_POSITIONS`, D-1173) starts the command
/// again with every module checked ([`Frontend::wanted_body`], D-1256).
///
/// # Errors
///
/// An [`Ice`] if a stored name cannot be read.
pub(crate) fn keys_before_check(
    view: &WarmView<'_>,
    build: &WarmBuild<'_>,
    limits: &crate::cache::consts::ConstLimits,
    package: &str,
) -> Result<BuildMisses, Ice> {
    let mut misses = BuildMisses::default();
    let cache = view.cache.cache;
    let store_build = cache.store.build_id();
    for (at, entry) in view.vouched.iter().enumerate() {
        let module =
            ModuleId::from_u32(u32::try_from(at).map_err(|_| Ice::new("too many modules"))?);
        let Some(entry) = entry else {
            // A package module the index does not vouch for was edited (or
            // is new) since this family's last build: its code misses, even
            // when another family's command wrote its front record since.
            if view
                .modules
                .get(module)
                .is_some_and(|info| info.package == PackageId::LOCAL)
            {
                misses.code.insert(module);
            }
            continue;
        };
        let facts = cache
            .store
            .get_record(&warm_facts_key(entry.f2, store_build))
            .ok()
            .flatten()
            .and_then(|bytes| WarmFacts::decode(&bytes));
        let read = match &facts {
            None => true,
            Some(facts) => facts.main == WarmMain::Opaque || (build.tests && facts.tests.is_none()),
        };
        if read {
            misses.body_reads.insert(module);
        }
    }
    // D-1134 with K5b: the modules of the constants the command's roots
    // reach whose entries miss, and every package module they import,
    // transitively. A constant reached only through `main`'s entry
    // dictionary is not counted; if its entry misses, the build wants its
    // module's body and the command starts again.
    let settings = crate::cache::consts::settings(build.mir);
    let roots = if build.tests {
        crate::cache::consts::ConstRoots::Tests
    } else {
        crate::cache::consts::ConstRoots::Main(build.main.unwrap_or(package))
    };
    let holders: Vec<u32> =
        crate::cache::consts::misses_before_check_from(view, &settings, limits, roots)
            .into_iter()
            .map(ModuleId::as_u32)
            .collect();
    for node in reachable(view.graph, &holders) {
        let module = ModuleId::from_u32(node);
        if view
            .modules
            .get(module)
            .is_some_and(|info| info.package == PackageId::LOCAL)
        {
            misses.constant_modules.insert(module);
        }
    }
    Ok(misses)
}

/// What a warm `cheby run` or `cheby test` decides its misses under
/// (D-1250): the MIR options of its build, whether it prints the reuse
/// report, and its roots.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WarmBuild<'a> {
    /// The build's MIR options.
    pub mir: MirOptions,
    /// `--reuse-report` (D-965).
    pub reuse: bool,
    /// `cheby test`'s build: its roots are every test of the package.
    pub tests: bool,
    /// `cheby run`'s `--module` path, whose `main` runs; `None` for the
    /// package's root module.
    pub main: Option<&'a str>,
}

/// A package hit of a warm build (D-1250): what the back end and the
/// commands read of it instead of its check, from its [`WarmFacts`].
#[derive(Clone, Debug)]
pub(crate) struct WarmModule {
    /// Its summaries, in the walk's form (`load_set.rs`).
    pub summaries: Vec<ReferenceSummary>,
    /// Its `main`, without a body: its def, its name's span and its entry.
    pub main: Option<thir::FnDef>,
    /// Its tests on native, in source order, each with its def (added to
    /// its decoded interface, [`add_hidden`]), name, span and entry, and an
    /// empty body that nothing lowers: a hit's tests unit comes from its
    /// record.
    pub tests: Vec<thir::TestDef>,
}

/// The private items of `iface`, tests left out, in item order, each
/// child with its parent's place in the list (a constructor's or interface
/// function's parent is as private as it, D-1121).
fn private_items(iface: &ModuleInterface, globals: &Globals) -> Vec<WarmItem> {
    let mut out: Vec<WarmItem> = Vec::new();
    let mut at: std::collections::BTreeMap<cheby_iface::DefIndex, u32> =
        std::collections::BTreeMap::new();
    for (index, item) in iface.items.iter_enumerated() {
        if item.vis != Vis::Priv || item.kind == DefKind::Test {
            continue;
        }
        let parent = match item.parent {
            None => None,
            Some(p) => match at.get(&p) {
                Some(&n) => Some(n),
                None => continue,
            },
        };
        let span = item
            .span
            .map_or((0, 0), |s| (s.range.start(), s.range.len()));
        if let Ok(n) = u32::try_from(out.len()) {
            at.insert(index, n);
        }
        out.push(WarmItem {
            kind: kind_tag(item.kind),
            name: globals.names.get(item.name).to_owned(),
            parent,
            span,
        });
    }
    out
}

/// Adds what no encoding holds of the hit whose front identity is `f2` and
/// whose file is `file` to its decoded interface `iface`, from its warm
/// facts, after its items: its private items (D-953), without signatures
/// (`ItemSig::Error`: a warm build lowers nothing against them, and a
/// module checked against them that names one reports E0043 and is checked
/// again on fresh interfaces, D-1136), and its tests as the resolver gives
/// them (private, on native, `ItemSig::Test`). Returns the facts, read
/// once ([`warm_module`] takes them); `None`, adding nothing, when they are
/// missing or malformed (the front end then starts again).
///
/// # Errors
///
/// An [`Ice`] if the interface refuses an item.
pub(crate) fn add_hidden(
    store: &crate::cache::Store,
    f2: Hash128,
    file: cheby_diag::FileId,
    globals: &Globals,
    iface: &mut ModuleInterface,
) -> Result<Option<WarmFacts>, Ice> {
    let Some(facts) = store
        .get_record(&warm_facts_key(f2, store.build_id()))
        .ok()
        .flatten()
        .and_then(|bytes| WarmFacts::decode(&bytes))
    else {
        return Ok(None);
    };
    // Malformed facts (an unknown kind, a child before its parent) add
    // nothing.
    let well_formed = facts.private.iter().enumerate().all(|(n, item)| {
        kind_of_tag(item.kind).is_some()
            && item
                .parent
                .is_none_or(|p| usize::try_from(p).is_ok_and(|p| p < n))
    });
    if !well_formed {
        return Ok(None);
    }
    let mut pushed: Vec<cheby_iface::DefIndex> = Vec::with_capacity(facts.private.len());
    for item in &facts.private {
        let Some(kind) = kind_of_tag(item.kind) else {
            return Ok(None);
        };
        let parent = match item.parent {
            None => None,
            Some(p) => Some(
                *usize::try_from(p)
                    .ok()
                    .and_then(|p| pushed.get(p))
                    .ok_or_else(|| Ice::new("a private item before its parent"))?,
            ),
        };
        pushed.push(iface.push(IfaceItem {
            name: globals.names.intern(&item.name),
            kind,
            vis: Vis::Priv,
            parent,
            span: Some(cheby_diag::Span::new(
                file,
                cheby_diag::TextRange::at(item.span.0, item.span.1),
            )),
            targets: cheby_package::Targets::ALL,
            deprecated: None,
            sig: cheby_iface::ItemSig::Error,
        })?);
    }
    for test in facts.tests.iter().flatten() {
        iface.push(IfaceItem {
            name: globals.names.intern(&test.name),
            kind: DefKind::Test,
            vis: Vis::Priv,
            parent: None,
            span: Some(cheby_diag::Span::new(
                file,
                cheby_diag::TextRange::at(test.span.0, test.span.1),
            )),
            targets: cheby_package::Targets::only(cheby_package::Target::Native),
            deprecated: None,
            sig: cheby_iface::ItemSig::Test,
        })?;
    }
    Ok(Some(facts))
}

/// The warm module of package hit `module`, whose file is `file`, from its
/// warm `facts` ([`add_hidden`] read them) against `naming`'s interfaces
/// (its own decoded one included); `None` when they name what the
/// interfaces lack: the front end then starts again, checking every
/// module.
pub(crate) fn warm_module(
    facts: WarmFacts,
    module: ModuleId,
    file: cheby_diag::FileId,
    naming: &Naming<'_>,
) -> Option<WarmModule> {
    let main = match facts.main {
        WarmMain::None => None,
        WarmMain::Opaque => return None,
        WarmMain::Entry { item, span, entry } => {
            let iface = naming.set.get(module)?;
            let (index, _) = iface.items.iter_enumerated().find(|(_, it)| {
                it.kind == DefKind::Fn
                    && it.parent.is_none()
                    && naming.globals.names.get(it.name) == item
            })?;
            Some(thir::FnDef {
                def: DefId::new(module, index),
                span: cheby_diag::Span::new(file, cheby_diag::TextRange::at(span.0, span.1)),
                native_external: None,
                body: None,
                entry: Some(naming.decode_entry(&entry)?),
            })
        }
    };
    // The tests [`add_hidden`] put after the decoded items, in order.
    let iface = naming.set.get(module)?;
    let defs: Vec<DefId> = iface
        .items
        .iter_enumerated()
        .filter(|(_, it)| it.kind == DefKind::Test)
        .map(|(index, _)| DefId::new(module, index))
        .collect();
    let facts_tests = facts.tests.unwrap_or_default();
    if defs.len() != facts_tests.len() {
        return None;
    }
    let mut tests = Vec::with_capacity(defs.len());
    for (def, test) in defs.into_iter().zip(facts_tests) {
        tests.push(thir::TestDef {
            def,
            name: test.name.into_boxed_str(),
            span: cheby_diag::Span::new(file, cheby_diag::TextRange::at(test.span.0, test.span.1)),
            body: thir::Body {
                params: Vec::new(),
                locals: cheby_iface::IndexVec::new(),
                exprs: cheby_iface::IndexVec::new(),
                value: thir::ExprId::from_u32(0),
                ret: Ty::NIL,
            },
            entry: naming.decode_entry(&test.entry)?,
        });
    }
    Some(WarmModule {
        summaries: facts.summaries,
        main,
        tests,
    })
}

/// Writes the warm facts of every package module `front` checked from
/// source and whose front record the command found or wrote, when its
/// store has none (D-1138, D-1139; a failed write is skipped, D-1129).
/// Only `cheby run` and `cheby test` call it.
///
/// # Errors
///
/// An [`Ice`] if a module's THIR and interfaces disagree.
pub(crate) fn store_facts(front: &Frontend) -> Result<(), Ice> {
    let Some((store, reads)) = front.cache_store.as_ref() else {
        return Ok(());
    };
    let naming = Naming {
        modules: &front.modules,
        set: &front.set,
        globals: &front.globals,
    };
    for (&module, &f2) in &front.warm_f2 {
        let Some(checked) = front.checked_quiet(module) else {
            continue;
        };
        let key = warm_facts_key(f2, store.build_id());
        if *reads && store.get_record(&key).is_some() {
            continue;
        }
        let summaries = crate::load_set::summarize(front, module)?;
        let main = match checked.thir.fns.iter().find(|f| f.entry.is_some()) {
            None => WarmMain::None,
            Some(f) => {
                let item = checked
                    .interface
                    .item(f.def.index)
                    .map(|it| front.globals.names.get(it.name).to_owned());
                let entry = f.entry.as_ref().and_then(|e| naming.encode_entry(e));
                match (item, entry) {
                    (Some(item), Some(entry)) => WarmMain::Entry {
                        item,
                        span: (f.span.range.start(), f.span.range.len()),
                        entry,
                    },
                    _ => WarmMain::Opaque,
                }
            }
        };
        let tests: Option<Vec<WarmTest>> = checked
            .thir
            .tests
            .iter()
            .filter(|t| crate::test::on_native(front, t))
            .map(|t| {
                Some(WarmTest {
                    name: t.name.to_string(),
                    span: (t.span.range.start(), t.span.range.len()),
                    entry: naming.encode_entry(&t.entry)?,
                })
            })
            .collect();
        let facts = WarmFacts {
            summaries,
            main,
            tests,
            private: private_items(&checked.interface, &front.globals),
        };
        let _ = store.put_record(&key, &facts.encode());
    }
    Ok(())
}

/// The names a warm `main`'s entry is encoded and decoded with: module
/// paths, item kinds and names (`Parent::name` for a constructor or an
/// interface function), never a `DefId` (D-264), since a decoded interface
/// numbers items in encoded order (D-1121).
pub(crate) struct Naming<'a> {
    /// Every module of the build.
    pub modules: &'a ModuleTable,
    /// The interfaces the names resolve in.
    pub set: &'a InterfaceSet,
    /// The build's globals.
    pub globals: &'a Globals,
}

/// How deep an encoded type or dictionary may nest.
const DEPTH: u32 = 64;

impl Naming<'_> {
    /// `entry`'s bytes, or `None` when it names something a decoded
    /// interface holds no name for (a private item, a test).
    pub(crate) fn encode_entry(&self, entry: &thir::EntryInfo) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        match entry {
            thir::EntryInfo::Nil => out.push(0),
            thir::EntryInfo::Result { err_ty, show } => {
                out.push(1);
                self.put_ty(*err_ty, &mut out, DEPTH)?;
                match show {
                    None => out.push(0),
                    Some(dict) => {
                        out.push(1);
                        self.put_dict(dict, &mut out, DEPTH)?;
                    }
                }
            }
        }
        Some(out)
    }

    /// The entry of `bytes`, resolved in this naming's interfaces.
    pub(crate) fn decode_entry(&self, bytes: &[u8]) -> Option<thir::EntryInfo> {
        let mut r = Bytes(bytes);
        let entry = match r.u8()? {
            0 => thir::EntryInfo::Nil,
            1 => {
                let err_ty = self.get_ty(&mut r, DEPTH)?;
                let show = match r.u8()? {
                    0 => None,
                    1 => Some(self.get_dict(&mut r, DEPTH)?),
                    _ => return None,
                };
                thir::EntryInfo::Result { err_ty, show }
            }
            _ => return None,
        };
        r.0.is_empty().then_some(entry)
    }

    /// `item`'s name in `iface`: `Parent::name` for a child.
    fn item_text(&self, iface: &ModuleInterface, item: &IfaceItem) -> Option<String> {
        let name = self.globals.names.get(item.name);
        Some(match item.parent {
            Some(parent) => format!(
                "{}::{name}",
                self.globals.names.get(iface.item(parent)?.name)
            ),
            None => name.to_owned(),
        })
    }

    fn put_def(&self, def: DefId, out: &mut Vec<u8>) -> Option<()> {
        let iface = self.set.get(def.module)?;
        let item = iface.item(def.index)?;
        let builtin = def.module == ModuleId::BUILTIN;
        if !builtin && (item.vis == Vis::Priv || item.kind == DefKind::Test) {
            return None;
        }
        let path = if builtin {
            String::new()
        } else {
            self.modules.get(def.module)?.path.to_string()
        };
        put_str(out, &path);
        out.push(kind_tag(item.kind));
        put_str(out, &self.item_text(iface, item)?);
        Some(())
    }

    fn get_def(&self, r: &mut Bytes<'_>) -> Option<DefId> {
        let path = r.str()?;
        let kind = r.u8()?;
        let text = r.str()?;
        let module = if path.is_empty() {
            ModuleId::BUILTIN
        } else {
            self.modules.find(&crate::parse_module_path(&path)?)?
        };
        let iface = self.set.get(module)?;
        iface
            .items
            .iter_enumerated()
            .find(|(_, it)| {
                kind_tag(it.kind) == kind && self.item_text(iface, it).as_deref() == Some(&text)
            })
            .map(|(index, _)| DefId::new(module, index))
    }

    fn put_tys(&self, tys: &[Ty], out: &mut Vec<u8>, depth: u32) -> Option<()> {
        put_u32(out, u32::try_from(tys.len()).ok()?);
        for &ty in tys {
            self.put_ty(ty, out, depth)?;
        }
        Some(())
    }

    fn get_tys(&self, r: &mut Bytes<'_>, depth: u32) -> Option<Box<[Ty]>> {
        let n = r.u32()?;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(self.get_ty(r, depth)?);
        }
        Some(out.into_boxed_slice())
    }

    fn put_ty(&self, ty: Ty, out: &mut Vec<u8>, depth: u32) -> Option<()> {
        let depth = depth.checked_sub(1)?;
        match self.globals.types.kind(ty) {
            TyKind::Error => out.push(0),
            TyKind::Never => out.push(1),
            TyKind::Named { def, args } => {
                out.push(2);
                self.put_def(*def, out)?;
                self.put_tys(args, out, depth)?;
            }
            TyKind::Tuple(elems) => {
                out.push(3);
                self.put_tys(elems, out, depth)?;
            }
            TyKind::Fn { params, ret } => {
                out.push(4);
                self.put_tys(params, out, depth)?;
                self.put_ty(*ret, out, depth)?;
            }
            TyKind::Param(param) => {
                out.push(5);
                put_u32(out, param.as_u32());
            }
            TyKind::Dyn { iface, args } => {
                out.push(6);
                self.put_def(*iface, out)?;
                self.put_tys(args, out, depth)?;
            }
        }
        Some(())
    }

    fn get_ty(&self, r: &mut Bytes<'_>, depth: u32) -> Option<Ty> {
        let depth = depth.checked_sub(1)?;
        let kind = match r.u8()? {
            0 => TyKind::Error,
            1 => TyKind::Never,
            2 => TyKind::Named {
                def: self.get_def(r)?,
                args: self.get_tys(r, depth)?,
            },
            3 => TyKind::Tuple(self.get_tys(r, depth)?),
            4 => TyKind::Fn {
                params: self.get_tys(r, depth)?,
                ret: self.get_ty(r, depth)?,
            },
            5 => TyKind::Param(ParamIdx::from_u32(r.u32()?)),
            6 => TyKind::Dyn {
                iface: self.get_def(r)?,
                args: self.get_tys(r, depth)?,
            },
            _ => return None,
        };
        Some(self.globals.types.intern(kind))
    }

    fn put_dicts(&self, dicts: &[thir::Dict], out: &mut Vec<u8>, depth: u32) -> Option<()> {
        put_u32(out, u32::try_from(dicts.len()).ok()?);
        for dict in dicts {
            self.put_dict(dict, out, depth)?;
        }
        Some(())
    }

    fn get_dicts(&self, r: &mut Bytes<'_>, depth: u32) -> Option<Box<[thir::Dict]>> {
        let n = r.u32()?;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(self.get_dict(r, depth)?);
        }
        Some(out.into_boxed_slice())
    }

    fn put_dict(&self, dict: &thir::Dict, out: &mut Vec<u8>, depth: u32) -> Option<()> {
        let depth = depth.checked_sub(1)?;
        match dict {
            thir::Dict::Impl { iface, ty, fns } => {
                out.push(0);
                self.put_def(*iface, out)?;
                self.put_ty(*ty, out, depth)?;
                put_u32(out, u32::try_from(fns.len()).ok()?);
                for f in fns {
                    self.put_def(f.def, out)?;
                    self.put_tys(&f.ty_args, out, depth)?;
                    self.put_dicts(&f.dicts, out, depth)?;
                }
            }
            thir::Dict::Param { param, bound } => {
                out.push(1);
                put_u32(out, param.as_u32());
                put_u32(out, *bound);
            }
            thir::Dict::Embedded { of, iface } => {
                out.push(2);
                self.put_dict(of, out, depth)?;
                self.put_def(*iface, out)?;
            }
            thir::Dict::Tuple { iface, ty, elems } => {
                out.push(3);
                self.put_def(*iface, out)?;
                self.put_ty(*ty, out, depth)?;
                self.put_dicts(elems, out, depth)?;
            }
            thir::Dict::Dyn { iface, ty } => {
                out.push(4);
                self.put_def(*iface, out)?;
                self.put_ty(*ty, out, depth)?;
            }
        }
        Some(())
    }

    fn get_dict(&self, r: &mut Bytes<'_>, depth: u32) -> Option<thir::Dict> {
        let depth = depth.checked_sub(1)?;
        Some(match r.u8()? {
            0 => {
                let iface = self.get_def(r)?;
                let ty = self.get_ty(r, depth)?;
                let n = r.u32()?;
                let mut fns = Vec::new();
                for _ in 0..n {
                    fns.push(thir::DictFn {
                        def: self.get_def(r)?,
                        ty_args: self.get_tys(r, depth)?,
                        dicts: self.get_dicts(r, depth)?,
                    });
                }
                thir::Dict::Impl {
                    iface,
                    ty,
                    fns: fns.into_boxed_slice(),
                }
            }
            1 => thir::Dict::Param {
                param: ParamIdx::from_u32(r.u32()?),
                bound: r.u32()?,
            },
            2 => thir::Dict::Embedded {
                of: Box::new(self.get_dict(r, depth)?),
                iface: self.get_def(r)?,
            },
            3 => thir::Dict::Tuple {
                iface: self.get_def(r)?,
                ty: self.get_ty(r, depth)?,
                elems: self.get_dicts(r, depth)?,
            },
            4 => thir::Dict::Dyn {
                iface: self.get_def(r)?,
                ty: self.get_ty(r, depth)?,
            },
            _ => return None,
        })
    }
}

/// A definition kind's byte in an encoded name.
const fn kind_tag(kind: DefKind) -> u8 {
    match kind {
        DefKind::Fn => 0,
        DefKind::Type => 1,
        DefKind::Alias => 2,
        DefKind::Const => 3,
        DefKind::Interface => 4,
        DefKind::InterfaceFn => 5,
        DefKind::Ctor => 6,
        DefKind::Test => 7,
    }
}

/// The definition kind of a byte [`kind_tag`] gave.
const fn kind_of_tag(tag: u8) -> Option<DefKind> {
    Some(match tag {
        0 => DefKind::Fn,
        1 => DefKind::Type,
        2 => DefKind::Alias,
        3 => DefKind::Const,
        4 => DefKind::Interface,
        5 => DefKind::InterfaceFn,
        6 => DefKind::Ctor,
        7 => DefKind::Test,
        _ => return None,
    })
}

/// Appends `v`, little-endian.
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Appends `s`, length-prefixed.
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, u32::try_from(s.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(s.as_bytes());
}

/// A reader over encoded entries.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    fn str(&mut self) -> Option<String> {
        let n = usize::try_from(self.u32()?).ok()?;
        std::str::from_utf8(self.take(n)?).ok().map(str::to_owned)
    }
}
