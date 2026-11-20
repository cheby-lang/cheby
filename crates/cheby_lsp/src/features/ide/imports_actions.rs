//! Code actions on imports (X §6.2, §6.3): add an import for E0042, remove
//! an unused one (W0003), organize imports (D-1691), move an import to the
//! import block (E0018), and E0042's fix to an item of another std module
//! (D-1694).
//!
//! Every fix is keyed on its diagnostic's code and primary span, read
//! against the tree, never on its message (D-1388):
//!
//! - E0042 whose span is a path's qualifier that the module scope does not
//!   bind ("`list` is not imported"): one "Import `std::list`" per module
//!   with that last segment, the session's own modules first, then std's,
//!   each sorted by path. The import comes from [`import_plan`], and when
//!   it needs an alias (D-1395) the diagnosed qualifier is written with it.
//!   A module that is imported already under another name, as in
//!   `import std::list as l` and then `list::map`, gets the qualifier
//!   rewritten to that name instead, "Use `l::map`", when the module has
//!   the item (D-1743).
//! - E0042 whose span is the item of `m::x`, where `m` is an imported std
//!   module without `x`: one fix per std item that the help names, found
//!   again with the lookup the help is written from (`MissingItem`,
//!   D-1658, D-1662), which rewrites the path to that item and imports its
//!   module when the file lacks it (D-1694). In a std checkout the item
//!   list is built from the checkout's own modules, as its run builds it
//!   (D-1700, D-1726).
//! - W0003: the whole import, from the line's start to its line break, or
//!   one name of a list with its comma (`removes`, D-1360).
//! - E0018 on an import that follows an item: the import, and every other
//!   import of the file with an E0018, is moved to where
//!   [`placement_without`] puts a new one, titled "Move the imports to the
//!   import block" when there are several (D-1744).
//! - Organize imports, a source action: W0003's imports removed, unless the
//!   report is substituted (D-1360), and the imports sorted as `cheby fmt`
//!   sorts them (D-1148). It is computed from the imports' lines alone,
//!   formatted as a text of their own, so a listing never formats the
//!   file; its edit is computed on resolve. With nothing to do it is
//!   listed with an empty edit, which the frame keeps only when `only`
//!   asks for it (D-1691), and a file with a grammar error gets none.
//!
//! The fixes carry their edits in the listing. The session's modules are
//! read through `own_modules`, and std's index only outside a std checkout,
//! where std's modules are the session's own (D-1706).

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    ItemHelp, MissingItem, ModuleId, ModulePath, Namespace, PackageId, PlainDiagnostic, PlainEdit,
    Res, Snapshot, StdIndex, StdItemName, StdItems, StdSuggestion, TextRange, TextSize,
    std_item_names, std_items_of,
};
use cheby_syntax::ast::{AstNode, AstToken, PathNode};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken, SyntaxTree};

use super::Refusal;
use super::actions::{Action, ActionId, ActionKind, DiagRef, FixCx};
use super::edits::formatted;
use super::imports::{
    import_plan, line_start, next_line, parse_path, path_text, placement_without,
};

/// D-1736's title of W0003's fix.
pub(crate) const REMOVE_IMPORT: &str = "Remove the unused import";

/// D-1736's title of E0018's fix.
pub(crate) const MOVE_IMPORT: &str = "Move the import to the import block";

/// D-1744's title of E0018's fix when it moves several imports.
pub(crate) const MOVE_IMPORTS: &str = "Move the imports to the import block";

/// D-1736's title of organize imports.
pub(crate) const ORGANIZE: &str = "Organize imports";

/// The import actions for `range`, given the report's diagnostics that
/// overlap it.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    _range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    actions(cx, std_index(cx), diagnostics)
}

