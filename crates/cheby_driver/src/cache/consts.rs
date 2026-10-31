//! Constant results in the store (D-977, D-964, D-1187 to D-1190).
//!
//! Each evaluated constant is a cache entry of its own, a record under
//! [`KeyKind::Const`] (D-977). Its key holds the build ID, the settings of
//! the evaluation (the `plain` kind with D-958's other settings, D-978),
//! the package's `[const-eval]` limits, and the inputs D-538 and D-593 hash
//! into the constant's seed: the package name, the module path, the
//! constant's name and its constant module set with each module's source
//! hash ([`const_key`]). The set's std modules are left out: the build ID
//! fixes std's sources (D-957), and which std modules a constant reaches
//! follows from the package modules it reaches, so the key still fixes
//! every byte of the seed (D-1259). The hashes are the cache's SipHash-1-3
//! 128 of the same bytes (D-963), which the package index holds, so a key
//! is computed the same way before the check, over the vouched front
//! records' reference summaries, as after it, over the command's reach;
//! a constant whose walk meets a module the index does not vouch for has
//! no key before the check and misses (D-1259). A warm command looks
//! before its check only at the constants its roots reach
//! ([`misses_before_check_from`], D-1250, D-1259), `cheby build` among them
//! (N9). No command computes every constant's key before its check: K5b's
//! unused `keys_before_check` is removed, and [`keys_from_records`], which
//! computes them over the vouched entries, stays for the test that holds
//! them equal to the keys after the check. GAP(consts-keys-before-check).
//!
//! The entry holds the constant's image (D-1188) or its failure's
//! diagnostics, stored structured and replayed as a module's are (D-965);
//! a time-limit failure is never stored (D-964), nor a constant that
//! failed only because one it read failed. REPL images name addresses and
//! are never stored, and a REPL session looks nothing up (D-1190): only
//! `backend.rs`'s evaluation modules and `cheby check`'s build come here.
//!
//! An image may reference objects of earlier images of the same
//! evaluation (D-1188's tag 3), so a cached image is valid only beside the
//! very images it was written with. Its entry therefore lists each image
//! it references with the hash of that image's bytes
//! (D-1261), and a hit is used only when every image
//! it references hits with those bytes. Otherwise it is evaluated again
//! with the constants it reads and the hits whose images reference it, so
//! the images installed together always come from one evaluation or from
//! entries that agree byte for byte ([`ConstPlan`]).
//!
//! Entry bytes (D-1260), sealed with their SipHash-1-3 128
//! as front records are: `CBCE`, a version byte, then a tag: 0 for an
//! image (the list of referenced images, each as its path and the 16-byte
//! hash of its bytes, then the image's bytes), 1 for a failure (the blob
//! of one list of diagnostics, `replay`'s encoding).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use cheby_consteval::ConstInfo;
use cheby_diag::{Diagnostic, LabelStyle, SourceMap};
use cheby_iface::{DefId, Ice, ModuleId, ModuleTable, PackageId};
use cheby_mir::reach::Reach;
use cheby_mir::{MirOptions, RcMode};
use cheby_package::{Target, Targets};
use cheby_runtime::consteval::image::ConstImage;

use super::front::{FrontRecord, MentionerKind, sealed, unsealed};
use super::index::IndexEntry;
use super::replay::{self, SpanCx};
use super::wire::{Reader, Writer};
use super::{BuildId, BuildKind, CacheLog, Hash128, Key, KeyKind, Settings, StoreShare};
use crate::frontend::Frontend;

/// The first bytes of an entry's payload.
const MAGIC: &[u8; 4] = b"CBCE";
/// The version of the entry's encoding.
const VERSION: u8 = 1;
/// The artifact name of the lookup lines (D-973).
const ARTIFACT: &str = "const";

/// The `[const-eval]` limits a constant's key holds (D-977), and the
/// package's checked targets: E0078's range check runs only when they
/// hold `js` (D-540), so they are part of the verdict
/// (D-1260).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConstLimits {
    /// The time limit of one top-level evaluation (D-536).
    pub time_limit: Duration,
    /// The memory limit of one top-level evaluation in bytes (D-536).
    pub memory_limit: u64,
    /// The package's checked targets: the manifest's, or the one
    /// `--target` selects (D-550).
    pub targets: Targets,
}

impl ConstLimits {
    /// The limits and targets of `front`'s package.
    pub(crate) const fn of(front: &Frontend) -> Self {
        Self {
            time_limit: front.const_eval_time_limit,
            memory_limit: front.const_eval_memory_limit,
            targets: front.package_targets,
        }
    }
}

/// The settings of constant evaluation for a command whose MIR options
/// are `mir` (D-978, D-958): the `plain` kind, and the command's
/// `CHEBY_RC`, unwind edges, poisoning, stack check and target.
pub(crate) fn settings(mir: MirOptions) -> Settings {
    Settings {
        kind: BuildKind::Plain,
        rc_naive: mir.rc == RcMode::Naive,
        unwind_edges: mir.unwind_edges,
        checked_counts: cheby_runtime::alloc::poison_enabled(),
        stack_check: cheby_codegen_clif::CodegenOptions::default().stack_check,
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    }
}

