//! `verify`: a stateless re-check of an edit plan (plan §1.6, D-1389).
//!
//! Rename, `willRenameFiles` and refactors call [`verify`] on a query
//! thread before they answer. It touches no [`super::Session`]: it builds
//! a [`Package`] from the snapshot's texts with the edits and moves
//! applied, runs it with `Retain::VERIFY`, and compares that run's
//! diagnostics with [`Snapshot::baseline`], the Q run's list. Both come
//! from `run_analysis`, so before and after compare like with like.
//!
//! The steps, in order:
//!
//! 1. [`VerifyOutcome::Stale`]: a file that the snapshot read from disk
//!    differs on disk now, touched or not, a module file under `src/`
//!    that the snapshot does not have exists on disk (D-1669), or a
//!    move's target appeared on disk (D-1389, D-1393, S-7).
//! 2. [`VerifyOutcome::Blocked`]: "before" cannot be trusted, because of a
//!    cut import, an E0040, or something lost to an internal error. Also
//!    an edit of a file that is not UTF-8, whose bytes the snapshot does
//!    not hold.
//! 3. The run on the edited text. `run_analysis` has no phase-A gate and
//!    `Retain::VERIFY` is tolerant, so a broken module elsewhere neither
//!    stops the run nor makes the check vacuous (S-6). A file that is not
//!    UTF-8 enters it as the Q run had it: the tree of an empty text and
//!    its E0001.
//! 4. [`VerifyOutcome::Introduces`]: the multiset difference of errors and
//!    W0007 by code, file and message (S-1). It counts, since a set would
//!    hide a second identical error. A rename, or a move's module paths,
//!    reads each `old` as its `new` in the messages of "before" first,
//!    every pair at once (D-1674), so a message that names the symbol is
//!    not new; a message that names another binding of the old name, which
//!    the rename leaves alone, is matched as it was. A move that changes a
//!    module's last segment also reads `old_last::` as `new_last::`, as a
//!    file that imports the module writes its items (D-1715).
//! 5. [`VerifyOutcome::Hides`], only when the request asks
//!    ([`VerifyRequest::hides`], rename alone): the same multisets read the
//!    other way, the errors of "before" that "after" no longer has
//!    (D-1668). A W0007 that goes away is no error and does not count.
//!    Where errors tie on code, file and message, an error of "after" takes
//!    one of "before" on its own line first, since a rename writes tokens
//!    only and moves no line, so the place quoted is that of one that went
//!    (D-1698). When the tie cannot be told apart by line, only the file is
//!    quoted (D-1709).
//!
//! A single-file snapshot (D-1371) is checked as its Q run ran: the
//! one-module package `script` with the synthetic manifest, for a loose
//! file (D-1667). Its stale step lists no `src/`, since the file belongs
//! to no package.
//!
//! A std checkout's snapshot (D-1700) is checked as its Q run ran too,
//! which replaces D-1667's std half: the checkout's std modules, from the
//! edited texts, are preloaded as package `std` under the synthetic root
//! that imports each of them, and the root's diagnostics are dropped. The
//! test bodies checked are those the Q run checked, of the modules that
//! had an open document (D-1701), so an error in one is in "before" and
//! "after" alike, and a closed module's test body is in neither. A file
//! under `std/` that names no module gets phase-A diagnostics only in the
//! Q run, so it is no input here, and an edit of it is refused as an edit
//! of no file of the package. The new-module step walks `std/` in place
//! of `src/`.
//!
//! Limits:
//!
//! - Bodies with an error are dropped from later checks (D-494), so
//!   `verify` cannot see a new E0061 or W0010 inside such a body. Rewrites
//!   that depend on those checks gate on facts instead (plan §1.11).
//! - Constants are not evaluated (D-459), as no server run evaluates them,
//!   so a new E0075 or E0077 passes. Constant errors come from the child
//!   (A §11). Rewrites of a constant's initializer gate on that.
//! - A file is compared by its message, not its position, so an error that
//!   moves inside a file is not new, and neither is one whose message
//!   another rename leaves unchanged.
//! - The run shares the snapshot's interners and targets. Runs that share
//!   `Globals` in parallel give the reports they give alone (D-264).

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Code, Diagnostic, FileId, SourceMap, Span, TextRange, TextSize, codes};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, PackageError, Targets};

use super::plain::{Placed, PlainDiagnostic, PlainEdit, exact_key, plain};
use super::session::checkout;
use super::snapshot::{BaselineEntry, FileLocation, Snapshot};
use crate::cache::Hash128;
use crate::frontend::{
    CheckoutStd, Frontend, Retain, RunExtras, run_analysis, run_analysis_checkout,
};
use crate::session::synthetic_manifest;
use crate::{Error, Module, Package, SourceHashes, parse_file};

/// The package, and the one module, of a single-file snapshot: the name
/// `analysis/session/single.rs` gives the Q run's (A §6.4, D-1667).
pub(super) const SINGLE_PACKAGE: &str = "script";

