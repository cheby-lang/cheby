//! Import plans shared by completion and fixes (X §1.5 "Adding an
//! import", X §4.4, plan §1.11).
//!
//! [`import_plan`] says how code at one module names another module: by
//! the name an existing import binds, or by a new import, whose edit it
//! gives. The new import never makes an error:
//!
//! - Its name is the module's last segment when that is free at every
//!   point of the file. Otherwise it gets an alias, `std_list` for
//!   `std::list` or `<parent>_<last>` for a package module, since the plain
//!   name would clash with a local or an item (E0044, E0041, D-1395).
//! - A module that imports the file's module, directly or not, is refused,
//!   since the import would close a cycle (E0040). So is the module itself,
//!   a std module std does not ship, and a module of another package. In a
//!   std checkout, std's modules are the session's own: the cycle rule
//!   holds among them, and a std module the checkout lacks is refused
//!   (D-1700).
//! - It goes where `cheby fmt` would keep it (D-1030, D-1953): into the
//!   block of its class, the standard library's, other packages' or the
//!   package's own, before the first import there whose module path sorts
//!   after it in byte order, else after the block's last; a file with no
//!   block of its class gets one at its place, with a blank line between
//!   blocks. A file with no import before its first item gets it after its
//!   inner attributes, else after its `//!` block and a blank line, else at
//!   its start, in E0018's order.

use std::collections::BTreeSet;

use cheby_driver::analysis::{
    ModuleId, ModulePath, ModuleView, PackageId, Snapshot, StdIndex, TextRange, TextSize,
};
use cheby_syntax::imports::{ImportChunk, ImportClass, ImportSection, import_section};
use cheby_syntax::names::{NameCase, check_candidate};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode};

use crate::features::ide::{package_name, scope};

/// How code in one module names another module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportPlan {
    /// The name to qualify with: `list` in `list::map`.
    pub name: String,
    /// The import to add first, as an insertion: `None` when the module is
    /// imported already under `name`.
    pub edit: Option<(TextRange, String)>,
}

/// How code in `view`, a module of `snapshot`, names the module at
/// `target`, such as `std::list`. `std` knows the std modules the run did
/// not load. `None` when the module cannot be imported here without an
/// error: the module itself, a module that imports it (E0040), a std
/// module std does not ship, a module of another package, or a module
/// with no resolution to read names from.
pub(crate) fn import_plan(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    std: Option<&StdIndex>,
    target: &ModulePath,
) -> Option<ImportPlan> {
    let resolved = view.resolved()?;
    let tree = view.tree()?;
    if view.path() == Some(target) {
        return None;
    }
    let loaded = snapshot
        .table()
        .find(target)
        .filter(|&id| snapshot.interfaces().get(id).is_some());
    if let Some(id) = loaded
        && let Some(&name) = resolved.scope.import_names().modules.get(&id)
    {
        return Some(ImportPlan {
            name: snapshot.globals().names.get(name).to_owned(),
            edit: None,
        });
    }
    if !importable(snapshot, view, std, target, loaded) {
        return None;
    }
    let name = binding(snapshot, view, target)?;
    let path_text = path_text(target);
    let line = if *target.segments().last()? == name {
        format!("import {path_text}")
    } else {
        format!("import {path_text} as {name}")
    };
    let package = package_name(snapshot);
    let edit = placement(
        tree.root(),
        tree.text(),
        package.as_deref(),
        &path_text,
        &line,
    );
    Some(ImportPlan {
        name,
        edit: Some(edit),
    })
}