/// D-977: a constant's key under [`KeyKind::Const`]: the build ID,
/// `settings` (the `plain` kind and D-958's other settings, D-978),
/// `limits`, and the inputs D-538 and D-593 hash into the seed: the
/// package name, the module path, the constant's name and `deps` (each
/// module of its constant module set outside std with its source hash,
/// sorted by path; module docs), each field length-prefixed.
pub(crate) fn const_key(
    build: BuildId,
    settings: &Settings,
    limits: &ConstLimits,
    package: &str,
    module_path: &str,
    name: &str,
    deps: &[(&str, Hash128)],
) -> Key {
    let nanos = u64::try_from(limits.time_limit.as_nanos()).unwrap_or(u64::MAX);
    let mut key = Key::builder(KeyKind::Const, build)
        .settings(settings)
        .u64(nanos)
        .u64(limits.memory_limit)
        .bytes(&[
            u8::from(limits.targets.contains(Target::Native)),
            u8::from(limits.targets.contains(Target::Js)),
        ])
        .str(package)
        .str(module_path)
        .str(name)
        .u64(u64::try_from(deps.len()).unwrap_or(u64::MAX));
    for (path, hash) in deps {
        key = key.str(path).hash(*hash);
    }
    key.finish()
}

/// A cached result: the image (D-1188) with the images it references
/// (each path with the hash of the bytes it was written beside), or the
/// failure's diagnostics as `replay`'s blob of one list (D-964: never a
/// time-limit one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConstEntry {
    /// The constant's image and the images it references.
    Image(ConstImage, Vec<(Box<str>, Hash128)>),
    /// The encoded diagnostics of the constant's failure.
    Failed(Vec<u8>),
}

impl ConstEntry {
    /// The entry's payload, sealed.
    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Image(image, refs) => encode_image(image, refs),
            Self::Failed(blob) => {
                let mut w = Writer::new();
                w.raw(MAGIC);
                w.u8(VERSION);
                w.u8(1);
                w.len(blob.len());
                w.raw(blob);
                sealed(w.finish())
            }
        }
    }

    /// The entry of `path` in a payload [`ConstEntry::encode`] wrote;
    /// `None` for anything else (a miss, D-960).
    pub(super) fn decode(path: &str, bytes: &[u8]) -> Option<Self> {
        let payload = unsealed(bytes)?;
        let mut r = Reader::new(payload);
        if r.take(4)? != MAGIC || r.u8()? != VERSION {
            return None;
        }
        let entry = match r.u8()? {
            0 => {
                let n = r.len()?;
                let mut refs = Vec::new();
                for _ in 0..n {
                    let path: Box<str> = r.str()?.into();
                    refs.push((path, Hash128(r.array16()?)));
                }
                let len = r.len()?;
                let image = ConstImage {
                    path: path.into(),
                    bytes: r.take(len)?.to_vec(),
                };
                Self::Image(image, refs)
            }
            1 => {
                let len = r.len()?;
                Self::Failed(r.take(len)?.to_vec())
            }
            _ => return None,
        };
        r.is_done().then_some(entry)
    }
}

/// The sealed payload of an image entry, as [`Writer`] would write it, in
/// one allocation of its final size: a large constant's image is copied
/// once here and once into the store's queued file, never more.
fn encode_image(image: &ConstImage, refs: &[(Box<str>, Hash128)]) -> Vec<u8> {
    let len = |n: usize| u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes();
    let refs_len: usize = refs
        .iter()
        .map(|(path, _)| path.len().saturating_add(24))
        .sum();
    let size = image
        .bytes
        .len()
        .saturating_add(refs_len)
        .saturating_add(4 + 1 + 1 + 8 + 8 + 16);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(0);
    out.extend_from_slice(&len(refs.len()));
    for (path, hash) in refs {
        out.extend_from_slice(&len(path.len()));
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&hash.0);
    }
    out.extend_from_slice(&len(image.bytes.len()));
    out.extend_from_slice(&image.bytes);
    sealed(out)
}

/// The store and log constant entries go through: a command's cache, or
/// the sink its front end handed to the back end.
pub(crate) struct ConstStore {
    store: StoreShare,
    log: CacheLog,
    /// Whether lookups read the store (`CHEBY_DUMP_DIR` bypasses reads and
    /// still writes, D-965).
    reads: bool,
}

impl ConstStore {
    /// The store of `cache`.
    pub(crate) fn of_cache(cache: &crate::Cache) -> Self {
        Self {
            store: cache.store.share(),
            log: cache.log.share(),
            reads: cache.reads,
        }
    }

    /// The store of `front`'s command, `None` without a cache.
    pub(crate) fn of_front(front: &Frontend) -> Option<Self> {
        let sink = front.records.as_ref()?;
        let (store, log, reads) = sink.const_parts();
        Some(Self {
            store: store.clone(),
            log: log.share(),
            reads,
        })
    }

    /// The entry of `key`, unlogged; `None` when it misses, does not
    /// decode, or reads are bypassed.
    fn find(&self, path: &str, key: &Key) -> Option<ConstEntry> {
        if !self.reads {
            return None;
        }
        let bytes = self.store.get_record(key)?;
        ConstEntry::decode(path, &bytes)
    }

    /// [`ConstStore::find`], logged as `const <path> hit|miss` (D-973).
    fn lookup(&self, path: &str, key: &Key) -> Option<ConstEntry> {
        let found = self.find(path, key);
        self.log.record(ARTIFACT, path, found.is_some());
        found
    }