/// An edit plan to check against a snapshot.
#[derive(Clone, Copy, Debug)]
pub struct VerifyRequest<'a> {
    /// The edits, by the snapshot's paths: an edit of a moved file names
    /// its old path, since the edits apply before the moves. Ranges are in
    /// the snapshot's text of that path and must not overlap.
    pub edits: &'a [PlainEdit],
    /// `willRenameFiles`: old path, new path, one module file each (the
    /// caller expands a folder). Applied after `edits`.
    pub moves: &'a [(PathBuf, PathBuf)],
    /// A rename, or a move's module paths: messages of "before" are read
    /// with each `old` as its `new`, at identifier boundaries (D-1674).
    /// Empty for neither.
    pub renamed: &'a [(&'a str, &'a str)],
    /// Rename's check that no error of "before" goes away (D-1668): an
    /// edit that makes one go is [`VerifyOutcome::Hides`]. Fixes and moves
    /// leave it off, since they exist to remove errors.
    pub hides: bool,
}

/// What [`verify`] finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// The edits add no error and no W0007.
    Clean,
    /// New errors, or a new W0007, in the order `cheby check` prints them,
    /// each with its file and place (D-1671, D-1709).
    Introduces(Vec<Introduced>),
    /// A closed file differs on disk from the snapshot's text, or a move's
    /// target that the snapshot does not have exists on disk.
    Stale(PathBuf),
    /// "Before" cannot be trusted: a cut import, E0040, or a loss to an
    /// internal error. Also when the run on the edited text loses a module
    /// or a body, since "after" then cannot be trusted either.
    Blocked(String),
    /// With [`VerifyRequest::hides`], errors of "before" that the edited
    /// text no longer has (D-1668), in the baseline's order, each as the
    /// snapshot has it, with `path:line:col` of its primary span in the
    /// snapshot's text (D-1698). The place is the file's path alone when
    /// errors that tie on code, file and message cannot be told apart, and
    /// `None` for one with no span (D-1709).
    Hides(Vec<(Option<String>, BaselineEntry)>),
}

/// A diagnostic that an edit plan adds (D-1671, D-1709).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Introduced {
    /// `path:line:col` of its primary span as `cheby check` shows it, or
    /// the path of [`Introduced::path`] as `cheby check` shows it for a
    /// diagnostic with no span.
    pub at: String,
    /// The file it is in: a file of the edited package, under its new path
    /// for a moved file. A diagnostic with no span is the manifest's, or
    /// in single-file mode the one module's, or in a std checkout, whose
    /// manifest is no file either, the first module's.
    pub path: PathBuf,
    /// The diagnostic. Its ranges lie in the edited text of its file.
    pub diagnostic: PlainDiagnostic,
}

/// One package file of the run on the edited text.
struct VerifyFile {
    path: PathBuf,
    file: FileId,
}

/// Checks the edit plan `request` against `snapshot` (module doc).
///
/// # Errors
///
/// An [`Error`] for a request that does not fit the snapshot: an edit or
/// a move of a path the snapshot has no package file for, overlapping or
/// misplaced edits, a move onto another file or to a path that is not a
/// module of the package. Also for a file too large for a source map, and
/// for an internal error or a panic in the run.
pub fn verify(snapshot: &Snapshot, request: &VerifyRequest<'_>) -> Result<VerifyOutcome, Error> {
    if let Some(path) = stale(snapshot, request)? {
        return Ok(VerifyOutcome::Stale(path));
    }
    if let Some(reason) = blocked(snapshot) {
        return Ok(VerifyOutcome::Blocked(reason));
    }
    let root = snapshot.root();
    let not_utf8 = not_utf8(snapshot);
    if let Some(edit) = request.edits.iter().find(|e| not_utf8.contains(&e.path)) {
        return Ok(VerifyOutcome::Blocked(format!(
            "`{}` is not valid UTF-8",
            cheby_diag::display_path(&edit.path, root)
        )));
    }
    let texts = edited_texts(snapshot, request)?;
    let kind = kind(snapshot, request)?;
    let not_utf8: BTreeSet<PathBuf> = not_utf8
        .into_iter()
        .map(|path| moved(request, &path))
        .collect();
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let (globals, targets) = snapshot.run_context();
    let Built {
        package,
        files,
        std,
        root_file,
    } = build(
        root,
        Texts {
            by_path: texts,
            not_utf8,
            kind,
        },
        targets,
        &mut sources,
        &mut diags,
    )?;
    if let Some(package) = package {
        let extras = RunExtras {
            globals,
            retain: Retain::VERIFY,
            cut_cycles: None,
        };
        let ran = catch_unwind(AssertUnwindSafe(|| match std {
            Some(std) => run_analysis_checkout(package, &mut sources, &mut diags, extras, std),
            None => run_analysis(package, &mut sources, &mut diags, extras),
        }));
        let front = match ran {
            Ok(Ok(front)) => front,
            Ok(Err(error)) => return Err(error),
            Err(payload) => return Err(Error::new(panic_message(payload.as_ref()))),
        };
        if let Some(reason) = lost_after(&front) {
            return Ok(VerifyOutcome::Blocked(reason));
        }
    }
    // A checkout's synthetic root is no file of the package, and the Q run
    // dropped its diagnostics too (D-1700).
    if let Some(root_file) = root_file {
        checkout::drop_root(&mut diags, root_file);
    }
    sources.sort(&mut diags);
    let Compared { introduced, hidden } =
        compare(snapshot, request, root, &sources, &files, &diags);
    Ok(if !introduced.is_empty() {
        VerifyOutcome::Introduces(introduced)
    } else if request.hides && !hidden.is_empty() {
        VerifyOutcome::Hides(hidden)
    } else {
        VerifyOutcome::Clean
    })
}

