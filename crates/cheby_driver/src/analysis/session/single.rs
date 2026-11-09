//! Single-file mode (D-1371, A §6.4): one file that is no module of any
//! package, analyzed as the only module of a package with a synthetic
//! manifest, as the REPL checks its entries.
//!
//! The manifest is the REPL's synthetic one, named `script`
//! (`crate::session::synthetic_manifest`), in a source file that no report
//! names, and the module is `script`, displayed by the file's name.
//! [`Live::files`] holds the file alone. Constants are never evaluated,
//! since no server run evaluates them and single-file mode has no child.
//!
//! - **Strict** (no `cheby.toml` above the file): the two-track rules with
//!   one module. Clean, and nothing lost or cut, the Q run is the check:
//!   its report holds all of its diagnostics but W0005, which is about
//!   uses elsewhere in a package. Broken, the file's phase-A list is in
//!   [`Live::phase_a`], and no D run is planned, since a D run reports no
//!   broken file and there is no other. Clean but not the check, a D run
//!   on the live text is planned, as for a package, and its report drops
//!   W0005 too. So no last good text is ever read.
//! - **Inside a package** (the file is not one of its modules): phase A is
//!   the check, since the file's imports of the package's modules cannot
//!   resolve. The report holds its phase-A list, empty when clean, and no
//!   D run is planned.
//!
//! In both, the Q run on the live text keeps a snapshot for queries. No
//! report is ever [`Report::clean`]: `cheby check` refuses such a file,
//! and single-file mode has no child (A §6.4).

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, codes};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, Targets};

use super::super::inputs::{FileText, LoadedFile, Overlay, TextOrigin, load_module};
use super::super::plain::InputsKey;
use super::{
    Live, Report, RunContext, RunFiles, Session, Source, StrictFile, StrictPlan, baseline,
    broken_paths, lost_anything, panic_message,
};
use crate::frontend::{CutCycles, Retain, RunExtras, run_analysis};
use crate::session::{synthetic_manifest, synthetic_manifest_text};
use crate::{Error, Package};

/// The synthetic package's name, which is also its one module's path
/// (A §6.4).
const PACKAGE: &str = "script";

/// What phase A of single-file mode loads: the package of one module,
/// with the run's source map, its phase-A diagnostics, and the file alone,
/// since the manifest is no file to publish.
struct SingleLoad {
    sources: SourceMap,
    phase: Vec<Diagnostic>,
    package: Package,
    files: Vec<LoadedFile>,
}

