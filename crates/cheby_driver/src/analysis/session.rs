//! `Session`: the analysis of one package over a changing [`Overlay`], by
//! the two-track algorithm (D-1347, A §2).
//!
//! [`Session::analyze_live`] runs phase A on the live text, then the Q run:
//! `run_analysis` with `Retain::LIVE` and a cycle cut (D-1361, D-1358).
//! Its result is a [`Live`]: a [`Snapshot`] for queries, and what to
//! publish, all of it plain data (D-1364).
//!
//! What a `Live` publishes:
//!
//! - Nothing broken, a clean manifest, no E0037, no cut import and nothing
//!   lost: the Q run is the check, so [`Live::report`] holds every file's
//!   diagnostics, as `cheby check` reports them (D-1359).
//! - Otherwise [`Live::phase_a`] holds each broken file's phase-A
//!   diagnostics and `cheby.toml`'s, taken before the Q run, so a failed Q
//!   run still publishes them and tolerant-run diagnostics never enter them
//!   (plan §1.2). [`Live::plan`] asks for a D run when its strict inputs
//!   changed (D-1356), and [`Session::analyze_strict`] runs it.
//!
//! The strict inputs (A §2.1) are the manifest and every module's text: a
//! clean file's live text, a broken file's last good text, or the empty
//! text of a stub, a broken file with no last good text (D-1357). A D run
//! is `run_analysis` with `Retain::BUILD` and no cycle cut, so it has no
//! containment and fails as `cheby check` does (D-1359). Its report leaves
//! out broken files, which `phase_a` holds, and holds the files that import
//! a stub, transitively, whose published list stays (A §2.4).
//!
//! A file that leaves the broken set while the state is still not single
//! plans a D run whatever the strict key, since its strict text may equal
//! its new live text and its phase-A list would otherwise stay published
//! (A §2.3, "B becomes clean"). A held file among them gets an empty list.
//!
//! An internal error that containment does not hold, or a panic, ends the
//! Q run only: it comes back in [`Live::failed`], with no snapshot, and the
//! session runs again on the next call (R-14). A contained internal error
//! leaves a snapshot and plans a D run (D-1359, D-1362).
//!
//! A session of [`PackageKind::SingleFile`] analyzes one file that is no
//! module of any package, under a synthetic manifest (D-1371, A §6.4):
//! `single`.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_diag::{Diagnostic, FileId, SourceMap, codes};
use cheby_iface::{Globals, ModuleId};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, PackageError, Target, Targets};

use super::inputs::{DiskView, Loaded, LoadedFile, Overlay, OverlayFile, TextOrigin, load};
use super::plain::{InputsKey, Placed, PlainDiagnostic, exact_key, plain};
use super::snapshot::{BaselineEntry, Parts, Snapshot, SnapshotId};
use crate::cache::Hash128;
use crate::frontend::{CutCycles, Frontend, Retain, RunExtras, run_analysis};
use crate::{Error, Module, Package, SourceHashes, parse_file};

mod single;

/// Which files a session analyzes (A §1.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// The package whose `cheby.toml` is in the session's root.
    Package,
    /// One file that is no module of any package, as the only module of a
    /// package with a synthetic manifest (D-1371, A §6.4). Constants are
    /// never evaluated in it.
    SingleFile {
        /// The file, an absolute path in the session's root.
        file: PathBuf,
        /// No `cheby.toml` is above the file: it gets all of a run's
        /// diagnostics but W0005. Otherwise it sits inside a package
        /// without being one of its modules, and gets its phase-A
        /// diagnostics only, since its imports of that package's modules
        /// cannot resolve.
        strict: bool,
    },
}

/// What a session analyzes and with what.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The package root, the directory of `cheby.toml`. Ignored in
    /// single-file mode, whose root is the directory of its file.
    pub root: PathBuf,
    /// A package, or one file in single-file mode.
    pub kind: PackageKind,
    /// `--target`: check for this target only (D-550).
    pub target: Option<Target>,
    /// The interners, one per process, shared by every session (A §1.6).
    pub globals: Arc<Globals>,
}

/// The analysis of one package.
#[derive(Debug)]
pub struct Session {
    id: u32,
    root: PathBuf,
    kind: PackageKind,
    target: Option<Target>,
    globals: Arc<Globals>,
    disk: DiskView,
    generation: u64,
    /// Each module's last good text, by display path (D-1356).
    last_good: BTreeMap<String, Arc<str>>,
    /// The newest manifest text with no phase-A error.
    last_good_manifest: Option<Arc<str>>,
    /// The strict key of the last D run, or of the last run that served
    /// as one.
    last_strict: Option<Hash128>,
    /// The broken files of the last run, by absolute path.
    last_broken: BTreeSet<PathBuf>,
    /// Files that left the broken set since the last run that published
    /// them: a D run is due for them whatever the strict key.
    fixed: BTreeSet<PathBuf>,
}