    /// Writes `entry` under `key` behind the command (D-1138), at most once
    /// per process (D-1139); a write that fails is skipped (D-1129).
    fn put(&self, key: &Key, entry: &ConstEntry) {
        self.put_payload(key, &entry.encode());
    }

    /// Writes an entry's sealed payload, as [`ConstStore::put`] does.
    fn put_payload(&self, key: &Key, payload: &[u8]) {
        let _ = self.store.put_record(key, payload);
    }
}

/// The key of each of `infos` after a check: its constant module set from
/// `reach` (D-593, `const_infos`' `deps`), std's modules left out (module
/// docs), each with its source's SipHash-1-3 128 (D-963).
///
/// # Errors
///
/// An [`Ice`] if a constant's module is unknown.
pub(crate) fn keys_after_check(
    front: &Frontend,
    sources: &SourceMap,
    reach: &Reach,
    infos: &[ConstInfo],
    build: BuildId,
    settings: &Settings,
    limits: &ConstLimits,
) -> Result<Vec<Key>, Ice> {
    let sets = reach.const_modules();
    let mut hashes: BTreeMap<ModuleId, Option<(String, Hash128)>> = BTreeMap::new();
    let mut keys = Vec::with_capacity(infos.len());
    for info in infos {
        let mut deps: Vec<(String, Hash128)> = Vec::new();
        for &module in sets.get(&info.def).map_or(&[][..], Vec::as_slice) {
            let dep = match hashes.entry(module) {
                std::collections::btree_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(dep_of(front, sources, module)?)
                }
            };
            if let Some(dep) = dep {
                deps.push(dep.clone());
            }
        }
        deps.sort();
        let deps: Vec<(&str, Hash128)> = deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
        let module = front.modules.try_get(info.def.module)?;
        let module_path = module.path.to_string();
        keys.push(const_key(
            build,
            settings,
            limits,
            module.path.package(),
            &module_path,
            &info.name,
            &deps,
        ));
    }
    Ok(keys)
}

/// `module`'s path and source hash as a key's dependency, `None` for a std
/// module (module docs) or the builtin one.
fn dep_of(
    front: &Frontend,
    sources: &SourceMap,
    module: ModuleId,
) -> Result<Option<(String, Hash128)>, Ice> {
    let info = front.modules.try_get(module)?;
    if info.package == PackageId::STD {
        return Ok(None);
    }
    let hash = if let Some(hashes) = front.source_hashes(module) {
        hashes.sip128
    } else if let Some(file) = info.file {
        let text = sources
            .file(file)
            .ok_or_else(|| Ice::new(format!("module `{}` has no source", info.path)))?
            .text();
        crate::SourceHashes::of(text.as_bytes()).sip128
    } else {
        // D-593: a module without a source file hashes as 0.
        Hash128([0; 16])
    };
    Ok(Some((info.path.to_string(), hash)))
}

/// What a build does with its constants: which hit, which it evaluates,
/// and how the results combine (module docs).
pub(crate) struct ConstPlan<'i> {
    store: Option<ConstStore>,
    infos: &'i [ConstInfo],
    keys: Vec<Key>,
    /// Per constant: its usable hit.
    hits: Vec<Option<Hit>>,
}

/// A usable hit.
enum Hit {
    Image(ConstImage, Vec<(Box<str>, Hash128)>),
    /// The failure's diagnostics, replayed.
    Failed(Vec<Diagnostic>),
}

impl<'i> ConstPlan<'i> {
    /// Looks up every constant of `infos` (the build's, in D-535's order,
    /// reached in `reach`) and decides what to evaluate (module docs).
    /// Without a cache everything is evaluated; with `dump`
    /// (`CHEBY_DUMP_DIR`) nothing is read (D-965) and everything is
    /// evaluated and written.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a constant's module is unknown.
    pub(crate) fn new(
        front: &Frontend,
        sources: &SourceMap,
        reach: &Reach,
        infos: &'i [ConstInfo],
        mir: MirOptions,
        dump: bool,
    ) -> Result<Self, Ice> {
        let Some(mut store) = ConstStore::of_front(front) else {
            return Ok(Self {
                store: None,
                infos,
                keys: Vec::new(),
                hits: infos.iter().map(|_| None).collect(),
            });
        };
        if dump {
            store.reads = false;
        }
        let build = store.store.build_id();
        let keys = keys_after_check(
            front,
            sources,
            reach,
            infos,
            build,
            &settings(mir),
            &ConstLimits::of(front),
        )?;
        let mut hits: Vec<Option<Hit>> = Vec::with_capacity(infos.len());
        for (info, key) in infos.iter().zip(&keys) {
            let hit = match store.lookup(&info.path, key) {
                Some(ConstEntry::Image(image, refs)) => Some(Hit::Image(image, refs)),
                Some(ConstEntry::Failed(blob)) => {
                    replay_failure(front, sources, info, &blob)?.map(Hit::Failed)
                }
                None => None,
            };
            hits.push(hit);
        }
        let mut plan = Self {
            store: Some(store),
            infos,
            keys,
            hits,
        };
        plan.close(reach);
        Ok(plan)
    }