impl Session {
    /// Phase A and the Q run of single-file mode (module doc).
    ///
    /// # Errors
    ///
    /// An [`Error`] for a file that is not open and cannot be read, or a
    /// target the synthetic manifest does not have.
    pub(super) fn analyze_single(
        &mut self,
        overlay: &Overlay,
        file: &Path,
        strict: bool,
    ) -> Result<Live, Error> {
        let SingleLoad {
            mut sources,
            mut phase,
            package,
            files,
        } = self.load_single(overlay, file)?;
        let broken: BTreeSet<FileId> = if phase.iter().any(Diagnostic::is_error) {
            files.iter().map(|f| f.file).collect()
        } else {
            BTreeSet::new()
        };
        let paths = BTreeSet::from([file.to_path_buf()]);
        let broken_now = broken_paths(&files, &broken);
        self.track_broken(&paths, &broken_now);
        let context = RunContext {
            targets: package.manifest.targets,
            manifest_is_last_good: false,
        };
        // The plan's texts are taken before the run, which owns the module.
        let strict_plan = (strict && broken.is_empty())
            .then(|| self.single_plan(&sources, &files))
            .flatten();
        let extras = RunExtras {
            globals: Arc::clone(&self.globals),
            retain: Retain::LIVE,
            cut_cycles: Some(CutCycles {
                prefer_from: Some(ModulePath::root(PACKAGE)),
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
        if strict {
            diags.retain(|d| d.code != codes::UNUSED_PACKAGE_ITEM);
        }
        sources.sort(&mut diags);
        sources.sort(&mut phase);
        let run = RunFiles::new(&self.root, &sources, &files);
        let checked = front.as_ref().is_ok_and(|front| !lost_anything(front));
        let (report, phase_a, plan) = if !strict {
            // Inside a package, phase A is the check (D-1371).
            let report = Report {
                clean: false,
                inputs: run.inputs(),
                files: run.reports(&phase, |_| true),
                held: BTreeSet::new(),
                tool: Vec::new(),
                substituted: false,
            };
            self.fixed.clear();
            (Some(report), BTreeMap::new(), None)
        } else if !broken.is_empty() {
            (None, run.reports(&phase, |_| true), None)
        } else if checked {
            self.last_strict = strict_plan.as_ref().map(StrictPlan::key);
            self.fixed.clear();
            let report = Report {
                clean: false,
                inputs: run.inputs(),
                files: run.reports(&diags, |_| true),
                held: BTreeSet::new(),
                tool: Vec::new(),
                substituted: false,
            };
            (Some(report), BTreeMap::new(), None)
        } else {
            (None, BTreeMap::new(), self.due(strict_plan))
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

    /// Phase A of single-file mode: the synthetic manifest, then `file`'s
    /// text, from `overlay` when it is open, parsed and validated as the
    /// one module `script`.
    fn load_single(&mut self, overlay: &Overlay, file: &Path) -> Result<SingleLoad, Error> {
        let mut sources = SourceMap::new();
        let mut phase = Vec::new();
        let manifest = self.manifest(&mut sources, &mut phase)?;
        let text = FileText::of(file, overlay, &mut self.disk)?;
        let display = cheby_diag::display_path(file, &self.root);
        let module = load_module(
            &mut sources,
            ModulePath::root(PACKAGE),
            &display,
            &text,
            &mut phase,
        )?;
        let files = vec![LoadedFile {
            path: file.to_path_buf(),
            file: module.source.file,
            origin: text.origin,
            version: text.version,
        }];
        Ok(SingleLoad {
            sources,
            phase,
            package: Package {
                root: self.root.clone(),
                manifest,
                modules: vec![module],
            },
            files,
        })
    }

    /// The synthetic manifest, in `sources`, narrowed to the session's
    /// target as `--target` narrows a manifest.
    fn manifest(
        &self,
        sources: &mut SourceMap,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Manifest, Error> {
        let mut manifest = synthetic_manifest(PACKAGE, sources, diagnostics)?;
        if let Some(target) = self.target {
            if !manifest.targets.contains(target) {
                return Err(Error::new(format!(
                    "a single file does not target `{target}`"
                )));
            }
            manifest.targets = Targets::only(target);
        }
        Ok(manifest)
    }

    /// The D run's inputs in strict single-file mode: the synthetic
    /// manifest and the file's live text, which is clean. Its manifest
    /// path names no file the report keeps (`analyze_strict`).
    fn single_plan(&self, sources: &SourceMap, files: &[LoadedFile]) -> Option<StrictPlan> {
        let loaded = files.first()?;
        let source = sources.file(loaded.file)?;
        let manifest = StrictFile {
            path: self.root.join(MANIFEST_FILE),
            display: MANIFEST_FILE.to_owned(),
            text: Arc::from(synthetic_manifest_text(PACKAGE)),
            source: Source::Live,
            origin: TextOrigin::Disk,
            version: None,
        };
        let file = StrictFile {
            path: loaded.path.clone(),
            display: source.path().to_owned(),
            text: source.text_arc(),
            source: Source::Live,
            origin: loaded.origin,
            version: loaded.version,
        };
        let mut key = InputsKey::new();
        key.str(&self.target.map_or_else(String::new, |t| t.to_string()));
        key.file(Path::new(MANIFEST_FILE), &manifest.text);
        key.file(Path::new(&file.display), &file.text);
        Some(StrictPlan {
            session: self.id,
            key: key.finish(),
            manifest,
            modules: vec![(ModulePath::root(PACKAGE), file)],
            walk: Vec::new(),
            tool: Vec::new(),
            fixed: BTreeSet::new(),
        })
    }
}
