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
//! D-1192: until K4c caches code, nothing
//! reads a record to skip work. Every command still lowers and infers
//! fresh, and writes the records behind the command (D-1138), at most once
//! per process (D-1139). A record that hits is compared with the fresh
//! result in a debug toolchain, and a difference is an internal error
//! naming the module (D-1131's pattern). Std modules get no records (the
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
        let stored = if self.reads {
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
    let masks = masks_of(units, &facts, unit_masks)?;
    #[cfg(test)]
    hook::check(front, units, &facts, &masks, unit_masks)?;
    let inferred = Inferred::new(front, units, facts)?;
    let applied: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| borrow::apply(unit, &masks))
        .collect();
    applied.into_iter().collect::<Result<(), Ice>>()?;
    Ok(inferred)
}

/// Every code unit's masks under `facts`, merged.
fn masks_of(
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
        Ok(Self { facts, parts })
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
fn build_kind(roots: &Roots<'_>, mir: MirOptions) -> BuildKind {
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

/// Writes the borrowing records of a lowering and logs its code keys
/// (module docs): for each package unit of `units` (counted, with the
/// build's masks) whose module has a front record, its lowering hint and
/// code identity from `reads`, its facts record and, for a code unit, its
/// masks record when the build inferred masks (`inferred`), and its code
/// key. Nothing without a cache.
///
/// # Errors
///
/// An [`Ice`] if a unit names a def it has no name for, or, in a debug
/// toolchain, if a record that hits differs from the fresh one.
pub(crate) fn write_records(
    front: &Frontend,
    units: &[MirModule],
    reads: &BTreeMap<ModuleId, UnitReads>,
    inferred: Option<&Inferred>,
    roots: &Roots<'_>,
    mir: MirOptions,
) -> Result<(), Ice> {
    let Some(sink) = sink(front) else {
        return Ok(());
    };
    let kind = build_kind(roots, mir);
    let all = all_modules(front);
    let view = DepView::new(&front.set, &all);
    let writing = Writing {
        front,
        sink,
        reads,
        inferred,
        kind,
        settings: settings(kind, mir),
        positions: mir.positions,
        defs: build_defs(front, &view),
    };
    // Each unit's records are independent (the store takes puts from any
    // thread, the log is sorted when the command ends), so they are
    // written in parallel; the first error in unit order is reported.
    let written: Vec<Result<(), Ice>> = units.par_iter().map(|unit| writing.unit(unit)).collect();
    written.into_iter().collect()
}

/// What [`write_records`] writes with, for each unit.
struct Writing<'a> {
    front: &'a Frontend,
    sink: &'a RecordSink,
    reads: &'a BTreeMap<ModuleId, UnitReads>,
    inferred: Option<&'a Inferred>,
    kind: BuildKind,
    settings: Settings,
    positions: bool,
    defs: Defs<'a>,
}

impl Writing<'_> {
    /// The records and the code key of `unit`, if it gets them.
    fn unit(&self, unit: &MirModule) -> Result<(), Ice> {
        let (front, sink) = (self.front, self.sink);
        let build = sink.store.build_id();
        let Some((part, module)) = recorded_unit(front, sink, unit) else {
            return Ok(());
        };
        let path = front.modules.try_get(module)?.path.to_string();
        let Some(keys) = sink.modules.get(&path).copied() else {
            return Ok(());
        };
        let hidden = self.reads.get(&module).and_then(|r| match part {
            UnitPart::Code => r.code.as_ref(),
            UnitPart::Tests => r.tests.as_ref(),
        });
        let Some(hidden) = hidden else {
            return Ok(());
        };
        let Some(named) = named_reads(front, sink, hidden)? else {
            return Ok(());
        };
        if !named.is_empty() {
            let paths: Vec<String> = named.iter().map(|(p, _)| p.clone()).collect();
            // A failed write is skipped (D-1129).
            let _ = sink.store.put_hint(
                &code_keys::lreads_key(keys.f2, part, build),
                &encode_hint(&paths),
            );
        }
        let identity = code_keys::code_identity(keys.f2, part, build, &named);
        let subject = match part {
            UnitPart::Code => path.clone(),
            UnitPart::Tests => format!("{path}#tests"),
        };
        let own = match part {
            UnitPart::Code => own_masks(unit)?,
            UnitPart::Tests => Vec::new(),
        };
        if let Some(inferred) = self.inferred
            && let Some(facts) = inferred.parts.get(&(part, module))
        {
            sink.record(
                "facts",
                &subject,
                &code_keys::facts_key(identity, build),
                &sealed(facts.encode()),
                &|bytes| {
                    unsealed(bytes)
                        .and_then(|b| UnitFacts::decode(b).ok())
                        .is_some_and(|stored| &stored == facts)
                },
            )?;
            if part == UnitPart::Code {
                let digest = Hash128::of(&inferred.facts.about_module(module, &self.defs)?);
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
        let callees = callee_slots(front, unit, part, module)?;
        let key = code_keys::code_key(&CodeKeyInputs {
            build,
            part,
            settings: &self.settings,
            positions: self.positions,
            identity,
            own: &own,
            callees: &callees,
        });
        sink.log
            .key(&format!("{}.{}", part.tag(), self.kind.tag()), &path, &key);
        Ok(())
    }
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

/// The slot symbols (D-1109: with the mask) of the callees whose masks
/// `unit`'s code depends on, sorted: a code unit's callees of the package
/// in other modules (D-787, D-812), a tests unit's callees of the package
/// in any module.
fn callee_slots(
    front: &Frontend,
    unit: &MirModule,
    part: UnitPart,
    module: ModuleId,
) -> Result<Vec<String>, Ice> {
    let mut out = BTreeSet::new();
    for decl in &unit.callees {
        let local = front
            .modules
            .get(decl.def.module)
            .is_some_and(|info| info.package == PackageId::LOCAL);
        let counts = match part {
            UnitPart::Code => decl.def.module != module,
            UnitPart::Tests => true,
        };
        if local && counts {
            out.insert(mangle::slot(unit.name(decl.def)?, decl.sig.borrow));
        }
    }
    Ok(out.into_iter().collect())
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
