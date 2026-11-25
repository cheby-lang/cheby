//! A std checkout's session (D-1699, D-1700): the `std/` of a checkout
//! of this repository, checked as package `std` from the checkout's own
//! texts, as `cargo xtask test-std` checks it.
//!
//! - **Modules.** Every `.cheby` file under `<checkout>/std/` whose path
//!   names a module, `std/json/tree.cheby` as `std::json::tree`, each from
//!   its open document or else from disk, an unsaved buffer included. A
//!   path that names no module, `std/main.cheby`, or a module the binary
//!   stages or plans (D-486, D-451) makes a loose file: it gets its
//!   phase-A diagnostics only, always in [`Live::phase_a`], as D-1371 gives
//!   a file of a package that is not a module.
//! - **The run.** The modules are preloaded as `PackageId::STD`, each with
//!   its tree and its grammar errors, under a synthetic root that imports
//!   every one of them, as `StdBase` does ([`CheckoutStd`]). A std import
//!   resolves among them only, so a module added in the checkout resolves
//!   and one removed is E0039. The two tracks of D-1347 work for them as for
//!   a package's modules: a broken module is no internal error, its phase-A
//!   list is published, and the D run reads its last good text.
//! - **Test bodies.** Those of a module with an open document are checked,
//!   the others are skipped (D-1701), so an open or a close changes the
//!   run's input. A D run reads the set from its plan's origins.
//! - **The synthetic root** is no file of the session: it has no path, no
//!   location names it, its module is not one of the snapshot's own, and
//!   its diagnostics are dropped from every run (`drop_root`).
//!
//! The checkout's texts reach no other session: `StdBase` and every
//! package run keep the binary's std (D-1372, D-1348). No constant is
//! evaluated (D-1491).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::Ice;
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, Targets};

use super::super::inputs::{FileText, LoadedFile, Overlay, TextOrigin, load_module};
use super::super::plain::InputsKey;
use super::{
    CheckoutStd, Live, Report, RunContext, RunFiles, Session, Source, StrictFile, StrictLoad,
    StrictPlan, baseline, broken_paths, lost_anything, panic_message,
};
use crate::frontend::{CutCycles, Retain, RunExtras, run_analysis_checkout};
use crate::session::{synthetic_manifest, synthetic_manifest_text};
use crate::{Error, Module, Package, SourceHashes, parse_file, std_lib};

/// The directory of a checkout that holds std's sources (D-1699).
pub(crate) const STD_DIR: &str = "std";

/// The synthetic package of a checkout's run, whose one module imports
/// every module of the checkout. Its name is no std module's.
const PACKAGE: &str = "std_checkout";

/// How many directories deep the walk of `std/` goes. A module path is at
/// most 64 directories deep (`cheby_package::discover`); a deeper file is
/// no module, and the bound stops a cycle of symbolic links.
const MAX_WALK_DEPTH: usize = 66;

/// The module that `file`, an absolute path, holds in the checkout at
/// `checkout`: `<checkout>/std/json/tree.cheby` holds `std::json::tree`
/// (D-1700). `None` for a file outside `std/`, a path that names no module
/// by package rules (`cheby_package::module_file`), `std/main.cheby`,
/// which would name `std` itself, and a module that the binary stages or
/// plans, which stays E0038 (D-486, D-451). The answer is lexical, so an
/// unsaved buffer has one.
#[must_use]
pub(crate) fn checkout_module(checkout: &Path, file: &Path) -> Option<ModulePath> {
    let relative = file.strip_prefix(checkout.join(STD_DIR)).ok()?;
    // `module_file` names the files under a package's `src/`: `std/` is
    // `src/` of a package named `std`.
    let as_source = Path::new(cheby_package::SOURCE_DIR).join(relative);
    let module = cheby_package::module_file(Path::new(""), "std", &as_source)?;
    if module.path.is_root() {
        return None;
    }
    let under = under_std(&module.path);
    let unshipped = std_lib::is_planned(&under)
        || std_lib::STAGED_STD_MODULES
            .iter()
            .any(|staged| staged.path == under);
    (!unshipped).then_some(module.path)
}