/// What [`Session::analyze_live`] gives.
#[derive(Debug)]
pub struct Live {
    /// For queries. `None` when no manifest was ever usable, or the run
    /// failed ([`Live::failed`]).
    pub snapshot: Option<Arc<Snapshot>>,
    /// When the run is not the check: each file with phase-A errors and
    /// `cheby.toml`, with their phase-A diagnostics. Publish these now.
    pub phase_a: BTreeMap<PathBuf, Arc<FileReport>>,
    /// `Some`: nothing was broken, cut or lost, so this run is the check.
    pub report: Option<Report>,
    /// `Some`: a D run is due, on strict inputs not run before
    /// ([`Session::analyze_strict`]).
    pub plan: Option<StrictPlan>,
    /// Every file of the package in this run, `cheby.toml` included. A
    /// published path outside this set has left the package (A §8.4).
    pub files: BTreeSet<PathBuf>,
    /// The module files with phase-A errors in this run, set even when the
    /// run failed. While it is not empty the package is not one that
    /// `cheby check` accepts, whatever the latest [`Report::clean`] says:
    /// a file that turns broken plans no D run when its last good text is
    /// the text just run (A §2.3), so no new report follows (D-1359).
    pub broken: BTreeSet<PathBuf>,
    /// The run ended in an internal error that containment does not hold,
    /// or a panic: `snapshot` is `None`, `phase_a` is good, and `plan` is
    /// made as for any other run.
    pub failed: Option<String>,
}

/// What to publish for a package. All of it is free of generation handles.
#[derive(Debug)]
pub struct Report {
    /// Every package file with its diagnostics, an empty list included,
    /// except broken files ([`Live::phase_a`] holds them) and
    /// [`Report::held`] files.
    pub files: BTreeMap<PathBuf, Arc<FileReport>>,
    /// Files that import a stub, transitively: their published list stays
    /// as it is (D-1357, A §2.4). A held file that just left the broken
    /// set is not here: [`Report::files`] gives it an empty list, so its
    /// phase-A list does not stay.
    pub held: BTreeSet<PathBuf>,
    /// Errors with no location, for `window/showMessage` (A §2.7).
    pub tool: Vec<String>,
    /// The key of the manifest and every text the run read, in path order.
    pub inputs: Hash128,
    /// No error in any file, none in `tool`, and no file broken, so the
    /// package is one that `cheby check` accepts (D-1359). The child may
    /// run when this holds for the latest report and the latest
    /// [`Live::broken`] is empty (A §11). Always false in single-file
    /// mode, which has no child (A §6.4).
    pub clean: bool,
    /// The run read a last good text or a stub in place of a live text.
    /// While it is true, no fix removes code for W0003, W0004 or W0005,
    /// and organize imports removes nothing (D-1360).
    pub substituted: bool,
}

/// One file's diagnostics, with the text their ranges refer to.
#[derive(Debug)]
pub struct FileReport {
    /// The text the ranges refer to.
    pub text: Arc<str>,
    /// Where `text` came from.
    pub origin: TextOrigin,
    /// The editor's version, for an open document.
    pub version: Option<i32>,
    /// [`exact_key`] of `text` (D-1393).
    pub hash: Hash128,
    /// The diagnostics whose primary label is in this file, in the order
    /// `cheby check` prints them. `cheby.toml`'s also hold those with no
    /// span.
    pub diagnostics: Vec<PlainDiagnostic>,
}

/// A D run that [`Live::plan`] asks for: the strict inputs, taken when the
/// plan was made (A §2.1). Plain data: texts, paths and a key.
#[derive(Debug)]
pub struct StrictPlan {
    /// The session that made it.
    session: u32,
    key: Hash128,
    manifest: StrictFile,
    /// Each module with its strict text, in display order.
    modules: Vec<(ModulePath, StrictFile)>,
    /// The E0037 diagnostics of the walk of `src/`, which have no span.
    walk: Vec<Diagnostic>,
    /// The message of a walk of `src/` that failed.
    tool: Vec<String>,
    /// The files that left the broken set since the last D run, by
    /// absolute path.
    fixed: BTreeSet<PathBuf>,
}

impl StrictPlan {
    /// The strict key: the hash of the inputs and the target (A §2.1).
    #[must_use]
    pub const fn key(&self) -> Hash128 {
        self.key
    }

    /// Whether a file enters the run with a last good text or a stub, so
    /// its report is [`Report::substituted`].
    #[must_use]
    pub fn substituted(&self) -> bool {
        self.manifest.source != Source::Live
            || self.modules.iter().any(|(_, m)| m.source != Source::Live)
    }

    /// How many modules enter the run as stubs.
    #[cfg(test)]
    pub(super) fn stubs(&self) -> usize {
        self.modules
            .iter()
            .filter(|(_, m)| m.source == Source::Stub)
            .count()
    }
}

/// Where the text of a file of a D run comes from (A §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// A clean file's live text.
    Live,
    /// A broken file's last good text.
    LastGood,
    /// A broken file with no last good text: the empty text (D-1357).
    Stub,
}

/// One file of a [`StrictPlan`].
#[derive(Debug)]
struct StrictFile {
    path: PathBuf,
    display: String,
    text: Arc<str>,
    source: Source,
    /// The live text's origin and version, which the report carries.
    origin: TextOrigin,
    version: Option<i32>,
}

