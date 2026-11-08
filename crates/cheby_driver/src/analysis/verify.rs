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
//!    differs on disk now, touched or not, or a move's target appeared on
//!    disk (D-1389, D-1393, S-7).
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
//!    hide a second identical error. A rename reads `old` as `new` in the
//!    messages of "before" first, so a message that names the symbol is
//!    not new; a message that names another binding of the old name, which
//!    the rename leaves alone, is matched as it was.
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
use cheby_package::{MANIFEST_FILE, Manifest, PackageError, Targets};

use super::plain::{Placed, PlainDiagnostic, PlainEdit, exact_key, plain};
use super::snapshot::{BaselineEntry, FileLocation, Snapshot};
use crate::cache::Hash128;
use crate::frontend::{Frontend, Retain, RunExtras, run_analysis};
use crate::{Error, Module, Package, SourceHashes, parse_file};

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
    /// A rename: messages of "before" are read with `old` as `new`, at
    /// identifier boundaries.
    pub renamed: Option<(&'a str, &'a str)>,
}

/// What [`verify`] finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// The edits add no error and no W0007.
    Clean,
    /// New errors, or a new W0007, in the order `cheby check` prints them.
    /// Their ranges lie in the edited text of their file, under its new
    /// path for a moved file.
    Introduces(Vec<PlainDiagnostic>),
    /// A closed file differs on disk from the snapshot's text, or a move's
    /// target that the snapshot does not have exists on disk.
    Stale(PathBuf),
    /// "Before" cannot be trusted: a cut import, E0040, or a loss to an
    /// internal error. Also when the run on the edited text loses a module
    /// or a body, since "after" then cannot be trusted either.
    Blocked(String),
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
    let not_utf8: BTreeSet<PathBuf> = not_utf8
        .into_iter()
        .map(|path| moved(request, &path))
        .collect();
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let (globals, targets) = snapshot.run_context();
    let (package, files) = build(
        root,
        Texts { texts, not_utf8 },
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
        let ran = catch_unwind(AssertUnwindSafe(|| {
            run_analysis(package, &mut sources, &mut diags, extras)
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
    sources.sort(&mut diags);
    let introduced = introduced(snapshot, request, &sources, &files, &diags);
    Ok(if introduced.is_empty() {
        VerifyOutcome::Clean
    } else {
        VerifyOutcome::Introduces(introduced)
    })
}

/// Step 1: the first file that the snapshot read from disk and that
/// differs on disk now, or is gone, touched or not (D-1389), else the
/// first move target that the snapshot does not have and that exists on
/// disk. A manifest the run replaced by the last good one is compared only
/// when touched, since it never equals the disk's.
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
        let Some(FileLocation::Package(path)) = snapshot.location(file) else {
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
    Ok(request
        .moves
        .iter()
        .find(|(old, new)| {
            old != new && snapshot.file_of(new).is_none() && std::fs::symlink_metadata(new).is_ok()
        })
        .map(|(_, new)| new.clone()))
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
    if let Some(module) = snapshot.local_modules().find(|m| {
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
        .filter_map(|(file, source)| match snapshot.location(file)? {
            FileLocation::Package(path) => Some((path.clone(), source.text().to_owned())),
            FileLocation::Std(_) | FileLocation::Synthetic => None,
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
    texts: BTreeMap<PathBuf, String>,
    not_utf8: BTreeSet<PathBuf>,
}

/// Loads the edited texts as `inputs::load` loads a package: the manifest
/// first, then each module in display order, parsed and validated. A file
/// that was not UTF-8 gets the tree of an empty text and E0001, as there.
/// The package is `None` when the edited manifest cannot be used; its
/// diagnostics are then all there is.
///
/// # Errors
///
/// An [`Error`] for a missing manifest, a file that is not a module of the
/// package, a file too large for the source map, or an edited manifest
/// that keeps none of the snapshot's `targets`.
fn build(
    root: &Path,
    Texts {
        mut texts,
        not_utf8,
    }: Texts,
    targets: Targets,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(Option<Package>, Vec<VerifyFile>), Error> {
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest_text = texts
        .remove(&manifest_path)
        .ok_or_else(|| Error::new(format!("verify: the package has no {MANIFEST_FILE}")))?;
    let file = sources
        .add_file(MANIFEST_FILE, manifest_text.as_str())
        .map_err(|_| Error::from(PackageError::too_large(MANIFEST_FILE)))?;
    let mut files = vec![VerifyFile {
        path: manifest_path,
        file,
    }];
    let Some(mut manifest) = Manifest::parse_at(file, &manifest_text, root, diagnostics) else {
        return Ok((None, files));
    };
    // The snapshot's targets are the manifest's after `--target`, so this
    // narrows to the session's target and keeps an unedited manifest's.
    manifest.targets = manifest.targets.intersection(targets);
    if manifest.targets.is_empty() {
        return Err(Error::new(
            "verify: the edited manifest keeps none of the run's targets".to_owned(),
        ));
    }
    let mut listed = BTreeMap::new();
    for (path, text) in texts {
        let module = cheby_package::module_file(root, &manifest.name, &path).ok_or_else(|| {
            Error::new(format!(
                "verify: `{}` is not a module of the package",
                cheby_diag::display_path(&path, root)
            ))
        })?;
        listed.insert(module.display.clone(), (module, path, text));
    }
    let mut modules = Vec::with_capacity(listed.len());
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
        files.push(VerifyFile {
            path,
            file: source.file,
        });
        modules.push(Module {
            path: module.path,
            source,
            hashes,
            parsed: true,
            syntax_diagnostics: own,
        });
    }
    Ok((
        Some(Package {
            root: root.to_path_buf(),
            manifest,
            modules,
        }),
        files,
    ))
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
/// rename that captures a prelude name gives alone (S-1).
fn counts(code: Code) -> bool {
    code.severity() == cheby_diag::Severity::Error || code == codes::HIDDEN_PRELUDE_NAME
}

/// Step 4: the diagnostics of the edited run, sorted, that "before" does
/// not account for, counted as a multiset.
///
/// With a rename, each "before" entry has two keys: its message with the
/// old name read as the new one, and its message as it was, for a binding
/// of the same name that the rename leaves alone. An "after" diagnostic
/// takes an entry by the first key if one is left, then by the second,
/// and each entry accounts for one diagnostic only.
fn introduced(
    snapshot: &Snapshot,
    request: &VerifyRequest<'_>,
    sources: &SourceMap,
    files: &[VerifyFile],
    diags: &[Diagnostic],
) -> Vec<PlainDiagnostic> {
    let mut by_new: BTreeMap<BaselineEntry, Vec<usize>> = BTreeMap::new();
    let mut by_old: BTreeMap<BaselineEntry, Vec<usize>> = BTreeMap::new();
    let mut entries = 0_usize;
    for (code, path, message) in snapshot.baseline() {
        if !counts(*code) {
            continue;
        }
        let path = path.as_ref().map(|path| moved(request, path));
        if let Some((old, new)) = request.renamed {
            let renamed = rename_in(message, old, new);
            if renamed != *message {
                by_old
                    .entry((*code, path.clone(), message.clone()))
                    .or_default()
                    .push(entries);
            }
            by_new
                .entry((*code, path, renamed))
                .or_default()
                .push(entries);
        } else {
            by_new
                .entry((*code, path, message.clone()))
                .or_default()
                .push(entries);
        }
        entries = entries.saturating_add(1);
    }
    let mut used = vec![false; entries];
    let mut take = |keys: &mut BTreeMap<BaselineEntry, Vec<usize>>, key: &BaselineEntry| {
        let list = keys.get_mut(key)?;
        while let Some(at) = list.pop() {
            if let Some(slot) = used.get_mut(at)
                && !*slot
            {
                *slot = true;
                return Some(());
            }
        }
        None
    };
    let place = |file: FileId| {
        let found = files.iter().find(|f| f.file == file)?;
        Some(Placed {
            path: &found.path,
            text: sources.file(file)?.text(),
        })
    };
    let manifest = files.first().map(|f| f.file);
    let after: Vec<(&Diagnostic, BaselineEntry)> = diags
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
            (diag, (diag.code, path, diag.message.clone()))
        })
        .collect();
    // The renamed keys first, so a diagnostic matched by its old message
    // never takes an entry that a renamed one needs.
    let matched: Vec<bool> = after
        .iter()
        .map(|(_, key)| take(&mut by_new, key).is_some())
        .collect();
    let mut out = Vec::new();
    for ((diag, key), matched) in after.iter().zip(matched) {
        if matched || take(&mut by_old, key).is_some() {
            continue;
        }
        let span = diag.primary_span();
        let owner = span
            .map(|span| span.file)
            .filter(|&file| files.iter().any(|f| f.file == file))
            .or(manifest);
        if let Some(owner) = owner {
            out.push(plain(diag, owner, place));
        }
    }
    out
}

/// `message` with each `old` that stands as a whole identifier read as
/// `new`: `count` in "`count` is already bound", not in `counter`.
pub(super) fn rename_in(message: &str, old: &str, new: &str) -> String {
    if old.is_empty() {
        return message.to_owned();
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    let mut previous: Option<char> = None;
    while let Some(at) = rest.find(old) {
        let (head, tail) = rest.split_at(at);
        let after = tail.get(old.len()..).unwrap_or_default();
        let before = head.chars().next_back().or(previous);
        let whole = !before.is_some_and(ident) && !after.chars().next().is_some_and(ident);
        out.push_str(head);
        out.push_str(if whole { new } else { old });
        previous = old.chars().next_back();
        rest = after;
    }
    out.push_str(rest);
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
