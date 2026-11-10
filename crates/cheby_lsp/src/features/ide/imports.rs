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
//!   a std module std does not ship, and a module of another package.
//! - It goes where `cheby fmt` would keep it (D-1030, D-1148): into the
//!   first block of imports, before the first whose module path sorts
//!   after it in byte order, else after the block's last. A file with no
//!   import gets it after its inner attributes, else after its `//!` block
//!   and a blank line, else at its start, in E0018's order.

use std::collections::BTreeSet;

use cheby_driver::analysis::{
    ModuleId, ModulePath, ModuleView, PackageId, Snapshot, StdIndex, TextRange, TextSize,
};
use cheby_syntax::names::{NameCase, check_candidate};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::features::ide::scope;

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
    let edit = placement(tree.root(), tree.text(), &path_text, &line);
    Some(ImportPlan {
        name,
        edit: Some(edit),
    })
}

/// Whether `view` may import the module at `target`: a std module std
/// ships, or a module of its own package that does not import it.
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
                return true;
            }
            let ours = table.get(view.id()).map(|i| i.package);
            !is_std && ours == Some(theirs.package) && !imports_module(snapshot, id, view.id())
        }
        // A std module the run did not load: one std ships, by its index.
        None => is_std && std.is_some_and(|index| index.module(&path_text(target)).is_some()),
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
fn binding(snapshot: &Snapshot, view: ModuleView<'_>, target: &ModulePath) -> Option<String> {
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

/// The insertion of `line`, an import of the module `path`, into the file
/// of `root`, whose text is `text`.
fn placement(root: SyntaxNode<'_>, text: &str, path: &str, line: &str) -> (TextRange, String) {
    let imports: Vec<SyntaxNode<'_>> = root
        .children()
        .filter(|c| c.kind() == K::IMPORT_DECL)
        .collect();
    if let Some((first, rest)) = imports.split_first() {
        let block = first_block(*first, rest, text);
        let key = (path.to_owned(), sort_text(line));
        if let Some(after) = block.iter().find(|import| import_key(**import) > key) {
            let at = line_start(text, after.text_range().start());
            return (TextRange::empty(at), format!("{line}\n"));
        }
        let last = block.last().copied().unwrap_or(*first);
        return after_line(text, last.text_range().end(), format!("{line}\n"));
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

/// The first block of imports (D-1148): `first` and the imports of `rest`
/// that follow it on consecutive lines, with nothing but a line break
/// between each and the next.
fn first_block<'t>(
    first: SyntaxNode<'t>,
    rest: &[SyntaxNode<'t>],
    text: &str,
) -> Vec<SyntaxNode<'t>> {
    let mut block = vec![first];
    for &next in rest {
        let Some(&prev) = block.last() else { break };
        let between = TextRange::new(prev.text_range().end(), next.text_range().start());
        let gap = between.slice(text).unwrap_or("x");
        let breaks = gap.matches('\n').count();
        if breaks != 1 || !gap.chars().all(char::is_whitespace) {
            break;
        }
        block.push(next);
    }
    block
}

/// The sort key of an import (D-1148): its module path's text, then its
/// whole text with tokens joined by single spaces.
fn import_key(import: SyntaxNode<'_>) -> (String, String) {
    let path = import
        .first_child_of_kind(K::MODULE_PATH)
        .map(|p| {
            p.tokens()
                .filter(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE)
                .map(SyntaxToken::text)
                .collect::<String>()
        })
        .unwrap_or_default();
    let whole: Vec<&str> = import
        .tokens()
        .filter(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE)
        .map(SyntaxToken::text)
        .collect();
    (path, whole.join(" "))
}

/// `line`, an import as written, with its tokens joined by single spaces,
/// as [`import_key`] compares them.
fn sort_text(line: &str) -> String {
    line.replace("::", " :: ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
fn line_start(text: &str, offset: TextSize) -> TextSize {
    let at = usize::try_from(offset).unwrap_or(0);
    let start = text
        .get(..at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |n| n.saturating_add(1));
    TextSize::try_from(start).unwrap_or(0)
}

/// The start of the line after the one that holds `offset`, if the line
/// ends with a line break.
fn next_line(text: &str, offset: TextSize) -> Option<TextSize> {
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