    /// Drops every hit that cannot be installed beside the others (module
    /// docs): one whose referenced images are not hits with the bytes it
    /// was written beside, one that reads a constant this build evaluates,
    /// and one that references a constant this build evaluates; and adds
    /// the constants each evaluated one reads, until nothing changes.
    fn close(&mut self, reach: &Reach) {
        let index: BTreeMap<&str, usize> = self
            .infos
            .iter()
            .enumerate()
            .map(|(n, info)| (&*info.path, n))
            .collect();
        // The hash of each hit's image that another hit references, once.
        let mut hashes: BTreeMap<usize, Hash128> = BTreeMap::new();
        for hit in self.hits.iter().flatten() {
            let Hit::Image(_, refs) = hit else { continue };
            for (path, _) in refs {
                if let Some(&n) = index.get(&**path)
                    && !hashes.contains_key(&n)
                    && let Some(Some(Hit::Image(image, _))) = self.hits.get(n)
                {
                    hashes.insert(n, Hash128::of(&image.bytes));
                }
            }
        }
        // A warm build whose every constant hits with matching references
        // walks nothing more.
        if self.hits.iter().all(|hit| match hit {
            Some(Hit::Image(_, refs)) => refs
                .iter()
                .all(|(path, hash)| index.get(&**path).and_then(|n| hashes.get(n)) == Some(hash)),
            Some(Hit::Failed(_)) => true,
            None => false,
        }) {
            return;
        }
        let at: BTreeMap<DefId, usize> = self
            .infos
            .iter()
            .enumerate()
            .map(|(n, info)| (info.def, n))
            .collect();
        let reads: Vec<BTreeSet<usize>> = self
            .infos
            .iter()
            .map(|info| constants_read(reach, info.def, &at))
            .collect();
        loop {
            let mut changed = false;
            for n in 0..self.hits.len() {
                let keep = match self.hits.get(n) {
                    Some(Some(Hit::Image(_, refs))) => {
                        refs.iter().all(|(path, hash)| {
                            index.get(&**path).is_some_and(|&m| {
                                matches!(self.hits.get(m), Some(Some(Hit::Image(..))))
                                    && hashes.get(&m) == Some(hash)
                            })
                        }) && self.reads_only_hits(reads.get(n))
                    }
                    Some(Some(Hit::Failed(_))) => self.reads_only_hits(reads.get(n)),
                    Some(None) | None => true,
                };
                if !keep && let Some(slot) = self.hits.get_mut(n) {
                    *slot = None;
                    changed = true;
                }
            }
            // An evaluated constant's evaluation also evaluates every
            // constant it reads, whose image it may reference.
            for n in 0..self.hits.len() {
                if matches!(self.hits.get(n), Some(None)) {
                    for &m in reads.get(n).into_iter().flatten() {
                        if let Some(slot) = self.hits.get_mut(m)
                            && slot.is_some()
                        {
                            *slot = None;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Whether every constant of `reads` is a hit.
    fn reads_only_hits(&self, reads: Option<&BTreeSet<usize>>) -> bool {
        reads
            .into_iter()
            .flatten()
            .all(|&m| matches!(self.hits.get(m), Some(Some(_))))
    }

    /// The constants to evaluate, in D-535's order: none when every one
    /// hits.
    pub(crate) fn to_evaluate(&self) -> Vec<ConstInfo> {
        self.infos
            .iter()
            .zip(&self.hits)
            .filter(|(_, hit)| hit.is_none())
            .map(|(info, _)| info.clone())
            .collect()
    }

    /// The build's images in D-535's order, or its diagnostics, sorted:
    /// `fresh` (the evaluation of [`ConstPlan::to_evaluate`], in its order)
    /// combined with the hits, and the fresh results stored. A failure is
    /// stored per constant, by the constant its diagnostics point at,
    /// except a time-limit one (D-964).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `fresh` holds another number of images than
    /// constants were evaluated, or a failure's span cannot be stored.
    pub(crate) fn finish(
        self,
        front: &Frontend,
        sources: &SourceMap,
        fresh: Result<Vec<ConstImage>, Vec<Diagnostic>>,
    ) -> Result<Result<Vec<ConstImage>, Vec<Diagnostic>>, Ice> {
        let evaluated: Vec<usize> = (0..self.infos.len())
            .filter(|&n| matches!(self.hits.get(n), Some(None)))
            .collect();
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let mut images: Vec<Option<ConstImage>> = self.infos.iter().map(|_| None).collect();
        match fresh {
            Ok(fresh) => {
                if fresh.len() != evaluated.len() {
                    return Err(Ice::new(format!(
                        "{} constant images for {} evaluated constants",
                        fresh.len(),
                        evaluated.len()
                    )));
                }
                for (&n, image) in evaluated.iter().zip(fresh) {
                    if let Some(slot) = images.get_mut(n) {
                        *slot = Some(image);
                    }
                }
                self.store_images(&evaluated, &images);
            }
            Err(failed) => {
                self.store_failures(front, sources, &failed)?;
                diagnostics.extend(failed);
            }
        }
        let mut out = Vec::with_capacity(self.infos.len());
        for (slot, hit) in images.into_iter().zip(self.hits) {
            match (slot, hit) {
                (Some(image), _) | (None, Some(Hit::Image(image, _))) => out.push(image),
                (None, Some(Hit::Failed(replayed))) => diagnostics.extend(replayed),
                (None, None) => {}
            }
        }
        if diagnostics.is_empty() {
            Ok(Ok(out))
        } else {
            sources.sort(&mut diagnostics);
            Ok(Err(diagnostics))
        }
    }

    /// Stores the fresh images of `evaluated`, each with the images it
    /// references; one whose references cannot be read is not stored.
    fn store_images(&self, evaluated: &[usize], images: &[Option<ConstImage>]) {
        let Some(store) = &self.store else {
            return;
        };
        let by_path: BTreeMap<&str, &ConstImage> =
            images.iter().flatten().map(|i| (&*i.path, i)).collect();
        for &n in evaluated {
            let (Some(Some(image)), Some(key)) = (images.get(n), self.keys.get(n)) else {
                continue;
            };
            let Some(paths) = cheby_consteval::image_references(&image.bytes) else {
                continue;
            };
            let refs: Option<Vec<(Box<str>, Hash128)>> = paths
                .into_iter()
                .map(|path| {
                    let hash = Hash128::of(&by_path.get(&*path)?.bytes);
                    Some((path, hash))
                })
                .collect();
            if let Some(refs) = refs {
                store.put_payload(key, &encode_image(image, &refs));
            }
        }
    }

    /// Stores each evaluated constant's failure, by the constant the
    /// diagnostics' primary spans point at: none of a constant with a
    /// time-limit failure (D-964), and none of a constant that failed
    /// without a diagnostic of its own (it read a failed one).
    fn store_failures(
        &self,
        front: &Frontend,
        sources: &SourceMap,
        failed: &[Diagnostic],
    ) -> Result<(), Ice> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let mut by_constant: BTreeMap<usize, Vec<Diagnostic>> = BTreeMap::new();
        for d in failed {
            let primary = d
                .labels
                .iter()
                .find(|l| l.style == LabelStyle::Primary)
                .map(|l| l.span);
            let owner =
                primary.and_then(|span| self.infos.iter().position(|info| info.name_span == span));
            if let Some(n) = owner
                && matches!(self.hits.get(n), Some(None))
            {
                by_constant.entry(n).or_default().push(d.clone());
            }
        }
        for (n, list) in by_constant {
            if list.iter().any(cheby_consteval::is_time_limit) {
                continue;
            }
            let (Some(info), Some(key)) = (self.infos.get(n), self.keys.get(n)) else {
                continue;
            };
            let Some(cx) = span_cx(front, sources, info)? else {
                continue;
            };
            if let Some(blob) = replay::encode_diagnostics(&[&list], &cx) {
                store.put(key, &ConstEntry::Failed(blob));
            }
        }
        Ok(())
    }
}

/// The images of `infos` (a build's constants reached in `reach`, in
/// D-535's order) through the store (D-977): the hits as stored, the rest
/// from `evaluate` (an evaluation module over the constants it is given,
/// in their order, D-978), stored behind the command (D-1138). When every
/// constant hits, `evaluate` is not called: no evaluation module is built
/// (D-1187's second compile gone). `Err` holds the diagnostics, fresh and
/// replayed, sorted. With `dump` every constant is evaluated (D-965).
///
/// # Errors
///
/// An [`Ice`] from `evaluate` or from combining the results.
pub(crate) fn images_through_cache(
    front: &Frontend,
    sources: &SourceMap,
    reach: &Reach,
    infos: &[ConstInfo],
    mir: MirOptions,
    dump: bool,
    evaluate: impl FnOnce(&[ConstInfo]) -> Result<Result<Vec<ConstImage>, Vec<Diagnostic>>, Ice>,
) -> Result<Result<Vec<ConstImage>, Vec<Diagnostic>>, Ice> {
    let plan = ConstPlan::new(front, sources, reach, infos, mir, dump)?;
    let subset = plan.to_evaluate();
    let fresh = if subset.is_empty() && !infos.is_empty() {
        Ok(Vec::new())
    } else {
        evaluate(&subset)?
    };
    plan.finish(front, sources, fresh)
}

/// The constants the initializer of `def` reads, directly or through
/// functions and other initializers, as positions in `at`.
fn constants_read(reach: &Reach, def: DefId, at: &BTreeMap<DefId, usize>) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    let mut seen = BTreeSet::from([def]);
    let mut stack: Vec<DefId> = reach.refers_to(def).collect();
    while let Some(item) = stack.pop() {
        if !seen.insert(item) {
            continue;
        }
        if reach.is_const(item)
            && let Some(&n) = at.get(&item)
        {
            out.insert(n);
        }
        stack.extend(reach.refers_to(item));
    }
    out
}

/// The spans of `info`'s module for its stored diagnostics, `None` for a
/// module without a source file.
fn span_cx<'a>(
    front: &'a Frontend,
    sources: &SourceMap,
    info: &ConstInfo,
) -> Result<Option<SpanCx<'a>>, Ice> {
    let Some(file) = front.modules.try_get(info.def.module)?.file else {
        return Ok(None);
    };
    let Some(source) = sources.file(file) else {
        return Ok(None);
    };
    Ok(Some(SpanCx {
        own: file,
        own_len: u32::try_from(source.text().len()).unwrap_or(u32::MAX),
        modules: &front.modules,
        set: &front.set,
        globals: &front.globals,
    }))
}

/// The diagnostics of a stored failure, `None` when they no longer
/// replay (the entry then misses).
fn replay_failure(
    front: &Frontend,
    sources: &SourceMap,
    info: &ConstInfo,
    blob: &[u8],
) -> Result<Option<Vec<Diagnostic>>, Ice> {
    let Some(cx) = span_cx(front, sources, info)? else {
        return Ok(None);
    };
    Ok(replay::decode_diagnostics(blob, 1, &cx).and_then(|lists| lists.into_iter().next()))
}

/// The front records of the vouched modules, by path, as
/// [`keys_from_records`] walks them.
struct Records<'a> {
    modules: &'a ModuleTable,
    vouched: &'a [Option<&'a IndexEntry>],
    /// Per module path: its summaries' mentions, by kind and item.
    summaries: BTreeMap<&'a str, BTreeMap<(MentionerKind, &'a str), &'a [super::front::ItemRef]>>,
}

