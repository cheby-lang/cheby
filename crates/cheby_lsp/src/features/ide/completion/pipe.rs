//! Completion after `|>` (X §4.3), and the pipe-style functions that
//! `dot` offers after `xs.` (X §4.2).
//!
//! [`complete`] is called for [`Context::Pipe`](super::context::Context::Pipe):
//! after `xs |> ` and while typing a name there. [`qualified`] is called
//! for a [`Context::Qualified`](super::context::Context::Qualified) with `piped`:
//! `xs |> list::`, where only the `map(_, …)` part is written.
//!
//! A function is offered when a parameter **accepts** the left side's
//! type: a one-way match with the function's type parameters as variables
//! (`accepts`, X §4.2), where each type a bounded variable stands for
//! satisfies its bounds, so no item makes E0063. Its rank says where it comes from (X §4.2, §4.3):
//!
//! | Rank | Source                                                     |
//! | ---- | ---------------------------------------------------------- |
//! | 10   | the module of the left side's head type, `std::list` for a `List` |
//! | 15   | a local of function type, written bare                     |
//! | 20   | the current module                                         |
//! | 30   | imported modules and imported functions                    |
//! | 40   | other loaded modules, with an import (after `\|>` only)    |
//! | 60   | std modules the run did not load, by head (after `\|>` only) |
//!
//! A function accepted only at a later parameter goes 5 ranks later. A
//! bare type parameter accepts anything, so it counts only for ranks 10 to
//! 30. The hole is always a whole argument at the accepting position, and
//! a one-parameter function is written bare, so neither E0027 nor E0028
//! can arise (D-244, D-245): `list::map(_, ${1:f})$0`, `list::length`.
//!
//! After `|>`, a function that needs an import, of rank 40, rank 60, or
//! of a head type's module the file does not import, follows auto-import's
//! rule (D-1479): it is offered only for a partial word of two characters
//! that its name holds as a subsequence, at most 100 of them by name, then
//! module path, the list incomplete when the cap cut it. Locals and the
//! functions of imported modules are not limited. With
//! `completion.autoImport` on, a partial word shorter than two characters
//! gives an incomplete list, so the client asks again where the imports
//! start (D-1487).
//!
//! With `completion.autoImport` off, no function that needs an import is
//! offered, after `|>` or after `xs.`, the head type's module included
//! when the file does not import it (D-1486).
//!
//! After `|>`, a function written through a module path, `list::map`, has
//! that path as its label, the bare name as `filterText` and the module's
//! whole path as `labelDetails.description` (D-1508). One the module
//! calls by its own name keeps that name as its label.
//!
//! Where a path's `::` follows the word after `|>`, as
//! `xs |> st‸::length`, [`complete`] is not called: only what a path
//! starts with is offered, the modules and the interfaces, written bare,
//! since a function written through a module path would write a second
//! path (D-1521, D-1530), the interfaces only when one segment follows
//! the `::` (D-1539). The list is complete there (D-1525). Before a
//! turbofish, as `xs |> re‸::<Int>`, the functions and the local
//! functions are offered as with no `::` after the word, and no module
//! name, since `list::<Int>` is no path, only names taking type arguments
//! there (D-1525, D-1529, D-1536).
//!
//! After `xs |> shape::`, the module's interfaces are offered with its
//! functions, written `Shape::` (D-1548), and they alone, written
//! `Shape`, where a path's `::` follows the segment (D-1538). After
//! `xs |> I::` and `xs |> m::I::`, the interface's functions come in the
//! pipe form, the hole at the first parameter of type `Self`, bare when
//! that is the only one, at rank 20, or 25 when `Self` comes later
//! (D-1545). A function whose `Self` is only inside a parameter's type
//! has no such parameter and is not offered.
//!
//! The interfaces start a path after `|>` as the modules do, whatever
//! the left side's type: those in scope bare, `Show::`, those of
//! imported modules through the module, `ops::Compare::`, from the
//! second character, and with `completion.autoImport` on those to
//! import, capped with the functions to import (D-1546). One the file
//! imports by name comes only by that name, `Compare::`, as a function
//! does (D-1550). With a one-character partial, an imported module's
//! interface makes the list incomplete, as it comes at the second
//! (D-1521).