/// The edits of an import action: organize imports' (D-1691). The other
/// import actions carry their edits in the listing.
///
/// # Errors
///
/// A refusal for an id this file does not list, or when the imports can no
/// longer be organized, as in a file with a grammar error.
pub(crate) fn resolve(
    cx: &FixCx<'_>,
    _range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    match id {
        ActionId::OrganizeImports => match organized(cx) {
            Some(plan) => Ok(plan.edit.into_iter().collect()),
            None => Err(Refusal(
                "the imports cannot be organized: the file has a grammar error".to_owned(),
            )),
        },
        ActionId::Fix { .. }
        | ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::FillArms
        | ActionId::Stub { .. }
        | ActionId::MakeVisible { .. }
        | ActionId::Habit { .. }
        | ActionId::Rename { .. } => Err(Refusal("this action has nothing to resolve".to_owned())),
    }
}

/// Std's index, read only outside a std checkout, whose std modules are
/// the session's own (D-1706).
fn std_index<'a>(cx: &FixCx<'a>) -> Option<&'a StdIndex> {
    if cx.analyzed.snapshot.is_std_checkout() {
        return None;
    }
    cx.std.index()
}

/// [`provide`] with `std` as std's index.
fn actions(
    cx: &FixCx<'_>,
    std: Option<&StdIndex>,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let mut out = Vec::new();
    let Some(tree) = cx.view.tree() else {
        return out;
    };
    for &(index, diagnostic) in diagnostics {
        let Some(at) = diagnostic.range else { continue };
        match diagnostic.code {
            codes::UNRESOLVED_NAME => {
                out.extend(add_import(cx, std, tree, index, at));
                out.extend(elsewhere(cx, std, tree, index, at));
            }
            codes::UNUSED_IMPORT => out.extend(remove(cx, tree, index, at)),
            codes::MODULE_ORDER => out.extend(move_import(cx, tree, index, at)),
            _ => {}
        }
    }
    out.extend(organize(cx));
    out
}

/// A quick fix of the report's diagnostic `index` with `edits`.
fn fix(title: String, index: usize, edits: Vec<PlainEdit>, removes: bool) -> Action {
    Action {
        title,
        kind: ActionKind::QuickFix,
        diagnostic: Some(DiagRef::Report(index)),
        preferred: true,
        removes,
        edit: Some(edits),
        data: None,
        disabled: None,
        command: None,
    }
}

/// An edit of the request's file.
fn edit_of(cx: &FixCx<'_>, range: TextRange, replacement: impl Into<String>) -> Option<PlainEdit> {
    Some(PlainEdit {
        path: cx.path()?.to_path_buf(),
        range,
        replacement: replacement.into(),
    })
}

/// The path whose segment token is exactly `at`, with that token's index
/// among its segments.
fn path_at(tree: &SyntaxTree, at: TextRange) -> Option<(PathNode<'_>, usize)> {
    let SyntaxElement::Token(token) = tree.root().covering_element(at)? else {
        return None;
    };
    if token.text_range() != at {
        return None;
    }
    let path = PathNode::cast(token.parent()?)?;
    let index = path
        .segments()
        .position(|segment| segment.syntax().text_range() == at)?;
    Some((path, index))
}

/// The namespace a path is resolved in: a type's and an interface's in a
/// type or a bound, else a value's.
fn namespace_of(path: PathNode<'_>) -> Namespace {
    let parent = path.syntax().parent().map(SyntaxNode::kind);
    if matches!(parent, Some(K::PATH_TYPE | K::INTERFACE_REF)) {
        Namespace::Type
    } else {
        Namespace::Value
    }
}

/// E0042 on a qualifier that the module scope does not bind: one import
/// per module with that last segment (X §6.3).
fn add_import(
    cx: &FixCx<'_>,
    std: Option<&StdIndex>,
    tree: &SyntaxTree,
    index: usize,
    at: TextRange,
) -> Vec<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let Some((path, 0)) = path_at(tree, at) else {
        return Vec::new();
    };
    let Some(qualifier) = path.qualifier() else {
        return Vec::new();
    };
    let text = qualifier.text();
    let bound = snapshot.globals().names.lookup(text).is_some_and(|name| {
        cx.view
            .resolved()
            .is_some_and(|resolved| resolved.scope.get(Namespace::Value, name).is_some())
    });
    if bound {
        return Vec::new();
    }
    candidates(snapshot, std, text)
        .into_iter()
        .filter_map(|target| {
            let plan = import_plan(snapshot, cx.view, std, &target)?;
            let Some((range, line)) = plan.edit else {
                return requalified(cx, path, &target, &plan.name, index, at);
            };
            let mut edits = vec![edit_of(cx, range, line)?];
            let written = path_text(&target);
            let title = if plan.name == text {
                format!("Import `{written}`")
            } else {
                edits.push(edit_of(cx, at, plan.name.clone())?);
                format!("Import `{written} as {}`", plan.name)
            };
            Some(fix(title, index, edits, false))
        })
        .collect()
}