/// What [`Records::walk`] met from one constant.
struct Walked {
    /// The constant module set outside std, with source hashes, sorted.
    deps: Vec<(String, Hash128)>,
    /// The package constants it reads, and every std item it mentions
    /// (some of them constants), as `module::name`.
    reads: BTreeSet<String>,
}

/// Where a walk over summaries stands on one module.
enum Module<'a> {
    /// A vouched package module, with its front record.
    Package(ModuleId, &'a FrontRecord),
    /// A std module or the builtin one: left out of keys (module docs).
    Std,
}

impl<'a> Records<'a> {
    fn new(modules: &'a ModuleTable, vouched: &'a [Option<&'a IndexEntry>]) -> Self {
        let mut summaries = BTreeMap::new();
        for entry in vouched.iter().flatten() {
            let by_item: BTreeMap<(MentionerKind, &str), &[super::front::ItemRef]> = entry
                .record
                .references
                .iter()
                .map(|s| ((s.kind, s.item.as_str()), s.mentions.as_slice()))
                .collect();
            summaries.insert(entry.path.as_str(), by_item);
        }
        Self {
            modules,
            vouched,
            summaries,
        }
    }

    /// The module at `path`; `None` for one that is neither vouched nor
    /// std's (its constants' keys are unknown before the check).
    fn module(&self, path: &str) -> Option<Module<'a>> {
        if path == cheby_iface::BUILTIN_MODULE_NAME {
            return Some(Module::Std);
        }
        let id = self.modules.find(&crate::parse_module_path(path)?)?;
        let info = self.modules.get(id)?;
        if info.package == PackageId::STD {
            return Some(Module::Std);
        }
        let entry = (*self.vouched.get(usize::try_from(id.as_u32()).ok()?)?)?;
        Some(Module::Package(id, &entry.record))
    }