/// The state of the live manifest (A §2.2, step 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestState {
    /// It parses with no error.
    Clean,
    /// It parses, with errors.
    Usable,
    /// It does not give a manifest.
    Unusable,
}

/// What a Q run's snapshot keeps of its inputs beyond the texts.
#[derive(Clone, Copy)]
struct RunContext {
    /// The manifest's targets, after `--target`.
    targets: Targets,
    /// The run used the last good manifest (A §2.7).
    manifest_is_last_good: bool,
}

/// The source of session ids, unique in the process.
static SESSIONS: AtomicU32 = AtomicU32::new(0);

/// What one run read, as the session keeps it while it publishes.
struct RunFiles<'a> {
    root: &'a Path,
    sources: &'a SourceMap,
    files: &'a [LoadedFile],
    /// The files whose text in the run is their live text, when not all
    /// are. A label or an edit elsewhere is dropped (A §2.3).
    placeable: Option<&'a BTreeSet<FileId>>,
}

impl<'a> RunFiles<'a> {
    /// The files of a run over the package at `root`, every one of them on
    /// its live text.
    const fn new(root: &'a Path, sources: &'a SourceMap, files: &'a [LoadedFile]) -> Self {
        Self {
            root,
            sources,
            files,
            placeable: None,
        }
    }

    /// The package file `file` with its live text, if the run read that
    /// text. Std files are not placed.
    fn place(&self, file: FileId) -> Option<Placed<'_>> {
        if self.placeable.is_some_and(|set| !set.contains(&file)) {
            return None;
        }
        let loaded = self.files.iter().find(|f| f.file == file)?;
        let text = self.sources.file(file)?.text();
        Some(Placed {
            path: &loaded.path,
            text,
        })
    }

    /// The package file that `diag` belongs to: its primary label's, or
    /// `cheby.toml` for a diagnostic with no span or a span elsewhere.
    fn owner(&self, diag: &Diagnostic) -> Option<&LoadedFile> {
        diag.primary_span()
            .and_then(|span| self.files.iter().find(|f| f.file == span.file))
            .or_else(|| self.files.first())
    }

    /// Each package file of `wanted` with the diagnostics of `diags` it
    /// owns, as plain data, in the order of `diags`.
    fn reports(
        &self,
        diags: &[Diagnostic],
        wanted: impl Fn(&LoadedFile) -> bool,
    ) -> BTreeMap<PathBuf, Arc<FileReport>> {
        let mut lists: BTreeMap<FileId, Vec<PlainDiagnostic>> = self
            .files
            .iter()
            .filter(|f| wanted(f))
            .map(|f| (f.file, Vec::new()))
            .collect();
        for diag in diags {
            let Some(owner) = self.owner(diag) else {
                continue;
            };
            if let Some(list) = lists.get_mut(&owner.file) {
                list.push(plain(diag, owner.file, |file| self.place(file)));
            }
        }
        self.files
            .iter()
            .filter_map(|f| {
                let diagnostics = lists.remove(&f.file)?;
                let text = self.sources.file(f.file)?.text_arc();
                Some((
                    f.path.clone(),
                    Arc::new(FileReport {
                        hash: exact_key(&text),
                        text,
                        origin: f.origin,
                        version: f.version,
                        diagnostics,
                    }),
                ))
            })
            .collect()
    }

    /// The key of every text of the run, by display path (A §1.3).
    fn inputs(&self) -> Hash128 {
        let mut key = InputsKey::new();
        for f in self.files {
            if let Some(source) = self.sources.file(f.file) {
                key.file(
                    Path::new(&cheby_diag::display_path(&f.path, self.root)),
                    source.text(),
                );
            }
        }
        key.finish()
    }
}

/// What a D run loads from its plan.
struct StrictLoad {
    package: Package,
    sources: SourceMap,
    diagnostics: Vec<Diagnostic>,
    files: Vec<LoadedFile>,
    /// The files on their live text.
    live: BTreeSet<FileId>,
    /// The files given the empty text.
    stubs: BTreeSet<FileId>,
}