/// The path under `std` of the std module `path`, as `CheckoutStd` keys
/// its modules: `json::tree` for `std::json::tree`.
pub(crate) fn under_std(path: &ModulePath) -> String {
    path.segments().get(1..).unwrap_or_default().join("::")
}

/// Drops the diagnostics whose primary label is in the synthetic root's
/// file `root`: its unused imports, W0003, would otherwise reach the first
/// file of the session (D-1700).
pub(crate) fn drop_root(diagnostics: &mut Vec<Diagnostic>, root: FileId) {
    diagnostics.retain(|d| d.primary_span().is_none_or(|span| span.file != root));
}

/// The synthetic root package of a checkout's run: one module, with no
/// file on disk, that imports each module of `modules` under an alias,
/// so two modules with one last segment bind no name twice. `root` is the
/// checkout. Gives the package and the root's file.
///
/// # Errors
///
/// An [`Error`] when the root's text does not parse, an internal error.
pub(crate) fn checkout_root(
    root: &Path,
    manifest: Manifest,
    modules: &[ModulePath],
    sources: &mut SourceMap,
) -> Result<(Package, FileId), Error> {
    let mut text = String::from("//! Imports every module of the std checkout (D-1700).\n\n");
    for (n, module) in modules.iter().enumerate() {
        let _ = writeln!(text, "import {module} as m{n}");
    }
    let hashes = SourceHashes::of(text.as_bytes());
    let mut own = Vec::new();
    let source = parse_file(
        sources,
        &format!("<{PACKAGE}>/main.cheby"),
        text.into_bytes(),
        &mut own,
    )?;
    if source.has_syntax_errors {
        return Err(Ice::new("a std checkout's root module does not parse").into());
    }
    let file = source.file;
    Ok((
        Package {
            dependencies: crate::dependencies::Dependencies::default(),
            root: root.to_path_buf(),
            manifest,
            modules: vec![Module {
                path: ModulePath::root(PACKAGE),
                source,
                hashes,
                parsed: true,
                syntax_diagnostics: Vec::new(),
            }],
        },
        file,
    ))
}

/// The `.cheby` files under `dir`, a checkout's `std/`, by absolute path
/// in path order. Names that start with `.` are skipped, as discovery
/// skips them; every other name is kept, since a file whose path names no
/// module still gets its phase-A diagnostics.
///
/// # Errors
///
/// An [`Error`] when `dir` is not a directory or one cannot be read.
pub(crate) fn walk_std(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    if !dir.is_dir() {
        return Err(Error::new(format!(
            "the std checkout has no `{STD_DIR}` directory"
        )));
    }
    let mut found = Vec::new();
    walk_dir(dir, 0, &mut found)?;
    found.sort();
    Ok(found)
}

/// One directory of [`walk_std`], `depth` directories under `std/`.
fn walk_dir(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<(), Error> {
    if depth > MAX_WALK_DEPTH {
        return Ok(());
    }
    let io = |e: std::io::Error| Error::new(format!("{}: {e}", dir.display()));
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io)? {
        names.push(entry.map_err(io)?.file_name());
    }
    names.sort();
    for name in names {
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = dir.join(&name);
        // `metadata` follows symbolic links, as discovery does; the depth
        // bounds a cycle. A dangling link is skipped.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk_dir(&path, depth.saturating_add(1), found)?;
        } else if path.extension().is_some_and(|e| e == "cheby") {
            found.push(path);
        }
    }
    Ok(())
}