    /// The constant module set of `module::name` outside std (D-593, as a
    /// walk from the constant alone over reference summaries meets it,
    /// D-967), with source hashes; `None` when it reaches a module the
    /// view does not vouch for.
    fn deps(&self, module: &str, name: &str) -> Option<Vec<(String, Hash128)>> {
        self.walk(module, name).map(|walked| walked.deps)
    }

    /// The walk of [`Records::deps`], with what it met besides the set.
    fn walk(&self, module: &'a str, name: &'a str) -> Option<Walked> {
        let mut set: BTreeSet<&str> = BTreeSet::from([module]);
        let mut seen: BTreeSet<(MentionerKind, &str, &str)> = BTreeSet::new();
        let mut stack: Vec<(&str, &str)> = Vec::new();
        let mut reads: BTreeSet<String> = BTreeSet::new();
        self.mentions(module, MentionerKind::Constant, name, &mut seen, &mut stack);
        while let Some((m, item)) = stack.pop() {
            let Module::Package(_, record) = self.module(m)? else {
                // A std item the constant may read, if it is a constant.
                reads.insert(format!("{m}::{item}"));
                continue;
            };
            if self
                .summaries
                .get(m)
                .is_some_and(|s| s.contains_key(&(MentionerKind::Constant, item)))
                && (m, item) != (module, name)
            {
                reads.insert(format!("{m}::{item}"));
            }
            if set.insert(m) {
                // A module the walk enters reaches its types' hooks
                // (D-585), as `load_set`'s walk does.
                for s in record
                    .references
                    .iter()
                    .filter(|s| s.kind == MentionerKind::Hook)
                {
                    for r in &s.mentions {
                        stack.push((&r.module, &r.item));
                    }
                }
            }
            for kind in [MentionerKind::Function, MentionerKind::Constant] {
                self.mentions(m, kind, item, &mut seen, &mut stack);
            }
        }
        let mut deps = Vec::with_capacity(set.len());
        for m in set {
            let Module::Package(id, _) = self.module(m)? else {
                continue;
            };
            let entry = (*self.vouched.get(usize::try_from(id.as_u32()).ok()?)?)?;
            deps.push((m.to_owned(), entry.source));
        }
        Some(Walked { deps, reads })
    }