/// Step 1: the first file that the snapshot read from disk and that
/// differs on disk now, or is gone, touched or not (D-1389), else the
/// first module file on disk that the snapshot does not have (D-1669),
/// else the first move target that the snapshot does not have and that
/// exists on disk. A manifest the run replaced by the last good one is
/// compared only when touched, since it never equals the disk's.
///
/// # Errors
///
/// An [`Error`] for a touched path with no package file in the snapshot.
fn stale(snapshot: &Snapshot, request: &VerifyRequest<'_>) -> Result<Option<PathBuf>, Error> {
    let touched: BTreeSet<&Path> = request
        .edits
        .iter()
        .map(|edit| edit.path.as_path())
        .chain(request.moves.iter().map(|(old, _)| old.as_path()))
        .collect();
    for path in &touched {
        package_text(snapshot, path)?;
    }
    let manifest = snapshot.root().join(MANIFEST_FILE);
    let last_good = snapshot.status().manifest_is_last_good;
    for (file, source) in snapshot.sources().files() {
        let Some(path) = input_path(snapshot, file) else {
            continue;
        };
        if snapshot.version_of(path).is_some()
            || (last_good && *path == manifest && !touched.contains(path.as_path()))
        {
            continue;
        }
        // A module's hash is of the bytes it was read from, which for a
        // file that was not UTF-8 is not its lossy text's.
        let expected = snapshot
            .bytes_hash(file)
            .unwrap_or_else(|| exact_key(source.text()));
        let same = std::fs::read(path).is_ok_and(|bytes| Hash128::of(&bytes) == expected);
        if !same {
            return Ok(Some(path.clone()));
        }
    }
    if let Some(path) = new_module(snapshot) {
        return Ok(Some(path));
    }
    Ok(request
        .moves
        .iter()
        .find(|(old, new)| {
            old != new && snapshot.file_of(new).is_none() && std::fs::symlink_metadata(new).is_ok()
        })
        .map(|(_, new)| new.clone()))
}

/// The first module file under `src/` on disk, in `discover`'s order, that
/// the snapshot does not have (D-1669): a request may be answered before
/// the watched-file job adds it (D-1443), and an edit plan would then miss
/// every occurrence in it. `None` for a single-file snapshot, whose file
/// belongs to no package, and when `src/` cannot be walked: the Q run's
/// walk failed then too, or fails on the next run, and reports it, and
/// the files the snapshot has are compared all the same. A std checkout's
/// modules are under `std/` instead (`new_std_module`).
fn new_module(snapshot: &Snapshot) -> Option<PathBuf> {
    if snapshot.is_single_file() {
        return None;
    }
    if snapshot.is_std_checkout() {
        return new_std_module(snapshot);
    }
    let root = snapshot.root();
    // E0037 names are skipped, as the Q run's walk skips them, and its
    // diagnostics are the Q run's to report.
    let mut skipped = Vec::new();
    // The package's name only names the modules, which are not needed.
    let found = cheby_package::discover(root, "main", &mut skipped).ok()?;
    found
        .into_iter()
        .map(|module| root.join(&module.file))
        .find(|path| snapshot.file_of(path).is_none())
}

/// In a std checkout, the first file under `std/` on disk, in path order,
/// that names a module of the checkout and that the snapshot does not
/// have: D-1669 for a checkout, whose run reads every module under `std/`
/// (D-1700). `None` when `std/` cannot be walked, as for `src/`.
fn new_std_module(snapshot: &Snapshot) -> Option<PathBuf> {
    let root = snapshot.root();
    let found = checkout::walk_std(&root.join(checkout::STD_DIR)).ok()?;
    found.into_iter().find(|path| {
        checkout::checkout_module(root, path).is_some() && snapshot.file_of(path).is_none()
    })
}

/// The path of `file` when it is an input of `verify`'s run: a file of the
/// package (`FileLocation::Package`), and in a std checkout one that names
/// a module of it. A file under `std/` that names none is no module of
/// the Q run, which gives it phase-A diagnostics only (D-1700).
fn input_path(snapshot: &Snapshot, file: FileId) -> Option<&PathBuf> {
    match snapshot.location(file)? {
        FileLocation::Package(path) => (!snapshot.is_std_checkout()
            || checkout::checkout_module(snapshot.root(), path).is_some())
        .then_some(path),
        FileLocation::Std(_) | FileLocation::Synthetic => None,
    }
}

/// The package files that were not UTF-8 in the Q run: those with E0001.
fn not_utf8(snapshot: &Snapshot) -> BTreeSet<PathBuf> {
    snapshot
        .baseline()
        .iter()
        .filter(|(code, _, _)| *code == codes::INVALID_UTF8)
        .filter_map(|(_, path, _)| path.clone())
        .collect()
}

/// `path` after the moves of `request`.
fn moved(request: &VerifyRequest<'_>, path: &Path) -> PathBuf {
    request
        .moves
        .iter()
        .find(|(old, _)| old == path)
        .map_or_else(|| path.to_path_buf(), |(_, new)| new.clone())
}

