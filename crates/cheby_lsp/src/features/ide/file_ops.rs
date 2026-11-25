//! File moves over the index, free of protocol types (X §3.6, D-1674,
//! D-1675): the edits of the importers that follow a module's move.
//!
//! [`will_rename`] reads each moved file or folder as the module files it
//! moves, maps each old and new path to its module path with
//! `cheby_driver::module_file` (G1 plan §1.4), and refuses a request with a move
//! that has no module meaning (D-1675): files of more than one package, the
//! root module `src/main.cheby` moved away, a file made the root module, a file
//! that leaves `src/` or its package, and a name that is not a valid module name
//! (E0037). A move onto a module file is refused too: "`src/b.cheby` already
//! exists", or, when the request moves that file away, as a swap does, "a
//! move that swaps modules is not supported" (D-1724). Every module must
//! parse (X §3.6 step 3).
//!
//! The edits, in each module that imports a moved module (X §3.6 step 4):
//!
//! - The import's module path is written anew.
//! - When the last segment changes and the import has no `as` and no list, each
//!   qualifier `old::` of the file becomes `new::`. When `new` is not free in the
//!   file, the qualifiers stay and the import gets `as old` instead.
//! - `import a::old::{x}` and `import a::old as o` need only the path.
//! - An import block whose order the new paths change is written again in
//!   `cheby fmt`'s order (D-1148), since a move's edit must leave every
//!   touched line a fixed point (D-1676).
//!
//! `verify` then checks the edit with the files moved: one module path pair per
//! moved module (D-1674), then, for a module whose last segment changes, the
//! pair `old_last::` to `new_last::`, as an importer writes its items
//! (D-1715), and no check for errors that go away (D-1668), so a move that
//! repairs a broken import passes. A new error refuses the move with
//! "Moving would cause: …" (D-1724).
//!
//! The client moves the files whatever the answer, so a move with nothing to
//! edit has no plan, and a refusal says why.
//!
//! In a std checkout's session every move is refused (D-1706): it renames a
//! std module, which leaves `std_lib.rs`'s list stale, so the refusal says
//! where std's modules are listed and specified.

use std::path::{Component, Path, PathBuf};

use cheby_driver::analysis::{
    FileLocation, ModuleId, ModulePath, ModuleView, PlainEdit, Snapshot, TextRange, TextSize,
    VerifyRequest,
};
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::Refusal;
use super::imports::{binding, path_text};
use super::rename::{Action, RenameError, RenamePlan, check};
use crate::features::index::{Index, Meaning, Role};
use crate::features::{Analyzed, CancelToken};

/// The directory of a package's modules (§7.2).
const SOURCE_DIR: &str = "src";

/// D-1706's refusal of a move in a std checkout.
pub(crate) const STD_MOVE: &str = "std's modules are listed in `crates/cheby_driver/src/std_lib.rs` and specified in `docs/stdlib/`";

/// One module file that a request moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Move {
    /// Its path in the snapshot.
    pub old: PathBuf,
    /// Its path after the move.
    pub new: PathBuf,
    /// The module.
    pub module: ModuleId,
    /// Its module path before the move.
    pub from: ModulePath,
    /// Its module path after the move.
    pub to: ModulePath,
}

/// The plan that follows `requested`, each an old and a new path, a file or
/// a folder. `None` when no module moves: a loose file in single-file mode,
/// a file outside `src/`, a folder that holds no module, or the package's
/// own directory, whose module paths do not change. A plan may have no edit
/// when nothing imports a moved module.
///
/// # Errors
///
/// [`RenameError::Refused`] for a move with no module meaning (D-1675), a
/// move in a std checkout (D-1706), a module that does not parse, or an
/// edit that `verify` refuses (D-1389);
/// [`RenameError::Stale`] when a closed file changed on disk, whose
/// refusal says the imports were not updated (D-1711); and
/// [`RenameError::Cancelled`].
pub(crate) fn will_rename(
    analyzed: &Analyzed,
    requested: &[(PathBuf, PathBuf)],
    cancel: &CancelToken,
) -> Result<Option<RenamePlan>, RenameError> {
    let snapshot = &*analyzed.snapshot;
    // The request reached the checkout's session by its first old path,
    // a file of the checkout's `std/` (D-1699, D-1706).
    if snapshot.is_std_checkout() && !requested.is_empty() {
        return Err(Refusal(STD_MOVE.to_owned()).into());
    }
    if snapshot.is_single_file() {
        return Ok(None);
    }
    let moves = moves(snapshot, requested)?;
    if moves.is_empty() {
        return Ok(None);
    }
    if let Some(path) = snapshot.status().broken.iter().next() {
        return Err(Refusal(format!("`{}` has syntax errors", shown(snapshot, path))).into());
    }
    cancel.check()?;
    let edits = edits(analyzed.index(), &moves, cancel)?;
    cancel.check()?;
    let files: Vec<(PathBuf, PathBuf)> = moves
        .iter()
        .map(|m| (m.old.clone(), m.new.clone()))
        .collect();
    let mut paths: Vec<(String, String)> = moves
        .iter()
        .map(|m| (path_text(&m.from), path_text(&m.to)))
        .collect();
    // After the whole paths: a message names an imported module's item by
    // the module's last segment, `util::Shape` (D-1715).
    for m in &moves {
        if let (Some(old), Some(new)) = (m.from.segments().last(), m.to.segments().last())
            && old != new
        {
            let pair = (format!("{old}::"), format!("{new}::"));
            if !paths.contains(&pair) {
                paths.push(pair);
            }
        }
    }
    let renamed: Vec<(&str, &str)> = paths
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect();
    check(
        analyzed,
        &VerifyRequest {
            edits: &edits,
            moves: &files,
            renamed: &renamed,
            hides: false,
            written: &[],
        },
        Action::Move,
    )
    .map_err(|error| match error {
        RenameError::Stale { path, .. } => RenameError::Stale {
            refusal: Refusal(format!(
                "`{}` changed on disk, so imports were not updated",
                shown(snapshot, &path)
            )),
            path,
        },
        RenameError::Refused(_) | RenameError::Cancelled => error,
    })?;
    Ok(Some(RenamePlan { edits, note: None }))
}