/// E0042 on the qualifier of `m::x` where `target`, a module whose last
/// segment is `m`, is imported already under `name`: the qualifier
/// rewritten to `name`, titled "Use `name::x`", when the module has `x`
/// and code here may name it (D-1743).
fn requalified(
    cx: &FixCx<'_>,
    path: PathNode<'_>,
    target: &ModulePath,
    name: &str,
    index: usize,
    at: TextRange,
) -> Option<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let mut segments = path.segments().skip(1);
    let item = segments.next()?;
    let module = snapshot.table().find(target)?;
    let item_name = snapshot.globals().names.lookup(item.text())?;
    let package = snapshot.table().get(cx.view.id())?.package;
    let iface = snapshot.interfaces().get(module)?;
    let found = iface.lookup(namespace_of(path), item_name)?;
    // A `priv` item, or a package item of another package, would trade
    // E0042 for E0043.
    if iface.is_hidden(found) || !iface.visible_from(found, package) {
        return None;
    }
    let rest = TextRange::new(
        item.syntax().text_range().start(),
        path.syntax().text_range().end(),
    )
    .slice(cx.text)?;
    let edits = vec![edit_of(cx, at, name)?];
    Some(fix(format!("Use `{name}::{rest}`"), index, edits, false))
}

/// The modules whose last segment is `last`: the session's own, then
/// std's from its index, each sorted by path.
fn candidates(snapshot: &Snapshot, std: Option<&StdIndex>, last: &str) -> Vec<ModulePath> {
    let mut own: Vec<ModulePath> = snapshot
        .own_modules()
        .filter_map(|view| view.path().cloned())
        .filter(|path| path.segments().last().is_some_and(|s| s == last))
        .collect();
    own.sort_by_key(path_text);
    let mut shipped: Vec<ModulePath> = std
        .map_or_default(|index| index.modules.as_slice())
        .iter()
        .filter_map(|module| parse_path(&module.path))
        .filter(|path| path.segments().last().is_some_and(|s| s == last))
        .filter(|path| !own.contains(path))
        .collect();
    shipped.sort_by_key(path_text);
    own.extend(shipped);
    own
}

/// E0042 on `m::x` where `m` is a std module without `x`: a fix per std
/// item that the help names (D-1694).
fn elsewhere(
    cx: &FixCx<'_>,
    std: Option<&StdIndex>,
    tree: &SyntaxTree,
    index: usize,
    at: TextRange,
) -> Vec<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let Some((path, 1)) = path_at(tree, at) else {
        return Vec::new();
    };
    let Some(StdSuggestion::Elsewhere { modules, name }) = suggestion(cx, path) else {
        return Vec::new();
    };
    let (Some(first), Some(last)) = (path.segments().next(), path.segments().nth(1)) else {
        return Vec::new();
    };
    let whole = TextRange::new(
        first.syntax().text_range().start(),
        last.syntax().text_range().end(),
    );
    modules
        .iter()
        .filter_map(|module| {
            let target = parse_path(&format!("std::{module}"))?;
            let plan = import_plan(snapshot, cx.view, std, &target)?;
            let written = format!("{}::{name}", plan.name);
            let mut edits = vec![edit_of(cx, whole, written.clone())?];
            let title = match plan.edit {
                Some((range, line)) => {
                    edits.push(edit_of(cx, range, line)?);
                    let import = if module.rsplit("::").next() == Some(plan.name.as_str()) {
                        format!("std::{module}")
                    } else {
                        format!("std::{module} as {}", plan.name)
                    };
                    format!("Use `{written}` and import `{import}`")
                }
                None => format!("Use `{written}`"),
            };
            Some(fix(title, index, edits, false))
        })
        .collect()
}