use std::collections::BTreeMap;

use cheby_driver::analysis::{
    DefId, DefKind, FnSig, Generics, ItemSig, LocalKind, ModuleId, ModuleView, Namespace,
    PackageId, Res, ScopeSource, StdItem, TextRange, Ty, TyKind, TypeHead, Verdict, accepts,
    accepts_binding, builtin, satisfies,
};
use cheby_syntax::SyntaxNode;

use super::auto_import::{admits, imports_an_interface, may_import, pipe_interfaces, push_capped};
use super::context::PathNs;
use super::items::{
    Entry, EntryKind, Insert, escape, interface_items, module_interfaces, path_interface,
    qualified_module, rank, scope_interfaces,
};
use super::{CompletionCx, Items};
use crate::features::ide::imports::{ImportPlan, import_plan, parse_path, path_text};
use crate::features::ide::scope;

/// Ranks of this module and `dot`'s that [`rank`] does not name.
pub(super) mod ranks {
    use super::rank;

    /// A function of the left side's head type's module (X §4.2).
    pub(crate) const HEAD: u8 = rank::LOCAL;
    /// What a function accepted only at a later parameter adds.
    pub(crate) const LATER: u8 = 5;
}

/// A function the left side can be piped into.
#[derive(Clone, Debug)]
pub(super) struct Candidate {
    /// The function.
    pub def: DefId,
    /// Its rank, [`ranks::LATER`] included.
    pub rank: u8,
    /// The position of the parameter that accepts the left side.
    pub hole: usize,
    /// How the code names it: `list::map`, or `map` for a function of the
    /// current module or one imported by name.
    pub path: String,
    /// The import that makes `path` valid, if one is needed (D-1395).
    pub import: Option<(TextRange, String)>,
}

/// The name of `def`'s module as code at the cursor writes it, with the
/// import that needs, or `None` when it cannot be named without an error.
fn qualifier(cx: &CompletionCx<'_>, view: ModuleView<'_>, module: ModuleId) -> Option<ImportPlan> {
    if let Some(name) = cx.module_binding(module) {
        return Some(ImportPlan {
            name: name.to_owned(),
            edit: None,
        });
    }
    let path = cx.snapshot.table().get(module)?.path.clone();
    import_plan(cx.snapshot, view, None, &path)
}

/// The signature of function `def`, if it is a function.
fn fn_sig<'s>(cx: &CompletionCx<'s>, def: DefId) -> Option<&'s FnSig> {
    match &cx.item(def)?.sig {
        ItemSig::Fn(sig) => Some(sig),
        _ => None,
    }
}

/// The first parameter of `sig` that accepts `left`, with the bounds of
/// the type parameters it binds holding, so that the call it writes
/// cannot fail with E0063. A bare type parameter counts only when
/// `generic` holds.
fn accepted_at(cx: &CompletionCx<'_>, sig: &FnSig, left: Ty, generic: bool) -> Option<usize> {
    let types = &cx.snapshot.globals().types;
    sig.params.iter().position(|param| {
        (generic || !matches!(types.kind(param.ty), TyKind::Param(_)))
            && accepts_binding(types, param.ty, left)
                .ok()
                .flatten()
                .is_some_and(|bound| bounds_hold(cx, &sig.generics, &bound))
    })
}

