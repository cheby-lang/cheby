//! Borrowing inference's place in the build cache (D-968, D-969, D-955;
//! `cache.md` §5.2, §5.3).
//!
//! Inference itself runs in `backend.rs` over a build's lowered units
//! (D-779): [`scope_facts`] gives the facts of the inference scope, each
//! code unit's masks are inferred from them, and the masks are written into
//! every unit. This module splits the result along the seams the cache
//! needs:
//!
//! - **Facts records.** Each package module's code unit and tests unit
//!   contribute their own facts ([`cheby_mir::borrow::unit_facts`]), which
//!   name functions by module path and item. They are stored under the
//!   unit's code identity, `C2` or `T2` ([`code_keys::code_identity`]):
//!   the front identity `F2` extended with the hidden reads of the unit's
//!   lowering and borrow inference (D-955), whose paths a hint under `F2`
//!   names ([`code_keys::lreads_key`]).
//! - **Masks records.** A package module's masks, per build kind, under
//!   `C2`, the kind and the digest of the build's facts about the module's
//!   own functions ([`cheby_mir::borrow::BuildFacts::about_module`]).
//! - **Code keys.** Every package unit a command compiles gets its code
//!   key ([`code_keys::code_key`]), logged as a `key` line under
//!   `CHEBY_CACHE_LOG` (D-973); K4c stores code records under them.
//!
//! Since K4c the records are read to skip work (D-1192): before it lowers
//! anything, a command computes each package unit's identity from its
//! hint and reads its facts record ([`plan`]); the units whose facts hit
//! are lowered only if their masks record ([`stored_masks`]) or their code
//! record misses, and every unit's code key is computed the same way
//! whether it was lowered or not ([`unit_keys`]; `backend.rs`'s
//! `lower_through_cache`). The records of the units a command lowers are
//! written behind the command (D-1138), at most once per process (D-1139)
//! ([`write_fresh`]). A record that hits for a unit lowered fresh is
//! compared with the fresh result in a debug toolchain, and a difference is
//! an internal error naming the module (D-1131's pattern); a unit loaded
//! from its code record lowers nothing to compare
//! (D-1221). Std modules get no records (the
//! build ID fixes std until K6), nor does the entries unit (D-964), nor a
//! module without a front record (one with errors is never lowered,
//! D-964). The records are artifacts (D-264, D-972): nothing in a key or a
//! record depends on a path, an address, a `DefId` or hash-map order.
//!
//! The inference scope (D-968): the package's masks are inferred over the
//! load set's code units and, in a test build, the tests units of the
//! modules with a selected test (`backend.rs` lowers exactly those); std's
//! masks keep today's scope, the build's std units, until K6 infers them
//! over every std module. D-1194: D-968 says the entries
//! unit contributes no facts, but its statics may hold `main`'s error
//! dictionary, which names a package function as a value; without that
//! fact the function would borrow and the dictionary's call would break
//! its convention (the counting stops on it). So the entries unit's facts
//! stay in the scope. The entries unit is never cached, so they are
//! always fresh; they enter the masks key through the digest.

use std::collections::{BTreeMap, BTreeSet};

use cheby_codegen_clif::record::CodeRecord;
use cheby_codegen_clif::{CodegenOptions, mangle};
use cheby_iface::{DepView, Ice, ModuleId, PackageId};
use cheby_mir::borrow::{self, BorrowMask, BuildFacts, UnitFacts};
use cheby_mir::{Defs, FnKey, MirModule, MirOptions, RcMode, UnitKind};
use rayon::prelude::*;

use crate::Cache;
use crate::backend::{Roots, UnitReads};
use crate::cache::code_keys::{self, CodeKeyInputs, MasksRecord, UnitPart};
use crate::cache::front::{encode_hint, sealed, unsealed};
use crate::cache::index::IndexEntry;
use crate::cache::{BuildKind, CacheLog, Hash128, Key, Settings, StoreShare};
use crate::frontend::Frontend;

/// Where a command's borrowing records go: the store, the lookup log and
/// each package module's front identity, from the front end's package
/// index entries. The front end makes one when the command has a cache;
/// the REPL and the cacheless entry points have none (D-964).
#[derive(Debug)]
pub(crate) struct RecordSink {
    store: StoreShare,
    log: CacheLog,
    /// Whether lookups read the store (`CHEBY_DUMP_DIR` bypasses reads
    /// and still writes, D-965).
    reads: bool,
    /// Per package module path: its front identity `F2` and its interface
    /// hash.
    modules: BTreeMap<String, ModuleKeys>,
}

/// A package module's keys from its front record.
#[derive(Clone, Copy, Debug)]
struct ModuleKeys {
    f2: Hash128,
    interface: Hash128,
}