    /// The package constants, as `module::name`, that a walk from `starts`
    /// (each a module path, a summary kind and an item) meets, through
    /// functions, constants' initializers and the hooks of every module it
    /// enters (D-585), as the build's reach does; `None` when it meets a
    /// module the view does not vouch for.
    fn reached(&self, starts: &[(&'a str, MentionerKind, &'a str)]) -> Option<BTreeSet<String>> {
        let mut entered: BTreeSet<&str> = BTreeSet::new();
        let mut seen: BTreeSet<(MentionerKind, &str, &str)> = BTreeSet::new();
        let mut stack: Vec<(&str, &str)> = Vec::new();
        let mut out: BTreeSet<String> = BTreeSet::new();
        for &(module, kind, item) in starts {
            let Module::Package(_, record) = self.module(module)? else {
                continue;
            };
            if entered.insert(module) {
                Self::hooks(record, &mut stack);
            }
            self.mentions(module, kind, item, &mut seen, &mut stack);
        }
        while let Some((m, item)) = stack.pop() {
            let Module::Package(_, record) = self.module(m)? else {
                continue;
            };
            if entered.insert(m) {
                Self::hooks(record, &mut stack);
            }
            if self
                .summaries
                .get(m)
                .is_some_and(|s| s.contains_key(&(MentionerKind::Constant, item)))
            {
                out.insert(format!("{m}::{item}"));
            }
            for kind in [MentionerKind::Function, MentionerKind::Constant] {
                self.mentions(m, kind, item, &mut seen, &mut stack);
            }
        }
        Some(out)
    }

    /// Pushes the mentions of `record`'s hook summaries.
    fn hooks(record: &'a FrontRecord, stack: &mut Vec<(&'a str, &'a str)>) {
        for s in record
            .references
            .iter()
            .filter(|s| s.kind == MentionerKind::Hook)
        {
            for r in &s.mentions {
                stack.push((&r.module, &r.item));
            }
        }
    }

    /// Pushes the mentions of `module`'s summary of `item` as `kind`, the
    /// first time it is met.
    fn mentions(
        &self,
        module: &'a str,
        kind: MentionerKind,
        item: &'a str,
        seen: &mut BTreeSet<(MentionerKind, &'a str, &'a str)>,
        stack: &mut Vec<(&'a str, &'a str)>,
    ) {
        let Some(mentions) = self
            .summaries
            .get(module)
            .and_then(|s| s.get(&(kind, item)))
        else {
            return;
        };
        if !seen.insert((kind, module, item)) {
            return;
        }
        for r in *mentions {
            stack.push((&r.module, &r.item));
        }
    }
}

/// D-1250 with D-1134: the key of every package constant of a vouched
/// module, as `module::name`, from the vouched entries alone: `deps` (the
/// constant module set with source hashes, D-538, D-593) from their
/// reference summaries (D-967) and source hashes, never from THIR or a
/// checked body; `None` when its constant module set reaches a module the
/// entries do not vouch for. Each key equals the one `const_infos`' `deps`
/// give after a check ([`keys_after_check`]; the test
/// `const_cache_keys_before_check_equal_keys_after`). A warm command decides
/// its misses through [`misses_before_check_from`] instead, which walks
/// only the constants its roots reach (D-1259).
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the keys-before-and-after test reads every key")
)]
pub(crate) fn keys_from_records(
    modules: &ModuleTable,
    vouched: &[Option<&IndexEntry>],
    build: BuildId,
    settings: &Settings,
    limits: &ConstLimits,
) -> Vec<(String, Option<Key>)> {
    let records = Records::new(modules, vouched);
    let mut out = Vec::new();
    for entry in vouched.iter().flatten() {
        let module = entry.path.as_str();
        let package = module.split("::").next().unwrap_or(module);
        for s in &entry.record.references {
            if s.kind != MentionerKind::Constant {
                continue;
            }
            let key = records.deps(module, &s.item).map(|deps| {
                let deps: Vec<(&str, Hash128)> =
                    deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
                const_key(build, settings, limits, package, module, &s.item, &deps)
            });
            out.push((format!("{module}::{}", s.item), key));
        }
    }
    out
}

/// The roots a warm command's constants are reached from (D-510, D-534).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ConstRoots<'a> {
    /// `cheby run`: the `main` of the module at this path.
    Main(&'a str),
    /// `cheby test`: every test of the package.
    Tests,
}

/// The vouched package modules of `view` that hold a constant `roots`
/// reach whose entry would not hit as [`ConstPlan`] uses hits: no key
/// before the check, no entry, a referenced image that is not a hit with
/// the bytes it was written beside, or a failure (whose diagnostics replay
/// only once the module's spans are known, so it is checked); and the
/// modules of the constants such a constant reads or that read it. Probes
/// are unlogged: the back end's lookup logs each constant once.
///
/// The reached constants come from walking the vouched modules' reference
/// summaries (D-967) as the build's reach does: a constant no root
/// reaches is never looked up, so its missing entry checks nothing
/// (D-1259). When the walk meets a module the view
/// does not vouch for, every package constant counts. A `main`'s entry
/// dictionary is not in its summary (`load_set.rs`), so a constant
/// reached only through it counts only if something else reaches it (K4d's
/// front end then starts again with every module checked).
pub(crate) fn misses_before_check_from(
    view: &crate::warm_keys::WarmView<'_>,
    settings: &Settings,
    limits: &ConstLimits,
    roots: ConstRoots<'_>,
) -> BTreeSet<ModuleId> {
    let records = Records::new(view.modules, view.vouched);
    let mut starts: Vec<(&str, MentionerKind, &str)> = Vec::new();
    match roots {
        ConstRoots::Main(module) => starts.push((module, MentionerKind::Function, "main")),
        ConstRoots::Tests => {
            for entry in view.vouched.iter().flatten() {
                for s in &entry.record.references {
                    if s.kind == MentionerKind::Test {
                        starts.push((entry.path.as_str(), s.kind, s.item.as_str()));
                    }
                }
            }
        }
    }
    let reached = records.reached(&starts);
    misses_among(view, settings, limits, reached.as_ref())
}

