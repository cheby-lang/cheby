//! `Session`: the analysis of one package over a changing [`Overlay`].
//!
//! [`Session::analyze_live`] runs phase A on the live text, then the Q run:
//! `run_analysis` with `Retain::LIVE` and a cycle cut (D-1361, D-1358).
//! Its result is a [`Live`]: a [`Snapshot`] for queries, and what to
//! publish, all of it plain data (D-1364).
//!
//! What a `Live` publishes:
//!
//! - Nothing broken, no cut import and nothing lost: the Q run is the
//!   check, so [`Live::report`] holds every file's diagnostics, as
//!   `cheby check` reports them.
//! - Otherwise [`Live::phase_a`] holds each broken file's phase-A
//!   diagnostics and `cheby.toml`'s, and the rest waits for the strict run
//!   of the two-track algorithm (A §2.2), which this wave does not have
//!   yet. Tolerant-run diagnostics never enter `phase_a` (plan §1.2).
//!
//! An internal error or a panic in the Q run ends that run only: it comes
//! back in [`Live::failed`], with no snapshot, and the session runs again on
//! the next call (R-14).

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::Globals;
use cheby_package::{ModulePath, Target};

use super::inputs::{DiskView, Loaded, LoadedFile, Overlay, TextOrigin, load};
use super::plain::{InputsKey, Placed, PlainDiagnostic, exact_key, plain};
use super::snapshot::{BaselineEntry, Parts, Snapshot, SnapshotId};
use crate::Error;
use crate::cache::Hash128;
use crate::frontend::{CutCycles, Frontend, Retain, RunExtras, run_analysis};

/// What a session analyzes and with what.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The package root, the directory of `cheby.toml`.
    pub root: PathBuf,
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
    target: Option<Target>,
    globals: Arc<Globals>,
    disk: DiskView,
    generation: u64,
}

/// What [`Session::analyze_live`] gives.
#[derive(Debug)]
pub struct Live {
    /// For queries. `None` when the manifest cannot be used, or the run
    /// failed ([`Live::failed`]).
    pub snapshot: Option<Arc<Snapshot>>,
    /// When the run is not the check: each file with phase-A errors and
    /// `cheby.toml`, with their phase-A diagnostics. Publish these now.
    pub phase_a: BTreeMap<PathBuf, Arc<FileReport>>,
    /// `Some`: nothing was broken, cut or lost, so this run is the check.
    pub report: Option<Report>,
    /// Every file of the package in this run, `cheby.toml` included. A
    /// published path outside this set has left the package (A §8.4).
    pub files: BTreeSet<PathBuf>,
    /// The run ended in an internal error or a panic: `snapshot` is `None`,
    /// `phase_a` is good.
    pub failed: Option<String>,
}

/// What to publish for a package. All of it is free of generation handles.
#[derive(Debug)]
pub struct Report {
    /// Every package file with its diagnostics, an empty list included.
    pub files: BTreeMap<PathBuf, Arc<FileReport>>,
    /// Errors with no location, for `window/showMessage` (A §2.7).
    pub tool: Vec<String>,
    /// The key of the manifest and every text the run read, in path order.
    pub inputs: Hash128,
    /// No error in any file and none in `tool`, so the package is one
    /// that `cheby check` accepts: the child may run (A §11).
    pub clean: bool,
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

/// The source of session ids, unique in the process.
static SESSIONS: AtomicU32 = AtomicU32::new(0);

/// What one run read, as the session keeps it while it publishes.
struct RunFiles<'a> {
    root: &'a Path,
    sources: &'a SourceMap,
    files: &'a [LoadedFile],
}

impl<'a> RunFiles<'a> {
    /// The files of a run over the package at `root`.
    const fn new(root: &'a Path, sources: &'a SourceMap, files: &'a [LoadedFile]) -> Self {
        Self {
            root,
            sources,
            files,
        }
    }

    /// The package file `file` with its live text: every package file of a
    /// Q run is placed, since it ran on the live text. Std files are not.
    fn place(&self, file: FileId) -> Option<Placed<'_>> {
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

impl Session {
    /// A session over the package at `config.root`, which is made
    /// absolute. Nothing is read yet.
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        let root = std::path::absolute(&config.root).unwrap_or(config.root);
        Self {
            id: SESSIONS.fetch_add(1, Ordering::Relaxed),
            disk: DiskView::new(root.clone()),
            root,
            target: config.target,
            globals: config.globals,
            generation: 0,
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

    /// Phase A on the live text of `overlay` and disk, then the Q run
    /// (module doc). An internal error or a panic in the run comes back in
    /// [`Live::failed`], so phase-A results still publish.
    ///
    /// # Errors
    ///
    /// An [`Error`] for a package that cannot be read (A §2.7), or a
    /// target it does not have.
    pub fn analyze_live(&mut self, overlay: &Overlay) -> Result<Live, Error> {
        self.generation = self.generation.saturating_add(1);
        let Loaded {
            package,
            mut sources,
            diagnostics: phase,
            files,
            tool,
        } = load(&self.root, overlay, &mut self.disk, self.target)?;
        let paths: BTreeSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let broken: BTreeSet<FileId> = phase
            .iter()
            .filter(|d| d.is_error())
            .filter_map(Diagnostic::primary_span)
            .map(|span| span.file)
            .filter(|&file| files.iter().skip(1).any(|f| f.file == file))
            .collect();
        let Some(package) = package else {
            let run = RunFiles::new(&self.root, &sources, &files);
            return Ok(Live {
                snapshot: None,
                phase_a: phase_a(&run, phase, &broken),
                report: None,
                files: paths,
                failed: None,
            });
        };
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
        let single = !phase.iter().any(Diagnostic::is_error)
            && front.as_ref().is_ok_and(|front| !lost_anything(front));
        let report = single.then(|| {
            let files = run.reports(&diags, |_| true);
            Report {
                clean: tool.is_empty() && !diags.iter().any(Diagnostic::is_error),
                inputs: run.inputs(),
                tool,
                files,
            }
        });
        let phase_a = if single {
            BTreeMap::new()
        } else {
            phase_a(&run, phase, &broken)
        };
        let baseline = baseline(&run, &diags);
        let (snapshot, failed) = match front {
            Ok(front) => {
                let parts = Parts {
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
                    broken: broken_paths(&files, &broken),
                    baseline,
                    sources,
                };
                (Some(Arc::new(Snapshot::new(parts))), None)
            }
            Err(message) => (None, Some(message)),
        };
        Ok(Live {
            snapshot,
            phase_a,
            report,
            files: paths,
            failed,
        })
    }
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

/// The paths of the files of `broken`.
fn broken_paths(files: &[LoadedFile], broken: &BTreeSet<FileId>) -> BTreeSet<PathBuf> {
    files
        .iter()
        .filter(|f| broken.contains(&f.file))
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