/// Whether each type in `bound`, by the index of the type parameter of
/// `generics` it stands for, satisfies that parameter's bounds with no
/// condition, read in the body at the cursor. A parameter the match left
/// unbound is not tested: its type comes from another argument.
fn bounds_hold(cx: &CompletionCx<'_>, generics: &Generics, bound: &[Option<Ty>]) -> bool {
    let wanted: Vec<(Ty, DefId)> = generics
        .params
        .iter()
        .zip(bound)
        .filter_map(|(param, ty)| Some((param, (*ty)?)))
        .flat_map(|(param, ty)| param.bounds.iter().map(move |&iface| (ty, iface)))
        .collect();
    if wanted.is_empty() {
        return true;
    }
    let Some(view) = cx.resolved_view() else {
        return false;
    };
    let Some(input) = cx.snapshot.query_input(view.id(), view) else {
        return false;
    };
    let empty = Generics::empty();
    let own = cx
        .body()
        .and_then(|(item, _)| cx.generics(DefId::new(view.id(), item)))
        .unwrap_or(&empty);
    wanted.into_iter().all(|(ty, iface)| {
        matches!(
            satisfies(&input.input, input.own, own, ty, iface),
            Ok(Verdict::Yes { when, .. }) if when.is_empty()
        )
    })
}

/// The functions of `module` that the cursor's module may call, in
/// declaration order.
fn functions_of(cx: &CompletionCx<'_>, view: ModuleView<'_>, module: ModuleId) -> Vec<DefId> {
    let Some(iface) = cx.snapshot.interfaces().get(module) else {
        return Vec::new();
    };
    let Some(package) = cx.snapshot.table().get(view.id()).map(|i| i.package) else {
        return Vec::new();
    };
    let own = module == view.id();
    iface
        .items
        .iter_enumerated()
        .filter(|&(index, item)| {
            item.kind == DefKind::Fn
                && iface.is_current(index)
                && !iface.is_hidden(index)
                && (own || iface.visible_from(index, package))
        })
        .map(|(index, _)| DefId::new(module, index))
        .collect()
}

/// The module that holds the functions of `left`'s head type: its
/// declaring module, or a builtin type's home (X §4.2).
fn head_module(cx: &CompletionCx<'_>, left: Ty) -> Option<ModuleId> {
    let def = match cx.snapshot.globals().types.kind(left) {
        TyKind::Named { def, .. } => *def,
        TyKind::Dyn { iface, .. } => *iface,
        _ => return None,
    };
    if def.module != ModuleId::BUILTIN {
        return Some(def.module);
    }
    let &(_, segment) = builtin::HOMES.iter().find(|(home, _)| *home == def)?;
    cx.snapshot.table().find(&builtin::home_path(segment))
}

/// The functions the cursor's module imports by name, with the names they
/// go by, so the head type's module does not offer them with a needless
/// import edit (D-1479, D-1486).
fn imported_by_name(cx: &CompletionCx<'_>, view: ModuleView<'_>) -> BTreeMap<DefId, String> {
    let Some(resolved) = view.resolved() else {
        return BTreeMap::new();
    };
    resolved
        .scope
        .iter(Namespace::Value)
        .filter_map(|(name, entry)| match entry.res {
            Res::Def(def) if matches!(entry.source, ScopeSource::Import(_)) => {
                Some((def, cx.name_text(name).to_owned()))
            }
            _ => None,
        })
        .collect()
}