/// What E0042's help names for the two-segment `path`, read with the
/// lookup the resolver writes the help from (W11-Q3): `None` unless its
/// qualifier is an imported std module without its item.
fn suggestion(cx: &FixCx<'_>, path: PathNode<'_>) -> Option<StdSuggestion> {
    let snapshot = &cx.analyzed.snapshot;
    let names = &snapshot.globals().names;
    let mut segments = path.segments();
    let (qualifier, item) = (segments.next()?, segments.next()?);
    if segments.next().is_some() {
        return None;
    }
    let module_name = names.lookup(qualifier.as_ident()?.text())?;
    let entry = cx
        .view
        .resolved()?
        .scope
        .get(Namespace::Value, module_name)?;
    let Res::Module(target) = entry.res else {
        return None;
    };
    let info = snapshot.table().get(target)?;
    let (root, under) = info.path.segments().split_first()?;
    if root != "std" || info.package != PackageId::STD {
        return None;
    }
    let std_module = under.join("::");
    let iface = snapshot.interfaces().get(target)?;
    let name = names.lookup(item.text())?;
    let ns = namespace_of(path);
    if iface.lookup(ns, name).is_some() {
        return None;
    }
    let from = snapshot.table().get(cx.view.id())?.package;
    let checkout: Vec<StdItemName>;
    let items = if snapshot.is_std_checkout() {
        checkout = checkout_items(snapshot);
        StdItems::Owned(&checkout)
    } else {
        StdItems::Shared(std_item_names)
    };
    let lookup = MissingItem {
        target: iface,
        std_module: Some(&std_module),
        builtins: snapshot.interfaces().get(ModuleId::BUILTIN),
        names,
        from,
        items,
    };
    match lookup.help(ns, name)? {
        ItemHelp::Std(suggestion) => Some(suggestion),
        ItemHelp::Close(_) => None,
    }
}

/// A std checkout's public items, from its own modules' trees, as its run
/// lists them for the help (D-1726).
fn checkout_items(snapshot: &Snapshot) -> Vec<StdItemName> {
    let modules: Vec<(String, &SyntaxTree)> = snapshot
        .own_modules()
        .filter_map(|view| {
            let (root, under) = view.path()?.segments().split_first()?;
            if root != "std" {
                return None;
            }
            Some((under.join("::"), view.tree()?))
        })
        .collect();
    std_items_of(modules.iter().map(|(path, tree)| (path.as_str(), *tree)))
}

/// The bytes from the first to the last significant token of `node`.
fn significant(node: SyntaxNode<'_>) -> Option<TextRange> {
    let mut tokens = node
        .tokens()
        .filter(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE);
    let first = tokens.next()?.text_range();
    let last = tokens.next_back().map_or(first, SyntaxToken::text_range);
    Some(TextRange::new(first.start(), last.end()))
}

/// The top-level imports of `tree`, in order.
fn imports(tree: &SyntaxTree) -> impl Iterator<Item = SyntaxNode<'_>> {
    tree.root()
        .children()
        .filter(|node| node.kind() == K::IMPORT_DECL)
}

/// The bytes that removing the whole import `node` deletes: from its
/// line's start to its line break, when nothing else is on the line,
/// else the import alone.
fn whole_import(text: &str, node: SyntaxNode<'_>) -> Option<TextRange> {
    let at = significant(node)?;
    let start = line_start(text, at.start());
    let alone_before = TextRange::new(start, at.start())
        .slice(text)
        .is_some_and(|before| before.trim().is_empty());
    let end = next_line(text, at.end()).unwrap_or_else(|| text_len(text));
    let alone_after = TextRange::new(at.end(), end)
        .slice(text)
        .is_some_and(|after| after.trim().is_empty());
    Some(if alone_before && alone_after {
        TextRange::new(start, end)
    } else {
        at
    })
}

