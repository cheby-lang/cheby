//! Auto-import of a completed name (X §4.4, D-1395).
//!
//! [`complete`] is called in `Expr` and `Type` contexts when the
//! `completion.autoImport` setting is on. Once the partial word has two
//! characters, it offers the items of modules the file does not import:
//!
//! - **Sources**: the modules of the package and the std modules the run
//!   loaded, by their interfaces, and the std modules it did not load, by
//!   std's index (D-1372). Modules of other packages are left out, since a
//!   package imports only its own and std's.
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
//! - **Order and cap**: by name, then module path, at most
//!   [`CAP`] items. When more match, the list is incomplete, so the client
//!   asks again as the word grows. [`admits`] and [`push_capped`] also
//!   serve the pipe candidates that need an import (D-1479).
//! - **Short partial**: below two characters nothing is imported, and the
//!   list is incomplete, so a client that filters by itself still asks
//!   again at the second character, where the imports start (D-1487).
//!
//! `labelDetails.description` is the module's whole path, so that two
//! items of one name can be told apart.

use std::collections::BTreeMap;

use cheby_driver::analysis::{
    CtorFields, DefId, DefKind, ItemSig, ModuleId, ModulePath, ModuleView, Namespace, PackageId,
    StdItem, builtin,
};

use super::items::{Entry, rank};
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

/// Pushes the items of `ns` from modules the file does not import. With
/// `as_value`, the position takes a function's name, not a call, so a
/// function is written bare, as the module's own are (X §4.1).
pub(crate) fn complete(cx: &CompletionCx<'_>, ns: Namespace, as_value: bool, out: &mut Items) {
    let partial = cx.partial_text();
    if !may_import(partial) {
        // The imports start at the second character, so the client must
        // ask again (D-1487).
        out.incomplete = true;
        return;
    }
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let mut found: Vec<(String, String, Found<'_>)> = Vec::new();
    loaded(cx, view, ns, partial, &mut found);
    indexed(cx, ns, partial, &mut found);
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
            Found::Loaded { def, .. } => loaded_entry(cx, def, name, path, plan, as_value),
            Found::Indexed(item) => Some(indexed_entry(cx, item, plan, as_value)),
        }
    });
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

/// Whether an item of `kind` is named in `ns`: functions, constants and
/// constructors as values, types, aliases and interfaces as types.
const fn fits(ns: Namespace, kind: DefKind) -> bool {
    match ns {
        Namespace::Value => matches!(kind, DefKind::Fn | DefKind::Const | DefKind::Ctor),
        Namespace::Type => matches!(kind, DefKind::Type | DefKind::Alias | DefKind::Interface),
    }
}

/// Whether `partial` is a subsequence of `name`, ignoring ASCII case.
fn matches(partial: &str, name: &str) -> bool {
    let mut chars = name.chars();
    partial
        .chars()
        .all(|p| chars.any(|c| c.eq_ignore_ascii_case(&p)))
}

/// Pushes the matching items of the loaded modules the file does not
/// import: the package's own and std's, the builtin items homed in std
/// modules that the prelude does not hold among them.
fn loaded<'s>(
    cx: &CompletionCx<'s>,
    view: ModuleView<'s>,
    ns: Namespace,
    partial: &str,
    found: &mut Vec<(String, String, Found<'s>)>,
) {
    let table = cx.snapshot.table();
    let Some(own) = table.get(view.id()).map(|i| i.package) else {
        return;
    };
    let targets = cx.snapshot.targets();
    for module in cx.snapshot.all_modules() {
        let id = module.id();
        let Some(info) = table.get(id) else { continue };
        if id == ModuleId::BUILTIN
            || id == view.id()
            || (info.package != own && info.package != PackageId::STD)
            || cx.module_binding(id).is_some()
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
                fits(ns, item.kind)
                    && iface.visible_from(index, own)
                    && iface.is_current(index)
                    && !iface.is_hidden(index)
            })
            .map(|(index, _)| DefId::new(id, index))
            .collect();
        defs.extend(homed(cx, &info.path, ns));
        for def in defs {
            let Some(item) = cx.item(def) else { continue };
            let name = cx.name_text(item.name);
            if item.targets.intersection(targets) != targets || !matches(partial, name) {
                continue;
            }
            found.push((name.to_owned(), path.clone(), Found::Loaded { def }));
        }
    }
}

/// The builtin items homed at `path` that the prelude does not hold, so
/// that only their home module names them (D-481), with a type's
/// constructors.
fn homed(cx: &CompletionCx<'_>, path: &ModulePath, ns: Namespace) -> Vec<DefId> {
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
    defs.retain(|&def| cx.item(def).is_some_and(|item| fits(ns, item.kind)));
    defs
}

/// Pushes the matching items of std's index whose module the run did not
/// load.
fn indexed<'s>(
    cx: &CompletionCx<'s>,
    ns: Namespace,
    partial: &str,
    found: &mut Vec<(String, String, Found<'s>)>,
) {
    let Some(index) = cx.std.get() else { return };
    let targets = cx.snapshot.targets();
    let mut loaded: BTreeMap<&str, bool> = BTreeMap::new();
    for item in &index.items {
        let name = item.key.text();
        if !fits(ns, item.kind)
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
/// `name`, qualified as `plan` says.
fn loaded_entry(
    cx: &CompletionCx<'_>,
    def: DefId,
    name: &str,
    path: &str,
    plan: &ImportPlan,
    as_value: bool,
) -> Option<Entry> {
    let item = cx.item(def)?;
    let mut entry = cx.def_entry(def, name, rank::NOT_IMPORTED)?;
    entry.description = Some(path.to_owned());
    let qualified = format!("{}::{name}", plan.name);
    entry.insert = match &item.sig {
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
/// written bare with `as_value`.
fn indexed_entry(
    cx: &CompletionCx<'_>,
    item: &StdItem,
    plan: &ImportPlan,
    as_value: bool,
) -> Entry {
    let mut entry = cx.std_entry(item, rank::NOT_IMPORTED);
    entry.description = Some(item.module.clone());
    let qualified = format!("{}::{}", plan.name, item.key.text());
    entry.insert = if item.kind == DefKind::Fn && !cx.before_paren() {
        let params: Vec<&str> = item.param_names.iter().map(String::as_str).collect();
        entry.label_detail = Some(format!("({})", params.join(", ")));
        if as_value {
            cx.insert(qualified)
        } else {
            insert_at(cx, cx.partial(), call(&qualified, &params))
        }
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