impl RecordSink {
    /// The sink of a command over `cache`, whose front end found or wrote
    /// a front record for each module of `entries`.
    pub(crate) fn new(cache: &Cache, entries: &[IndexEntry]) -> Self {
        Self {
            store: cache.store.share(),
            log: cache.log.share(),
            reads: cache.reads,
            modules: entries
                .iter()
                .map(|entry| {
                    (
                        entry.path.clone(),
                        ModuleKeys {
                            f2: entry.f2,
                            interface: entry.record.interface,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Looks up the record `key` of `artifact` for `subject`, logs the
    /// lookup, and writes `payload` on a miss. On a hit, a debug toolchain
    /// checks with `same` that the stored payload says what the fresh one
    /// does (D-1131's pattern).
    fn record(
        &self,
        artifact: &str,
        subject: &str,
        key: &Key,
        payload: &[u8],
        same: &dyn Fn(&[u8]) -> bool,
    ) -> Result<(), Ice> {
        self.record_known(artifact, subject, key, payload, same, false)
    }

    /// [`RecordSink::record`], without reading again when `known_miss`
    /// says an earlier lookup of `key` in this command missed (K4c's
    /// [`plan`]): the miss is logged and the payload written.
    fn record_known(
        &self,
        artifact: &str,
        subject: &str,
        key: &Key,
        payload: &[u8],
        same: &dyn Fn(&[u8]) -> bool,
        known_miss: bool,
    ) -> Result<(), Ice> {
        let stored = if self.reads && !known_miss {
            self.store.get_record(key)
        } else {
            None
        };
        let hit = stored.is_some();
        self.log.record(artifact, subject, hit);
        match stored {
            Some(bytes) => {
                if cfg!(debug_assertions) && !same(&bytes) {
                    return Err(Ice::new(format!(
                        "the {artifact} record of `{subject}` differs from its fresh {artifact} (D-969, D-1131)"
                    )));
                }
            }
            // A write that fails is skipped: the next command misses
            // (D-1129).
            None => {
                let _ = self.store.put_record(key, payload);
            }
        }
        Ok(())
    }
}

impl RecordSink {
    /// The code or tests record under `key` (K4c, D-1193,
    /// D-1221), unlogged: the caller logs one line per
    /// unit ([`RecordSink::log_lookup`]). `read` is false when the command
    /// bypasses code-record reads (`--reuse-report`,
    /// D-1223); `CHEBY_DUMP_DIR` bypasses
    /// every read (D-965). A record that hits is decoded with
    /// `CodeRecord::decode`'s checks and nothing more, since no fresh
    /// compile exists to compare (D-1131); one of another `RECORD_VERSION`,
    /// or that does not decode, is a miss, and the command writes it again.
    pub(crate) fn code_find(&self, key: &Key, read: bool) -> Option<CodeRecord> {
        if !(read && self.reads) {
            return None;
        }
        self.store
            .get_record(key)
            .and_then(|bytes| CodeRecord::decode(&bytes).ok())
    }

    /// Logs a lookup as `artifact subject hit|miss`
    /// (D-1224, D-973): K4c's `code.<kind>` and
    /// `tests.<kind>` lines, and the `facts` and `masks` hits of units the
    /// command loads from the cache without lowering them.
    pub(crate) fn log_lookup(&self, artifact: &str, subject: &str, hit: bool) {
        self.log.record(artifact, subject, hit);
    }

    /// Stores `record` under `key`, behind the command (D-1138), at most
    /// once per process (D-1139); a write that fails is skipped (D-1129).
    pub(crate) fn code_store(&self, key: &Key, record: &CodeRecord) {
        let _ = self.store.put_record(key, &record.encode());
    }

    /// Reads the lowering read-set hint `key` (D-955, D-1195): the module
    /// paths it names, `None` without a hint or with one that does not
    /// decode.
    pub(crate) fn hint(&self, key: &Key) -> Option<Vec<String>> {
        if !self.reads {
            return None;
        }
        self.store
            .get_hint(key)
            .and_then(|bytes| crate::cache::front::decode_hint(&bytes))
    }
}

/// The code keys of a lowering's package units (D-1193), by part and
/// module.
#[derive(Clone, Debug, Default)]
pub(crate) struct UnitKeys {
    /// Each package unit's key.
    pub keys: BTreeMap<(UnitPart, ModuleId), Key>,
}

/// The sink of `front`'s command, if it has a cache.
pub(crate) const fn sink(front: &Frontend) -> Option<&RecordSink> {
    front.records.as_ref()
}

/// The facts of a build's inference scope (D-968, D-969): the union of
/// every lowered unit's own facts, the entries unit's included
/// (D-1194, module docs).
pub(crate) fn scope_facts(units: &[MirModule]) -> BuildFacts {
    let refs: Vec<&MirModule> = units.iter().collect();
    borrow::collect_facts(&refs)
}

/// The masks one code unit gets from the build's facts
/// ([`borrow::infer`] with its module's eligibility, `backend.rs`).
pub(crate) type UnitMasks<'f> =
    dyn Fn(&MirModule, &BuildFacts) -> Result<borrow::Masks, Ice> + Sync + 'f;

/// Borrowing inference over a build's lowered units (D-715, D-779,
/// D-787): the facts of the scope ([`scope_facts`]), each code unit's
/// masks from them with `unit_masks` (in parallel, each module on its
/// own), what the records need ([`Inferred::new`]), and then the whole
/// build's masks written into every unit ([`borrow::apply`]).
///
/// # Errors
///
/// What `unit_masks` or [`borrow::apply`] returns, and [`Inferred::new`]'s
/// errors.
pub(crate) fn infer(
    front: &Frontend,
    units: &mut [MirModule],
    unit_masks: &UnitMasks<'_>,
) -> Result<Inferred, Ice> {
    let facts = scope_facts(units);
    infer_with(front, units, facts, &borrow::Masks::new(), unit_masks)
}

/// The facts of a build some of whose package units were not lowered
/// because their code comes from the cache (K4c, D-1192's "read to skip
/// work"): those of the lowered `units` ([`scope_facts`]) and the stored
/// facts `stored` of the others, by name (D-1191). The union is the facts
/// the whole build's lowering would give, since a facts record is keyed by
/// what its unit's lowering reads.
///
/// # Errors
///
/// An [`Ice`] if a stored name names no def the build can see.
pub(crate) fn build_facts(
    front: &Frontend,
    units: &[MirModule],
    stored: &[&UnitFacts],
) -> Result<BuildFacts, Ice> {
    let facts = scope_facts(units);
    if stored.is_empty() {
        return Ok(facts);
    }
    let all = all_modules(front);
    let view = DepView::new(&front.set, &all);
    facts.with_parts(stored, &build_defs(front, &view))
}

/// [`infer`] with the build's facts `facts` ([`build_facts`]) and the
/// stored masks `stored` of the package modules whose code units were not
/// lowered (K4c): each lowered code unit's masks are inferred from the
/// facts, and every lowered unit gets those and the stored ones.
///
/// # Errors
///
/// As [`infer`].
pub(crate) fn infer_with(
    front: &Frontend,
    units: &mut [MirModule],
    facts: BuildFacts,
    stored: &borrow::Masks,
    unit_masks: &UnitMasks<'_>,
) -> Result<Inferred, Ice> {
    let mut masks = masks_of(units, &facts, unit_masks)?;
    #[cfg(test)]
    if stored.is_empty() {
        hook::check(front, units, &facts, &masks, unit_masks)?;
    }
    let inferred = Inferred::new(front, units, facts)?;
    masks.extend(stored.iter().map(|(&def, &mask)| (def, mask)));
    let applied: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| borrow::apply(unit, &masks))
        .collect();
    applied.into_iter().collect::<Result<(), Ice>>()?;
    Ok(Inferred { masks, ..inferred })
}

/// Every code unit's masks under `facts`, merged.
pub(crate) fn masks_of(
    units: &[MirModule],
    facts: &BuildFacts,
    unit_masks: &UnitMasks<'_>,
) -> Result<borrow::Masks, Ice> {
    let found: Vec<Result<borrow::Masks, Ice>> = units
        .par_iter()
        .map(|unit| unit_masks(unit, facts))
        .collect();
    let mut masks = borrow::Masks::new();
    for result in found {
        masks.extend(result?);
    }
    Ok(masks)
}

/// What inference leaves for the records: the build's facts and, when
/// the command has a cache, each package unit's own facts, taken before
/// the masks are written into the units.
pub(crate) struct Inferred {
    facts: BuildFacts,
    parts: BTreeMap<(UnitPart, ModuleId), UnitFacts>,
    /// The masks written into the units: inferred for the lowered code
    /// units, stored for the others (K4c).
    pub masks: borrow::Masks,
}

/// One package unit's own facts, by part and module.
type Part = ((UnitPart, ModuleId), UnitFacts);

impl Inferred {
    /// The record inputs of `units`, at `Stage::Lowered`, whose facts are
    /// `facts`: each package unit's own facts when `front`'s command has a
    /// cache (computed in parallel, kept by unit).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a unit names a def the build cannot see.
    pub(crate) fn new(
        front: &Frontend,
        units: &[MirModule],
        facts: BuildFacts,
    ) -> Result<Self, Ice> {
        let mut parts = BTreeMap::new();
        if let Some(sink) = sink(front) {
            let all = all_modules(front);
            let view = DepView::new(&front.set, &all);
            let defs = build_defs(front, &view);
            let found: Vec<Option<Result<Part, Ice>>> = units
                .par_iter()
                .map(|unit| {
                    let (part, module) = recorded_unit(front, sink, unit)?;
                    Some(borrow::unit_facts(unit, &defs).map(|f| ((part, module), f)))
                })
                .collect();
            for result in found.into_iter().flatten() {
                let (at, f) = result?;
                parts.insert(at, f);
            }
        }
        Ok(Self {
            facts,
            parts,
            masks: borrow::Masks::new(),
        })
    }

    /// The record inputs of a lowering without borrowing (`plain`, D-971):
    /// each package unit's own facts, which name its callees for its code
    /// key (K4c), and no masks. So a `plain` unit gets a facts record too,
    /// where D-1192 gave it none, since a `plain` command computes its keys
    /// before lowering from the stored facts (D-1221).
    ///
    /// # Errors
    ///
    /// As [`Inferred::new`].
    pub(crate) fn plain(front: &Frontend, units: &[MirModule]) -> Result<Self, Ice> {
        Self::new(front, units, BuildFacts::default())
    }

    /// Adds the record inputs of `more`, units lowered after the others
    /// because their stored code missed (K4c): their own facts, and their
    /// masks `masks`.
    pub(crate) fn extend(&mut self, more: Self, masks: &borrow::Masks) {
        self.parts.extend(more.parts);
        self.masks
            .extend(masks.iter().map(|(&def, &mask)| (def, mask)));
    }

    /// The build's facts.
    pub(crate) const fn facts(&self) -> &BuildFacts {
        &self.facts
    }

    /// The own facts of the lowered package unit `part` of `module`.
    pub(crate) fn part(&self, part: UnitPart, module: ModuleId) -> Option<&UnitFacts> {
        self.parts.get(&(part, module))
    }
}

/// The ids of every module of the build, sorted: a view of all of them
/// names any def a unit can hold.
fn all_modules(front: &Frontend) -> Vec<ModuleId> {
    front.modules.iter().map(|(id, _)| id).collect()
}

/// The interfaces of the whole build, through `view`, without recording:
/// naming a fact reads nothing lowering did not (D-955).
const fn build_defs<'a>(front: &'a Frontend, view: &'a DepView<'a>) -> Defs<'a> {
    Defs {
        own: None,
        deps: view,
        modules: &front.modules,
        globals: &front.globals,
    }
}

/// Which part of which package module `unit` is, if it gets records: a
/// code or tests unit of a package module with a front record.
fn recorded_unit(
    front: &Frontend,
    sink: &RecordSink,
    unit: &MirModule,
) -> Option<(UnitPart, ModuleId)> {
    let (part, module) = match unit.unit {
        UnitKind::Code(m) => (UnitPart::Code, m),
        UnitKind::Tests(m) => (UnitPart::Tests, m),
        UnitKind::Entries => return None,
    };
    let info = front.modules.get(module)?;
    (info.package == PackageId::LOCAL && sink.modules.contains_key(&info.path.to_string()))
        .then_some((part, module))
}

/// The build kind of a lowering of `roots` with `mir` (D-958): `plain`
/// without borrowing (D-971, D-978), `test` for tests, `run` otherwise
/// (`cheby run` and `cheby build`).
pub(crate) fn build_kind(roots: &Roots<'_>, mir: MirOptions) -> BuildKind {
    if !mir.borrow {
        BuildKind::Plain
    } else if matches!(roots, Roots::Tests(_)) {
        BuildKind::Test
    } else {
        BuildKind::Run
    }
}

/// The settings that change code (D-958) of a lowering with `mir`, as a
/// command compiles it: its kind, `CHEBY_RC`, unwind edges, the checked
/// count operations of the poisoning mode, the stack check and the
/// target.
fn settings(kind: BuildKind, mir: MirOptions) -> Settings {
    Settings {
        kind,
        rc_naive: mir.rc == RcMode::Naive,
        unwind_edges: mir.unwind_edges,
        checked_counts: cheby_runtime::alloc::poison_enabled(),
        stack_check: CodegenOptions::default().stack_check,
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    }
}

/// What a command knows of a package unit before it lowers it (K4c,
/// D-1192's "read to skip work", D-955's two steps): its code identity
/// from its lowering hint and, when the command reads code records, its
/// stored facts.
#[derive(Clone, Debug)]
pub(crate) struct Planned {
    /// The module's path.
    pub path: String,
    /// The module paths the unit's lowering hint names (D-955, D-1195),
    /// empty without a hint.
    pub hint: Vec<String>,
    /// `C2` or `T2` over the hint's reads; `None` when a read it names has
    /// no front record.
    pub identity: Option<Key>,
    /// The unit's facts record under `identity`, when the command read it
    /// and it hit.
    pub facts: Option<UnitFacts>,
}

/// Each package unit's [`Planned`], by part and module.
pub(crate) type Plan = BTreeMap<(UnitPart, ModuleId), Planned>;

/// What `front`'s command knows of each package unit of `parts` before it
/// lowers any (K4c): its identity from its hint and, with `read`, its
/// stored facts. A unit whose facts hit need not be lowered for the build's
/// facts (D-1191); whether it is lowered at all is decided by its masks
/// and code records. Nothing without a cache, nor for std or a module
/// without a front record (D-964).
pub(crate) fn plan(front: &Frontend, parts: &[(UnitPart, ModuleId)], read: bool) -> Plan {
    let Some(sink) = sink(front) else {
        return Plan::new();
    };
    let build = sink.store.build_id();
    let planned: Vec<Option<((UnitPart, ModuleId), Planned)>> = parts
        .par_iter()
        .map(|&(part, module)| {
            let info = front.modules.get(module)?;
            if info.package != PackageId::LOCAL {
                return None;
            }
            let path = info.path.to_string();
            let keys = sink.modules.get(&path).copied()?;
            let hint = sink
                .hint(&code_keys::lreads_key(keys.f2, part, build))
                .unwrap_or_default();
            let named: Option<Vec<(String, Hash128)>> = hint
                .iter()
                .map(|p| sink.modules.get(p).map(|k| (p.clone(), k.interface)))
                .collect();
            let identity = named.map(|mut named| {
                named.sort();
                code_keys::code_identity(keys.f2, part, build, &named)
            });
            let facts = identity
                .filter(|_| read && sink.reads)
                .and_then(|id| sink.store.get_record(&code_keys::facts_key(id, build)))
                .and_then(|bytes| unsealed(&bytes).and_then(|b| UnitFacts::decode(b).ok()));
            Some((
                (part, module),
                Planned {
                    path,
                    hint,
                    identity,
                    facts,
                },
            ))
        })
        .collect();
    planned.into_iter().flatten().collect()
}

/// A code unit's stored masks ([`stored_masks`]): the record, and each
/// mask by def.
#[derive(Clone, Debug)]
pub(crate) struct StoredMasks {
    /// The masks by item, as stored.
    pub record: MasksRecord,
    /// The same masks by def.
    pub by_def: borrow::Masks,
}

/// The masks record of the code unit of `module`, which the command did not
/// lower (K4c, D-1192's "read to skip work"): under its planned identity,
/// the build kind and the digest of `facts` about the module (D-1191), each
/// item found by def through `defs` ([`with_build_defs`]). `None` on a miss,
/// without an identity, or when an item no longer names one def: the unit
/// is then lowered.
///
/// # Errors
///
/// An [`Ice`] if the facts name a def `defs` cannot see.
pub(crate) fn stored_masks(
    front: &Frontend,
    planned: &Planned,
    module: ModuleId,
    kind: BuildKind,
    facts: &BuildFacts,
    defs: &Defs<'_>,
) -> Result<Option<StoredMasks>, Ice> {
    let (Some(sink), Some(identity)) = (sink(front), planned.identity) else {
        return Ok(None);
    };
    if !sink.reads {
        return Ok(None);
    }
    let digest = Hash128::of(&facts.about_module(module, defs)?);
    let key = code_keys::masks_key(identity, kind, digest, sink.store.build_id());
    let Some(record) = sink
        .store
        .get_record(&key)
        .and_then(|bytes| MasksRecord::decode(&bytes))
    else {
        return Ok(None);
    };
    let names: Vec<(&str, &str)> = record
        .masks
        .iter()
        .map(|(item, _)| (planned.path.as_str(), item.as_str()))
        .collect();
    let found = borrow::find_defs(&names, defs)?;
    let mut by_def = borrow::Masks::new();
    for ((_, mask), def) in record.masks.iter().zip(found) {
        let Some(def) = def else {
            return Ok(None);
        };
        by_def.insert(def, *mask);
    }
    Ok(Some(StoredMasks { record, by_def }))
}

/// Calls `f` with the interfaces of the whole build, which name every def
/// a unit, a facts record or a masks record can hold.
pub(crate) fn with_build_defs<R>(front: &Frontend, f: impl FnOnce(&Defs<'_>) -> R) -> R {
    let all = all_modules(front);
    let view = DepView::new(&front.set, &all);
    f(&build_defs(front, &view))
}

/// What [`write_fresh`] gives each package unit the command lowered.
#[derive(Clone, Debug)]
pub(crate) struct FreshUnit {
    /// Its code identity from its hidden reads (D-955).
    pub identity: Key,
    /// A code unit's own masks by item (empty for a tests unit).
    pub own: Vec<(String, BorrowMask)>,
}

/// The borrowing records of the package units a command lowered (K4b's
/// records, D-1192, D-955; module docs): for each of `units` (counted, with
/// the build's masks) whose module has a front record, its lowering hint,
/// written when its hidden reads are not empty and rewritten empty when
/// its planned hint named reads that are gone (D-1195), its code identity
/// from `reads`, its facts record and, for a code unit of a build that
/// `borrows`, its masks record, each looked up (a debug toolchain compares
/// a hit with the fresh one), and written on a miss. A lookup that
/// [`plan`] already made with the same key and that missed is not made
/// again. Returns each unit's identity and own masks. Nothing without a
/// cache.
///
/// # Errors
///
/// An [`Ice`] if a unit names a def it has no name for, or, in a debug
/// toolchain, if a record that hits differs from the fresh one.
#[expect(
    clippy::too_many_arguments,
    reason = "one lowering's units, reads, inference and plan, as `lower_scoped` holds them"
)]
pub(crate) fn write_fresh(
    front: &Frontend,
    units: &[&MirModule],
    reads: &BTreeMap<ModuleId, UnitReads>,
    inferred: &Inferred,
    kind: BuildKind,
    borrows: bool,
    plan: &Plan,
    read: bool,
) -> Result<BTreeMap<(UnitPart, ModuleId), FreshUnit>, Ice> {
    let Some(sink) = sink(front) else {
        return Ok(BTreeMap::new());
    };
    with_build_defs(front, |defs| {
        let writing = Writing {
            front,
            sink,
            reads,
            inferred,
            kind,
            borrows,
            plan,
            read,
            defs,
        };
        // Each unit's records are independent (the store takes puts from
        // any thread, the log is sorted when the command ends), so they are
        // written in parallel; the first error in unit order is reported.
        let written: Vec<Result<Option<FreshEntry>, Ice>> =
            units.par_iter().map(|unit| writing.unit(unit)).collect();
        let mut out = BTreeMap::new();
        for result in written {
            if let Some((at, fresh)) = result? {
                out.insert(at, fresh);
            }
        }
        Ok(out)
    })
}

/// One lowered package unit's [`FreshUnit`], by part and module.
type FreshEntry = ((UnitPart, ModuleId), FreshUnit);

/// What [`write_fresh`] writes with, for each unit.
struct Writing<'a> {
    front: &'a Frontend,
    sink: &'a RecordSink,
    reads: &'a BTreeMap<ModuleId, UnitReads>,
    inferred: &'a Inferred,
    kind: BuildKind,
    borrows: bool,
    plan: &'a Plan,
    /// Whether [`plan`] looked up facts records.
    read: bool,
    defs: &'a Defs<'a>,
}

impl Writing<'_> {
    /// The records, identity and own masks of `unit`, if it gets them.
    fn unit(&self, unit: &MirModule) -> Result<Option<FreshEntry>, Ice> {
        let (front, sink) = (self.front, self.sink);
        let build = sink.store.build_id();
        let Some((part, module)) = recorded_unit(front, sink, unit) else {
            return Ok(None);
        };
        let path = front.modules.try_get(module)?.path.to_string();
        let Some(keys) = sink.modules.get(&path).copied() else {
            return Ok(None);
        };
        let hidden = self.reads.get(&module).and_then(|r| match part {
            UnitPart::Code => r.code.as_ref(),
            UnitPart::Tests => r.tests.as_ref(),
        });
        let Some(hidden) = hidden else {
            return Ok(None);
        };
        let Some(named) = named_reads(front, sink, hidden)? else {
            return Ok(None);
        };
        let planned = self.plan.get(&(part, module));
        let paths: Vec<String> = named.iter().map(|(p, _)| p.clone()).collect();
        // D-1195: a hint is written when the reads are not empty, and a
        // stale non-empty one is rewritten (empty) when they became empty.
        // A failed write is skipped (D-1129).
        if !paths.is_empty() || planned.is_some_and(|p| !p.hint.is_empty()) {
            let _ = sink.store.put_hint(
                &code_keys::lreads_key(keys.f2, part, build),
                &encode_hint(&paths),
            );
        }
        let identity = code_keys::code_identity(keys.f2, part, build, &named);
        // The facts lookup `plan` made under this identity, if it missed.
        let known_miss = self.read
            && sink.reads
            && planned.is_some_and(|p| p.identity == Some(identity) && p.facts.is_none());
        let subject = match part {
            UnitPart::Code => path.clone(),
            UnitPart::Tests => format!("{path}#tests"),
        };
        let own = match part {
            UnitPart::Code => own_masks(unit)?,
            UnitPart::Tests => Vec::new(),
        };
        if let Some(facts) = self.inferred.parts.get(&(part, module)) {
            sink.record_known(
                "facts",
                &subject,
                &code_keys::facts_key(identity, build),
                &sealed(facts.encode()),
                &|bytes| {
                    unsealed(bytes)
                        .and_then(|b| UnitFacts::decode(b).ok())
                        .is_some_and(|stored| &stored == facts)
                },
                known_miss,
            )?;
            if part == UnitPart::Code && self.borrows {
                let digest = Hash128::of(&self.inferred.facts.about_module(module, self.defs)?);
                let record = MasksRecord { masks: own.clone() };
                sink.record(
                    "masks",
                    &subject,
                    &code_keys::masks_key(identity, self.kind, digest, build),
                    &record.encode(),
                    &|bytes| MasksRecord::decode(bytes).is_some_and(|stored| stored == record),
                )?;
            }
        }
        Ok(Some(((part, module), FreshUnit { identity, own })))
    }
}

