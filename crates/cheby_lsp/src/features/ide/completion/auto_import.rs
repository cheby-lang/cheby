//! Names of other modules in `Expr` and `Type` contexts: the items of the
//! modules the file imports (D-1507), and auto-import of the items of the
//! modules it does not (X §4.4, D-1395).
//!
//! [`complete`] is called in `Expr` and `Type` contexts. Once the partial
//! word has two characters, it offers:
//!
//! - **Imported modules' items** (D-1507), whatever
//!   `completion.autoImport` says and whatever package holds the module
//!   (a dependency's, from M5): written through the import's binding,
//!   `shape::scale` or an alias's `s::scale`, with no import edit, at
//!   rank 30, above the items of modules the file does not import. They
//!   are not capped.
//! - **Items to import**, when `completion.autoImport` is on: the items of
//!   the package's modules and the std modules the run loaded, by their
//!   interfaces, and of the std modules it did not load, by std's index
//!   (D-1372). Modules of other packages are left out of these, as
//!   before M5 a package imports only its own and std's.
//! - **Filters**: visible from the package, current, the partial word a
//!   subsequence of the name (ignoring ASCII case), and present on every
//!   target of the run (§12.5).
//! - **Text**: qualified by the module, `list::map(…)`, with the import as
//!   an additional edit from [`import_plan`], aliased when the name is
//!   taken (D-1395). Where the position takes a function's name, read from
//!   its expected type as for the module's own items, a function is the
//!   bare `list::map` (X §4.1). An item whose module [`import_plan`] refuses is left
//!   out, so no item makes an error. Item imports (`import a::{x}`) are
//!   never added: modules are what Cheby code qualifies with.
//! - **Label**: the qualified name as written, `list::map`, with the bare
//!   name as `filterText`, so the list tells two `map`s apart and the
//!   client still matches the word (D-1508).
//! - **Interfaces**: in an expression, and after `|>` through
//!   [`pipe_interfaces`], the interfaces of both kinds come too, since one
//!   starts a path that names its functions (D-173), as in
//!   `ops::Compare::compare(a, b)`. Each is written with the path's
//!   `::`, `ops::Compare::`, as a module is written `list::`, under the
//!   same partial, cap and rank rules as the functions of its source
//!   (D-1546). After `|>`, one
//!   to import ranks 40 for a loaded module and 60 for one of std's
//!   index, as a pipe's function to import does (X §4.3), and one the
//!   file imports by name comes only by that name, not through its
//!   module (D-1550). In a type an interface is a type and is written
//!   with no `::`.
//! - **Order and cap**: the items to import by name, then module path, at
//!   most [`CAP`] items. When more match, the list is incomplete, so the
//!   client asks again as the word grows. [`admits`] and [`push_capped`]
//!   also serve the pipe candidates that need an import (D-1479).
//! - **Short partial**: below two characters neither kind is offered, and
//!   the list is incomplete when either could be, so a client that filters
//!   by itself still asks again at the second character, where they start
//!   (D-1487, D-1507). With `completion.autoImport` off, so it is when
//!   the file imports a module (D-1521), and after `|>` when an imported
//!   module has an interface, which comes at the second (D-1546).
//! - **Before `::`**: where a path's `::` follows the word, [`complete`]
//!   is not called, since only what a path starts with fits there
//!   (D-1530): `st‸::length` would become `string::starts_with::length`
//!   (D-1521). Of these items, only the interfaces of imported modules
//!   fit, as `shape::Shape`, which [`imported_interfaces`] gives with no
//!   partial filter, and only when one segment follows the `::`, as in
//!   `Sha‸::area(s)`, not in `sha‸::Shape::area(s)` (D-1539). Nothing
//!   to import is offered there, and nothing more comes at the second
//!   character, so the list is complete (D-1525).
//!   Only a `::` right after the word counts: `st‸ ::length` is as with
//!   none.
//! - **Before a turbofish**: where the `::` is a turbofish's, as in
//!   `re‸::<Int>([1, 2])`, both kinds are offered as with no `::` after
//!   the word, since `list::reverse::<Int>` is a valid path (D-1525), but
//!   only functions and constructors, since a constant takes no type
//!   arguments (D-1536). The `<` is found as the parser reads a turbofish
//!   (D-1529).
//!
//! `labelDetails.description` is the module's whole path.