/// The candidates of ranks 10 to 30 (X §4.2): the head type's module, the
/// current module, imported modules and imported functions. With
/// `others`, also every other loaded module of the package or of std,
/// rank 40 (X §4.3). With `completion.autoImport` off, a function whose
/// module needs an import is left out (D-1486). A function imported by
/// name is written by that name, with no import edit, at any rank.
pub(super) fn candidates(cx: &CompletionCx<'_>, left: Ty, others: bool) -> Vec<Candidate> {
    let Some(view) = cx.resolved_view() else {
        return Vec::new();
    };
    let Some(resolved) = view.resolved() else {
        return Vec::new();
    };
    let mut found: BTreeMap<DefId, Candidate> = BTreeMap::new();
    let mut plans: BTreeMap<ModuleId, Option<ImportPlan>> = BTreeMap::new();
    let by_name = imported_by_name(cx, view);
    let mut offer = |def: DefId, base: u8, generic: bool, path: Option<String>| {
        let Some(item) = cx.item(def) else { return };
        let ItemSig::Fn(sig) = &item.sig else { return };
        let Some(hole) = accepted_at(cx, sig, left, generic) else {
            return;
        };
        let rank = if hole == 0 {
            base
        } else {
            base.saturating_add(ranks::LATER)
        };
        if found.get(&def).is_some_and(|c| c.rank <= rank) {
            return;
        }
        let name = cx.name_text(item.name);
        let (path, import) = match path {
            Some(path) => (path, None),
            None if def.module == view.id() => (name.to_owned(), None),
            None if let Some(local) = by_name.get(&def) => (local.clone(), None),
            None => {
                let plan = plans
                    .entry(def.module)
                    .or_insert_with(|| qualifier(cx, view, def.module));
                let Some(plan) = plan else { return };
                if plan.edit.is_some() && !cx.options.auto_import {
                    return;
                }
                (format!("{}::{name}", plan.name), plan.edit.clone())
            }
        };
        found.insert(
            def,
            Candidate {
                def,
                rank,
                hole,
                path,
                import,
            },
        );
    };
    if let Some(head) = head_module(cx, left) {
        for def in functions_of(cx, view, head) {
            offer(def, ranks::HEAD, true, None);
        }
    }
    for def in functions_of(cx, view, view.id()) {
        offer(def, rank::MODULE, true, None);
    }
    let mut imported: Vec<ModuleId> = Vec::new();
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        match entry.res {
            Res::Module(module) => imported.push(module),
            Res::Def(def) if matches!(entry.source, ScopeSource::Import(_)) => {
                offer(
                    def,
                    rank::IMPORTED,
                    true,
                    Some(cx.name_text(name).to_owned()),
                );
            }
            _ => {}
        }
    }
    imported.sort_unstable();
    for &module in &imported {
        for def in functions_of(cx, view, module) {
            offer(def, rank::IMPORTED, true, None);
        }
    }
    if others {
        let own = cx.snapshot.table().get(view.id()).map(|i| i.package);
        for module in cx.snapshot.all_modules() {
            let id = module.id();
            let package = cx.snapshot.table().get(id).map(|i| i.package);
            if id == ModuleId::BUILTIN
                || id == view.id()
                || imported.contains(&id)
                || (package != own && package != Some(PackageId::STD))
            {
                continue;
            }
            for def in functions_of(cx, view, id) {
                offer(def, rank::PRELUDE, false, None);
            }
        }
    }
    found.into_values().collect()
}

/// The call of `path` with the parameters `params`, the one at `hole`
/// written as `fill`, `_` when `fill` is `None`, the others as
/// placeholders (X §4.2): the snippet, when it has a placeholder, and the
/// plain text, with parameter names in place of placeholders. A
/// one-parameter function piped is written bare, so `f(_)` never appears
/// (D-245).
pub(super) fn call_text(
    path: &str,
    params: &[&str],
    hole: usize,
    fill: Option<&str>,
) -> (Option<String>, String) {
    if params.len() == 1 && fill.is_none() {
        return (None, path.to_owned());
    }
    let mut snippet: Vec<String> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    let mut stop = 1_u32;
    for (k, &param) in params.iter().enumerate() {
        if k == hole {
            let written = fill.unwrap_or("_");
            snippet.push(escape(written));
            plain.push(written.to_owned());
        } else {
            snippet.push(format!("${{{stop}:{}}}", escape(param)));
            plain.push(param.to_owned());
            stop = stop.saturating_add(1);
        }
    }
    let snippet = (stop > 1).then(|| format!("{}({})$0", escape(path), snippet.join(", ")));
    (snippet, format!("{path}({})", plain.join(", ")))
}