/// The module files that `requested` moves, a folder expanded to the
/// modules below it, in request order, each once. A file that is no module
/// of the snapshot is left out, and so is a move that keeps a module's
/// path.
///
/// # Errors
///
/// A [`Refusal`] for the first move with no module meaning (D-1675), and
/// for a move onto a module file (D-1724).
pub(crate) fn moves(
    snapshot: &Snapshot,
    requested: &[(PathBuf, PathBuf)],
) -> Result<Vec<Move>, Refusal> {
    let root = snapshot.root();
    let Some(package) = package_name(snapshot) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<Move> = Vec::new();
    for (old, new) in requested {
        if !old.starts_with(root) {
            // A path in no package, such as a folder of notes beside it,
            // moves no module; one in another package is refused (D-1675).
            if cheby_driver::find_package_root(old).is_some() {
                return Err(Refusal("the files are in more than one package".to_owned()));
            }
            continue;
        }
        // The package's own directory: every module keeps its path.
        if old == root {
            continue;
        }
        for (file_old, view) in moved_files(snapshot, old) {
            let rest = file_old.strip_prefix(old).unwrap_or_else(|_| Path::new(""));
            let file_new = if rest.as_os_str().is_empty() {
                new.clone()
            } else {
                new.join(rest)
            };
            let Some(from) = view.path() else {
                continue;
            };
            let to = target(snapshot, &package, &file_old, &file_new)?;
            if from.segments().len() == 1 {
                return Err(Refusal(format!(
                    "`{}` is the package's root module and cannot move",
                    shown(snapshot, &file_old)
                )));
            }
            if to.segments().len() == 1 {
                return Err(Refusal(format!(
                    "`{}` would become the package's root module `{}`",
                    shown(snapshot, &file_old),
                    shown(snapshot, &file_new)
                )));
            }
            if *from == to || out.iter().any(|m| m.old == file_old) {
                continue;
            }
            out.push(Move {
                old: file_old,
                new: file_new,
                module: view.id(),
                from: from.clone(),
                to,
            });
        }
    }
    onto_modules(snapshot, &out)?;
    Ok(out)
}

/// Refuses a move onto a module file of the snapshot (D-1724): one that
/// stays is "already exists", and one that the request moves away, as a
/// swap does, is not supported.
///
/// # Errors
///
/// A [`Refusal`] for the first such move.
fn onto_modules(snapshot: &Snapshot, moves: &[Move]) -> Result<(), Refusal> {
    for m in moves {
        let taken = snapshot
            .file_of(&m.new)
            .and_then(|file| snapshot.module_at(file))
            .is_some_and(ModuleView::is_own);
        if !taken {
            continue;
        }
        if moves.iter().any(|other| other.old == m.new) {
            return Err(Refusal(
                "a move that swaps modules is not supported".to_owned(),
            ));
        }
        return Err(Refusal(format!(
            "`{}` already exists",
            shown(snapshot, &m.new)
        )));
    }
    Ok(())
}

/// The name of the package: the first segment of its modules' paths.
fn package_name(snapshot: &Snapshot) -> Option<String> {
    snapshot
        .own_modules()
        .find_map(|view| view.path()?.segments().first().cloned())
}