use std::collections::{BTreeMap, BTreeSet};

use cheby_driver::analysis::{
    CtorFields, DefId, DefKind, ItemSig, ModuleId, ModulePath, ModuleView, Namespace, PackageId,
    Res, ScopeSource, StdItem, builtin,
};

use super::items::{Entry, rank, value_fits};
use super::pipe::{insert_at, is_loaded, param_names};
use super::{CompletionCx, Items};
use crate::features::ide::imports::{ImportPlan, import_plan, parse_path, path_text};

/// The most items one answer holds (X §4.4).
pub(crate) const CAP: usize = 100;

/// How many characters the partial word needs before names are imported.
const MIN_PARTIAL: usize = 2;

/// One item to offer, before its import is planned.
#[derive(Clone, Debug)]
enum Found<'s> {
    /// An item of a loaded module.
    Loaded {
        /// The item.
        def: DefId,
    },
    /// An item of std's index, of a module the run did not load.
    Indexed(&'s StdItem),
}

/// An item of a module the file imports: its name, its module's path,
/// the item, and the name the file binds to the module (D-1507). The
/// module is the binding's, also for a builtin item homed there.
type Imported = (String, String, DefId, &'static str);

/// The kinds of item a source offers, and how an interface is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    /// Functions, constants and constructors, and interfaces written with
    /// the `::` of the path they start, as `ops::Compare::` (D-1546). No
    /// constant and no interface before a turbofish (D-1536).
    Values,
    /// Types, aliases and interfaces, an interface written as a type.
    Types,
    /// Interfaces only: written bare before a path's `::` (D-1530), or
    /// with their `::` where none follows, as after `|>` (D-1546).
    Interfaces {
        /// Each is written with its `::`.
        colons: bool,
    },
}

impl Want {
    /// Whether an interface is written with its `::` (D-1546).
    const fn colons(self) -> bool {
        matches!(self, Self::Values | Self::Interfaces { colons: true })
    }
}