/// The module of the newest edit in `overlay` among the checkout's open
/// modules: the cycle cut prefers an import out of it (D-1358).
fn newest_edit(checkout: &Path, overlay: &Overlay) -> Option<ModulePath> {
    overlay
        .files
        .iter()
        .filter_map(|(path, file)| checkout_module(checkout, path).map(|m| (file.stamp, m)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// What phase A of a checkout's run loads.
struct CheckoutLoad {
    sources: SourceMap,
    /// The modules' phase-A diagnostics, unsorted.
    phase: Vec<Diagnostic>,
    /// The loose files' phase-A diagnostics, unsorted.
    loose_phase: Vec<Diagnostic>,
    /// The modules' files in display order, then the loose files in
    /// display order.
    files: Vec<LoadedFile>,
    /// Each module's file, with its path.
    modules: BTreeMap<FileId, ModulePath>,
    /// The run's std, every module of `modules`.
    std: CheckoutStd,
    /// The modules with an open document, whose test bodies are checked.
    tested: BTreeSet<ModulePath>,
    /// The synthetic root package and its file.
    package: Package,
    root_file: FileId,
}

impl Session {
    /// The `.cheby` files under the checkout's `std/` on disk, walked once
    /// until `invalidate` or `sweep` forgets them.
    fn std_files(&mut self) -> Result<Vec<PathBuf>, Error> {
        if let Some(files) = &self.std_walk {
            return Ok(files.clone());
        }
        let files = walk_std(&self.root.join(STD_DIR))?;
        self.std_walk = Some(files.clone());
        Ok(files)
    }

    /// Walks a checkout's `std/` again, if it was walked. True when a file
    /// came or went; the walk is then forgotten. Always false in any other
    /// session, which never walks `std/`.
    pub(super) fn sweep_std(&mut self) -> bool {
        let Some(known) = &self.std_walk else {
            return false;
        };
        let now = walk_std(&self.root.join(STD_DIR)).ok();
        if now.as_ref() == Some(known) {
            return false;
        }
        self.std_walk = None;
        true
    }

    /// The synthetic manifest of a checkout's run, narrowed to the
    /// session's target as `--target` narrows a manifest.
    fn checkout_manifest(
        &self,
        sources: &mut SourceMap,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Manifest, Error> {
        let mut manifest = synthetic_manifest(PACKAGE, sources, diagnostics)?;
        if let Some(target) = self.target {
            if !manifest.targets.contains(target) {
                return Err(Error::new(format!(
                    "a std checkout does not target `{target}`"
                )));
            }
            manifest.targets = Targets::only(target);
        }
        Ok(manifest)
    }

    /// Phase A of a checkout's run (module doc): every `.cheby` file under
    /// `std/`, on disk or open, from its open document or else from disk.
    fn load_checkout(&mut self, overlay: &Overlay) -> Result<CheckoutLoad, Error> {
        let std_dir = self.root.join(STD_DIR);
        let mut listed: BTreeMap<String, PathBuf> = BTreeMap::new();
        for path in self.std_files()? {
            listed.insert(cheby_diag::display_path(&path, &self.root), path);
        }
        // An open document under `std/` is a file of the run before it
        // exists on disk (D-1707).
        for path in overlay.files.keys() {
            if path.starts_with(&std_dir) && path.extension().is_some_and(|e| e == "cheby") {
                listed.insert(cheby_diag::display_path(path, &self.root), path.clone());
            }
        }
        let mut sources = SourceMap::new();
        let mut phase = Vec::new();
        let mut loose_phase = Vec::new();
        let manifest = self.checkout_manifest(&mut sources, &mut phase)?;
        let mut files = Vec::new();
        let mut loose = Vec::new();
        let mut modules = BTreeMap::new();
        let mut std = CheckoutStd {
            modules: BTreeMap::new(),
            tested: BTreeSet::new(),
        };
        let mut tested = BTreeSet::new();
        for (display, path) in listed {
            let text = FileText::of(&path, overlay, &mut self.disk)?;
            let Some(module_path) = checkout_module(&self.root, &path) else {
                let parsed = parse_file(
                    &mut sources,
                    &display,
                    text.bytes.to_vec(),
                    &mut loose_phase,
                )?;
                loose.push(LoadedFile {
                    path,
                    file: parsed.file,
                    origin: text.origin,
                    version: text.version,
                });
                continue;
            };
            let module = load_module(
                &mut sources,
                module_path.clone(),
                &display,
                &text,
                &mut phase,
            )?;
            let under = under_std(&module_path);
            if text.origin == TextOrigin::Editor {
                std.tested.insert(under.clone());
                tested.insert(module_path.clone());
            }
            modules.insert(module.source.file, module_path);
            files.push(LoadedFile {
                path,
                file: module.source.file,
                origin: text.origin,
                version: text.version,
            });
            std.modules.insert(under, module);
        }
        files.extend(loose);
        let paths: Vec<ModulePath> = modules.values().cloned().collect();
        let (package, root_file) = checkout_root(&self.root, manifest, &paths, &mut sources)?;
        Ok(CheckoutLoad {
            sources,
            phase,
            loose_phase,
            files,
            modules,
            std,
            tested,
            package,
            root_file,
        })
    }

    /// Phase A and the Q run of a checkout's session (module doc), with the
    /// two-track rules of a package (A §2.2): clean, and nothing lost or
    /// cut, the Q run is the check; otherwise each broken module's phase-A
    /// list is published and a D run is planned on the strict texts.
    ///
    /// # Errors
    ///
    /// An [`Error`] for a checkout with no `std/`, a file that is not open
    /// and cannot be read, or a target the synthetic manifest does not
    /// have.
    pub(super) fn analyze_checkout(&mut self, overlay: &Overlay) -> Result<Live, Error> {
        let CheckoutLoad {
            mut sources,
            mut phase,
            mut loose_phase,
            files,
            modules,
            std,
            tested,
            package,
            root_file,
        } = self.load_checkout(overlay)?;
        let paths: BTreeSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let broken: BTreeSet<FileId> = phase
            .iter()
            .filter(|d| d.is_error())
            .filter_map(Diagnostic::primary_span)
            .map(|span| span.file)
            .filter(|file| modules.contains_key(file))
            .collect();
        let module_files = files.get(..modules.len()).unwrap_or_default();
        self.remember_modules(&sources, module_files, &broken);
        let broken_now = broken_paths(&files, &broken);
        self.track_broken(&paths, &broken_now);
        self.tested = tested;
        let context = RunContext {
            targets: package.manifest.targets,
            manifest_is_last_good: false,
        };
        let strict = self.checkout_plan(
            &RunFiles::new(&self.root, &sources, &files),
            &modules,
            &broken,
        );
        let extras = RunExtras {
            globals: Arc::clone(&self.globals),
            retain: Retain::CHECKOUT,
            cut_cycles: Some(CutCycles {
                prefer_from: newest_edit(&self.root, overlay),
            }),
        };
        let mut diags = phase.clone();
        let ran = catch_unwind(AssertUnwindSafe(|| {
            run_analysis_checkout(package, &mut sources, &mut diags, extras, std)
        }));
        let front = match ran {
            Ok(Ok(front)) => Ok(front),
            Ok(Err(error)) => Err(error.message().to_owned()),
            Err(payload) => Err(panic_message(payload.as_ref())),
        };
        drop_root(&mut diags, root_file);
        sources.sort(&mut diags);
        sources.sort(&mut phase);
        sources.sort(&mut loose_phase);
        let run = RunFiles::new(&self.root, &sources, &files);
        let is_module = |f: &LoadedFile| modules.contains_key(&f.file);
        // A loose file's phase-A list is all it ever gets (D-1700).
        let mut phase_a = run.reports(&loose_phase, |f| !is_module(f));
        let single = !phase.iter().any(Diagnostic::is_error)
            && front.as_ref().is_ok_and(|front| !lost_anything(front));
        let (report, plan) = if single {
            self.last_strict = Some(strict.key);
            self.fixed.clear();
            let report = Report {
                clean: !diags.iter().any(Diagnostic::is_error),
                inputs: run.inputs(),
                files: run.reports(&diags, is_module),
                held: BTreeSet::new(),
                tool: Vec::new(),
                substituted: false,
            };
            (Some(report), None)
        } else {
            phase_a.extend(run.reports(&phase, |f| broken.contains(&f.file)));
            (None, self.due(Some(strict)))
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

    /// The strict inputs of a checkout's run, with their key (A §2.1): the
    /// synthetic manifest, which is no file of the report, and each module
    /// with its live text, its last good text, or the empty text of a stub.
    /// A module's origin says whether it is open, so the D run checks the
    /// same test bodies, and the key holds it, so an open or a close plans
    /// a D run (D-1701).
    fn checkout_plan(
        &self,
        run: &RunFiles<'_>,
        modules: &BTreeMap<FileId, ModulePath>,
        broken: &BTreeSet<FileId>,
    ) -> StrictPlan {
        let manifest = StrictFile {
            path: self.root.join(MANIFEST_FILE),
            display: MANIFEST_FILE.to_owned(),
            text: Arc::from(synthetic_manifest_text(PACKAGE)),
            source: Source::Live,
            origin: TextOrigin::Disk,
            version: None,
        };
        let mut planned = Vec::new();
        for f in run.files {
            let (Some(path), Some(file)) = (modules.get(&f.file), run.sources.file(f.file)) else {
                continue;
            };
            let display = file.path().to_owned();
            let (text, source) = if broken.contains(&f.file) {
                match self.last_good.get(&display) {
                    Some(text) => (Arc::clone(text), Source::LastGood),
                    None => (Arc::from(""), Source::Stub),
                }
            } else {
                (file.text_arc(), Source::Live)
            };
            planned.push((
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
        let mut key = InputsKey::new();
        key.str(&self.target.map_or_else(String::new, |t| t.to_string()));
        key.file(Path::new(MANIFEST_FILE), &manifest.text);
        for (_, m) in &planned {
            key.file(Path::new(&m.display), &m.text).bytes(&[
                u8::from(m.source == Source::Stub),
                u8::from(m.origin == TextOrigin::Editor),
            ]);
        }
        StrictPlan {
            session: self.id,
            key: key.finish(),
            manifest,
            modules: planned,
            walk: Vec::new(),
            tool: Vec::new(),
            fixed: BTreeSet::new(),
        }
    }

    /// Loads a checkout's strict inputs (`checkout_plan`) as
    /// `load_checkout` loads live ones, with no read from disk: the
    /// synthetic root and the plan's modules as the run's std, whose open
    /// ones, by their origin, have their test bodies checked.
    pub(super) fn load_checkout_strict(&self, plan: &StrictPlan) -> Result<StrictLoad, Error> {
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let manifest = self.checkout_manifest(&mut sources, &mut diagnostics)?;
        let mut files = Vec::with_capacity(plan.modules.len());
        let mut live = BTreeSet::new();
        let mut stubs = BTreeSet::new();
        let mut std = CheckoutStd {
            modules: BTreeMap::new(),
            tested: BTreeSet::new(),
        };
        let mut paths = Vec::with_capacity(plan.modules.len());
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
            let under = under_std(path);
            if m.origin == TextOrigin::Editor {
                std.tested.insert(under.clone());
            }
            std.modules.insert(
                under,
                Module {
                    path: path.clone(),
                    source,
                    hashes: SourceHashes::of(m.text.as_bytes()),
                    parsed: true,
                    syntax_diagnostics: own,
                },
            );
            paths.push(path.clone());
        }
        let (package, root_file) = checkout_root(&self.root, manifest, &paths, &mut sources)?;
        Ok(StrictLoad {
            package,
            sources,
            diagnostics,
            files,
            live,
            stubs,
            std: Some(std),
            root_file: Some(root_file),
        })
    }
}