/// The module files of the snapshot that a move of `old` moves: the file
/// itself, or every module file below the folder `old`, by path.
fn moved_files<'s>(snapshot: &'s Snapshot, old: &Path) -> Vec<(PathBuf, ModuleView<'s>)> {
    if let Some(view) = snapshot
        .file_of(old)
        .and_then(|file| snapshot.module_at(file))
        .filter(|view| view.is_own())
    {
        return vec![(old.to_path_buf(), view)];
    }
    let mut found: Vec<(PathBuf, ModuleView<'s>)> = snapshot
        .own_modules()
        .filter_map(|view| match snapshot.location(view.file()?)? {
            FileLocation::Package(path) if path.starts_with(old) && path != old => {
                Some((path.clone(), view))
            }
            FileLocation::Package(_)
            | FileLocation::Std(_)
            | FileLocation::Dependency(_)
            | FileLocation::Synthetic => None,
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The module path of `file_new`, where `file_old` moves, in `package`.
///
/// # Errors
///
/// A [`Refusal`] when `file_new` is no module file of the package: it is
/// outside the package or `src/`, a name on its path is not a valid module
/// name (E0037), or it is no `.cheby` file that a module can be.
fn target(
    snapshot: &Snapshot,
    package: &str,
    file_old: &Path,
    file_new: &Path,
) -> Result<ModulePath, Refusal> {
    let root = snapshot.root();
    let old = shown(snapshot, file_old);
    let Ok(relative) = file_new.strip_prefix(root) else {
        return Err(Refusal(format!("`{old}` would leave its package")));
    };
    if let Some(module) = cheby_driver::module_file(root, package, file_new) {
        return Ok(module.path);
    }
    let names: Vec<&str> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_str().unwrap_or("\u{fffd}")),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => None,
        })
        .collect();
    let Some((&name, dirs)) = names.split_last() else {
        return Err(Refusal(format!("`{old}` would leave `src/`")));
    };
    match dirs.split_first() {
        Some((&first, _)) if first == SOURCE_DIR => {}
        Some(_) | None => {
            return Err(Refusal(format!("`{old}` would leave `src/`")));
        }
    }
    let stem = name
        .strip_suffix(cheby_driver::EXTENSION)
        .and_then(|s| s.strip_suffix('.'));
    let invalid = dirs
        .iter()
        .skip(1)
        .copied()
        .chain(stem)
        .find(|name| !cheby_driver::is_valid_name(name));
    // A hidden file or directory is no module, and no E0037 either.
    let hidden = name.starts_with('.') || dirs.iter().any(|d| d.starts_with('.'));
    match (stem, invalid) {
        (Some(_), Some(invalid)) if !hidden => Err(not_a_module_name(invalid)),
        (Some(_) | None, _) => Err(Refusal(format!(
            "`{}` would not be a module file",
            shown(snapshot, file_new)
        ))),
    }
}

/// E0037's refusal for `name`, with the name `cheby_driver::suggest_name`
/// offers.
fn not_a_module_name(name: &str) -> Refusal {
    let hint = cheby_driver::suggest_name(name)
        .filter(|s| cheby_driver::is_valid_name(s))
        .map_or_default(|s| format!(": try `{s}`"));
    Refusal(format!("`{name}` is not a valid module name (E0037){hint}"))
}

/// `path` as the user reads it: from the package's root, with `/`.
/// A path outside the root is shown whole.
fn shown(snapshot: &Snapshot, path: &Path) -> String {
    let Ok(relative) = path.strip_prefix(snapshot.root()) else {
        return path.display().to_string();
    };
    let names: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    names.join("/")
}

/// What a move does to one import: the module path it writes, and the alias
/// it adds.
#[derive(Clone, Debug)]
struct ImportChange {
    /// The new module path's text.
    path: String,
    /// The text from the start of the path to its end.
    range: TextRange,
    /// `as old`'s name, when the import gets one.
    alias: Option<String>,
}

/// The edits of every importer of the modules `moves` moves, sorted by path,
/// then by start.
///
/// # Errors
///
/// [`RenameError::Cancelled`] when `cancel` is set, checked per module.
fn edits(
    index: Index<'_>,
    moves: &[Move],
    cancel: &CancelToken,
) -> Result<Vec<PlainEdit>, RenameError> {
    let snapshot = index.snapshot();
    let mut out = Vec::new();
    for view in snapshot.own_modules() {
        cancel.check()?;
        let (Some(file), Some(tree)) = (view.file(), view.tree()) else {
            continue;
        };
        let (Some(FileLocation::Package(path)), Some(source)) =
            (snapshot.location(file), snapshot.source(file))
        else {
            continue;
        };
        let text = source.text();
        let imports: Vec<SyntaxNode<'_>> = tree
            .root()
            .children()
            .filter(|c| c.kind() == K::IMPORT_DECL)
            .collect();
        let mut changes: Vec<Option<ImportChange>> = Vec::with_capacity(imports.len());
        let mut qualifiers: Vec<(TextRange, String)> = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        for import in &imports {
            let change = import_change(index, view, *import, moves, &mut taken, &mut qualifiers);
            changes.push(change);
        }
        if changes.iter().all(Option::is_none) {
            continue;
        }
        let mut file_edits = qualifiers;
        // The imports whose block the loop below has written.
        let mut done: Vec<usize> = Vec::new();
        for block in blocks(&imports, text) {
            let (Some(&first), Some(&last)) = (block.first(), block.last()) else {
                continue;
            };
            let members: Vec<(SyntaxNode<'_>, Option<&ImportChange>)> = block
                .iter()
                .filter_map(|&n| Some((*imports.get(n)?, changes.get(n)?.as_ref())))
                .collect();
            if members.iter().all(|(_, change)| change.is_none()) {
                continue;
            }
            let keys: Vec<(String, String)> = members
                .iter()
                .map(|(node, change)| sort_key(*node, *change))
                .collect();
            if keys.is_sorted() {
                file_edits.extend(members.iter().filter_map(|(_, c)| *c).map(token_edit));
                done.extend(block.iter().copied());
                continue;
            }
            let mut order: Vec<usize> = (0..members.len()).collect();
            order.sort_by(|&a, &b| keys.get(a).cmp(&keys.get(b)));
            let written: Vec<String> = order
                .iter()
                .filter_map(|&k| members.get(k))
                .map(|(node, change)| rewritten(*node, *change, text))
                .collect();
            let (Some(start), Some(end)) = (
                imports.get(first).map(|n| n.text_range().start()),
                imports.get(last).map(|n| n.text_range().end()),
            ) else {
                continue;
            };
            // The text between the block's first two imports, `\n` or
            // `\r\n`, joins them again, so the file keeps its line ends.
            let separator = block
                .get(1)
                .and_then(|&second| {
                    let from = imports.get(first)?.text_range().end();
                    let to = imports.get(second)?.text_range().start();
                    TextRange::new(from, to).slice(text)
                })
                .unwrap_or("\n");
            file_edits.push((TextRange::new(start, end), written.join(separator)));
            done.extend(block.iter().copied());
        }
        // Imports in no block of two or more keep their place.
        file_edits.extend(
            changes
                .iter()
                .enumerate()
                .filter(|(n, _)| !done.contains(n))
                .filter_map(|(_, change)| change.as_ref())
                .map(token_edit),
        );
        file_edits.sort_by_key(|(range, _)| (range.start(), range.end()));
        out.extend(
            file_edits
                .into_iter()
                .map(|(range, replacement)| PlainEdit {
                    path: path.clone(),
                    range,
                    replacement,
                }),
        );
    }
    out.sort_by(|a, b| {
        (&a.path, a.range.start(), a.range.end()).cmp(&(&b.path, b.range.start(), b.range.end()))
    });
    Ok(out)
}

/// What a move does to `import`, a top-level import of `view`, or `None`
/// when it imports no moved module. When the import's name changes with
/// the module's last segment, it pushes the rewritten qualifiers of the
/// file to `qualifiers`, and the new name to `taken`, so a second import
/// cannot take the same name.
fn import_change(
    index: Index<'_>,
    view: ModuleView<'_>,
    import: SyntaxNode<'_>,
    moves: &[Move],
    taken: &mut Vec<String>,
    qualifiers: &mut Vec<(TextRange, String)>,
) -> Option<ImportChange> {
    let path = import.first_child_of_kind(K::MODULE_PATH)?;
    let segments: Vec<SyntaxToken<'_>> = path
        .tokens()
        .filter(|t| t.kind() == K::LOWER_IDENT)
        .collect();
    let written: Vec<&str> = segments.iter().map(|t| t.text()).collect();
    let found = moves.iter().find(|m| {
        m.from
            .segments()
            .iter()
            .map(String::as_str)
            .eq(written.iter().copied())
    })?;
    let range = TextRange::new(
        segments.first()?.text_range().start(),
        segments.last()?.text_range().end(),
    );
    let mut change = ImportChange {
        path: path_text(&found.to),
        range,
        alias: None,
    };
    let (Some(old), Some(new)) = (found.from.segments().last(), found.to.segments().last()) else {
        return Some(change);
    };
    let bare = import.first_child_of_kind(K::RENAME).is_none()
        && import.first_child_of_kind(K::IMPORT_LIST).is_none();
    if !bare || old == new {
        return Some(change);
    }
    let snapshot = index.snapshot();
    let free =
        !taken.contains(new) && binding(snapshot, view, &found.to).is_some_and(|name| name == *new);
    if !free {
        change.alias = Some(old.clone());
        return Some(change);
    }
    taken.push(new.clone());
    if let Some(module) = index.module(view.id()) {
        let text = view
            .file()
            .and_then(|file| snapshot.source(file))
            .map(cheby_driver::analysis::SourceFile::text);
        for occurrence in &module.occurrences {
            let names_it = module
                .entry(occurrence.symbol)
                .is_some_and(|entry| entry.meaning == Meaning::Module(found.module));
            let written = text.and_then(|text| occurrence.range.slice(text));
            if occurrence.role == Role::Qualifier && names_it && written == Some(old.as_str()) {
                qualifiers.push((occurrence.range, new.clone()));
            }
        }
    }
    Some(change)
}

/// The token edit of `change`: the module path, and the alias after it.
fn token_edit(change: &ImportChange) -> (TextRange, String) {
    let alias = change
        .alias
        .as_ref()
        .map_or_default(|alias| format!(" as {alias}"));
    (change.range, format!("{}{alias}", change.path))
}

/// The text of `import` with `change` applied, from the import's start to its
/// end. `text` is the file's.
fn rewritten(import: SyntaxNode<'_>, change: Option<&ImportChange>, text: &str) -> String {
    let range = import.text_range();
    let whole = range.slice(text).unwrap_or_default().to_owned();
    let Some(change) = change else {
        return whole;
    };
    let at = |offset: TextSize| usize::try_from(offset.saturating_sub(range.start())).ok();
    let (Some(start), Some(end)) = (at(change.range.start()), at(change.range.end())) else {
        return whole;
    };
    let (Some(before), Some(after)) = (whole.get(..start), whole.get(end..)) else {
        return whole;
    };
    let alias = change
        .alias
        .as_ref()
        .map_or_default(|alias| format!(" as {alias}"));
    format!("{before}{}{alias}{after}", change.path)
}

/// The key `cheby fmt` sorts `import` by once `change` is applied (D-1148):
/// its module path's text, then its tokens joined by single spaces.
fn sort_key(import: SyntaxNode<'_>, change: Option<&ImportChange>) -> (String, String) {
    let mut path = String::new();
    let mut whole: Vec<String> = Vec::new();
    let mut in_path = false;
    for token in import.tokens() {
        if token.kind().is_trivia() || token.kind() == K::NEWLINE {
            continue;
        }
        let is_path = token
            .parent()
            .is_some_and(|parent| parent.kind() == K::MODULE_PATH);
        match (is_path, change) {
            (true, Some(change)) => {
                if !in_path {
                    path.clone_from(&change.path);
                    whole.extend(spaced_path(&change.path));
                    if let Some(alias) = &change.alias {
                        whole.extend(["as".to_owned(), alias.clone()]);
                    }
                }
            }
            (true, None) => {
                path.push_str(token.text());
                whole.push(token.text().to_owned());
            }
            (false, _) => whole.push(token.text().to_owned()),
        }
        in_path = is_path;
    }
    (path, whole.join(" "))
}

/// A module path's text as its tokens: `a::b` as `a`, `::`, `b`.
fn spaced_path(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (n, segment) in path.split("::").enumerate() {
        if n > 0 {
            out.push("::".to_owned());
        }
        out.push(segment.to_owned());
    }
    out
}

/// The import blocks of a file whose top-level imports are `imports`, as
/// indexes into it (D-1148): runs of two or more imports on consecutive
/// lines, with nothing but white space and one line break between each and
/// the next, none with a comment inside. An import with a comment after it
/// on its line ends the run before it, as `cheby fmt` keeps it in place.
fn blocks(imports: &[SyntaxNode<'_>], text: &str) -> Vec<Vec<usize>> {
    let commented = |node: &SyntaxNode<'_>| {
        node.tokens()
            .any(|t| matches!(t.kind(), K::COMMENT | K::DOC_COMMENT | K::MODULE_DOC))
    };
    let gap = |from: TextSize, to: TextSize| TextRange::new(from, to).slice(text).unwrap_or("x");
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    let close = |run: &mut Vec<usize>, out: &mut Vec<Vec<usize>>| {
        if let Some(&last) = run.last()
            && let Some(node) = imports.get(last)
        {
            let end = node.text_range().end();
            let rest = text
                .get(usize::try_from(end).unwrap_or(usize::MAX)..)
                .unwrap_or("");
            let line = rest.split('\n').next().unwrap_or("");
            if !line.trim().is_empty() {
                run.pop();
            }
        }
        if run.len() > 1 {
            out.push(std::mem::take(run));
        }
        run.clear();
    };
    for (n, node) in imports.iter().enumerate() {
        if commented(node) {
            close(&mut run, &mut out);
            continue;
        }
        let joined = run
            .last()
            .and_then(|&prev| imports.get(prev))
            .is_some_and(|prev| {
                let between = gap(prev.text_range().end(), node.text_range().start());
                between.matches('\n').count() == 1 && between.chars().all(char::is_whitespace)
            });
        if !joined {
            close(&mut run, &mut out);
        }
        run.push(n);
    }
    close(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    //! The path mapping, the importers' edits and every refusal, with the
    //! helpers of `ide::hover::tests`. Each test runs only the front end, so
    //! it runs on Windows too (D-1516, G1 plan §1.10).

    use std::path::PathBuf;

    use super::{Move, moves, will_rename};
    use crate::features::ide::Refusal;
    use crate::features::ide::hover::tests::{Temp, analyze};
    use crate::features::ide::rename::{RenameError, RenamePlan};
    use crate::features::{Analyzed, CancelToken};

    /// The package `app` with `files`, paths below `src/` that may name
    /// folders, and its Q run.
    struct Case {
        temp: Temp,
        analyzed: Analyzed,
    }

    impl Case {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let temp = Temp::package(name, &[]);
            for (path, text) in files {
                let path = temp.src(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            let analyzed = analyze(&temp.0, &[]);
            Self { temp, analyzed }
        }

        /// A request's pairs, each path below the package.
        fn pairs(&self, pairs: &[(&str, &str)]) -> Vec<(PathBuf, PathBuf)> {
            pairs
                .iter()
                .map(|(old, new)| (self.temp.0.join(old), self.temp.0.join(new)))
                .collect()
        }

        fn moves(&self, pairs: &[(&str, &str)]) -> Result<Vec<Move>, Refusal> {
            moves(&self.analyzed.snapshot, &self.pairs(pairs))
        }

        fn will_rename(&self, pairs: &[(&str, &str)]) -> Result<Option<RenamePlan>, RenameError> {
            will_rename(&self.analyzed, &self.pairs(pairs), &CancelToken::new())
        }

        /// The plan of `pairs`, which must be accepted.
        #[track_caller]
        fn plan(&self, pairs: &[(&str, &str)]) -> RenamePlan {
            match self.will_rename(pairs) {
                Ok(Some(plan)) => plan,
                other => panic!("expected a plan, got {other:?}"),
            }
        }

        /// The text of `src/<file>` with the edits of `plan` applied.
        fn applied(&self, plan: &RenamePlan, file: &str) -> String {
            let path = self.temp.src(file);
            let mut text = std::fs::read_to_string(&path).unwrap();
            let mut edits: Vec<_> = plan.edits.iter().filter(|e| e.path == path).collect();
            edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
            for edit in edits {
                let start = usize::try_from(edit.range.start()).unwrap();
                let end = usize::try_from(edit.range.end()).unwrap();
                text.replace_range(start..end, &edit.replacement);
            }
            text
        }

        /// The files `plan` edits, below `src/`.
        fn edited(&self, plan: &RenamePlan) -> Vec<String> {
            let mut files: Vec<String> = plan
                .edits
                .iter()
                .map(|e| {
                    let relative = e.path.strip_prefix(self.temp.src("")).unwrap();
                    relative.to_string_lossy().replace('\\', "/")
                })
                .collect();
            files.dedup();
            files
        }

        /// The message of a request that must be refused.
        #[track_caller]
        fn refused(&self, pairs: &[(&str, &str)]) -> String {
            match self.will_rename(pairs) {
                Err(RenameError::Refused(Refusal(message))) => message,
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }

    const UTIL: &str =
        "pub type Shape {\n  Shape(Int),\n}\n\npub fn greet() -> String {\n  \"hi\"\n}\n";

    const MAIN: &str = "import app::util\nimport app::util::{greet}\nimport std::io\n\nfn main() {\n  let s: util::Shape = util::Shape(1)\n  let _ = s\n  io::println(util::greet())\n  io::println(greet())\n}\n";

    /// A file renamed: its importers' paths, and each qualifier of a bare
    /// import, follow it (X §3.6 step 4).
    #[test]
    fn a_renamed_file_rewrites_its_importers() {
        let case = Case::new("renamed", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        let plan = case.plan(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(plan.note, None);
        assert_eq!(case.edited(&plan), ["main.cheby"]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            MAIN.replace("app::util", "app::helpers")
                .replace("util::Shape", "helpers::Shape")
                .replace("util::greet()", "helpers::greet()")
        );
        let [found] = case
            .moves(&[("src/util.cheby", "src/helpers.cheby")])
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(found.from.to_string(), "app::util");
        assert_eq!(found.to.to_string(), "app::helpers");
        assert_eq!(found.old, case.temp.src("util.cheby"));
        assert_eq!(found.new, case.temp.src("helpers.cheby"));
    }

    /// A move into a folder adds a segment: the paths change, and the
    /// last segment, and so the qualifiers, do not.
    #[test]
    fn a_move_into_a_folder_writes_only_the_paths() {
        let case = Case::new("folder", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        let plan = case.plan(&[("src/util.cheby", "src/lib/util.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            MAIN.replace("app::util", "app::lib::util")
        );
        assert_eq!(plan.edits.len(), 2, "{plan:?}");
    }

    /// A folder moves every module below it, and a file of the folder's
    /// name, which is another module (§7.2), stays.
    #[test]
    fn a_folder_rename_moves_every_module_below_it() {
        let main = "import app::geo\nimport app::geo::lines\nimport app::geo::shapes\n\nfn main() {\n  assert geo::origin() + lines::one() + shapes::two() == 3\n}\n";
        let case = Case::new(
            "geo",
            &[
                ("main.cheby", main),
                ("geo.cheby", "pub fn origin() -> Int {\n  0\n}\n"),
                ("geo/lines.cheby", "pub fn one() -> Int {\n  1\n}\n"),
                ("geo/shapes.cheby", "pub fn two() -> Int {\n  2\n}\n"),
            ],
        );
        let found: Vec<String> = case
            .moves(&[("src/geo", "src/plane")])
            .unwrap()
            .iter()
            .map(|m| format!("{} {}", m.from, m.to))
            .collect();
        assert_eq!(
            found,
            [
                "app::geo::lines app::plane::lines",
                "app::geo::shapes app::plane::shapes"
            ]
        );
        let plan = case.plan(&[("src/geo", "src/plane")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace("app::geo::", "app::plane::")
        );
    }

    /// When the new last segment is not free in the importer, the import
    /// keeps the old name with `as` and no qualifier changes. The alias
    /// sorts the import after the list (D-1148), so the block is written
    /// again.
    #[test]
    fn a_name_that_is_not_free_keeps_the_old_one_with_as() {
        let main = MAIN.replace(
            "  io::println(greet())\n",
            "  io::println(greet())\n  let helpers = 1\n  assert helpers == 1\n",
        );
        let case = Case::new("clash", &[("main.cheby", &main), ("util.cheby", UTIL)]);
        let plan = case.plan(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace(
                "import app::util\nimport app::util::{greet}\n",
                "import app::helpers::{greet}\nimport app::helpers as util\n"
            )
        );
    }

    /// Two moves to one last segment in one file: the second import keeps
    /// its old name, so the two do not clash.
    #[test]
    fn two_moves_never_bind_one_name() {
        let main = "import app::a::item\nimport app::b::item as b_item\nimport app::c\nimport app::d\n\nfn main() {\n  assert c::one() + d::two() + item::x() + b_item::x() == 5\n}\n";
        let one = |n: &str| format!("pub fn one() -> Int {{\n  {n}\n}}\n");
        let case = Case::new(
            "two",
            &[
                ("main.cheby", main),
                ("c.cheby", &one("1")),
                ("d.cheby", "pub fn two() -> Int {\n  2\n}\n"),
                ("a/item.cheby", "pub fn x() -> Int {\n  1\n}\n"),
                ("b/item.cheby", "pub fn x() -> Int {\n  1\n}\n"),
            ],
        );
        let plan = case.plan(&[
            ("src/c.cheby", "src/e.cheby"),
            ("src/d.cheby", "src/e2.cheby"),
        ]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace(
                "import app::c\nimport app::d\n",
                "import app::e\nimport app::e2\n"
            )
            .replace("c::one()", "e::one()")
            .replace("d::two()", "e2::two()")
        );
        let plan = case.plan(&[
            ("src/c.cheby", "src/x/e.cheby"),
            ("src/d.cheby", "src/y/e.cheby"),
        ]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace(
                "import app::c\nimport app::d\n",
                "import app::x::e\nimport app::y::e as d\n"
            )
            .replace("c::one()", "e::one()")
        );
    }

    /// An import with `as`, or with a list, binds no module name that
    /// changes, so only its path is written.
    #[test]
    fn an_aliased_import_and_a_list_need_only_the_path() {
        let main = "import app::util::{greet}\nimport app::util as u\n\nfn main() {\n  assert u::greet() == greet()\n}\n";
        let case = Case::new("alias", &[("main.cheby", main), ("util.cheby", UTIL)]);
        let plan = case.plan(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace("app::util", "app::helpers")
        );
        assert_eq!(plan.edits.len(), 2, "{plan:?}");
    }

    /// A path that changes the order of its block writes the block again in
    /// `cheby fmt`'s order (D-1148). A block the move keeps in order, and
    /// an import with a comment after it, which `cheby fmt` never moves, keep
    /// their places.
    #[test]
    fn a_block_out_of_order_is_written_in_fmt_order() {
        let main = "import app::alpha\nimport app::beta\nimport std::io\n\nfn main() {\n  io::println(alpha::name() + beta::name())\n}\n";
        let name = "pub fn name() -> String {\n  \"n\"\n}\n";
        let case = Case::new(
            "order",
            &[
                ("main.cheby", main),
                ("alpha.cheby", name),
                ("beta.cheby", name),
            ],
        );
        let plan = case.plan(&[("src/alpha.cheby", "src/zeta.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            "import app::beta\nimport app::zeta\nimport std::io\n\nfn main() {\n  io::println(zeta::name() + beta::name())\n}\n"
        );
        let plan = case.plan(&[("src/alpha.cheby", "src/aa.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace("alpha", "aa")
        );
        let commented = main.replace("import app::alpha\n", "import app::alpha // first\n");
        let case = Case::new(
            "commented",
            &[
                ("main.cheby", &commented),
                ("alpha.cheby", name),
                ("beta.cheby", name),
            ],
        );
        let plan = case.plan(&[("src/alpha.cheby", "src/zeta.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            commented.replace("alpha", "zeta")
        );
    }

    /// A move does not ask that no error go away (D-1668): one that
    /// repairs a broken import passes.
    #[test]
    fn a_move_that_repairs_a_broken_import_passes() {
        let other = "import app::helpers\n\npub fn twice() -> String {\n  helpers::greet() + helpers::greet()\n}\n";
        let case = Case::new(
            "repair",
            &[
                ("main.cheby", MAIN),
                ("util.cheby", UTIL),
                ("other.cheby", other),
            ],
        );
        let plan = case.plan(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(case.edited(&plan), ["main.cheby"]);
    }

    /// The errors before the move are read with each moved module's path
    /// as its new one (D-1674), so an E0042 that names the module is not
    /// new after it.
    #[test]
    fn a_message_that_names_the_module_is_read_with_the_move() {
        let main = MAIN.replace("util::greet()", "util::nope()");
        let case = Case::new("message", &[("main.cheby", &main), ("util.cheby", UTIL)]);
        let plan = case.plan(&[("src/util.cheby", "src/lib/util.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            main.replace("app::util", "app::lib::util")
        );
    }

    /// A package where `main` needs `util::Shape` to satisfy `Wrap`,
    /// which it does not: an E0063 that names `util::Shape`.
    const MAIN_WRAP: &str = "import app::util\n\ninterface Wrap {\n  fn wrap(Self) -> Int\n}\n\nfn wrapped<T: Wrap>(x: T) -> Int {\n  Wrap::wrap(x)\n}\n\nfn main() {\n  assert wrapped(util::make()) == 1\n}\n";
    const SHAPES: &str =
        "pub type Shape {\n  side: Int,\n}\n\npub fn make() -> Shape {\n  Shape { side: 1 }\n}\n";

    /// D-1715: a move that changes a module's last segment reads the
    /// E0063 that names `util::Shape` with `util::` as `helpers::`, so it
    /// is not new after the move.
    #[test]
    fn a_message_that_names_the_last_segment_is_read_with_the_move() {
        let case = Case::new(
            "last_segment",
            &[("main.cheby", MAIN_WRAP), ("util.cheby", SHAPES)],
        );
        let plan = case.plan(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            MAIN_WRAP
                .replace("app::util", "app::helpers")
                .replace("util::make", "helpers::make")
        );
    }

    /// D-1715: two modules of one last segment moved in one request to two
    /// new ones. The E0063 in an importer of the second names `util::Shape`,
    /// and is read with either new segment, so the move is not refused.
    #[test]
    fn two_moves_of_one_last_segment_read_it_each_way() {
        let user = MAIN_WRAP
            .replace("app::util", "app::b::util")
            .replace("fn main() {\n  assert", "pub fn run() -> Bool {\n ");
        let main = "import app::a::util\nimport app::user\n\nfn main() {\n  let _ = util::make()\n  assert user::run()\n}\n";
        let case = Case::new(
            "shared_segment",
            &[
                ("main.cheby", main),
                ("user.cheby", &user),
                ("a/util.cheby", SHAPES),
                ("b/util.cheby", SHAPES),
            ],
        );
        let plan = case.plan(&[
            ("src/a/util.cheby", "src/a/tools.cheby"),
            ("src/b/util.cheby", "src/b/helpers.cheby"),
        ]);
        assert_eq!(case.edited(&plan), ["main.cheby", "user.cheby"]);
    }

    /// Every move with no module meaning is refused, saying why (D-1675).
    #[test]
    fn each_move_with_no_module_meaning_is_refused() {
        let case = Case::new("refused", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        // A directory in no package until the test writes a manifest into
        // it, named for this process and removed on drop, so runs that
        // share the temporary directory do not meet.
        let guard = Temp(std::env::temp_dir().join(format!(
            "cheby_lsp_file_ops_elsewhere_{}",
            std::process::id()
        )));
        let _ = std::fs::remove_dir_all(&guard.0);
        let elsewhere = guard.0.clone();
        let rows: [(&str, &str, &str); 8] = [
            (
                "src/main.cheby",
                "src/start.cheby",
                "`src/main.cheby` is the package's root module and cannot move",
            ),
            (
                "src/util.cheby",
                "src/main.cheby",
                "`src/util.cheby` would become the package's root module `src/main.cheby`",
            ),
            (
                "src/util.cheby",
                "util.cheby",
                "`src/util.cheby` would leave `src/`",
            ),
            (
                "src/util.cheby",
                "src/My-Util.cheby",
                "`My-Util` is not a valid module name (E0037): try `my_util`",
            ),
            (
                "src/util.cheby",
                "src/Lib/util.cheby",
                "`Lib` is not a valid module name (E0037): try `lib`",
            ),
            (
                "src/util.cheby",
                "src/util.txt",
                "`src/util.txt` would not be a module file",
            ),
            (
                "src/util.cheby",
                "src/.util.cheby",
                "`src/.util.cheby` would not be a module file",
            ),
            ("src", "source", "`src/main.cheby` would leave `src/`"),
        ];
        for (old, new, message) in rows {
            assert_eq!(case.refused(&[(old, new)]), message, "{old} to {new}");
        }
        let away = [(
            case.temp.src("util.cheby"),
            elsewhere.join("src/util.cheby"),
        )];
        assert_eq!(
            moves(&case.analyzed.snapshot, &away),
            Err(Refusal(
                "`src/util.cheby` would leave its package".to_owned()
            ))
        );
        let other = [
            (case.temp.src("util.cheby"), case.temp.src("helpers.cheby")),
            (elsewhere.join("src/a.cheby"), elsewhere.join("src/b.cheby")),
        ];
        // A path in no package moves no module, and is left out.
        assert_eq!(
            moves(&case.analyzed.snapshot, &other)
                .unwrap()
                .iter()
                .map(|m| m.to.to_string())
                .collect::<Vec<_>>(),
            ["app::helpers"]
        );
        // One in another package is refused.
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            elsewhere.join("cheby.toml"),
            "name = \"other\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let found = moves(&case.analyzed.snapshot, &other);
        drop(guard);
        assert_eq!(
            found,
            Err(Refusal("the files are in more than one package".to_owned()))
        );
    }

    /// A block written again in a CRLF file keeps `\r\n` between its
    /// imports, so the file keeps one kind of line end.
    #[test]
    fn a_block_written_again_keeps_crlf() {
        let main = "import app::b\r\nimport app::m\r\n\r\nfn main() {\r\n  assert b::one() + m::one() == 2\r\n}\r\n";
        let module = "//! A module.\r\n\r\npub fn one() -> Int {\r\n  1\r\n}\r\n";
        let case = Case::new(
            "crlf",
            &[
                ("main.cheby", main),
                ("b.cheby", module),
                ("m.cheby", module),
            ],
        );
        let plan = case.plan(&[("src/b.cheby", "src/z.cheby")]);
        assert_eq!(
            case.applied(&plan, "main.cheby"),
            "import app::m\r\nimport app::z\r\n\r\nfn main() {\r\n  assert z::one() + m::one() == 2\r\n}\r\n"
        );
    }

    /// D-1724: a move onto a module file that stays is refused, and so is a
    /// request that moves a module onto one it moves away, as a swap does.
    #[test]
    fn a_move_onto_a_module_is_refused() {
        let case = Case::new(
            "onto",
            &[
                ("main.cheby", MAIN),
                ("util.cheby", UTIL),
                ("helpers.cheby", "pub fn one() -> Int {\n  1\n}\n"),
            ],
        );
        assert_eq!(
            case.refused(&[("src/util.cheby", "src/helpers.cheby")]),
            "`src/helpers.cheby` already exists"
        );
        assert_eq!(
            case.refused(&[
                ("src/util.cheby", "src/helpers.cheby"),
                ("src/helpers.cheby", "src/util.cheby"),
            ]),
            "a move that swaps modules is not supported"
        );
        // A folder moved onto another holds a module of the same path.
        let folders = Case::new(
            "onto_folder",
            &[
                (
                    "main.cheby",
                    "import app::a::x\nimport app::b::x as y\n\nfn main() {\n  assert x::one() == y::one()\n}\n",
                ),
                ("a/x.cheby", "pub fn one() -> Int {\n  1\n}\n"),
                ("b/x.cheby", "pub fn one() -> Int {\n  1\n}\n"),
            ],
        );
        assert_eq!(
            folders.refused(&[("src/a", "src/b")]),
            "`src/b/x.cheby` already exists"
        );
    }

    /// D-1724: a new error refuses a move with "Moving would cause", here
    /// from `check` with an edit that misses the importer.
    /// `a_move_that_makes_a_new_error_is_refused` reaches it through
    /// `will_rename`.
    #[test]
    fn a_new_error_refuses_a_move_with_its_own_verb() {
        let case = Case::new("verb", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        let moves = [(case.temp.src("util.cheby"), case.temp.src("helpers.cheby"))];
        let found = super::check(
            &case.analyzed,
            &cheby_driver::analysis::VerifyRequest {
                edits: &[],
                moves: &moves,
                renamed: &[("app::util", "app::helpers"), ("util::", "helpers::")],
                hides: false,
                written: &[],
            },
            super::Action::Move,
        );
        match found {
            Err(RenameError::Refused(Refusal(message))) => assert!(
                message.starts_with("Moving would cause: error[E0039]: "),
                "{message}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// D-1724: a move onto the path of a broken import, which the importer
    /// then imports twice, is refused by `verify` with "Moving would cause".
    #[test]
    fn a_move_that_makes_a_new_error_is_refused() {
        let main = "import app::a\nimport app::b\n\nfn main() {\n  assert a::one() == 1\n}\n";
        let case = Case::new(
            "twice",
            &[
                ("main.cheby", main),
                ("a.cheby", "pub fn one() -> Int {\n  1\n}\n"),
            ],
        );
        assert_eq!(
            case.refused(&[("src/a.cheby", "src/b.cheby")]),
            "Moving would cause: error[E0041]: `b` is imported twice at src/main.cheby:2:13"
        );
    }

    /// Every module must parse (X §3.6 step 3).
    #[test]
    fn a_syntax_error_elsewhere_refuses_the_move() {
        let case = Case::new(
            "broken",
            &[
                ("main.cheby", MAIN),
                ("util.cheby", UTIL),
                ("other.cheby", "fn broken( {\n"),
            ],
        );
        assert_eq!(
            case.refused(&[("src/util.cheby", "src/helpers.cheby")]),
            "`src/other.cheby` has syntax errors"
        );
    }

    /// No module moves: a file outside `src/`, a folder with no module, the
    /// package's own directory, and a move that keeps the path.
    #[test]
    fn a_move_of_no_module_has_no_plan() {
        let case = Case::new("none", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        std::fs::write(case.temp.0.join("notes.cheby"), "fn main() {}\n").unwrap();
        for (old, new) in [
            ("notes.cheby", "src/notes.cheby"),
            ("docs", "papers"),
            ("src/util.cheby", "src/./util.cheby"),
        ] {
            assert_eq!(case.will_rename(&[(old, new)]), Ok(None), "{old} to {new}");
        }
        let root = case.temp.0.clone();
        let renamed = root.with_file_name("cheby_lsp_file_ops_renamed_root");
        assert_eq!(
            will_rename(&case.analyzed, &[(root, renamed)], &CancelToken::new()),
            Ok(None)
        );
        // Moved, but imported by no module: a plan with no edit.
        let lone = Case::new(
            "lone",
            &[("main.cheby", "fn main() {}\n"), ("util.cheby", UTIL)],
        );
        assert_eq!(
            lone.will_rename(&[("src/util.cheby", "src/helpers.cheby")]),
            Ok(Some(RenamePlan::default()))
        );
    }

    /// A closed file that changed on disk since the run is `Stale`, and a
    /// cancelled request is cancelled.
    #[test]
    fn a_stale_file_and_a_cancel_stop_the_move() {
        let case = Case::new("stale", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        let cancel = CancelToken::new();
        cancel.cancel();
        let pairs = case.pairs(&[("src/util.cheby", "src/helpers.cheby")]);
        assert_eq!(
            will_rename(&case.analyzed, &pairs, &cancel),
            Err(RenameError::Cancelled)
        );
        std::fs::write(case.temp.src("util.cheby"), format!("{UTIL}\n")).unwrap();
        match case.will_rename(&[("src/util.cheby", "src/helpers.cheby")]) {
            Err(RenameError::Stale { refusal, path }) => {
                assert_eq!(path, case.temp.src("util.cheby"));
                assert_eq!(
                    refusal,
                    Refusal(
                        "`src/util.cheby` changed on disk, so imports were not updated".to_owned()
                    )
                );
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }

    /// D-1706: in a std checkout's session every move is refused, a file's
    /// and a folder's, with the message that says where std's modules are
    /// listed and specified, since a move renames a std module. The server
    /// refuses them from routing before a session is asked
    /// (`checkout_moves_are_refused_from_routing`); this is the session's
    /// own guard.
    #[test]
    fn a_move_in_a_std_checkout_is_refused() {
        let checkout = crate::features::ide::checkout_tests::Checkout::new("file_ops_move");
        let analyzed = checkout.analyze(&[]);
        for (old, new) in [
            ("std/list.cheby", "std/lists.cheby"),
            ("std/json", "std/js"),
            ("std/probe_a.cheby", "std/inner/probe_a.cheby"),
        ] {
            let pairs = [(checkout.at(old), checkout.at(new))];
            assert_eq!(
                will_rename(&analyzed, &pairs, &CancelToken::new()),
                Err(RenameError::Refused(Refusal(super::STD_MOVE.to_owned()))),
                "{old}"
            );
        }
        assert_eq!(
            super::STD_MOVE,
            "std's modules are listed in `crates/cheby_driver/src/std_lib.rs` and specified in \
             `docs/stdlib/`"
        );
    }

    /// Paths are shown from the package's root with `/`.
    #[test]
    fn paths_are_shown_from_the_root() {
        let case = Case::new("shown", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
        let snapshot = &case.analyzed.snapshot;
        assert_eq!(
            super::shown(snapshot, &case.temp.src("a/b.cheby")),
            "src/a/b.cheby"
        );
        let outside = case.temp.0.with_file_name("elsewhere.cheby");
        assert_eq!(
            super::shown(snapshot, &outside),
            outside.display().to_string()
        );
    }
}