/// One package unit's inputs to its code key ([`unit_keys`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyUnit<'a> {
    /// The unit's part and module.
    pub part: UnitPart,
    /// Its module.
    pub module: ModuleId,
    /// Its code identity: fresh for a lowered unit, planned otherwise.
    pub identity: Key,
    /// For a code unit, its module's own masks by item; for a tests unit,
    /// those of its module's code unit (D-1222).
    pub own: &'a [(String, BorrowMask)],
    /// Its facts: fresh for a lowered unit, stored otherwise.
    pub facts: &'a UnitFacts,
}

/// The code key of each of `units` (D-1193), computed the same way whether
/// the unit was lowered or not, so a key computed before lowering finds the
/// record a cold command stored (K4c): the build ID, the settings of the
/// build kind `kind` and `mir` (D-958), the positions switch (D-1173), the
/// identity, the own masks, and the slot symbol, with the mask `masks`
/// gives (D-1109), of each function of the package in another module that
/// the unit's facts name as a callee (D-1191's facts list exactly a unit's
/// callees in other modules). A tests unit's own module enters through its
/// code unit's own masks rather than through its callees' slots, since its
/// facts do not list callees of its own module (D-1222).
/// [`log_keys`] logs them once they are final.
///
/// # Errors
///
/// An [`Ice`] if a name cannot be read.
pub(crate) fn unit_keys(
    front: &Frontend,
    kind: BuildKind,
    mir: MirOptions,
    units: &[KeyUnit<'_>],
    masks: &borrow::Masks,
) -> Result<UnitKeys, Ice> {
    let Some(sink) = sink(front) else {
        return Ok(UnitKeys::default());
    };
    let build = sink.store.build_id();
    let settings = settings(kind, mir);
    let names: Vec<Vec<(&str, &str)>> = units.iter().map(|u| u.facts.callee_names()).collect();
    let flat: Vec<(&str, &str)> = names.iter().flatten().copied().collect();
    let found = with_build_defs(front, |defs| borrow::find_defs(&flat, defs))?;
    let mut found = found.into_iter();
    let mut keys = BTreeMap::new();
    for (unit, names) in units.iter().zip(&names) {
        let mut callees = BTreeSet::new();
        for &(module, item) in names {
            let Some(def) = found.next().flatten() else {
                continue;
            };
            let local = front
                .modules
                .get(def.module)
                .is_some_and(|info| info.package == PackageId::LOCAL);
            if local && def.module != unit.module {
                let name = cheby_mir::DefName {
                    def,
                    module: module.into(),
                    item: item.into(),
                };
                let mask = masks.get(&def).copied().unwrap_or_default();
                callees.insert(mangle::slot(&name, mask));
            }
        }
        let callees: Vec<String> = callees.into_iter().collect();
        let key = code_keys::code_key(&CodeKeyInputs {
            build,
            part: unit.part,
            settings: &settings,
            positions: mir.positions,
            identity: unit.identity,
            own: unit.own,
            callees: &callees,
        });
        keys.insert((unit.part, unit.module), key);
    }
    Ok(UnitKeys { keys })
}

/// Logs each of `keys` as a `key code.<kind>|tests.<kind> <module> <hex>`
/// line (D-973, D-1193).
///
/// # Errors
///
/// An [`Ice`] if a key's module is unknown.
pub(crate) fn log_keys(front: &Frontend, kind: BuildKind, keys: &UnitKeys) -> Result<(), Ice> {
    let Some(sink) = sink(front) else {
        return Ok(());
    };
    for (&(part, module), key) in &keys.keys {
        let path = front.modules.try_get(module)?.path.to_string();
        sink.log
            .key(&format!("{}.{}", part.tag(), kind.tag()), &path, key);
    }
    Ok(())
}

/// The hidden reads `hidden` with their module paths and interface
/// hashes, sorted by path; `None` when one has no front record.
fn named_reads(
    front: &Frontend,
    sink: &RecordSink,
    hidden: &[ModuleId],
) -> Result<Option<Vec<(String, Hash128)>>, Ice> {
    let mut named = Vec::with_capacity(hidden.len());
    for &read in hidden {
        let path = front.modules.try_get(read)?.path.to_string();
        let Some(keys) = sink.modules.get(&path) else {
            return Ok(None);
        };
        named.push((path, keys.interface));
    }
    named.sort();
    Ok(Some(named))
}

/// The masks of `unit`'s own functions that borrow something, by item,
/// sorted.
fn own_masks(unit: &MirModule) -> Result<Vec<(String, BorrowMask)>, Ice> {
    let mut out = Vec::new();
    for f in &unit.functions {
        if let FnKey::Item(def) = f.key
            && !f.borrow.is_owned()
        {
            out.push((unit.name(def)?.item.to_string(), f.borrow));
        }
    }
    out.sort();
    Ok(out)
}

/// A test hook (D-686: keyed by thread in a static, never a
/// `thread_local!`): while a test thread watches, each inference on it
/// also rebuilds the facts from every unit's own facts by name and infers
/// the masks again from them, and records how they compare.
#[cfg(test)]
pub(crate) mod hook {
    use std::sync::{Mutex, PoisonError};
    use std::thread::ThreadId;

    use cheby_iface::{DepView, Ice};
    use cheby_mir::borrow::{self, BuildFacts, UnitFacts};
    use cheby_mir::{MirModule, UnitKind};

    use super::{UnitMasks, all_modules, build_defs, masks_of};
    use crate::frontend::Frontend;

    /// One inference, as the hook saw it.
    #[derive(Clone, Debug)]
    pub(crate) struct Check {
        /// Whether [`BuildFacts::union`] of every unit's own facts equals
        /// the facts the build used.
        pub union_equal: bool,
        /// Whether the masks inferred from that union equal the build's.
        pub masks_equal: bool,
        /// How many functions borrow something.
        pub borrowing: usize,
        /// The functions whose masks would differ if the entries unit
        /// contributed no facts (D-1194), by path.
        pub without_entries: Vec<String>,
    }

    /// The watching threads, each with what its inferences gave.
    static WATCHED: Mutex<Vec<(ThreadId, Vec<Check>)>> = Mutex::new(Vec::new());

    /// Starts recording the inferences of this thread.
    pub(crate) fn watch() {
        let me = std::thread::current().id();
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        watched.retain(|(id, _)| *id != me);
        watched.push((me, Vec::new()));
    }

    /// Stops recording, and returns what this thread's inferences gave.
    pub(crate) fn take() -> Vec<Check> {
        let me = std::thread::current().id();
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        let found = watched
            .iter()
            .position(|(id, _)| *id == me)
            .map(|at| watched.swap_remove(at).1);
        found.unwrap_or_default()
    }

    /// Appends `check` to this thread's list, if it watches.
    fn push(me: ThreadId, check: Check) {
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, list)) = watched.iter_mut().find(|(id, _)| *id == me) {
            list.push(check);
        }
    }

    /// Whether this thread watches.
    fn watching(me: ThreadId) -> bool {
        WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(id, _)| *id == me)
    }

    /// The hook's work for one inference, if this thread watches.
    pub(super) fn check(
        front: &Frontend,
        units: &[MirModule],
        facts: &BuildFacts,
        masks: &borrow::Masks,
        unit_masks: &UnitMasks<'_>,
    ) -> Result<(), Ice> {
        let me = std::thread::current().id();
        if !watching(me) {
            return Ok(());
        }
        let all = all_modules(front);
        let view = DepView::new(&front.set, &all);
        let defs = build_defs(front, &view);
        let parts = units
            .iter()
            .map(|unit| borrow::unit_facts(unit, &defs))
            .collect::<Result<Vec<UnitFacts>, Ice>>()?;
        let refs: Vec<&UnitFacts> = parts.iter().collect();
        let union = BuildFacts::union(&refs, &defs)?;
        let from_union = masks_of(units, &union, unit_masks)?;
        let scoped: Vec<&MirModule> = units
            .iter()
            .filter(|u| u.unit != UnitKind::Entries)
            .collect();
        let without = masks_of(units, &borrow::collect_facts(&scoped), unit_masks)?;
        let mut without_entries = Vec::new();
        for def in masks.keys().chain(without.keys()) {
            if masks.get(def) != without.get(def) {
                let name = units
                    .iter()
                    .find_map(|u| u.name(*def).ok())
                    .map_or_else(|| format!("{def:?}"), cheby_mir::DefName::path);
                without_entries.push(name);
            }
        }
        without_entries.sort();
        without_entries.dedup();
        let check = Check {
            union_equal: &union == facts,
            masks_equal: &from_union == masks,
            borrowing: masks.len(),
            without_entries,
        };
        push(me, check);
        Ok(())
    }
}