/// The parameter names of function `def`.
pub(super) fn param_names(cx: &CompletionCx<'_>, def: DefId) -> Vec<&'static str> {
    fn_sig(cx, def)
        .map(|sig| sig.params.iter().map(|p| cx.name_text(p.name)).collect())
        .unwrap_or_default()
}

/// The text of [`call_text`] over `range`: the snippet for a client that
/// takes snippets, else the plain text.
pub(super) fn insert_at(
    cx: &CompletionCx<'_>,
    range: TextRange,
    (snippet, plain): (Option<String>, String),
) -> Insert {
    match snippet {
        Some(text) if cx.options.snippets => Insert {
            range,
            text,
            snippet: true,
        },
        _ => Insert {
            range,
            text: plain,
            snippet: false,
        },
    }
}

/// The type of the left side, when completion may use it.
fn left_ty(cx: &CompletionCx<'_>, lhs: SyntaxNode<'_>) -> Option<Ty> {
    if cx.options.type_driven {
        cx.expr_ty(lhs)
    } else {
        None
    }
}

/// Pushes the items after `|>` of `pipe`, a `BIN_EXPR` whose left side is
/// `lhs`. The partial word, if any, is `cx.partial()`. With the left
/// side's type unknown, only locals of one-parameter function type and
/// the imported modules' names are offered: guessing by name would be
/// worse than nothing (X §4.2).
pub(crate) fn complete(
    cx: &CompletionCx<'_>,
    pipe: SyntaxNode<'_>,
    lhs: SyntaxNode<'_>,
    out: &mut Items,
) {
    let _ = pipe;
    // Before a path's `::` this is not called (D-1530). Before a
    // turbofish's, as `xs |> re‸::<Int>`, the functions are offered as
    // with no `::` after the word, and no module name: only names that
    // take type arguments (D-1525, D-1536).
    let partial = cx.partial_text();
    // A function that needs an import follows auto-import's rule: two
    // characters, a subsequence of its name, and the cap (D-1479).
    let importing = may_import(partial);
    if !importing && (cx.options.auto_import || imports_an_interface(cx)) {
        // The imports, and the interfaces of imported modules, start at
        // the second character, so the client must ask again (D-1487,
        // D-1521, D-1546).
        out.incomplete = true;
    }
    let left = left_ty(cx, lhs);
    local_functions(cx, left, out);
    module_names(cx, out);
    // The interfaces start a path that names their functions, as
    // `xs |> Show::show`, so they come as the modules do, whatever the
    // left side's type, those to import capped with the functions
    // (D-1479, D-1546). Before a turbofish none fits (D-1536).
    let mut needs_import: Vec<(String, String, Entry)> = Vec::new();
    if !cx.before_turbofish() {
        scope_interfaces(cx, out);
        needs_import = pipe_interfaces(cx, out);
    }
    let Some(left) = left else {
        push_capped(needs_import, out, |_, _, entry| Some(entry));
        return;
    };
    let range = cx.partial();
    for candidate in candidates(cx, left, cx.options.auto_import && importing) {
        let name = cx
            .item(candidate.def)
            .map_or("", |item| cx.name_text(item.name));
        if candidate.import.is_some() && !admits(partial, name) {
            continue;
        }
        // A function written through a module path is labelled by that
        // path and matched by its name (D-1508).
        let Some(mut entry) = cx.def_entry(candidate.def, &candidate.path, candidate.rank) else {
            continue;
        };
        let path = cx
            .snapshot
            .table()
            .get(candidate.def.module)
            .map(|info| path_text(&info.path))
            .unwrap_or_default();
        if candidate.path.contains("::") {
            entry.filter_text = Some(name.to_owned());
            entry.description = Some(path.clone());
        }
        entry.insert = if cx.before_paren() {
            cx.insert(candidate.path.clone())
        } else {
            let params = param_names(cx, candidate.def);
            insert_at(
                cx,
                range,
                call_text(&candidate.path, &params, candidate.hole, None),
            )
        };
        match candidate.import {
            Some(import) => {
                entry.additional = vec![import];
                needs_import.push((name.to_owned(), path, entry));
            }
            None => out.push(entry),
        }
    }
    if cx.options.auto_import && importing {
        unloaded(cx, left, partial, &mut needs_import);
    }
    push_capped(needs_import, out, |_, _, entry| Some(entry));
}

/// Rank 60 (X §4.3): functions of std modules the run did not load whose
/// parameter has the left side's head, with their import, those whose
/// name `partial` admits (D-1479) pushed into `found` by name and path.
fn unloaded(
    cx: &CompletionCx<'_>,
    left: Ty,
    partial: &str,
    found: &mut Vec<(String, String, Entry)>,
) {
    let head = cx.snapshot.head_of(left);
    if matches!(head, TypeHead::Param | TypeHead::Unknown) {
        return;
    }
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let Some(index) = cx.std.get() else { return };
    let targets = cx.snapshot.targets();
    let mut plans: BTreeMap<String, Option<ImportPlan>> = BTreeMap::new();
    for (item, position) in index.accepting(&head) {
        let name = item.key.text();
        if !admits(partial, name)
            || is_loaded(cx, &item.module)
            || item.targets.intersection(targets) != targets
        {
            continue;
        }
        let plan = plans.entry(item.module.clone()).or_insert_with(|| {
            let path = parse_path(&item.module)?;
            import_plan(cx.snapshot, view, Some(index), &path)
        });
        let Some(plan) = plan else { continue };
        let hole = usize::from(position);
        let rank = if hole == 0 {
            rank::NOT_IMPORTED
        } else {
            rank::NOT_IMPORTED.saturating_add(ranks::LATER)
        };
        found.push((
            name.to_owned(),
            item.module.clone(),
            std_call(cx, item, rank, plan, hole),
        ));
    }
}

/// The entry of `item`, a function of std's index, piped at `hole` and
/// qualified as `plan` says: labelled by that path, matched by its name
/// (D-1508), with the module's whole path as auto-import has it.
fn std_call(
    cx: &CompletionCx<'_>,
    item: &StdItem,
    rank: u8,
    plan: &ImportPlan,
    hole: usize,
) -> Entry {
    let mut entry = cx.std_entry(item, rank);
    let params: Vec<&str> = item.param_names.iter().map(String::as_str).collect();
    entry.label_detail = Some(format!("({})", params.join(", ")));
    entry.description = Some(item.module.clone());
    let path = format!("{}::{}", plan.name, item.key.text());
    entry.label.clone_from(&path);
    entry.filter_text = Some(item.key.text().to_owned());
    entry.insert = if cx.before_paren() {
        cx.insert(path)
    } else {
        insert_at(cx, cx.partial(), call_text(&path, &params, hole, None))
    };
    entry.additional = plan.edit.clone().into_iter().collect();
    entry
}

/// Whether the run loaded the module at `path`, so its items come from
/// its interface.
pub(super) fn is_loaded(cx: &CompletionCx<'_>, path: &str) -> bool {
    parse_path(path)
        .and_then(|p| cx.snapshot.table().find(&p))
        .is_some_and(|id| cx.snapshot.interfaces().get(id).is_some())
}

/// Rank 15 (X §4.3): the locals of a one-parameter function type whose
/// parameter accepts `left`, any when `left` is unknown, written bare.
fn local_functions(cx: &CompletionCx<'_>, left: Option<Ty>, out: &mut Items) {
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let types = &cx.snapshot.globals().types;
    for (local, order) in scope::locals_at(view, cx.offset).iter().zip(0_u16..) {
        let name = cx.name_text(local.decl.name);
        if !is_plain_name(name) {
            continue;
        }
        let Some(ty) = scope::local_fact(view, local.item, local.slot).and_then(|l| cx.known(l.ty))
        else {
            continue;
        };
        let TyKind::Fn { params, .. } = types.kind(ty) else {
            continue;
        };
        let [param] = params.as_ref() else { continue };
        if left.is_some_and(|left| !accepts(types, *param, left).unwrap_or(false)) {
            continue;
        }
        let kind = if local.decl.kind == LocalKind::LocalFn {
            EntryKind::Function
        } else {
            EntryKind::Variable
        };
        let mut entry = Entry::new(name, kind, rank::LOCAL_FN, cx.insert(name));
        entry.order = Some(order);
        entry.detail = cx.type_text(ty, Some(DefId::new(view.id(), local.item)));
        out.push(entry);
    }
}

/// Whether `name` is a name a user wrote, not `_` or one the compiler
/// made.
fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && name != "_"
}