/// The bytes that removing the import or the import list's name whose
/// significant text is `at` deletes (X §6.3): the whole import, or the
/// name with the comma after it, or before it for the last. Only a list
/// with another name loses one, so `{ x }` keeps its braces.
fn removal(text: &str, tree: &SyntaxTree, at: TextRange) -> Option<TextRange> {
    for import in imports(tree) {
        let whole = significant(import)?;
        if whole == at {
            return whole_import(text, import);
        }
        if !whole.contains_range(at) {
            continue;
        }
        let list = import.first_child_of_kind(K::IMPORT_LIST)?;
        let names: Vec<SyntaxNode<'_>> = list
            .children()
            .filter(|node| node.kind() == K::IMPORT_ITEM)
            .collect();
        let n = names
            .iter()
            .position(|name| significant(*name) == Some(at))?;
        let this = significant(*names.get(n)?)?;
        let before = n.checked_sub(1).and_then(|p| names.get(p));
        let after = n.checked_add(1).and_then(|p| names.get(p));
        return match (before, after) {
            (_, Some(next)) => Some(TextRange::new(this.start(), significant(*next)?.start())),
            (Some(before), None) => Some(TextRange::new(significant(*before)?.end(), this.end())),
            (None, None) => None,
        };
    }
    None
}

/// W0003: the removal of the unused import or name.
fn remove(cx: &FixCx<'_>, tree: &SyntaxTree, index: usize, at: TextRange) -> Option<Action> {
    let range = removal(cx.text, tree, at)?;
    let edits = vec![edit_of(cx, range, "")?];
    Some(fix(REMOVE_IMPORT.to_owned(), index, edits, true))
}

/// E0018 on an import after an item: that import and every other import
/// of the file that the report gives an E0018, moved to the import block
/// in one edit (D-1744). They go in as one run of lines where the first of
/// them by module path goes, and the frame's formatting sorts the block
/// (D-1148, D-1735).
fn move_import(cx: &FixCx<'_>, tree: &SyntaxTree, index: usize, at: TextRange) -> Option<Action> {
    let misplaced: Vec<TextRange> = cx
        .report
        .map_or_default(|report| report.diagnostics.as_slice())
        .iter()
        .filter(|d| d.code == codes::MODULE_ORDER)
        .filter_map(|d| d.range)
        .collect();
    let moved: Vec<SyntaxNode<'_>> = imports(tree)
        .filter(|node| {
            significant(*node).is_some_and(|range| range == at || misplaced.contains(&range))
        })
        .collect();
    if !moved.iter().any(|node| significant(*node) == Some(at)) {
        return None;
    }
    let mut lines: Vec<(String, &str)> = Vec::with_capacity(moved.len());
    for import in &moved {
        let module = import.first_child_of_kind(K::MODULE_PATH)?;
        let path: String = module
            .tokens()
            .filter(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE)
            .map(SyntaxToken::text)
            .collect();
        lines.push((path, significant(*import)?.slice(cx.text)?));
    }
    lines.sort();
    let (first, _) = lines.first()?;
    let written: Vec<&str> = lines.iter().map(|(_, line)| *line).collect();
    let ranges: Vec<TextRange> = moved.iter().map(|node| node.text_range()).collect();
    let (to, line) = placement_without(tree.root(), cx.text, first, &written.join("\n"), &ranges);
    let mut edits = vec![edit_of(cx, to, line)?];
    for import in &moved {
        edits.push(edit_of(cx, whole_import(cx.text, *import)?, "")?);
    }
    let title = if moved.len() > 1 {
        MOVE_IMPORTS
    } else {
        MOVE_IMPORT
    };
    Some(fix(title.to_owned(), index, edits, false))
}

/// What organize imports does to the file (X §6.3).
struct Organized {
    /// The replacement of the imports' lines, `None` when they are in
    /// order and none is to be removed.
    edit: Option<PlainEdit>,
    /// It removes an import that W0003 reports.
    removes: bool,
}