/// The snapshot's text of the package file at `path`.
///
/// # Errors
///
/// An [`Error`] when the snapshot has no package file at `path`.
fn package_text<'s>(snapshot: &'s Snapshot, path: &Path) -> Result<&'s str, Error> {
    snapshot
        .file_of(path)
        .and_then(|file| snapshot.source(file))
        .map(cheby_diag::SourceFile::text)
        .ok_or_else(|| {
            Error::new(format!(
                "verify: `{}` is not a file of the package",
                cheby_diag::display_path(path, snapshot.root())
            ))
        })
}

/// Step 2: why "before" cannot be trusted, if it cannot.
fn blocked(snapshot: &Snapshot) -> Option<String> {
    let root = snapshot.root();
    if let Some((path, _)) = snapshot.status().cut_imports.first() {
        return Some(format!(
            "`{}` imports through a cycle",
            cheby_diag::display_path(path, root)
        ));
    }
    if snapshot
        .baseline()
        .iter()
        .any(|(code, _, _)| *code == codes::IMPORT_CYCLE)
    {
        return Some("the package has an import cycle".to_owned());
    }
    if let Some((module, _)) = snapshot.lost().first() {
        let name = snapshot
            .module(*module)
            .and_then(super::snapshot::ModuleView::path)
            .map_or_else(|| "a module".to_owned(), |p| format!("`{p}`"));
        return Some(format!("an internal error stopped the check of {name}"));
    }
    if let Some(module) = snapshot.own_modules().find(|m| {
        m.facts()
            .is_some_and(|f| !f.lost.is_empty() || !f.lost_passes.is_empty())
    }) {
        let name = module
            .path()
            .map_or_else(|| "a module".to_owned(), |p| format!("`{p}`"));
        return Some(format!(
            "an internal error stopped part of the check of {name}"
        ));
    }
    snapshot
        .silent_drop()
        .then(|| "the check dropped a body with no error".to_owned())
}

/// Whether the run on the edited text lost a module or a body, and so
/// cannot be compared.
fn lost_after(front: &Frontend) -> Option<String> {
    if let Some((module, _)) = front.lost.first() {
        let name = front
            .modules
            .get(*module)
            .map_or_else(|| "a module".to_owned(), |info| format!("`{}`", info.path));
        return Some(format!(
            "an internal error stopped the check of {name} in the edited package"
        ));
    }
    front
        .silent_drop
        .then(|| "the check of the edited package dropped a body with no error".to_owned())
}

/// The snapshot's package texts, `cheby.toml` included, by absolute path,
/// with the edits and then the moves applied.
///
/// # Errors
///
/// As [`verify`]: an unknown path, overlapping or misplaced edits, or a
/// move onto another file.
fn edited_texts(
    snapshot: &Snapshot,
    request: &VerifyRequest<'_>,
) -> Result<BTreeMap<PathBuf, String>, Error> {
    let mut texts: BTreeMap<PathBuf, String> = snapshot
        .sources()
        .files()
        .filter_map(|(file, source)| {
            input_path(snapshot, file).map(|path| (path.clone(), source.text().to_owned()))
        })
        .collect();
    let mut by_path: BTreeMap<&Path, Vec<&PlainEdit>> = BTreeMap::new();
    for edit in request.edits {
        by_path.entry(edit.path.as_path()).or_default().push(edit);
    }
    let root = snapshot.root();
    for (path, edits) in by_path {
        let display = cheby_diag::display_path(path, root);
        let text = texts.get_mut(path).ok_or_else(|| {
            Error::new(format!("verify: `{display}` is not a file of the package"))
        })?;
        *text = apply(text, edits)
            .ok_or_else(|| Error::new(format!("verify: the edits of `{display}` do not fit it")))?;
    }
    for (old, new) in request.moves {
        if old == new {
            continue;
        }
        let text = texts.remove(old).ok_or_else(|| {
            Error::new(format!(
                "verify: `{}` is not a file of the package",
                cheby_diag::display_path(old, root)
            ))
        })?;
        if texts.insert(new.clone(), text).is_some() {
            return Err(Error::new(format!(
                "verify: `{}` would move onto another file",
                cheby_diag::display_path(old, root)
            )));
        }
    }
    Ok(texts)
}

/// `text` with `edits` applied, `None` when one falls outside it, off a
/// character boundary, or overlaps another. Insertions at one offset keep
/// their order.
fn apply(text: &str, mut edits: Vec<&PlainEdit>) -> Option<String> {
    edits.sort_by_key(|edit| (edit.range.start(), edit.range.end()));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0_usize;
    for edit in edits {
        let start = usize::try_from(edit.range.start()).ok()?;
        let end = usize::try_from(edit.range.end()).ok()?;
        out.push_str(text.get(cursor..start)?);
        edit.range.slice(text)?;
        out.push_str(&edit.replacement);
        cursor = end;
    }
    out.push_str(text.get(cursor..)?);
    Some(out)
}

/// The edited texts of a package, by absolute path, and the files among
/// them that were not UTF-8, whose texts are lossy.
struct Texts {
    by_path: BTreeMap<PathBuf, String>,
    not_utf8: BTreeSet<PathBuf>,
    kind: Kind,
}

/// What a snapshot's run checked, and so what [`build`] loads.
enum Kind {
    /// A package: `by_path` holds its manifest and its modules.
    Package,
    /// One file in single-file mode (D-1371): `by_path` holds that file
    /// alone, and no manifest.
    Single,
    /// A std checkout (D-1700): `by_path` holds the checkout's std
    /// modules, and no manifest.
    Checkout {
        /// The synthetic root package's name, as the Q run named it.
        name: String,
        /// The files, after the moves, whose test bodies the Q run checked
        /// (D-1701).
        tested: BTreeSet<PathBuf>,
    },
}