impl Session {
    /// A session over the package at `config.root`, or over the file of
    /// single-file mode, rooted at the file's directory (A §6.4). Both are
    /// made absolute. Nothing is read yet.
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        let kind = match config.kind {
            PackageKind::SingleFile { file, strict } => PackageKind::SingleFile {
                file: std::path::absolute(&file).unwrap_or(file),
                strict,
            },
            PackageKind::Package => PackageKind::Package,
        };
        let root = match &kind {
            PackageKind::SingleFile { file, .. } => file.parent().map(Path::to_path_buf),
            PackageKind::Package => None,
        }
        .unwrap_or_else(|| std::path::absolute(&config.root).unwrap_or(config.root));
        Self {
            id: SESSIONS.fetch_add(1, Ordering::Relaxed),
            disk: DiskView::new(root.clone()),
            root,
            kind,
            target: config.target,
            globals: config.globals,
            generation: 0,
            last_good: BTreeMap::new(),
            last_good_manifest: None,
            last_strict: None,
            last_broken: BTreeSet::new(),
            fixed: BTreeSet::new(),
        }
    }

    /// The package root, absolute.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Forgets what the session read from these absolute paths.
    pub fn invalidate(&mut self, paths: &[PathBuf]) {
        self.disk.invalidate(paths);
    }

    /// Forgets the walk of `src/` and every file read.
    pub fn invalidate_all(&mut self) {
        self.disk.invalidate_all();
    }

    /// Stats every known file. True when one changed, appeared or went.
    #[must_use]
    pub fn sweep(&mut self) -> bool {
        self.disk.sweep()
    }

    /// A D run was skipped or failed: the next [`Session::analyze_live`]
    /// plans one whatever its strict key.
    pub const fn forget_strict_key(&mut self) {
        self.last_strict = None;
    }

    /// Phase A on the live text of `overlay` and disk, then the Q run
    /// (module doc). An internal error that containment does not hold, or a
    /// panic, in the run comes back in [`Live::failed`], so phase-A results
    /// still publish.
    ///
    /// A live manifest that gives no manifest is replaced by the last good
    /// one for the run, and its own diagnostics go to `phase_a`. With no
    /// last good one, only the open files are parsed (A §2.7).
    ///
    /// In single-file mode the run is `single`'s instead (D-1371).
    ///
    /// # Errors
    ///
    /// An [`Error`] for a package that cannot be read (A §2.7), or a
    /// target it does not have.
    pub fn analyze_live(&mut self, overlay: &Overlay) -> Result<Live, Error> {
        self.generation = self.generation.saturating_add(1);
        if let PackageKind::SingleFile { file, strict } = &self.kind {
            let (file, strict) = (file.clone(), *strict);
            return self.analyze_single(overlay, &file, strict);
        }
        let first = load(&self.root, overlay, &mut self.disk, self.target)?;
        let state = manifest_state(&first);
        if state == ManifestState::Clean {
            self.last_good_manifest = first
                .files
                .first()
                .and_then(|f| first.sources.file(f.file))
                .map(cheby_diag::SourceFile::text_arc);
        }
        let (loaded, live_manifest) = match (state, self.last_good_manifest.clone()) {
            (ManifestState::Unusable, Some(good)) => {
                let live_manifest = manifest_report(&self.root, &first);
                let mut swapped = overlay.clone();
                swapped.files.insert(
                    self.root.join(MANIFEST_FILE),
                    OverlayFile {
                        text: good,
                        version: 0,
                        stamp: 0,
                    },
                );
                let mut loaded = load(&self.root, &swapped, &mut self.disk, self.target)?;
                if let (Some(to), Some(from)) = (loaded.files.first_mut(), first.files.first()) {
                    to.origin = from.origin;
                    to.version = from.version;
                }
                (loaded, live_manifest)
            }
            _ => (first, None),
        };
        if loaded.package.is_none() {
            return self.syntax_only(loaded, overlay, live_manifest);
        }
        self.run_live(state, loaded, overlay, live_manifest)
    }

    /// Steps 3 to 7 of A §2.2 on a loaded package.
    fn run_live(
        &mut self,
        state: ManifestState,
        loaded: Loaded,
        overlay: &Overlay,
        live_manifest: Option<Arc<FileReport>>,
    ) -> Result<Live, Error> {
        let Loaded {
            package,
            mut sources,
            diagnostics: phase,
            files,
            tool,
        } = loaded;
        let paths: BTreeSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let broken: BTreeSet<FileId> = phase
            .iter()
            .filter(|d| d.is_error())
            .filter_map(Diagnostic::primary_span)
            .map(|span| span.file)
            .filter(|&file| files.iter().skip(1).any(|f| f.file == file))
            .collect();
        let Some(package) = package else {
            return Err(Error::new("the package has no manifest".to_owned()));
        };
        let modules: BTreeMap<FileId, ModulePath> = package
            .modules
            .iter()
            .map(|m| (m.source.file, m.path.clone()))
            .collect();
        self.remember(&sources, &files, &broken);
        let broken_now = broken_paths(&files, &broken);
        self.track_broken(&paths, &broken_now);
        let context = RunContext {
            targets: package.manifest.targets,
            manifest_is_last_good: state == ManifestState::Unusable,
        };
        let strict = self.strict_plan(
            state,
            &RunFiles::new(&self.root, &sources, &files),
            Inputs {
                modules: &modules,
                broken: &broken,
                phase: &phase,
                tool: &tool,
            },
        );
        let extras = RunExtras {
            globals: Arc::clone(&self.globals),
            retain: Retain::LIVE,
            cut_cycles: Some(CutCycles {
                prefer_from: newest_edit(&self.root, &package.manifest.name, overlay),
            }),
        };
        let mut diags = phase.clone();
        let ran = catch_unwind(AssertUnwindSafe(|| {
            run_analysis(package, &mut sources, &mut diags, extras)
        }));
        let front = match ran {
            Ok(Ok(front)) => Ok(front),
            Ok(Err(error)) => Err(error.message().to_owned()),
            Err(payload) => Err(panic_message(payload.as_ref())),
        };
        sources.sort(&mut diags);
        let run = RunFiles::new(&self.root, &sources, &files);
        let single = state == ManifestState::Clean
            && !phase.iter().any(Diagnostic::is_error)
            && front.as_ref().is_ok_and(|front| !lost_anything(front));
        let (report, phase_a, plan) = if single {
            self.last_strict = strict.as_ref().map(StrictPlan::key);
            self.fixed.clear();
            let report = Report {
                clean: tool.is_empty() && !diags.iter().any(Diagnostic::is_error),
                inputs: run.inputs(),
                files: run.reports(&diags, |_| true),
                held: BTreeSet::new(),
                tool,
                substituted: false,
            };
            (Some(report), BTreeMap::new(), None)
        } else {
            let mut phase_a = phase_a(&run, phase, &broken);
            if let (Some(report), Some(manifest)) = (live_manifest, files.first()) {
                phase_a.insert(manifest.path.clone(), report);
            }
            (None, phase_a, self.due(strict))
        };
        let baseline = baseline(&run, &diags);
        let (snapshot, failed) = match front {
            Ok(front) => {
                let snapshot = self.snapshot(front, sources, &files, &broken, baseline, context);
                (Some(snapshot), None)
            }
            Err(message) => (None, Some(message)),
        };
        Ok(Live {
            snapshot,
            phase_a,
            report,
            plan,
            files: paths,
            broken: broken_now,
            failed,
        })
    }

    /// `strict` if a D run is due on it: its key was not run before, or a
    /// file left the broken set since the last D run (module doc).
    fn due(&self, strict: Option<StrictPlan>) -> Option<StrictPlan> {
        strict
            .filter(|plan| !self.fixed.is_empty() || Some(plan.key) != self.last_strict)
            .map(|plan| StrictPlan {
                fixed: self.fixed.clone(),
                ..plan
            })
    }

    /// Records this run's broken files. A file of the package that was
    /// broken in the last run and is not now joins [`Session::fixed`]; one
    /// that is broken again, or left the package, leaves it.
    fn track_broken(&mut self, paths: &BTreeSet<PathBuf>, broken: &BTreeSet<PathBuf>) {
        let left: Vec<PathBuf> = self.last_broken.difference(broken).cloned().collect();
        self.fixed.extend(left);
        self.fixed
            .retain(|path| paths.contains(path) && !broken.contains(path));
        self.last_broken.clone_from(broken);
    }

    /// The snapshot of a Q run that ended.
    fn snapshot(
        &self,
        front: Frontend,
        sources: SourceMap,
        files: &[LoadedFile],
        broken: &BTreeSet<FileId>,
        baseline: Vec<BaselineEntry>,
        context: RunContext,
    ) -> Arc<Snapshot> {
        Arc::new(Snapshot::new(Parts {
            id: SnapshotId {
                session: self.id,
                generation: self.generation,
            },
            root: self.root.clone(),
            front,
            versions: files
                .iter()
                .filter_map(|f| Some((f.path.clone(), f.version?)))
                .collect(),
            files: files.iter().map(|f| (f.path.clone(), f.file)).collect(),
            broken: broken_paths(files, broken),
            baseline,
            targets: context.targets,
            manifest_is_last_good: context.manifest_is_last_good,
            single_file: matches!(self.kind, PackageKind::SingleFile { .. }),
            sources,
        }))
    }

    /// The D run that [`Live::plan`] asked for (A §2.2): `run_analysis` on
    /// the plan's strict inputs with `Retain::BUILD` and no cycle cut. The
    /// report leaves out broken files and holds the importers of a stub;
    /// W0005 is dropped when a stub entered the run (D-1357), and a label
    /// or an edit in a file whose strict text is not its live text is
    /// dropped (A §2.3).
    ///
    /// # Errors
    ///
    /// An [`Error`] for a plan of another session, an inputs error as
    /// [`Session::analyze_live`] has, and an internal error or a panic in
    /// the run, which a D run does not contain (D-1359). The strict key is
    /// then not recorded, so the next [`Session::analyze_live`] plans the
    /// run again.
    pub fn analyze_strict(&mut self, plan: StrictPlan) -> Result<Report, Error> {
        if plan.session != self.id {
            return Err(Error::new("a strict plan of another session".to_owned()));
        }
        let StrictLoad {
            package,
            mut sources,
            mut diagnostics,
            mut files,
            live,
            stubs,
        } = load_strict(&self.root, &plan, self.target)?;
        let single = matches!(self.kind, PackageKind::SingleFile { .. });
        if single {
            // The synthetic manifest is no file to publish: a diagnostic
            // with no span goes to the file, as in the Q run (A §6.4).
            files.retain(|f| f.path != plan.manifest.path);
        }
        let extras = RunExtras {
            globals: Arc::clone(&self.globals),
            retain: Retain::BUILD,
            cut_cycles: None,
        };
        let ran = catch_unwind(AssertUnwindSafe(|| {
            run_analysis(package, &mut sources, &mut diagnostics, extras)
        }));
        let front = match ran {
            Ok(Ok(front)) => front,
            Ok(Err(error)) => return Err(error),
            Err(payload) => return Err(Error::new(panic_message(payload.as_ref()))),
        };
        sources.sort(&mut diagnostics);
        let mut held = held(&front, &stubs);
        // A held file that just left the broken set would keep its phase-A
        // list: it gets an empty one instead.
        let fixed_held: BTreeSet<FileId> = files
            .iter()
            .filter(|f| held.contains(&f.file) && plan.fixed.contains(&f.path))
            .map(|f| f.file)
            .collect();
        held.retain(|file| !fixed_held.contains(file));
        let clean = !single
            && !plan.substituted()
            && plan.tool.is_empty()
            && !diagnostics.iter().any(Diagnostic::is_error);
        // Single-file mode never reports W0005 (D-1371).
        if !stubs.is_empty() || single {
            diagnostics.retain(|d| d.code != codes::UNUSED_PACKAGE_ITEM);
        }
        let run = RunFiles {
            root: &self.root,
            sources: &sources,
            files: &files,
            placeable: Some(&live),
        };
        let mut report_files = run.reports(&diagnostics, |f| {
            live.contains(&f.file) && !held.contains(&f.file) && !fixed_held.contains(&f.file)
        });
        report_files.extend(run.reports(&[], |f| fixed_held.contains(&f.file)));
        self.last_strict = Some(plan.key);
        self.fixed.retain(|path| !plan.fixed.contains(path));
        Ok(Report {
            files: report_files,
            held: broken_paths(&files, &held),
            inputs: run.inputs(),
            clean,
            substituted: plan.substituted(),
            tool: plan.tool,
        })
    }

    /// Step 4 of A §2.2: each clean module's live text becomes its last
    /// good text. A broken open document with none takes its disk text if
    /// that is clean (D-1356). Files that left the package are forgotten.
    fn remember(&mut self, sources: &SourceMap, files: &[LoadedFile], broken: &BTreeSet<FileId>) {
        let mut seen = BTreeSet::new();
        for f in files.iter().skip(1) {
            let Some(source) = sources.file(f.file) else {
                continue;
            };
            let display = source.path().to_owned();
            if !broken.contains(&f.file) {
                self.last_good.insert(display.clone(), source.text_arc());
            } else if f.origin == TextOrigin::Editor
                && !self.last_good.contains_key(&display)
                && let Some(text) = self.clean_disk_text(&f.path, &display)
            {
                self.last_good.insert(display.clone(), text);
            }
            seen.insert(display);
        }
        self.last_good.retain(|display, _| seen.contains(display));
    }

    /// The text on disk of the module at `path`, if it has no phase-A
    /// error.
    fn clean_disk_text(&mut self, path: &Path, display: &str) -> Option<Arc<str>> {
        let bytes = self.disk.read(path).ok()?;
        let mut scratch = SourceMap::new();
        let mut diagnostics = Vec::new();
        let parsed = parse_file(&mut scratch, display, bytes.to_vec(), &mut diagnostics).ok()?;
        if diagnostics.iter().any(Diagnostic::is_error) {
            return None;
        }
        scratch
            .file(parsed.file)
            .map(cheby_diag::SourceFile::text_arc)
    }

    /// The strict inputs of this run, with their key (A §2.1). `None` when
    /// there is no manifest to run on: the live one has errors and none was
    /// ever clean (example 030).
    fn strict_plan(
        &self,
        state: ManifestState,
        run: &RunFiles<'_>,
        inputs: Inputs<'_>,
    ) -> Option<StrictPlan> {
        let manifest_file = run.files.first()?;
        let (text, source) = if state == ManifestState::Clean {
            (
                run.sources.file(manifest_file.file)?.text_arc(),
                Source::Live,
            )
        } else {
            (self.last_good_manifest.clone()?, Source::LastGood)
        };
        let manifest = StrictFile {
            path: manifest_file.path.clone(),
            display: MANIFEST_FILE.to_owned(),
            text,
            source,
            origin: manifest_file.origin,
            version: manifest_file.version,
        };
        let mut modules = Vec::new();
        for f in run.files.iter().skip(1) {
            let (Some(path), Some(file)) = (inputs.modules.get(&f.file), run.sources.file(f.file))
            else {
                continue;
            };
            let display = file.path().to_owned();
            let (text, source) = if inputs.broken.contains(&f.file) {
                match self.last_good.get(&display) {
                    Some(text) => (Arc::clone(text), Source::LastGood),
                    None => (Arc::from(""), Source::Stub),
                }
            } else {
                (file.text_arc(), Source::Live)
            };
            modules.push((
                path.clone(),
                StrictFile {
                    path: f.path.clone(),
                    display,
                    text,
                    source,
                    origin: f.origin,
                    version: f.version,
                },
            ));
        }
        let walk: Vec<Diagnostic> = inputs
            .phase
            .iter()
            .filter(|d| d.code == codes::INVALID_MODULE_NAME && d.primary_span().is_none())
            .cloned()
            .collect();
        let mut key = InputsKey::new();
        key.str(&self.target.map_or_else(String::new, |t| t.to_string()));
        key.file(Path::new(MANIFEST_FILE), &manifest.text);
        for (_, m) in &modules {
            key.file(Path::new(&m.display), &m.text)
                .bytes(&[u8::from(m.source == Source::Stub)]);
        }
        for d in &walk {
            key.str(&d.message);
        }
        for message in inputs.tool {
            key.str(message);
        }
        Some(StrictPlan {
            session: self.id,
            key: key.finish(),
            manifest,
            modules,
            walk,
            tool: inputs.tool.to_vec(),
            fixed: BTreeSet::new(),
        })
    }

    /// The live manifest gives no manifest and none was ever good: only
    /// `cheby.toml` and the open documents under `src/` are read, each with
    /// its own phase-A list from a parse (A §2.7).
    fn syntax_only(
        &mut self,
        loaded: Loaded,
        overlay: &Overlay,
        live_manifest: Option<Arc<FileReport>>,
    ) -> Result<Live, Error> {
        let Loaded {
            mut sources,
            diagnostics: mut phase,
            mut files,
            ..
        } = loaded;
        let src = self.root.join(cheby_package::SOURCE_DIR);
        for (path, open) in &overlay.files {
            if !path.starts_with(&src) || path.extension().is_none_or(|e| e != "cheby") {
                continue;
            }
            let display = cheby_diag::display_path(path, &self.root);
            let parsed = parse_file(
                &mut sources,
                &display,
                open.text.as_bytes().to_vec(),
                &mut phase,
            )?;
            files.push(LoadedFile {
                path: path.clone(),
                file: parsed.file,
                origin: TextOrigin::Editor,
                version: Some(open.version),
            });
        }
        sources.sort(&mut phase);
        let broken: BTreeSet<FileId> = phase
            .iter()
            .filter(|d| d.is_error())
            .filter_map(Diagnostic::primary_span)
            .map(|span| span.file)
            .filter(|&file| files.iter().skip(1).any(|f| f.file == file))
            .collect();
        let broken = broken_paths(&files, &broken);
        let paths: BTreeSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        self.track_broken(&paths, &broken);
        let run = RunFiles::new(&self.root, &sources, &files);
        let mut phase_a = run.reports(&phase, |_| true);
        if let (Some(report), Some(manifest)) = (live_manifest, files.first()) {
            phase_a.insert(manifest.path.clone(), report);
        }
        Ok(Live {
            snapshot: None,
            phase_a,
            report: None,
            plan: None,
            files: paths,
            broken,
            failed: None,
        })
    }
}