/// Organize imports, offered as [`organized`] says.
fn organize(cx: &FixCx<'_>) -> Option<Action> {
    let plan = organized(cx)?;
    let (edit, data) = match plan.edit {
        Some(_) => (None, Some(ActionId::OrganizeImports)),
        None => (Some(Vec::new()), None),
    };
    Some(Action {
        title: ORGANIZE.to_owned(),
        kind: ActionKind::SourceOrganizeImports,
        diagnostic: None,
        preferred: false,
        removes: plan.removes,
        edit,
        data,
        disabled: None,
        command: None,
    })
}

/// Organize imports for the request's file: the lines from the first
/// import to the last, with W0003's imports removed while the report is
/// not substituted (D-1360), formatted as a text of their own, which sorts
/// each block (D-1148). `None` for a file with a grammar error, or one
/// whose imports have an item between them (E0018), which it refuses.
fn organized(cx: &FixCx<'_>) -> Option<Organized> {
    if cx.view.is_broken() {
        return None;
    }
    let tree = cx.view.tree()?;
    let nodes: Vec<SyntaxNode<'_>> = tree.root().children().collect();
    let first = nodes.iter().position(|n| n.kind() == K::IMPORT_DECL);
    let last = nodes.iter().rposition(|n| n.kind() == K::IMPORT_DECL);
    let (Some(first), Some(last)) = (first, last) else {
        return Some(Organized {
            edit: None,
            removes: false,
        });
    };
    let between = nodes.get(first..=last)?;
    if between.iter().any(|n| n.kind() != K::IMPORT_DECL) {
        return None;
    }
    let start = line_start(cx.text, significant(*between.first()?)?.start());
    let end = line_end(cx.text, significant(*between.last()?)?.end());
    let region = TextRange::new(start, end);
    let crlf = region.slice(cx.text)?.contains("\r\n");
    let mut cuts: Vec<TextRange> = if cx.substituted {
        Vec::new()
    } else {
        cx.report
            .map_or_default(|report| report.diagnostics.as_slice())
            .iter()
            .filter(|d| d.code == codes::UNUSED_IMPORT)
            .filter_map(|d| d.range)
            .filter(|at| region.contains_range(*at))
            .filter_map(|at| removal(cx.text, tree, at))
            .map(|range| TextRange::new(range.start(), range.end().min(region.end())))
            .collect()
    };
    cuts.sort_by_key(|range| (range.start(), range.end()));
    // Two names of one list can overlap, the last one's comma before it:
    // they are cut as one.
    let mut removed: Vec<TextRange> = Vec::with_capacity(cuts.len());
    for cut in cuts {
        match removed.last_mut() {
            Some(last) if cut.start() <= last.end() => {
                *last = TextRange::new(last.start(), last.end().max(cut.end()));
            }
            Some(_) | None => removed.push(cut),
        }
    }
    let mut kept = String::new();
    let mut at = start;
    for range in &removed {
        kept.push_str(TextRange::new(at, range.start()).slice(cx.text)?);
        at = range.end();
    }
    kept.push_str(TextRange::new(at, end).slice(cx.text)?);
    kept.push('\n');
    let sorted = formatted(&kept.replace("\r\n", "\n"))?;
    let sorted = sorted.strip_suffix('\n').unwrap_or(&sorted);
    let old = region.slice(cx.text)?;
    let edit = if sorted == old.replace("\r\n", "\n") {
        None
    } else if crlf {
        edit_of(cx, region, sorted.replace('\n', "\r\n"))
    } else {
        edit_of(cx, region, sorted)
    };
    Some(Organized {
        edit,
        removes: !removed.is_empty(),
    })
}

/// The end of the line that holds `offset`, before its line break.
fn line_end(text: &str, offset: TextSize) -> TextSize {
    let Some(next) = next_line(text, offset) else {
        return text_len(text);
    };
    let before = next.saturating_sub(1);
    let cr = before
        .checked_sub(1)
        .and_then(|at| TextRange::new(at, before).slice(text))
        == Some("\r");
    if cr { before.saturating_sub(1) } else { before }
}

/// The length of `text` as a text offset.
fn text_len(text: &str) -> TextSize {
    TextSize::try_from(text.len()).unwrap_or(TextSize::MAX)
}

#[cfg(test)]
#[path = "imports_actions_tests.rs"]
mod tests;