/// Whether `view` may import the module at `target`: a std module std
/// ships, or a module of its own package that does not import it. In a
/// std checkout, std's modules are the session's own (D-1700): one that
/// imports `view` is refused (E0040), and a module the run did not load
/// is not in the checkout, so importing it is E0039.
fn importable(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    std: Option<&StdIndex>,
    target: &ModulePath,
    loaded: Option<ModuleId>,
) -> bool {
    let is_std = target.segments().first().is_some_and(|s| s == "std");
    let table = snapshot.table();
    match loaded {
        Some(id) => {
            let Some(theirs) = table.get(id) else {
                return false;
            };
            if theirs.package == PackageId::STD {
                return !snapshot.is_std_checkout() || !imports_module(snapshot, id, view.id());
            }
            let ours = table.get(view.id()).map(|i| i.package);
            !is_std && ours == Some(theirs.package) && !imports_module(snapshot, id, view.id())
        }
        // A std module the run did not load: one std ships, by its index.
        None => {
            is_std
                && !snapshot.is_std_checkout()
                && std.is_some_and(|index| index.module(&path_text(target)).is_some())
        }
    }
}

/// Whether `from` imports `to`, directly or through other modules, so
/// that `to` importing `from` would close a cycle (E0040).
fn imports_module(snapshot: &Snapshot, from: ModuleId, to: ModuleId) -> bool {
    let mut seen: BTreeSet<ModuleId> = BTreeSet::new();
    let mut stack = vec![to];
    while let Some(module) = stack.pop() {
        if module == from {
            return true;
        }
        if seen.insert(module) {
            stack.extend(snapshot.importers(module).iter().copied());
        }
    }
    false
}

/// The name a new import of `target` binds in `view`: its last segment
/// when that is free at every point of the module, else an alias
/// (D-1395): `std_<last>` for std, `<parent>_<last>` for a package
/// module, then `_value`, `_value_2` and so on until one is free.
/// A file move asks it whether a moved module's new last segment is
/// free in an importer (X §3.6).
pub(crate) fn binding(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    target: &ModulePath,
) -> Option<String> {
    let segments = target.segments();
    let (last, parents) = segments.split_last()?;
    let free = |name: &str| {
        check_candidate(name, NameCase::Lower).is_ok()
            && scope::is_free_in_module(snapshot, view, name)
    };
    if free(last) {
        return Some(last.clone());
    }
    let alias = match parents.last() {
        Some(parent) => format!("{parent}_{last}"),
        None => last.clone(),
    };
    let value = format!("{alias}_value");
    std::iter::once(alias)
        .chain(std::iter::once(value.clone()))
        .chain((2..=u32::MAX).map(|n| format!("{value}_{n}")))
        .take(1000)
        .find(|name| free(name))
}

/// `path` as code writes it: `std::list`.
pub(crate) fn path_text(path: &ModulePath) -> String {
    path.segments().join("::")
}

/// The module path that `text`, such as `std::json::decode`, names.
pub(crate) fn parse_path(text: &str) -> Option<ModulePath> {
    let mut segments = text.split("::");
    let first = segments.next().filter(|s| !s.is_empty())?;
    Some(segments.fold(ModulePath::root(first), |path, s| path.child(s)))
}