/// What [`Session::strict_plan`] reads of a load besides its files.
#[derive(Clone, Copy)]
struct Inputs<'a> {
    modules: &'a BTreeMap<FileId, ModulePath>,
    broken: &'a BTreeSet<FileId>,
    phase: &'a [Diagnostic],
    tool: &'a [String],
}

/// The state of the manifest that `loaded` read: an error of its own is
/// one in `cheby.toml` or one with no span other than E0037.
fn manifest_state(loaded: &Loaded) -> ManifestState {
    if loaded.package.is_none() {
        return ManifestState::Unusable;
    }
    let manifest = loaded.files.first().map(|f| f.file);
    let errors = loaded
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && d.code != codes::INVALID_MODULE_NAME)
        .any(|d| {
            d.primary_span()
                .is_none_or(|span| Some(span.file) == manifest)
        });
    if errors {
        ManifestState::Usable
    } else {
        ManifestState::Clean
    }
}

/// `cheby.toml`'s report of `loaded`, with its phase-A diagnostics.
fn manifest_report(root: &Path, loaded: &Loaded) -> Option<Arc<FileReport>> {
    let manifest = loaded.files.first()?;
    let mut phase = loaded.diagnostics.clone();
    loaded.sources.sort(&mut phase);
    RunFiles::new(root, &loaded.sources, &loaded.files)
        .reports(&phase, |f| f.file == manifest.file)
        .remove(&manifest.path)
}