/// Pushes the items of `ns` from other modules: those of the modules the
/// file imports (D-1507), and with `completion.autoImport` those of the
/// modules it does not (X §4.4). With `as_value`, the position takes a
/// function's name, not a call, so a function is written bare, as the
/// module's own are (X §4.1). It is not called where a path's `::`
/// follows the word (D-1530); a turbofish's `::` is as none (D-1525).
/// In an expression, the interfaces come too, written with the `::` of
/// the path they start, as `ops::Compare::`, under the same partial and
/// cap rules (D-1546).
pub(crate) fn complete(cx: &CompletionCx<'_>, ns: Namespace, as_value: bool, out: &mut Items) {
    let want = match ns {
        Namespace::Value => Want::Values,
        Namespace::Type => Want::Types,
    };
    let importing = cx.options.auto_import;
    let partial = cx.partial_text();
    if !may_import(partial) {
        // Both kinds start at the second character, so the client must
        // ask again (D-1487, D-1507).
        if importing || imports_a_module(cx) {
            out.incomplete = true;
        }
        return;
    }
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let mut imported: Vec<Imported> = Vec::new();
    let mut found: Vec<(String, String, Found<'_>)> = Vec::new();
    loaded(
        cx,
        view,
        want,
        partial,
        &mut imported,
        importing.then_some(&mut found),
    );
    push_imported(cx, imported, want, as_value, out);
    if !importing {
        return;
    }
    indexed(cx, want, partial, &mut found);
    let mut plans: BTreeMap<String, Option<ImportPlan>> = BTreeMap::new();
    push_capped(found, out, |name, path, item| {
        let plan = plans.entry(path.to_owned()).or_insert_with(|| {
            let target = parse_path(path)?;
            let std = match item {
                Found::Indexed(_) => cx.std.get(),
                Found::Loaded { .. } => None,
            };
            import_plan(cx.snapshot, view, std, &target)
        });
        let plan = plan.as_ref()?;
        match item {
            Found::Loaded { def, .. } => loaded_entry(cx, def, name, path, plan, want, as_value),
            Found::Indexed(item) => Some(indexed_entry(cx, item, plan, want, as_value)),
        }
    });
}

/// Pushes the entries of `imported`, each written through the import's
/// binding with no import edit (D-1507).
fn push_imported(
    cx: &CompletionCx<'_>,
    imported: Vec<Imported>,
    want: Want,
    as_value: bool,
    out: &mut Items,
) {
    for (name, path, def, binding) in imported {
        let plan = ImportPlan {
            name: binding.to_owned(),
            edit: None,
        };
        if let Some(entry) = loaded_entry(cx, def, &name, &path, &plan, want, as_value) {
            out.push(entry);
        }
    }
}

/// The interfaces of other modules after `|>`, where no `::` follows the
/// word, written with the `::` of the path they start (D-1546): those of
/// the modules the file imports, through the import's binding, pushed
/// into `out` at rank 30 (D-1507), and with `completion.autoImport`
/// those of the modules it does not, with their import edit, returned by
/// name and module path for the cap the pipe's functions share (D-1479).
/// Each to import ranks as a function of its source after `|>`: 40 for a
/// loaded module, 60 for one of std's index (X §4.3). Both need a partial
/// of two characters that the name holds as a subsequence. One the file
/// imports by name comes only by that name, from the scope, and not again
/// through its module, imported or to import, as a pipe's function
/// imported by name comes once (D-1479, D-1550).
pub(crate) fn pipe_interfaces(
    cx: &CompletionCx<'_>,
    out: &mut Items,
) -> Vec<(String, String, Entry)> {
    let want = Want::Interfaces { colons: true };
    let partial = cx.partial_text();
    let Some(view) = cx.resolved_view() else {
        return Vec::new();
    };
    if !may_import(partial) {
        return Vec::new();
    }
    let importing = cx.options.auto_import;
    let mut imported: Vec<Imported> = Vec::new();
    let mut found: Vec<(String, String, Found<'_>)> = Vec::new();
    loaded(
        cx,
        view,
        want,
        partial,
        &mut imported,
        importing.then_some(&mut found),
    );
    // An interface the file imports by name comes by that name only,
    // `Compare::`, from the scope (D-1550): not again as `ops::Compare::`
    // through its imported module, and not with an import edit, which
    // would also leave the first import unused (W0003).
    let by_name: BTreeSet<DefId> = view
        .resolved()
        .map(|resolved| {
            resolved
                .scope
                .iter(Namespace::Type)
                .filter_map(|(_, entry)| match entry.res {
                    Res::Def(def) if matches!(entry.source, ScopeSource::Import(_)) => Some(def),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    imported.retain(|(_, _, def, _)| !by_name.contains(def));
    push_imported(cx, imported, want, false, out);
    if !importing {
        return Vec::new();
    }
    found.retain(|(_, _, item)| !matches!(item, Found::Loaded { def } if by_name.contains(def)));
    indexed(cx, want, partial, &mut found);
    let mut plans: BTreeMap<String, Option<ImportPlan>> = BTreeMap::new();
    let mut entries: Vec<(String, String, Entry)> = Vec::new();
    for (name, path, item) in found {
        let plan = plans.entry(path.clone()).or_insert_with(|| {
            let target = parse_path(&path)?;
            let std = match item {
                Found::Indexed(_) => cx.std.get(),
                Found::Loaded { .. } => None,
            };
            import_plan(cx.snapshot, view, std, &target)
        });
        let Some(plan) = plan.as_ref() else { continue };
        let entry = match item {
            Found::Loaded { def } => {
                loaded_entry(cx, def, &name, &path, plan, want, false).map(|mut entry| {
                    entry.rank = rank::PRELUDE;
                    entry
                })
            }
            Found::Indexed(item) => Some(indexed_entry(cx, item, plan, want, false)),
        };
        if let Some(entry) = entry {
            entries.push((name, path, entry));
        }
    }
    entries
}

/// Whether a module the file imports has an interface that a partial
/// word of two characters would offer after `|>` (D-1546), so a shorter
/// one leaves the list incomplete (D-1521).
pub(crate) fn imports_an_interface(cx: &CompletionCx<'_>) -> bool {
    let Some(view) = cx.resolved_view() else {
        return false;
    };
    let mut imported: Vec<Imported> = Vec::new();
    loaded(
        cx,
        view,
        Want::Interfaces { colons: true },
        "",
        &mut imported,
        None,
    );
    !imported.is_empty()
}

/// Pushes the interfaces of the modules the file imports, written
/// through the import's binding, as `shape::Shape`, for a path's `::`
/// after the word, which names an interface function (D-173, D-1530).
/// They are D-1507's items of a type, interfaces only, at rank 30 with
/// the bare name as `filterText` (D-1508). No partial word filters them:
/// nothing more comes at the second character, so the list is complete
/// there (D-1525).
pub(crate) fn imported_interfaces(cx: &CompletionCx<'_>, out: &mut Items) {
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let want = Want::Interfaces { colons: false };
    let mut imported: Vec<Imported> = Vec::new();
    loaded(cx, view, want, "", &mut imported, None);
    push_imported(cx, imported, want, false, out);
}

/// Whether the file imports a module, whose items a partial word of two
/// characters would offer (D-1507).
fn imports_a_module(cx: &CompletionCx<'_>) -> bool {
    cx.resolved_view()
        .and_then(ModuleView::resolved)
        .is_some_and(|resolved| !resolved.scope.import_names().modules.is_empty())
}

/// Whether a name that needs an import may be offered for the partial
/// word `partial`: it has two characters at least, and `name` holds it as
/// a subsequence (X §4.4). Pipe candidates follow the same rule (D-1479).
pub(crate) fn admits(partial: &str, name: &str) -> bool {
    may_import(partial) && matches(partial, name)
}

/// Whether the partial word `partial` is long enough for names that need
/// an import (X §4.4, D-1479).
pub(crate) fn may_import(partial: &str) -> bool {
    partial.chars().count() >= MIN_PARTIAL
}

/// Pushes the items of `found`, each a name, its module's path and what
/// `build` makes its entry from, sorted by name, then path, at most
/// [`CAP`] of them (X §4.4, D-1479). An item `build` leaves out does not
/// count. When more would follow, the list is marked incomplete, so the
/// client asks again as the word grows.
pub(crate) fn push_capped<T>(
    mut found: Vec<(String, String, T)>,
    out: &mut Items,
    mut build: impl FnMut(&str, &str, T) -> Option<Entry>,
) {
    found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut taken = 0_usize;
    for (name, path, item) in found {
        let Some(entry) = build(&name, &path, item) else {
            continue;
        };
        if taken == CAP {
            out.incomplete = true;
            break;
        }
        out.push(entry);
        taken = taken.saturating_add(1);
    }
}

/// Whether an item of `kind` is one `want` offers: functions, constants
/// and constructors as values, with no constant before a turbofish
/// (D-1536), and interfaces there but before a turbofish, since none
/// takes type arguments (D-1546); types, aliases and interfaces as
/// types; or interfaces alone.
fn fits(cx: &CompletionCx<'_>, want: Want, kind: DefKind) -> bool {
    match want {
        Want::Values => {
            value_fits(cx, kind) || (kind == DefKind::Interface && !cx.before_turbofish())
        }
        Want::Types => matches!(kind, DefKind::Type | DefKind::Alias | DefKind::Interface),
        Want::Interfaces { .. } => kind == DefKind::Interface,
    }
}

/// Whether `partial` is a subsequence of `name`, ignoring ASCII case.
fn matches(partial: &str, name: &str) -> bool {
    let mut chars = name.chars();
    partial
        .chars()
        .all(|p| chars.any(|c| c.eq_ignore_ascii_case(&p)))
}

/// Pushes the matching items of the loaded modules: those of the modules
/// the file imports into `imported`, and, given `others`, those of the
/// package's and std's other modules into it, the builtin items homed in
/// std modules that the prelude does not hold among them.
fn loaded<'s>(
    cx: &CompletionCx<'s>,
    view: ModuleView<'s>,
    want: Want,
    partial: &str,
    imported: &mut Vec<Imported>,
    mut others: Option<&mut Vec<(String, String, Found<'s>)>>,
) {
    let table = cx.snapshot.table();
    let Some(own) = table.get(view.id()).map(|i| i.package) else {
        return;
    };
    let targets = cx.snapshot.targets();
    for module in cx.snapshot.all_modules() {
        let id = module.id();
        let Some(info) = table.get(id) else { continue };
        if id == ModuleId::BUILTIN || id == view.id() {
            continue;
        }
        // A module the file imports is offered whatever its package
        // (D-1507); one to import must be the package's own or std's.
        let binding = cx.module_binding(id);
        if binding.is_none()
            && (others.is_none() || (info.package != own && info.package != PackageId::STD))
        {
            continue;
        }
        let Some(iface) = cx.snapshot.interfaces().get(id) else {
            continue;
        };
        let path = path_text(&info.path);
        let mut defs: Vec<DefId> = iface
            .items
            .iter_enumerated()
            .filter(|&(index, item)| {
                fits(cx, want, item.kind)
                    && iface.visible_from(index, own)
                    && iface.is_current(index)
                    && !iface.is_hidden(index)
            })
            .map(|(index, _)| DefId::new(id, index))
            .collect();
        defs.extend(homed(cx, &info.path, want));
        for def in defs {
            let Some(item) = cx.item(def) else { continue };
            let name = cx.name_text(item.name);
            if item.targets.intersection(targets) != targets || !matches(partial, name) {
                continue;
            }
            match (binding, others.as_deref_mut()) {
                (Some(binding), _) => imported.push((name.to_owned(), path.clone(), def, binding)),
                (None, Some(others)) => {
                    others.push((name.to_owned(), path.clone(), Found::Loaded { def }));
                }
                (None, None) => {}
            }
        }
    }
}

/// The builtin items homed at `path` that the prelude does not hold, so
/// that only their home module names them (D-481), with a type's
/// constructors.
fn homed(cx: &CompletionCx<'_>, path: &ModulePath, want: Want) -> Vec<DefId> {
    let mut defs: Vec<DefId> = Vec::new();
    for ty in builtin::homed_at(path) {
        if builtin::is_prelude(ty) {
            continue;
        }
        defs.push(ty);
        if let Some(ItemSig::Type(sig)) = cx.item(ty).map(|i| &i.sig)
            && let cheby_driver::analysis::TypeDefKind::Adt { ctors } = &sig.kind
        {
            defs.extend(ctors.iter().map(|&c| DefId::new(ty.module, c)));
        }
    }
    defs.retain(|&def| cx.item(def).is_some_and(|item| fits(cx, want, item.kind)));
    defs
}

/// Pushes the matching items of std's index whose module the run did not
/// load.
fn indexed<'s>(
    cx: &CompletionCx<'s>,
    want: Want,
    partial: &str,
    found: &mut Vec<(String, String, Found<'s>)>,
) {
    let Some(index) = cx.std.get() else { return };
    let targets = cx.snapshot.targets();
    let mut loaded: BTreeMap<&str, bool> = BTreeMap::new();
    for item in &index.items {
        let name = item.key.text();
        if !fits(cx, want, item.kind)
            || item.targets.intersection(targets) != targets
            || !matches(partial, name)
        {
            continue;
        }
        let is_loaded = *loaded
            .entry(item.module.as_str())
            .or_insert_with(|| is_loaded(cx, &item.module));
        if !is_loaded {
            found.push((name.to_owned(), item.module.clone(), Found::Indexed(item)));
        }
    }
}

/// The entry of `def`, an item of a loaded module at `path`, named
/// `name`, qualified as `plan` says: at rank 30 when the plan needs no
/// import, the module's binding (D-1507), else at rank 60. An interface
/// is written with its `::` when `want` says, as a module is written
/// `shape::`, since a path follows it (D-1546).
fn loaded_entry(
    cx: &CompletionCx<'_>,
    def: DefId,
    name: &str,
    path: &str,
    plan: &ImportPlan,
    want: Want,
    as_value: bool,
) -> Option<Entry> {
    let item = cx.item(def)?;
    let qualified = format!("{}::{name}", plan.name);
    let rank = if plan.edit.is_some() {
        rank::NOT_IMPORTED
    } else {
        rank::IMPORTED
    };
    let mut entry = cx.def_entry(def, &qualified, rank)?;
    entry.filter_text = Some(name.to_owned());
    entry.description = Some(path.to_owned());
    entry.insert = match &item.sig {
        ItemSig::Interface(_) if want.colons() => cx.insert(format!("{qualified}::")),
        _ if as_value => cx.insert(qualified),
        ItemSig::Fn(_) if !cx.before_paren() => {
            let params = param_names(cx, def);
            insert_at(cx, cx.partial(), call(&qualified, &params))
        }
        ItemSig::Ctor(ctor) if !cx.before_paren() => match &ctor.fields {
            CtorFields::Positional(fields) => {
                let stops: Vec<String> = (1..=fields.len()).map(|n| format!("${n}")).collect();
                cx.snippet(format!("{qualified}({})$0", stops.join(", ")), qualified)
            }
            CtorFields::Unit | CtorFields::Named(_) => cx.insert(qualified),
        },
        _ => cx.insert(qualified),
    };
    entry.additional = plan.edit.clone().into_iter().collect();
    Some(entry)
}

/// The entry of `item`, of std's index, qualified as `plan` says, and
/// written bare with `as_value`. An interface is written with its `::`
/// when `want` says (D-1546).
fn indexed_entry(
    cx: &CompletionCx<'_>,
    item: &StdItem,
    plan: &ImportPlan,
    want: Want,
    as_value: bool,
) -> Entry {
    let mut entry = cx.std_entry(item, rank::NOT_IMPORTED);
    entry.description = Some(item.module.clone());
    let qualified = format!("{}::{}", plan.name, item.key.text());
    entry.label.clone_from(&qualified);
    entry.filter_text = Some(item.key.text().to_owned());
    entry.insert = if item.kind == DefKind::Fn && !cx.before_paren() {
        let params: Vec<&str> = item.param_names.iter().map(String::as_str).collect();
        entry.label_detail = Some(format!("({})", params.join(", ")));
        if as_value {
            cx.insert(qualified)
        } else {
            insert_at(cx, cx.partial(), call(&qualified, &params))
        }
    } else if item.kind == DefKind::Interface && want.colons() {
        cx.insert(format!("{qualified}::"))
    } else {
        cx.insert(qualified)
    };
    entry.additional = plan.edit.clone().into_iter().collect();
    entry
}

/// A call of `path` with every parameter a placeholder: the snippet, and
/// the plain text with the parameters' names.
fn call(path: &str, params: &[&str]) -> (Option<String>, String) {
    if params.is_empty() {
        return (None, format!("{path}()"));
    }
    let snippet: Vec<String> = params
        .iter()
        .zip(1_u32..)
        .map(|(p, n)| format!("${{{n}:{}}}", super::items::escape(p)))
        .collect();
    (
        Some(format!(
            "{}({})$0",
            super::items::escape(path),
            snippet.join(", ")
        )),
        format!("{path}({})", params.join(", ")),
    )
}

#[cfg(test)]
#[path = "auto_import_tests.rs"]
mod tests;