/// The names of the imported modules, written `list::`, so a qualified
/// function can follow (rank 30). None before a turbofish (D-1525).
fn module_names(cx: &CompletionCx<'_>, out: &mut Items) {
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        let Res::Module(module) = entry.res else {
            continue;
        };
        let label = cx.name_text(name);
        let Some(text) = cx.module_text(label) else {
            continue;
        };
        let mut item = Entry::new(label, EntryKind::Module, rank::IMPORTED, cx.insert(text));
        item.description = cx.snapshot.table().get(module).map(|i| i.path.to_string());
        out.push(item);
    }
}

/// Pushes the items of `xs |> m::`, where `lhs` is the pipe's left side
/// and `qualifier` the segments before the partial word, read in `ns`.
/// Only the function's part is written: `map(_, ${1:f})$0`. A function
/// that accepts the left side ranks 20, 25 at a later parameter; one that
/// does not stays at rank 70, with the hole first (X §4.3). The module's
/// interfaces come too, written `Shape::`, as `xs |> shape::Shape::`
/// starts `shape::Shape::area` (D-173, D-1548), and they alone, written
/// `Shape`, where a path's `::` follows, as in `xs |> shape::Sh‸::area`
/// (D-1538). Before a turbofish, only the functions (D-1536).
///
/// After `xs |> I::` or `xs |> m::I::`, the interface's functions come in
/// the pipe form, the hole at the first `Self` (D-1545). None fits before
/// a path's `::` or a turbofish, which no interface function takes
/// (E0023, D-1536).
pub(crate) fn qualified(
    cx: &CompletionCx<'_>,
    lhs: SyntaxNode<'_>,
    qualifier: &[String],
    ns: PathNs,
    out: &mut Items,
) {
    if ns != PathNs::Value {
        return;
    }
    if let Some(def) = path_interface(cx, qualifier) {
        if !cx.before_path() && !cx.before_turbofish() {
            interface_items(cx, def, true, out);
        }
        return;
    }
    let [one] = qualifier else { return };
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let Some(module) = qualified_module(cx, one) else {
        return;
    };
    if !cx.before_turbofish() {
        module_interfaces(cx, module, out);
    }
    if cx.before_path() {
        return;
    }
    let left = left_ty(cx, lhs);
    for def in functions_of(cx, view, module) {
        let Some(sig) = fn_sig(cx, def) else { continue };
        // A function with no parameter cannot take a pipe.
        if sig.params.is_empty() {
            continue;
        }
        let hole = left.and_then(|left| accepted_at(cx, sig, left, true));
        let rank = match (left, hole) {
            (None, _) | (Some(_), Some(0)) => rank::MODULE,
            (Some(_), Some(_)) => rank::MODULE.saturating_add(ranks::LATER),
            (Some(_), None) => rank::NOT_ACCEPTING,
        };
        let name = cx.item(def).map_or("", |item| cx.name_text(item.name));
        let Some(mut entry) = cx.def_entry(def, name, rank) else {
            continue;
        };
        entry.insert = if cx.before_paren() {
            cx.insert(name)
        } else {
            let params = param_names(cx, def);
            insert_at(
                cx,
                cx.partial(),
                call_text(name, &params, hole.unwrap_or(0), None),
            )
        };
        out.push(entry);
    }
}

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;