/// The kind of `snapshot`'s run, with a checkout's tested files after the
/// moves of `request`.
///
/// # Errors
///
/// An [`Error`] for a checkout's snapshot with no synthetic root, an
/// internal error.
fn kind(snapshot: &Snapshot, request: &VerifyRequest<'_>) -> Result<Kind, Error> {
    if snapshot.is_single_file() {
        return Ok(Kind::Single);
    }
    let Some(tested_std) = snapshot.tested_std() else {
        return Ok(Kind::Package);
    };
    // The synthetic root is the run's one module of the package itself.
    let name = snapshot
        .local_modules()
        .find_map(|m| m.path().map(|path| path.package().to_owned()))
        .ok_or_else(|| Error::new("verify: a std checkout's run has no root".to_owned()))?;
    let root = snapshot.root();
    let tested = snapshot
        .sources()
        .files()
        .filter_map(|(file, _)| input_path(snapshot, file))
        .filter(|path| {
            checkout::checkout_module(root, path).is_some_and(|m| tested_std.contains(&m))
        })
        .map(|path| moved(request, path))
        .collect();
    Ok(Kind::Checkout { name, tested })
}

/// What [`build`] loads.
struct Built {
    /// The package to run, `None` when the edited manifest cannot be used.
    package: Option<Package>,
    /// The files of the result, each with its path.
    files: Vec<VerifyFile>,
    /// A checkout's std modules, which the run preloads (D-1700).
    std: Option<CheckoutStd>,
    /// A checkout's synthetic root file, whose diagnostics are dropped.
    root_file: Option<FileId>,
}