/// The package constants `main` of the module at `module` reaches, from
/// the vouched entries alone, as [`misses_before_check_from`] walks them.
#[cfg(test)]
pub(super) fn reached_from_main(
    modules: &ModuleTable,
    vouched: &[Option<&IndexEntry>],
    module: &str,
) -> Option<BTreeSet<String>> {
    Records::new(modules, vouched).reached(&[(module, MentionerKind::Function, "main")])
}

/// [`misses_before_check_from`] over the package constants of `only`, or
/// every one when it is `None`.
fn misses_among(
    view: &crate::warm_keys::WarmView<'_>,
    settings: &Settings,
    limits: &ConstLimits,
    only: Option<&BTreeSet<String>>,
) -> BTreeSet<ModuleId> {
    let store = ConstStore::of_cache(view.cache.cache);
    let build = store.store.build_id();
    let records = Records::new(view.modules, view.vouched);
    let mut found: Found = BTreeMap::new();
    for entry in view.vouched.iter().flatten() {
        let module = entry.path.as_str();
        let package = module.split("::").next().unwrap_or(module);
        for s in &entry.record.references {
            if s.kind != MentionerKind::Constant {
                continue;
            }
            let path = format!("{module}::{}", s.item);
            if only.is_some_and(|only| !only.contains(&path)) {
                continue;
            }
            let walked = records.walk(module, &s.item);
            let hit = walked.as_ref().and_then(|walked| {
                let deps: Vec<(&str, Hash128)> =
                    walked.deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
                let key = const_key(build, settings, limits, package, module, &s.item, &deps);
                store.find(&path, &key)
            });
            let reads = walked.map(|w| w.reads).unwrap_or_default();
            found.insert(path, (hit, reads));
        }
    }
    // A std constant's entry: its key holds no source (module docs);
    // `None` for a std item that is no constant.
    let std_entry = |path: &str| -> Option<Option<ConstEntry>> {
        let (module, name) = path.rsplit_once("::")?;
        let id = view.modules.find(&crate::parse_module_path(module)?)?;
        if view.modules.get(id)?.package != PackageId::STD {
            return None;
        }
        let iface = view.set.get(id)?;
        let is_const = iface.items.iter().any(|item| {
            item.kind == cheby_iface::DefKind::Const
                && item.parent.is_none()
                && view.globals.names.get(item.name) == name
        });
        if !is_const {
            return None;
        }
        let key = const_key(build, settings, limits, "std", module, name, &[]);
        Some(store.find(path, &key))
    };
    let image_hash = |path: &str| -> Option<Hash128> {
        let entry = match found.get(path) {
            Some((entry, _)) => entry.clone(),
            None => std_entry(path)?,
        };
        match entry? {
            ConstEntry::Image(image, _) => Some(Hash128::of(&image.bytes)),
            ConstEntry::Failed(_) => None,
        }
    };
    // The constants that miss (a failure is checked: its replay needs the
    // module's spans), closed as `ConstPlan` closes them.
    let mut evaluated: BTreeSet<&str> = BTreeSet::new();
    for (path, (entry, reads)) in &found {
        let usable = match entry {
            Some(ConstEntry::Image(_, refs)) => refs
                .iter()
                .all(|(path, hash)| image_hash(path) == Some(*hash)),
            Some(ConstEntry::Failed(_)) | None => false,
        } && reads
            .iter()
            .filter(|read| !found.contains_key(*read))
            .all(|read| std_entry(read).is_none_or(|e| e.is_some()));
        if !usable {
            evaluated.insert(path);
        }
    }
    close_found(&found, &mut evaluated);
    let mut missing: BTreeSet<ModuleId> = BTreeSet::new();
    for path in evaluated {
        let Some((module, _)) = path.rsplit_once("::") else {
            continue;
        };
        if let Some(Module::Package(id, _)) = records.module(module) {
            missing.insert(id);
        }
    }
    missing
}

/// Each package constant before the check: its entry, if it hits, and
/// what it reads.
type Found = BTreeMap<String, (Option<ConstEntry>, BTreeSet<String>)>;

/// Adds to `evaluated` what [`ConstPlan`]'s closing adds: the constants an
/// evaluated one reads, and the constants that read or reference an
/// evaluated one, until nothing changes.
fn close_found<'f>(found: &'f Found, evaluated: &mut BTreeSet<&'f str>) {
    loop {
        let mut more: Vec<&str> = Vec::new();
        for (path, (entry, reads)) in found {
            if evaluated.contains(path.as_str()) {
                more.extend(
                    reads
                        .iter()
                        .map(String::as_str)
                        .filter(|r| found.contains_key(*r)),
                );
                continue;
            }
            let reads_evaluated = reads.iter().any(|r| evaluated.contains(r.as_str()));
            let refs_evaluated = matches!(entry, Some(ConstEntry::Image(_, refs))
                if refs.iter().any(|(r, _)| evaluated.contains(&**r)));
            if reads_evaluated || refs_evaluated {
                more.push(path);
            }
        }
        let before = evaluated.len();
        evaluated.extend(more);
        if evaluated.len() == before {
            break;
        }
    }
}