/// The insertion of `line`, one import of the module `path`, into the
/// file of `root`, whose text is `text`, a file of the package named
/// `package` (D-1953). E0018's fix puts the imports that follow an item
/// where it puts them, one at a time (D-1744).
///
/// The import goes into the block of its class (`ImportClass`), before
/// the first import of that block that sorts after it, else after the
/// block's last. A file with no block of its class gets one, with a blank
/// line between it and its neighbours: before the first import of a later
/// class, else after the section's last import. An insertion before an
/// import goes before the comments that travel with it, so a section in
/// `cheby fmt`'s order stays in it (D-1953).
pub(crate) fn placement(
    root: SyntaxNode<'_>,
    text: &str,
    package: Option<&str>,
    path: &str,
    line: &str,
) -> (TextRange, String) {
    let section = import_section(root, package);
    let chunks: &[ImportChunk<'_>] = section.as_ref().map_or(&[], ImportSection::chunks);
    if let Some(last) = chunks.last() {
        let class = ImportClass::of(path.split("::").next().unwrap_or_default(), package);
        let key = (path.to_owned(), sort_text(line));
        let before = |chunk: &ImportChunk<'_>, insert: String| {
            (
                TextRange::empty(line_start(text, chunk.range().start())),
                insert,
            )
        };
        if let Some(next) = chunks.iter().find(|c| c.class == class && c.key > key) {
            return before(next, format!("{line}\n"));
        }
        if let Some(own) = chunks.iter().rev().find(|c| c.class == class) {
            return after_line(text, own.range().end(), format!("{line}\n"));
        }
        // A new block: before the first of a later block, else last.
        if let Some(next) = chunks.iter().find(|c| c.class > class) {
            return before(next, format!("{line}\n\n"));
        }
        return after_line(text, last.range().end(), format!("\n{line}\n"));
    }
    let anchor = root
        .children_with_tokens()
        .filter(|e| match e {
            SyntaxElement::Node(n) => n.kind() == K::INNER_ATTR,
            SyntaxElement::Token(t) => t.kind() == K::MODULE_DOC,
        })
        .map(|e| e.text_range().end())
        .last();
    let Some(anchor) = anchor else {
        // Nothing comes before imports: the file's start, and a blank line
        // before whatever follows.
        let blank = line_is_blank(text, 0);
        let gap = if text.is_empty() || blank { "" } else { "\n" };
        return (TextRange::empty(0), format!("{line}\n{gap}"));
    };
    // After the header's last line, with a blank line between, and one
    // before the item that follows.
    let next = next_line(text, anchor);
    match next {
        Some(next) if line_is_blank(text, next) => {
            let after = next_line(text, next);
            let gap = match after {
                Some(after) if !line_is_blank(text, after) => "\n",
                _ => "",
            };
            match after {
                Some(after) => (TextRange::empty(after), format!("{line}\n{gap}")),
                None => (TextRange::empty(text_len(text)), format!("{line}\n")),
            }
        }
        Some(next) => (TextRange::empty(next), format!("\n{line}\n\n")),
        None => after_line(text, anchor, format!("\n{line}\n")),
    }
}

/// `line`, one import as written, with its tokens joined by single spaces,
/// as `cheby_syntax::imports::import_key` joins them.
fn sort_text(line: &str) -> String {
    let mut spaced = line.replace("::", " :: ");
    for punct in ['{', '}', ','] {
        spaced = spaced.replace(punct, &format!(" {punct} "));
    }
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The insertion of `text` at the start of the line after the one that
/// holds `offset`, or at the end of the file after a line break when that
/// line is the last.
fn after_line(text: &str, offset: TextSize, insert: String) -> (TextRange, String) {
    match next_line(text, offset) {
        Some(at) => (TextRange::empty(at), insert),
        None => (TextRange::empty(text_len(text)), format!("\n{insert}")),
    }
}

/// The start of the line that holds `offset`.
pub(crate) fn line_start(text: &str, offset: TextSize) -> TextSize {
    let at = usize::try_from(offset).unwrap_or(0);
    let start = text
        .get(..at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |n| n.saturating_add(1));
    TextSize::try_from(start).unwrap_or(0)
}

/// The start of the line after the one that holds `offset`, if the line
/// ends with a line break.
pub(crate) fn next_line(text: &str, offset: TextSize) -> Option<TextSize> {
    let at = usize::try_from(offset).ok()?;
    let rest = text.get(at..)?;
    let n = rest.find('\n')?;
    TextSize::try_from(at.checked_add(n)?.checked_add(1)?).ok()
}

/// Whether the line that starts at `start` holds only white space. The
/// end of the file counts as blank.
fn line_is_blank(text: &str, start: TextSize) -> bool {
    let at = usize::try_from(start).unwrap_or(usize::MAX);
    let rest = text.get(at..).unwrap_or("");
    let line = rest.split('\n').next().unwrap_or("");
    line.trim().is_empty()
}

/// The length of `text` as a text offset.
fn text_len(text: &str) -> TextSize {
    TextSize::try_from(text.len()).unwrap_or(TextSize::MAX)
}

#[cfg(test)]
#[path = "imports_tests.rs"]
mod tests;