/// Loads the edited texts as `inputs::load` loads a package: the manifest
/// first, then each module in display order, parsed and validated. A file
/// that was not UTF-8 gets the tree of an empty text and E0001, as there.
/// The package is `None` when the edited manifest cannot be used; its
/// diagnostics are then all there is.
///
/// A single-file snapshot loads as `analyze_single` does instead (D-1667):
/// the synthetic manifest, which is no file of the result, and the file as
/// the one module `script`, displayed by its path from `root`, the file's
/// directory.
///
/// A std checkout's snapshot loads as its Q run does (D-1700): the
/// synthetic manifest, each module of the checkout in display order as a
/// std module that the run preloads, its test bodies checked when the Q
/// run checked them, and the synthetic root that imports every module.
///
/// # Errors
///
/// An [`Error`] for a missing manifest, a file that is not a module of the
/// package, a single-file snapshot that does not hold one file, a file too
/// large for the source map, or an edited manifest that keeps none of the
/// snapshot's `targets`.
fn build(
    root: &Path,
    Texts {
        by_path: mut texts,
        not_utf8,
        kind,
    }: Texts,
    targets: Targets,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Built, Error> {
    let (mut manifest, mut files) = match &kind {
        Kind::Single => (
            synthetic_manifest(SINGLE_PACKAGE, sources, diagnostics)?,
            Vec::new(),
        ),
        Kind::Checkout { name, .. } => {
            (synthetic_manifest(name, sources, diagnostics)?, Vec::new())
        }
        Kind::Package => {
            let manifest_path = root.join(MANIFEST_FILE);
            let manifest_text = texts
                .remove(&manifest_path)
                .ok_or_else(|| Error::new(format!("verify: the package has no {MANIFEST_FILE}")))?;
            let file = sources
                .add_file(MANIFEST_FILE, manifest_text.as_str())
                .map_err(|_| Error::from(PackageError::too_large(MANIFEST_FILE)))?;
            let files = vec![VerifyFile {
                path: manifest_path,
                file,
            }];
            let Some(manifest) = Manifest::parse_at(file, &manifest_text, root, diagnostics) else {
                return Ok(Built {
                    package: None,
                    files,
                    std: None,
                    root_file: None,
                });
            };
            (manifest, files)
        }
    };
    // The snapshot's targets are the manifest's after `--target`, so this
    // narrows to the session's target and keeps an unedited manifest's.
    manifest.targets = manifest.targets.intersection(targets);
    if manifest.targets.is_empty() {
        return Err(Error::new(
            "verify: the edited manifest keeps none of the run's targets".to_owned(),
        ));
    }
    let listed = listed(root, &kind, &manifest.name, texts)?;
    let mut modules = Vec::with_capacity(listed.len());
    let mut tested_std = BTreeSet::new();
    for (display, (module, path, text)) in listed {
        let hashes = SourceHashes::of(text.as_bytes());
        let mut own = Vec::new();
        let mut source = if not_utf8.contains(&path) {
            not_utf8_file(sources, &display, text, &mut own)?
        } else {
            parse_file(sources, &display, text.into_bytes(), &mut own)?
        };
        if source.syntax.is_none() {
            let (tree, _) = cheby_syntax::parse(source.file, Arc::from("")).into_parts();
            source.syntax = Some(tree);
        }
        diagnostics.extend(own.iter().cloned());
        if let Kind::Checkout { tested, .. } = &kind
            && tested.contains(&path)
        {
            tested_std.insert(checkout::under_std(&module));
        }
        files.push(VerifyFile {
            path,
            file: source.file,
        });
        modules.push(Module {
            path: module,
            source,
            hashes,
            parsed: true,
            syntax_diagnostics: own,
        });
    }
    match kind {
        Kind::Package | Kind::Single => Ok(Built {
            package: Some(Package {
                root: root.to_path_buf(),
                manifest,
                modules,
            }),
            files,
            std: None,
            root_file: None,
        }),
        Kind::Checkout { .. } => {
            let paths: Vec<ModulePath> = modules.iter().map(|m| m.path.clone()).collect();
            let (package, root_file) = checkout::checkout_root(root, manifest, &paths, sources)?;
            let std = CheckoutStd {
                modules: modules
                    .into_iter()
                    .map(|m| (checkout::under_std(&m.path), m))
                    .collect(),
                tested: tested_std,
            };
            Ok(Built {
                package: Some(package),
                files,
                std: Some(std),
                root_file: Some(root_file),
            })
        }
    }
}

/// The modules of `texts` by display path, each with its module path, its
/// path and its text, as `kind` names them; `package` is the manifest's
/// name.
///
/// # Errors
///
/// An [`Error`] for a file that is not a module of the package, or a
/// single-file snapshot that does not hold one file.
fn listed(
    root: &Path,
    kind: &Kind,
    package: &str,
    texts: BTreeMap<PathBuf, String>,
) -> Result<BTreeMap<String, (ModulePath, PathBuf, String)>, Error> {
    let not_a_module = |path: &Path| {
        Error::new(format!(
            "verify: `{}` is not a module of the package",
            cheby_diag::display_path(path, root)
        ))
    };
    let mut listed = BTreeMap::new();
    match kind {
        Kind::Single => {
            let mut texts = texts.into_iter();
            let (Some((path, text)), None) = (texts.next(), texts.next()) else {
                return Err(Error::new(
                    "verify: a single-file snapshot holds one file".to_owned(),
                ));
            };
            let display = cheby_diag::display_path(&path, root);
            listed.insert(display, (ModulePath::root(SINGLE_PACKAGE), path, text));
        }
        Kind::Checkout { .. } => {
            for (path, text) in texts {
                let module =
                    checkout::checkout_module(root, &path).ok_or_else(|| not_a_module(&path))?;
                listed.insert(cheby_diag::display_path(&path, root), (module, path, text));
            }
        }
        Kind::Package => {
            for (path, text) in texts {
                let module = cheby_package::module_file(root, package, &path)
                    .ok_or_else(|| not_a_module(&path))?;
                listed.insert(module.display, (module.path, path, text));
            }
        }
    }
    Ok(listed)
}

/// A file that was not UTF-8, by its lossy text, as [`parse_file`] gives
/// it: no tree, and E0001 at the first replacement character.
///
/// # Errors
///
/// An [`Error`] for a file too large for the source map.
fn not_utf8_file(
    sources: &mut SourceMap,
    display: &str,
    lossy: String,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<crate::ParsedFile, Error> {
    let at = lossy.find('\u{fffd}').unwrap_or(0);
    let file = sources
        .add_file(display, lossy)
        .map_err(|_| Error::from(PackageError::too_large(display)))?;
    let start = TextSize::try_from(at).unwrap_or(TextSize::MAX);
    let width = TextSize::try_from('\u{fffd}'.len_utf8()).unwrap_or(1);
    diagnostics.push(
        Diagnostic::new(codes::INVALID_UTF8, "source file is not valid UTF-8")
            .with_primary(
                Span::new(file, TextRange::at(start, width)),
                "this byte is not valid UTF-8",
            )
            .with_help("save the file as UTF-8"),
    );
    Ok(crate::ParsedFile {
        file,
        syntax: None,
        has_syntax_errors: true,
    })
}

/// Whether a diagnostic of `code` counts: every error, and W0007, which a
/// rename that captures a prelude name gives alone (S-1). E0037 does not:
/// the Q run's walk of `src/` reports it, and the edited run, built from
/// the snapshot's texts, walks nothing. No edit renames a file, and a move
/// to a name that is not a module name is refused before `verify`
/// (D-1675), so leaving it out on both sides hides nothing, where counting
/// it would read each one as an error that a rename makes go away
/// (D-1668).
fn counts(code: Code) -> bool {
    code != codes::INVALID_MODULE_NAME
        && (code.severity() == cheby_diag::Severity::Error || code == codes::HIDDEN_PRELUDE_NAME)
}

/// What [`compare`] finds.
struct Compared {
    /// The diagnostics of the edited run that "before" does not account
    /// for, in the order `cheby check` prints them, each with its place.
    introduced: Vec<Introduced>,
    /// The errors of "before" that account for none, each with its place,
    /// as the snapshot has them (not read as the request renames them), in
    /// the baseline's order.
    hidden: Vec<(Option<String>, BaselineEntry)>,
}

/// The line of `place`, `path:line:col` as `cheby check` shows it.
fn place_line(place: &str) -> Option<u32> {
    let mut parts = place.rsplitn(3, ':');
    let _col: u32 = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    parts.next().map(|_| line)
}

/// The path of `place`, `path:line:col`, without its line and column.
fn place_file(place: &str) -> Option<&str> {
    place_line(place)?;
    place.rsplitn(3, ':').nth(2)
}

/// One entry of "before" in [`compare`].
struct Entry {
    /// Its index in the baseline, which a hidden error is reported by, as
    /// it was and at its place.
    index: usize,
    /// The line of its place.
    line: Option<u32>,
    /// Its message with every pair read at once: one key per reading when
    /// several pairs share an `old` (`readings`).
    new_keys: Vec<BaselineEntry>,
    /// Its message as it was, when the pairs change it.
    old_key: Option<BaselineEntry>,
    /// Whether a diagnostic of "after" took it.
    used: bool,
}

/// Steps 4 and 5: the diagnostics of the edited run, sorted, that "before"
/// does not account for, and the errors of "before" left over, both
/// counted as multisets.
///
/// With a rename or a move, each "before" entry has two keys: its message
/// with every pair read at once, and its message as it was, for a binding
/// of the same name that the rename leaves alone. An "after" diagnostic
/// takes an entry on its own line first, by the first key, then by the
/// second; then an entry on any line, by the first key, then by the
/// second (module doc). Each entry accounts for one diagnostic only.
/// The entries left over are what went away ([`left_over`]).
fn compare(
    snapshot: &Snapshot,
    request: &VerifyRequest<'_>,
    root: &Path,
    sources: &SourceMap,
    files: &[VerifyFile],
    diags: &[Diagnostic],
) -> Compared {
    let mut by_new: BTreeMap<BaselineEntry, Vec<usize>> = BTreeMap::new();
    let mut by_old: BTreeMap<BaselineEntry, Vec<usize>> = BTreeMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    for (index, (code, path, message)) in snapshot.baseline().iter().enumerate() {
        if !counts(*code) {
            continue;
        }
        let path = path.as_ref().map(|path| moved(request, path));
        let renamed = readings(message, request.renamed);
        let at = entries.len();
        let old_key = renamed
            .iter()
            .any(|r| r != message)
            .then(|| (*code, path.clone(), message.clone()));
        if let Some(key) = &old_key {
            by_old.entry(key.clone()).or_default().push(at);
        }
        let new_keys: Vec<BaselineEntry> = renamed
            .into_iter()
            .map(|reading| (*code, path.clone(), reading))
            .collect();
        for key in &new_keys {
            by_new.entry(key.clone()).or_default().push(at);
        }
        entries.push(Entry {
            index,
            line: snapshot.baseline_place(index).and_then(place_line),
            new_keys,
            old_key,
            used: false,
        });
    }
    // The keys that had an entry taken off its diagnostic's line.
    let mut loose: BTreeSet<BaselineEntry> = BTreeSet::new();
    // Takes an unused entry of `key`, the last first: with `line`, only
    // one on that line.
    let mut take = |list: &mut BTreeMap<BaselineEntry, Vec<usize>>,
                    key: &BaselineEntry,
                    line: Option<Option<u32>>| {
        let candidates = list.get_mut(key)?;
        let found = candidates.iter().rposition(|&at| {
            entries
                .get(at)
                .is_some_and(|e| !e.used && line.is_none_or(|line| e.line == line))
        })?;
        let at = candidates.remove(found);
        if let Some(entry) = entries.get_mut(at) {
            entry.used = true;
        }
        if line.is_none() {
            loose.insert(key.clone());
        }
        Some(())
    };
    let after = after_keys(sources, files, diags);
    // The entries on a diagnostic's own line first, by either key, since a
    // rename moves no line: an error that stands where it stood keeps its
    // entry, and a new one with the renamed reading of an unrelated entry
    // cannot take it off its line (D-1709). Within each, the renamed keys
    // first, so a diagnostic matched by its old message never takes an
    // entry that a renamed one needs.
    let mut matched = vec![false; after.len()];
    for (old, on_line) in [(false, true), (true, true), (false, false), (true, false)] {
        let list = if old { &mut by_old } else { &mut by_new };
        for ((_, key, line), done) in after.iter().zip(matched.iter_mut()) {
            if !*done {
                *done = take(list, key, on_line.then_some(*line)).is_some();
            }
        }
    }
    let place = |file: FileId| {
        let found = files.iter().find(|f| f.file == file)?;
        Some(Placed {
            path: &found.path,
            text: sources.file(file)?.text(),
        })
    };
    // A diagnostic outside the files is the manifest's, or in single-file
    // mode, whose manifest is no file, the one module's; in a checkout,
    // whose manifest is no file either, the first module's.
    let fallback = files.first().map(|f| f.file);
    let mut introduced = Vec::new();
    for ((diag, _, _), matched) in after.iter().zip(matched) {
        if matched {
            continue;
        }
        let span = diag.primary_span();
        let owner = span
            .map(|span| span.file)
            .filter(|&file| files.iter().any(|f| f.file == file))
            .or(fallback)
            .and_then(|owner| Some((owner, files.iter().find(|f| f.file == owner)?)));
        if let Some((owner, file)) = owner {
            let at = span.and_then(|span| sources.location(span)).map_or_else(
                || cheby_diag::display_path(&file.path, root),
                |at| at.to_string(),
            );
            introduced.push(Introduced {
                at,
                path: file.path.clone(),
                diagnostic: plain(diag, owner, place),
            });
        }
    }
    let hidden = left_over(snapshot, &entries, &loose);
    Compared { introduced, hidden }
}

/// The diagnostics of the edited run that count, each with its key, code,
/// file and message, and the line of its primary span.
fn after_keys<'d>(
    sources: &SourceMap,
    files: &[VerifyFile],
    diags: &'d [Diagnostic],
) -> Vec<(&'d Diagnostic, BaselineEntry, Option<u32>)> {
    diags
        .iter()
        .filter(|d| counts(d.code))
        .map(|diag| {
            let path = diag.primary_span().and_then(|span| {
                files
                    .iter()
                    .find(|f| f.file == span.file)
                    .map(|f| f.path.clone())
                    .or_else(|| sources.file(span.file).map(|s| PathBuf::from(s.path())))
            });
            let line = diag
                .primary_span()
                .and_then(|span| sources.location(span))
                .map(|at| at.line);
            (diag, (diag.code, path, diag.message.clone()), line)
        })
        .collect()
}

/// The errors of "before" that no diagnostic of "after" took, each with
/// its place, as the snapshot has them, in the baseline's order: what went
/// away (D-1668). A W0007 is no error and is left out. One whose key also
/// had an entry taken off its diagnostic's line (`loose`), or whose key
/// and line it shares with an entry that was taken, may not be the one
/// that went, so only its file is quoted (D-1709).
fn left_over(
    snapshot: &Snapshot,
    entries: &[Entry],
    loose: &BTreeSet<BaselineEntry>,
) -> Vec<(Option<String>, BaselineEntry)> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.used)
        .filter_map(|(at, entry)| {
            let found = snapshot.baseline().get(entry.index)?;
            if found.0.severity() != cheby_diag::Severity::Error {
                return None;
            }
            let tied = entry.line.is_some()
                && entries.iter().enumerate().any(|(other, e)| {
                    other != at
                        && e.used
                        && e.line == entry.line
                        && e.new_keys.iter().any(|key| entry.new_keys.contains(key))
                });
            let unsure = tied
                || entry.new_keys.iter().any(|key| loose.contains(key))
                || entry
                    .old_key
                    .as_ref()
                    .is_some_and(|key| loose.contains(key));
            let place = snapshot.baseline_place(entry.index).map(|place| {
                let file = place_file(place).filter(|_| unsure);
                file.unwrap_or(place).to_owned()
            });
            Some((place, found.clone()))
        })
        .collect()
}