/// Loads a plan's strict inputs as [`load`] loads live ones, with no read
/// from disk but the manifest's `[native]` check.
fn load_strict(
    root: &Path,
    plan: &StrictPlan,
    target: Option<Target>,
) -> Result<StrictLoad, Error> {
    let mut sources = SourceMap::new();
    let mut diagnostics = plan.walk.clone();
    let text = &plan.manifest.text;
    let file = sources
        .add_file(MANIFEST_FILE, Arc::clone(text))
        .map_err(|_| Error::from(PackageError::too_large(MANIFEST_FILE)))?;
    let mut manifest = Manifest::parse_at(file, text, root, &mut diagnostics)
        .ok_or_else(|| Error::new(format!("the strict run's {MANIFEST_FILE} does not parse")))?;
    if let Some(target) = target {
        if !manifest.targets.contains(target) {
            return Err(Error::new(format!(
                "the package does not target `{target}`"
            )));
        }
        manifest.targets = Targets::only(target);
    }
    let mut files = vec![LoadedFile {
        path: plan.manifest.path.clone(),
        file,
        origin: plan.manifest.origin,
        version: plan.manifest.version,
    }];
    let mut live = BTreeSet::new();
    let mut stubs = BTreeSet::new();
    if plan.manifest.source == Source::Live {
        live.insert(file);
    }
    let mut modules = Vec::with_capacity(plan.modules.len());
    for (path, m) in &plan.modules {
        let mut own = Vec::new();
        let source = parse_file(
            &mut sources,
            &m.display,
            m.text.as_bytes().to_vec(),
            &mut own,
        )?;
        diagnostics.extend(own.iter().cloned());
        match m.source {
            Source::Live => live.insert(source.file),
            Source::Stub => stubs.insert(source.file),
            Source::LastGood => false,
        };
        files.push(LoadedFile {
            path: m.path.clone(),
            file: source.file,
            origin: m.origin,
            version: m.version,
        });
        modules.push(Module {
            path: path.clone(),
            source,
            hashes: SourceHashes::of(m.text.as_bytes()),
            parsed: true,
            syntax_diagnostics: own,
        });
    }
    Ok(StrictLoad {
        package: Package {
            root: root.to_path_buf(),
            manifest,
            modules,
        },
        sources,
        diagnostics,
        files,
        live,
        stubs,
    })
}

/// The files of the modules that import a module of `stubs`, directly or
/// through others, the stubs left out (D-1357).
fn held(front: &Frontend, stubs: &BTreeSet<FileId>) -> BTreeSet<FileId> {
    let roots: BTreeSet<ModuleId> = front
        .modules
        .iter()
        .filter(|(_, info)| info.file.is_some_and(|file| stubs.contains(&file)))
        .map(|(id, _)| id)
        .collect();
    let mut reach = roots.clone();
    loop {
        let mut grew = false;
        for (from, targets) in front.import_targets.iter().enumerate() {
            let Some(from) = u32::try_from(from).ok().map(ModuleId::from_u32) else {
                continue;
            };
            if !reach.contains(&from)
                && targets
                    .iter()
                    .filter_map(|t| t.module())
                    .any(|to| reach.contains(&to))
            {
                reach.insert(from);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    reach
        .difference(&roots)
        .filter_map(|&id| front.modules.get(id)?.file)
        .collect()
}

/// `cheby.toml` and every file of `broken`, with their phase-A
/// diagnostics, sorted. Diagnostics with no span go to `cheby.toml`.
fn phase_a(
    run: &RunFiles<'_>,
    mut phase: Vec<Diagnostic>,
    broken: &BTreeSet<FileId>,
) -> BTreeMap<PathBuf, Arc<FileReport>> {
    run.sources.sort(&mut phase);
    let manifest = run.files.first().map(|f| f.file);
    run.reports(&phase, |f| {
        Some(f.file) == manifest || broken.contains(&f.file)
    })
}

/// The paths of the files of `set`.
fn broken_paths(files: &[LoadedFile], set: &BTreeSet<FileId>) -> BTreeSet<PathBuf> {
    files
        .iter()
        .filter(|f| set.contains(&f.file))
        .map(|f| f.path.clone())
        .collect()
}

/// Whether a run lost a module, a body or a pass, or dropped a body with no
/// diagnostic: its diagnostics are then not the check's (plan §1.2).
fn lost_anything(front: &Frontend) -> bool {
    !front.lost.is_empty()
        || front.silent_drop
        || !front.cut_imports.is_empty()
        || front.modules.iter().any(|(id, _)| {
            front
                .checked_quiet(id)
                .and_then(|c| c.facts.as_ref())
                .is_some_and(|f| !f.lost.is_empty() || !f.lost_passes.is_empty())
        })
}

/// The module of the newest edit in `overlay`, by stamp, among the open
/// documents that are modules of the package: the cycle cut prefers an
/// import out of it (D-1358).
fn newest_edit(root: &Path, package: &str, overlay: &Overlay) -> Option<ModulePath> {
    overlay
        .files
        .iter()
        .filter_map(|(path, file)| {
            cheby_package::module_file(root, package, path).map(|m| (file.stamp, m.path))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// The run's diagnostics as a sorted multiset of code, file and message.
fn baseline(run: &RunFiles<'_>, diags: &[Diagnostic]) -> Vec<BaselineEntry> {
    let mut entries: Vec<BaselineEntry> = diags
        .iter()
        .map(|d| {
            let path = d.primary_span().and_then(|span| {
                run.files
                    .iter()
                    .find(|f| f.file == span.file)
                    .map(|f| f.path.clone())
                    .or_else(|| run.sources.file(span.file).map(|s| PathBuf::from(s.path())))
            });
            (d.code, path, d.message.clone())
        })
        .collect();
    entries.sort();
    entries
}

/// The message of a panic's payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message");
    format!("the analysis panicked: {text}")
}