/// The readings of `message` with the pairs (`rename_all`): one, unless
/// several pairs share an `old`, as two moves that give two modules of one
/// last segment new last segments do (D-1715). Then each choice of one
/// `new` per such `old` is a reading, since a message does not say which
/// module it names, and an entry matches by any of them. At most
/// `MAX_READINGS`, each once, the first the reading of the pairs in order.
pub(super) fn readings(message: &str, pairs: &[(&str, &str)]) -> Vec<String> {
    let mut olds: Vec<&str> = Vec::new();
    for (old, _) in pairs {
        if !olds.contains(old) {
            olds.push(old);
        }
    }
    if olds.len() == pairs.len() {
        return vec![rename_all(message, pairs)];
    }
    let mut choices: Vec<Vec<(&str, &str)>> = vec![Vec::new()];
    for old in olds {
        let mut next = Vec::new();
        for choice in &choices {
            for &(o, new) in pairs.iter().filter(|(o, _)| *o == old) {
                let mut longer = choice.clone();
                longer.push((o, new));
                next.push(longer);
            }
        }
        next.truncate(MAX_READINGS);
        choices = next;
    }
    let mut out: Vec<String> = Vec::new();
    for choice in choices {
        let reading = rename_all(message, &choice);
        if !out.contains(&reading) {
            out.push(reading);
        }
    }
    out
}

/// The most readings of one message (`readings`).
const MAX_READINGS: usize = 16;

/// `message` with the `old` of each pair that stands as a whole
/// identifier read as its `new` (`count` in "`count` is already bound",
/// not in `counter`), every pair at once (D-1674): a `new` is
/// never read again, so the moves `b` to `c` and `a` to `b` read `a` as
/// `b`, in either order. Where two `old`s start at one place, the longer
/// is read, so `app::geo::x` is not read as `app::geo` followed by `::x`.
/// An `old` that ends in `::`, a module's last segment as a qualifier
/// (D-1715), is read before any name, and never after another `:`, so
/// `util::` reads `util::Shape` but not `app::util::Shape` or `xutil::`.
pub(super) fn rename_all(message: &str, pairs: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(&str, &str)> = pairs
        .iter()
        .copied()
        .filter(|(old, _)| !old.is_empty())
        .collect();
    if pairs.is_empty() {
        return message.to_owned();
    }
    pairs.sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    let mut previous: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let found = if previous.is_some_and(ident) {
            None
        } else {
            pairs.iter().find(|(old, _)| {
                let qualifier = old.ends_with("::");
                !(qualifier && previous == Some(':'))
                    && rest
                        .strip_prefix(old)
                        .is_some_and(|after| qualifier || !after.chars().next().is_some_and(ident))
            })
        };
        if let Some((old, new)) = found {
            out.push_str(new);
            rest = rest.get(old.len()..).unwrap_or_default();
            previous = old.chars().next_back();
        } else {
            out.push(c);
            rest = rest.get(c.len_utf8()..).unwrap_or_default();
            previous = Some(c);
        }
    }
    out
}

/// The message of a panic's payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message");
    format!("verify panicked: {text}")
}
