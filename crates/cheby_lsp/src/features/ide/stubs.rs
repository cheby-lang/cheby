//! Stubs for E0063's missing functions, "Satisfy interface…" over
//! `near_miss::near_misses` (X §6.6, D-1696), the bound and the debug
//! printing of E0063, and "Make visible" for E0043, E0074, W0005 and E0063
//! `LessVisible` (D-1684, D-1706). Titles are D-1736's.
//!
//! Everything is read from facts, interfaces, the occurrence index and the
//! tree at a diagnostic's code and primary span, never from its message
//! (D-1388).
//!
//! **Stubs.** On an E0063 whose reason is `Missing`, one action per function
//! of the interface that the type's module lacks, and "Add all missing
//! functions" when it lacks several. "Satisfy interface…" is a `source`
//! action on a type's declaration, one per near miss. A stub follows X
//! §6.6's rules: it goes in the type's module, with the type's visibility
//! (D-164); `Self` is the type with its own parameters, which the stub
//! declares; parameter names come from `scope::fresh` in that module; `-> Nil`
//! is left out (D-083); each type is written by `Printer::type_source` in that
//! module, with the imports it needs, and a type it cannot write, or one
//! whose import would need an alias, offers nothing (D-1678). It goes after
//! the last function of the module that takes the type first, else after
//! the type's declaration, with one blank line before it, and its body is
//! `todo` on a line of its own.
//!
//! - A type of the binary's std, seen from a package, gets the action
//!   `disabled`: its module is no file the session may edit (ADR-0023). So
//!   does a name the module already uses for a function of another type,
//!   which E0063's help asks to move away (D-045).
//! - In a std checkout a stub is offered on a checkout type as on a package
//!   type, unless it would have to be `pub` (D-1706).
//!
//! **Make visible.** The item is raised to the visibility its use needs:
//! package-visible for a `priv` item that another module of the package names
//! (E0043), the visibility its signature's item needs for a type or an
//! interface (E0074, D-636), `pub` for an item nothing in the package uses
//! (W0005), and the type's for a satisfying function (E0063 `LessVisible`,
//! D-164). Only an item of the session's own modules is raised, and in a std
//! checkout never to `pub` (D-1706). A field path's step names a field, not an
//! item, so E0043 there offers nothing (D-1694). E0074 on a type named
//! through an alias raises the type the alias stands for, which the checker
//! reports, and an E0063 reached through `Element` or `Bound` stubs or
//! raises at the end of its chain, the type and interface that failed
//! (D-1745).
//!
//! An edit within the request's file is in the listing; one in another file
//! is computed on resolve, from the same reading of the report, and the
//! frame runs `verify` on it (D-1684).

use std::path::Path;

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    DefId, DefIndex, DefKind, FileLocation, ItemSig, ModuleView, Namespace, PlainDiagnostic,
    PlainEdit, Reason, Res, Snapshot, TextRange, Ty, TyKind, Unsatisfied, UnsatisfiedFact, Vis,
    Want, builtin, infer, interface_fns,
};
use cheby_syntax::names::to_snake_case;
use cheby_syntax::{SyntaxElement, SyntaxKind as K};

use super::Refusal;
use super::actions::{Action, ActionId, ActionKind, DiagRef, FixCx, formatted};
use super::cursor::token_at;
use super::fill_arms::{line_end, newline_at};
use super::imports::import_plan;
use super::near_miss::near_misses;
use super::print::{Printer, hides_prelude};
use super::scope;
use crate::features::index::Meaning;

/// D-1736's title of "Add all missing functions".
pub(crate) const ADD_ALL: &str = "Add all missing functions";

/// The stub and visibility actions for `range`.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let Some(path) = cx.path() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &(index, diagnostic) in diagnostics {
        if diagnostic.code == codes::UNSATISFIED_INTERFACE {
            unsatisfied(cx, path, index, diagnostic, &mut out);
        }
        for fix in visibility_fixes(cx, diagnostic) {
            out.extend(visibility_action(cx, path, index, &fix));
        }
    }
    out.extend(satisfy(cx, path, range));
    out
}

/// The edits of a stub action, found again from the report at `range` or
/// the type's declaration around it.
///
/// # Errors
///
/// A [`Refusal`] when the stub is not offered any more.
pub(crate) fn resolve(
    cx: &FixCx<'_>,
    range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let gone = || Refusal("the interface's functions cannot be added here any more".to_owned());
    let ActionId::Stub {
        interface,
        function,
    } = id
    else {
        return Err(gone());
    };
    let snapshot = &cx.analyzed.snapshot;
    let wanted = |iface: DefId| item_path(snapshot, iface).as_deref() == Some(interface.as_str());
    for (_, diagnostic) in report_at(cx, range) {
        let Some(fact) = fact_of(cx.view, diagnostic) else {
            continue;
        };
        let err = leaf(&fact.err);
        let Want::Iface(iface) = err.want else {
            continue;
        };
        if !wanted(iface) {
            continue;
        }
        let Some(ty) = nominal(snapshot, err.ty) else {
            continue;
        };
        let missing = missing_fns(snapshot, ty, iface);
        let fns: Vec<DefId> = match function {
            Some(name) => missing
                .into_iter()
                .filter(|f| fn_name(snapshot, *f).as_deref() == Some(name.as_str()))
                .collect(),
            None => missing,
        };
        if fns.is_empty() {
            continue;
        }
        return stub_edits(cx, ty, &fns).map_err(refusal_of);
    }
    if let Some(ty) = type_decl_at(cx, range) {
        for miss in near_misses(snapshot, cx.view, ty.index) {
            if wanted(miss.interface)
                && function.as_deref() == fn_name(snapshot, miss.function).as_deref()
            {
                return stub_edits(cx, ty, &[miss.function]).map_err(refusal_of);
            }
        }
    }
    Err(gone())
}

/// The edits of "Make visible", found again from the report at `range`.
///
/// # Errors
///
/// A [`Refusal`] when the item needs no raise any more.
pub(crate) fn resolve_make_visible(
    cx: &FixCx<'_>,
    range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let gone = || Refusal("the item is visible enough already".to_owned());
    let ActionId::MakeVisible { module, item } = id else {
        return Err(gone());
    };
    let snapshot = &cx.analyzed.snapshot;
    for (_, diagnostic) in report_at(cx, range) {
        for fix in visibility_fixes(cx, diagnostic) {
            let Some(at) = snapshot.module(fix.def.module) else {
                continue;
            };
            let same_module = at.path().is_some_and(|p| p.to_string() == *module);
            if !same_module || item_name(snapshot, fix.def).as_deref() != Some(item.as_str()) {
                continue;
            }
            let edit = visibility_edit(snapshot, fix.def, fix.to).ok_or_else(gone)?;
            let (_, text, _) = file_of(snapshot, at).ok_or_else(gone)?;
            let path = edit.path.clone();
            return Ok(formatted(text, &path, vec![edit], false, cx.cancel));
        }
    }
    Err(gone())
}

/// The report's diagnostics whose range touches `range`, as the frame lists
/// them.
fn report_at<'a>(cx: &FixCx<'a>, range: TextRange) -> Vec<(usize, &'a PlainDiagnostic)> {
    cx.report.map_or_else(Vec::new, |report| {
        report
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.range
                    .is_some_and(|at| at.start() <= range.end() && range.start() <= at.end())
            })
            .collect()
    })
}

/// The `UnsatisfiedFact` of an E0063 or E0065 of `view`, by its primary span.
fn fact_of<'a>(view: ModuleView<'a>, diagnostic: &PlainDiagnostic) -> Option<&'a UnsatisfiedFact> {
    if diagnostic.code != codes::UNSATISFIED_INTERFACE {
        return None;
    }
    let at = diagnostic.range?;
    let item = scope::body_at(view, at.start())?;
    view.facts()?.body(item)?.unsatisfied_at(at)
}

/// The actions of the E0063 `diagnostic`, the report's `index`-th.
fn unsatisfied(
    cx: &FixCx<'_>,
    path: &Path,
    index: usize,
    diagnostic: &PlainDiagnostic,
    out: &mut Vec<Action>,
) {
    let Some(at) = diagnostic.range else {
        return;
    };
    if let Some(edit) = debug_printing(cx, path, at) {
        out.push(fixing(edit, index));
    }
    let Some(fact) = fact_of(cx.view, diagnostic) else {
        return;
    };
    if let Some(bound) = bound(cx, path, at, &fact.err) {
        out.push(fixing(bound, index));
    }
    out.extend(
        stub_actions(cx, path, leaf(&fact.err))
            .into_iter()
            .map(|a| fixing(a, index)),
    );
}

/// The end of `err`'s chain of `Element` and `Bound` reasons: the type
/// and the interface whose satisfaction failed, where a stub or a raise
/// goes (D-1745).
fn leaf(err: &Unsatisfied) -> &Unsatisfied {
    let mut leaf = err;
    while let Reason::Bound { inner, .. } | Reason::Element { inner, .. } = &leaf.reason {
        leaf = inner;
    }
    leaf
}

/// An action of this file with nothing else set.
fn action(title: String, kind: ActionKind) -> Action {
    Action {
        title,
        kind,
        diagnostic: None,
        preferred: false,
        removes: false,
        edit: None,
        data: None,
        disabled: None,
        command: None,
    }
}

/// `action`, fixing the report's `index`-th diagnostic.
fn fixing(mut action: Action, index: usize) -> Action {
    action.diagnostic = Some(DiagRef::Report(index));
    action
}

/// "Use debug printing `{x:?}`" for an E0063 at `at` inside an interpolation
/// with no `:?` yet, whatever the reason (X §6.6).
fn debug_printing(cx: &FixCx<'_>, path: &Path, at: TextRange) -> Option<Action> {
    let root = cx.view.tree()?.root();
    let token = token_at(root, at.start()).right()?;
    let interpolation = token
        .parent()?
        .ancestors()
        .find(|n| n.kind() == K::INTERPOLATION)?;
    let tokens: Vec<_> = interpolation.tokens().collect();
    if tokens.iter().any(|t| t.kind() == K::COLON_QUESTION) {
        return None;
    }
    let end = tokens.iter().rev().find(|t| t.kind() == K::INTERP_END)?;
    let start = tokens.iter().find(|t| t.kind() == K::INTERP_START)?;
    let inner = TextRange::new(start.text_range().end(), end.text_range().start());
    let value = inner.slice(cx.text)?.trim();
    if value.is_empty() || value.contains('\n') {
        return None;
    }
    let mut fix = action(
        format!("Use debug printing `{{{value}:?}}`"),
        ActionKind::QuickFix,
    );
    fix.edit = Some(vec![PlainEdit {
        path: path.to_path_buf(),
        range: TextRange::empty(end.text_range().start()),
        replacement: ":?".to_owned(),
    }]);
    Some(fix)
}

/// "Add bound `T: Show`" when the reason, at the end of its chain, is a type
/// parameter of the enclosing item without the bound (X §6.6).
fn bound(cx: &FixCx<'_>, path: &Path, at: TextRange, err: &Unsatisfied) -> Option<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let types = &snapshot.globals().types;
    let leaf = leaf(err);
    let (Reason::Param { param }, Want::Iface(iface)) = (&leaf.reason, leaf.want) else {
        return None;
    };
    if !matches!(types.kind(leaf.ty), TyKind::Param(on) if on == param) {
        return None;
    }
    let item = scope::body_at(cx.view, at.start())?;
    let resolved = cx.view.resolved()?;
    let own = cx.view.interface()?;
    let ItemSig::Fn(sig) = &own.item(item)?.sig else {
        return None;
    };
    let generic = sig.generics.params.get(*param)?;
    let name = snapshot.globals().names.get(generic.name);
    let interface = type_name(cx, iface, Some(&sig.generics))?;
    let decl = cx.view.tree()?.node(resolved.item(item)?.node)?;
    let list = decl.children().find(|c| c.kind() == K::TYPE_PARAM_LIST)?;
    let position = sig
        .generics
        .params
        .iter_enumerated()
        .position(|(at, _)| at == *param)?;
    let node = list
        .children()
        .filter(|c| c.kind() == K::TYPE_PARAM)
        .nth(position)?;
    let written = node
        .children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .find(|t| t.kind() == K::UPPER_IDENT)?;
    if written.text() != name {
        return None;
    }
    let (range, replacement) = match node.children().find(|c| c.kind() == K::BOUND_LIST) {
        Some(bounds) => (
            TextRange::empty(bounds.text_range().end()),
            format!(" + {interface}"),
        ),
        None => (
            TextRange::empty(written.text_range().end()),
            format!(": {interface}"),
        ),
    };
    let mut fix = action(
        format!("Add bound `{name}: {interface}`"),
        ActionKind::QuickFix,
    );
    fix.edit = Some(vec![PlainEdit {
        path: path.to_path_buf(),
        range,
        replacement,
    }]);
    Some(fix)
}

/// The stubs of an E0063 whose reason is a missing function, or a name of
/// another type's function, which stays disabled (D-045).
fn stub_actions(cx: &FixCx<'_>, path: &Path, err: &Unsatisfied) -> Vec<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let Want::Iface(iface) = err.want else {
        return Vec::new();
    };
    let Some(ty) = nominal(snapshot, err.ty) else {
        return Vec::new();
    };
    let Some(interface) = item_path(snapshot, iface) else {
        return Vec::new();
    };
    match &err.reason {
        Reason::Missing { .. } => {}
        Reason::Signature { found, .. } if of_another_type(snapshot, *found, ty) => {
            let Some(name) = item_name(snapshot, *found) else {
                return Vec::new();
            };
            let mut stub = action(
                format!("Add the missing function `{name}`"),
                ActionKind::QuickFix,
            );
            let ty_name = item_name(snapshot, ty).unwrap_or_default();
            stub.disabled = Some(format!(
                "move `{ty_name}` and its functions to a module of their own"
            ));
            return vec![stub];
        }
        Reason::Signature { .. }
        | Reason::LessVisible { .. }
        | Reason::KeyFunction { .. }
        | Reason::CallSiteFunction { .. }
        | Reason::HomeNotInDeps { .. }
        | Reason::NoModule
        | Reason::Param { .. }
        | Reason::Dyn { .. }
        | Reason::Element { .. }
        | Reason::Bound { .. }
        | Reason::Undetermined { .. }
        | Reason::Descriptor { .. }
        | Reason::TooDeep => return Vec::new(),
    }
    let missing = missing_fns(snapshot, ty, iface);
    let mut out = Vec::new();
    let mut each = |title: String, fns: &[DefId], function: Option<String>| {
        let mut stub = action(title, ActionKind::QuickFix);
        match stub_edits(cx, ty, fns) {
            Ok(edits) => place(&mut stub, path, edits, || ActionId::Stub {
                interface: interface.clone(),
                function,
            }),
            Err(Offer::Disabled(reason)) => stub.disabled = Some(reason),
            Err(Offer::Not) => return,
        }
        out.push(stub);
    };
    for &f in &missing {
        let Some(name) = fn_name(snapshot, f) else {
            continue;
        };
        each(
            format!("Add the missing function `{name}`"),
            &[f],
            Some(name),
        );
    }
    if missing.len() > 1 {
        each(ADD_ALL.to_owned(), &missing, None);
    }
    out
}

/// Gives `action` its edits when they are all in the request's file, else
/// the id a resolve computes them from (D-1684).
fn place(action: &mut Action, path: &Path, edits: Vec<PlainEdit>, id: impl FnOnce() -> ActionId) {
    if edits.iter().all(|edit| edit.path == path) {
        action.edit = Some(edits);
    } else {
        action.data = Some(id());
    }
}

/// "Satisfy interface…" on the type whose declaration holds `range`: one
/// `source` action per near miss (D-1696).
fn satisfy(cx: &FixCx<'_>, path: &Path, range: TextRange) -> Vec<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let Some(ty) = type_decl_at(cx, range) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for miss in near_misses(snapshot, cx.view, ty.index) {
        let (Some(function), Some(interface)) = (
            fn_name(snapshot, miss.function),
            item_path(snapshot, miss.interface),
        ) else {
            continue;
        };
        let Some(shown) =
            type_name(cx, miss.interface, None).or_else(|| item_name(snapshot, miss.interface))
        else {
            continue;
        };
        let mut stub = action(
            format!("Satisfy interface `{shown}`: add `{function}`"),
            ActionKind::Source,
        );
        match stub_edits(cx, ty, &[miss.function]) {
            Ok(edits) => place(&mut stub, path, edits, || ActionId::Stub {
                interface,
                function: Some(function),
            }),
            Err(Offer::Disabled(reason)) => stub.disabled = Some(reason),
            Err(Offer::Not) => continue,
        }
        out.push(stub);
    }
    out
}

/// The type whose declaration, in the request's module, holds `range`.
fn type_decl_at(cx: &FixCx<'_>, range: TextRange) -> Option<DefId> {
    let root = cx.view.tree()?.root();
    let token = token_at(root, range.start());
    let token = token.right().or_else(|| token.left())?;
    let decl = token
        .parent()?
        .ancestors()
        .find(|n| n.kind() == K::TYPE_DECL)?;
    if range.end() > decl.text_range().end() {
        return None;
    }
    let resolved = cx.view.resolved()?;
    let (index, _) = resolved
        .items
        .iter_enumerated()
        .find(|(_, item)| item.kind == DefKind::Type && item.node == decl.id())?;
    Some(resolved.def_id(index))
}

/// Why a stub or a raise is not offered as an edit.
enum Offer {
    /// Shown as a `disabled` action with this reason (X §6.6).
    Disabled(String),
    /// Not offered at all.
    Not,
}

impl From<Offer> for String {
    fn from(offer: Offer) -> Self {
        match offer {
            Offer::Disabled(reason) => reason,
            Offer::Not => "the stub cannot be written here".to_owned(),
        }
    }
}

/// The [`Refusal`] a resolve answers for an [`Offer`].
fn refusal_of(offer: Offer) -> Refusal {
    Refusal(offer.into())
}

/// The nominal type `ty` names, if it is one.
fn nominal(snapshot: &Snapshot, ty: Ty) -> Option<DefId> {
    match snapshot.globals().types.kind(ty) {
        TyKind::Named { def, .. } => Some(*def),
        TyKind::Error
        | TyKind::Never
        | TyKind::Param(_)
        | TyKind::Dyn { .. }
        | TyKind::Tuple(_)
        | TyKind::Fn { .. } => None,
    }
}

/// The module whose functions count for the type `ty` (§8.2): its own, or
/// a builtin type's home module (D-447).
fn home_of(snapshot: &Snapshot, ty: DefId) -> Option<ModuleView<'_>> {
    let item = snapshot.interfaces().get(ty.module)?.item(ty.index)?;
    let ItemSig::Type(def) = &item.sig else {
        return None;
    };
    if let Some(home) = &def.home {
        return snapshot.module(snapshot.table().find(home)?);
    }
    snapshot.module(ty.module)
}

/// The functions of `iface` that `ty`'s module has no function of the name
/// of, in the interface's order.
fn missing_fns(snapshot: &Snapshot, ty: DefId, iface: DefId) -> Vec<DefId> {
    let interfaces = snapshot.interfaces();
    let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
    let Some(home) = home_of(snapshot, ty).and_then(ModuleView::interface) else {
        return Vec::new();
    };
    interface_fns(iface, &item_of)
        .unwrap_or_default()
        .into_iter()
        .filter(|&f| item_of(f).is_some_and(|f| home.lookup(Namespace::Value, f.name).is_none()))
        .collect()
}

/// Whether `found`, a function of the name an interface asks of `ty`'s
/// module, takes another type of that module first: a module satisfies an
/// interface for one type only (D-045).
fn of_another_type(snapshot: &Snapshot, found: DefId, ty: DefId) -> bool {
    let Some(item) = snapshot
        .interfaces()
        .get(found.module)
        .and_then(|i| i.item(found.index))
    else {
        return false;
    };
    let ItemSig::Fn(sig) = &item.sig else {
        return false;
    };
    sig.params.first().is_some_and(|first| {
        matches!(
            snapshot.globals().types.kind(first.ty),
            TyKind::Named { def, .. } if *def != ty && def.module == found.module
        )
    })
}

/// The stubs of `fns`, functions of one interface, for the type `ty` (module
/// doc), with the imports their types need, formatted when they are in
/// another file than the request's (D-1735).
fn stub_edits(cx: &FixCx<'_>, ty: DefId, fns: &[DefId]) -> Result<Vec<PlainEdit>, Offer> {
    let snapshot = &cx.analyzed.snapshot;
    let ty_item = snapshot
        .interfaces()
        .get(ty.module)
        .and_then(|i| i.item(ty.index))
        .ok_or(Offer::Not)?;
    let ItemSig::Type(def) = &ty_item.sig else {
        return Err(Offer::Not);
    };
    let home = home_of(snapshot, ty).ok_or(Offer::Not)?;
    if !home.is_own() {
        let ty_name = snapshot.globals().names.get(ty_item.name);
        return Err(Offer::Disabled(format!(
            "`{ty_name}` is in `std`: wrap it in a new type (ADR-0023)"
        )));
    }
    if snapshot.is_std_checkout() && ty_item.vis == Vis::Pub {
        return Err(Offer::Not);
    }
    let (path, text, _) = file_of(snapshot, home).ok_or(Offer::Not)?;
    let at = placement(snapshot, home, ty).ok_or(Offer::Not)?;
    let mut writer = StubWriter {
        cx,
        home,
        path,
        printer: Printer::new(snapshot, home),
        generics: &def.generics,
        imports: Vec::new(),
    };
    let newline = newline_at(text, at);
    let mut stubs: Vec<String> = Vec::with_capacity(fns.len());
    for &f in fns {
        stubs.push(writer.stub(ty, ty_item.vis, f, at, newline)?);
    }
    let sep = format!("{newline}{newline}");
    let mut edits = writer.imports;
    edits.push(PlainEdit {
        path: path.to_path_buf(),
        range: TextRange::empty(at),
        replacement: format!("{sep}{}", stubs.join(&sep)),
    });
    if cx.path() == Some(path) {
        return Ok(edits);
    }
    Ok(formatted(text, path, edits, false, cx.cancel))
}

/// What writing the stubs of one type reads, and the imports they need.
struct StubWriter<'a> {
    cx: &'a FixCx<'a>,
    /// The type's home module, where the stubs go.
    home: ModuleView<'a>,
    /// Its file.
    path: &'a Path,
    printer: Printer<'a>,
    /// The type's parameters, which the stubs declare.
    generics: &'a cheby_driver::analysis::Generics,
    /// The import edits the types written so far need.
    imports: Vec<PlainEdit>,
}

impl StubWriter<'_> {
    /// `ty` as code in the home module, with its imports added.
    fn write(&mut self, ty: Ty) -> Result<String, Offer> {
        let snapshot = &self.cx.analyzed.snapshot;
        let source = self
            .printer
            .type_source(ty, Some(self.generics))
            .ok_or(Offer::Not)?;
        let std = if snapshot.is_std_checkout() {
            None
        } else {
            self.cx.std.index()
        };
        for module in &source.imports {
            let plan = import_plan(snapshot, self.home, std, module).ok_or(Offer::Not)?;
            // A taken name needs an alias, which the type's text lacks.
            if module.segments().last().map(String::as_str) != Some(plan.name.as_str()) {
                return Err(Offer::Not);
            }
            if let Some((range, line)) = plan.edit {
                let edit = PlainEdit {
                    path: self.path.to_path_buf(),
                    range,
                    replacement: line,
                };
                if !self.imports.contains(&edit) {
                    self.imports.push(edit);
                }
            }
        }
        Ok(source.text)
    }

    /// The stub of the interface function `f` for the type `ty` of visibility
    /// `vis`, to be written at `at` with line breaks `newline`.
    fn stub(
        &mut self,
        ty: DefId,
        vis: Vis,
        f: DefId,
        at: cheby_driver::analysis::TextSize,
        newline: &str,
    ) -> Result<String, Offer> {
        let snapshot = &self.cx.analyzed.snapshot;
        let types = &snapshot.globals().types;
        let names = &snapshot.globals().names;
        let item = snapshot
            .interfaces()
            .get(f.module)
            .and_then(|i| i.item(f.index))
            .ok_or(Offer::Not)?;
        let ItemSig::InterfaceFn(sig) = &item.sig else {
            return Err(Offer::Not);
        };
        let args: Box<[Ty]> = self
            .generics
            .params
            .iter_enumerated()
            .map(|(param, _)| types.param(param))
            .collect();
        let self_ty = types.named(ty, args);
        let ty_name = item_name(snapshot, ty).ok_or(Offer::Not)?;
        let mut taken: Vec<String> = Vec::new();
        let mut params: Vec<String> = Vec::new();
        let mut self_count = 0_u32;
        for &param in &sig.params {
            let param = infer::substitute(types, param, &[self_ty]);
            let wanted = if param == self_ty {
                self_count = self_count.saturating_add(1);
                if self_count == 1 {
                    to_snake_case(&ty_name)
                } else {
                    "other".to_owned()
                }
            } else {
                param_name(snapshot, param)
            };
            let name = scope::fresh(snapshot, self.home, at, &wanted, &taken).ok_or(Offer::Not)?;
            taken.push(name.clone());
            params.push(format!("{name}: {}", self.write(param)?));
        }
        let ret = infer::substitute(types, sig.ret, &[self_ty]);
        let ret = if ret == Ty::NIL {
            String::new()
        } else {
            format!(" -> {}", self.write(ret)?)
        };
        let vis = match vis {
            Vis::Pub => "pub ",
            Vis::Package => "",
            Vis::Priv => "priv ",
        };
        let type_params = if self.generics.params.is_empty() {
            String::new()
        } else {
            let list: Vec<&str> = self
                .generics
                .params
                .iter()
                .map(|p| names.get(p.name))
                .collect();
            format!("<{}>", list.join(", "))
        };
        let fn_name = names.get(item.name);
        Ok(format!(
            "{vis}fn {fn_name}{type_params}({}){ret} {{{newline}  todo{newline}}}",
            params.join(", ")
        ))
    }
}

/// The name of a stub's parameter of type `ty`, other than `Self` (X §6.6): the
/// snake case of its type's name, with `_value` for a builtin type, `f` for
/// a function.
fn param_name(snapshot: &Snapshot, ty: Ty) -> String {
    let names = &snapshot.globals().names;
    let item_name = |def: DefId| {
        snapshot
            .interfaces()
            .get(def.module)
            .and_then(|i| i.item(def.index))
            .map(|item| to_snake_case(names.get(item.name)))
    };
    match snapshot.globals().types.kind(ty) {
        TyKind::Named { def, .. } if def.module == cheby_driver::analysis::ModuleId::BUILTIN => {
            item_name(*def).map_or_else(|| "value".to_owned(), |n| format!("{n}_value"))
        }
        TyKind::Named { def, .. } => item_name(*def).unwrap_or_else(|| "value".to_owned()),
        TyKind::Dyn { iface, .. } => item_name(*iface).unwrap_or_else(|| "value".to_owned()),
        TyKind::Fn { .. } => "f".to_owned(),
        TyKind::Error | TyKind::Never | TyKind::Param(_) | TyKind::Tuple(_) => "value".to_owned(),
    }
}

/// Where the stubs of `ty` go in `home`: after the last function that takes
/// the type first, else after the type's declaration.
fn placement(
    snapshot: &Snapshot,
    home: ModuleView<'_>,
    ty: DefId,
) -> Option<cheby_driver::analysis::TextSize> {
    let tree = home.tree()?;
    let resolved = home.resolved()?;
    let own = home.interface()?;
    let types = &snapshot.globals().types;
    let takes = |index: DefIndex| {
        own.item(index).is_some_and(|item| match &item.sig {
            ItemSig::Fn(sig) => sig.params.first().is_some_and(
                |first| matches!(types.kind(first.ty), TyKind::Named { def, .. } if *def == ty),
            ),
            ItemSig::Type(_)
            | ItemSig::Ctor(_)
            | ItemSig::Alias(_)
            | ItemSig::Const(_)
            | ItemSig::Interface(_)
            | ItemSig::InterfaceFn(_)
            | ItemSig::Test
            | ItemSig::Error => false,
        })
    };
    let last_fn = resolved
        .items
        .iter_enumerated()
        .filter(|(index, item)| item.kind == DefKind::Fn && takes(*index))
        .filter_map(|(_, item)| tree.node(item.node))
        .map(|node| node.text_range().end())
        .max();
    let anchor = match last_fn {
        Some(end) => end,
        None if ty.module == home.id() => {
            let decl = resolved.item(ty.index)?;
            tree.node(decl.node)?.text_range().end()
        }
        None => {
            // A builtin type's home module: after its last item.
            tree.root()
                .children()
                .filter(|n| n.kind() != K::ERROR)
                .last()?
                .text_range()
                .end()
        }
    };
    Some(anchor)
}

/// The path, text and tree of `view`'s file, when it is a file of the
/// session.
fn file_of<'s>(
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
) -> Option<(&'s Path, &'s str, &'s cheby_syntax::SyntaxTree)> {
    let file = view.file()?;
    let FileLocation::Package(path) = snapshot.location(file)? else {
        return None;
    };
    Some((path, snapshot.source(file)?.text(), view.tree()?))
}

/// The path of the item `def`, `app::shape::Area`, as `data` names it.
fn item_path(snapshot: &Snapshot, def: DefId) -> Option<String> {
    let module = snapshot.table().get(def.module)?;
    Some(format!("{}::{}", module.path, item_name(snapshot, def)?))
}

/// The name of the item `def`.
fn item_name(snapshot: &Snapshot, def: DefId) -> Option<String> {
    let item = snapshot.interfaces().get(def.module)?.item(def.index)?;
    Some(snapshot.globals().names.get(item.name).to_owned())
}

/// The name of the interface function `f`.
fn fn_name(snapshot: &Snapshot, f: DefId) -> Option<String> {
    item_name(snapshot, f)
}

/// How the request's module writes the type or interface `def`: bare when it
/// declares it, imports it by name or has it from the prelude, else
/// through its module's import. `None` when it cannot name it, or a prelude
/// name it hides, also by a type parameter of `generics` (D-1678, D-1719).
fn type_name(
    cx: &FixCx<'_>,
    def: DefId,
    generics: Option<&cheby_driver::analysis::Generics>,
) -> Option<String> {
    let snapshot = &cx.analyzed.snapshot;
    let item = snapshot.interfaces().get(def.module)?.item(def.index)?;
    let name = snapshot.globals().names.get(item.name);
    if hides_prelude(snapshot, cx.view, def, generics) {
        return None;
    }
    if def.module == cx.view.id() || builtin::is_prelude(def) {
        return Some(name.to_owned());
    }
    let resolved = cx.view.resolved()?;
    if resolved
        .scope
        .get(Namespace::Type, item.name)
        .is_some_and(|entry| entry.res == Res::Def(def))
    {
        return Some(name.to_owned());
    }
    let imports = resolved.scope.import_names();
    let module = imports.modules.get(&def.module)?;
    Some(format!("{}::{name}", snapshot.globals().names.get(*module)))
}

/// One raise of an item's visibility (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisFix {
    /// The item.
    def: DefId,
    /// The visibility it needs.
    to: Vis,
}

/// The raises a diagnostic asks for (module doc).
fn visibility_fixes(cx: &FixCx<'_>, diagnostic: &PlainDiagnostic) -> Vec<VisFix> {
    let snapshot = &cx.analyzed.snapshot;
    let Some(at) = diagnostic.range else {
        return Vec::new();
    };
    let fix = match diagnostic.code {
        codes::PRIVATE_ITEM => item_at(cx, at)
            .or_else(|| qualified_item(cx, at))
            .map(|def| VisFix {
                def,
                to: Vis::Package,
            }),
        codes::LESS_VISIBLE_IN_SIGNATURE => item_at(cx, at)
            .or_else(|| type_item_at(cx, at))
            .and_then(|def| {
                let to = needed_by_signature(cx, at)?;
                let def = match def_kind(snapshot, def)? {
                    DefKind::Type | DefKind::Interface => def,
                    // An alias is transparent, so the checker reports the first
                    // type it stands for that is less visible (D-081, D-1745).
                    DefKind::Alias => through_alias(snapshot, def, to)?,
                    DefKind::Fn
                    | DefKind::Const
                    | DefKind::InterfaceFn
                    | DefKind::Ctor
                    | DefKind::Test => return None,
                };
                Some(VisFix { def, to })
            }),
        codes::UNUSED_PACKAGE_ITEM => item_at(cx, at).map(|def| VisFix { def, to: Vis::Pub }),
        codes::UNSATISFIED_INTERFACE => {
            fact_of(cx.view, diagnostic).and_then(|fact| match &leaf(&fact.err).reason {
                Reason::LessVisible {
                    found, type_vis, ..
                } => Some(VisFix {
                    def: *found,
                    to: *type_vis,
                }),
                Reason::Missing { .. }
                | Reason::KeyFunction { .. }
                | Reason::CallSiteFunction { .. }
                | Reason::Signature { .. }
                | Reason::HomeNotInDeps { .. }
                | Reason::NoModule
                | Reason::Param { .. }
                | Reason::Dyn { .. }
                | Reason::Element { .. }
                | Reason::Bound { .. }
                | Reason::Undetermined { .. }
                | Reason::Descriptor { .. }
                | Reason::TooDeep => None,
            })
        }
        _ => None,
    };
    fix.filter(|fix| {
        let current = snapshot
            .interfaces()
            .get(fix.def.module)
            .and_then(|i| i.item(fix.def.index))
            .map(|item| item.vis);
        current.is_some_and(|vis| rank(vis) < rank(fix.to))
            && snapshot
                .module(fix.def.module)
                .is_some_and(ModuleView::is_own)
            && !(snapshot.is_std_checkout() && fix.to == Vis::Pub)
    })
    .into_iter()
    .collect()
}

/// The type or interface that the alias `alias` stands for which is less
/// visible than `to`, the first one in the order the checker reads the
/// alias's target (D-081, D-1745): the type itself, then its arguments.
fn through_alias(snapshot: &Snapshot, alias: DefId, to: Vis) -> Option<DefId> {
    let item = snapshot.interfaces().get(alias.module)?.item(alias.index)?;
    let ItemSig::Alias(def) = &item.sig else {
        return None;
    };
    first_less_visible(snapshot, def.target, to)
}

/// The first type or interface in `ty` less visible than `to`, as the
/// checker's E0074 finds it.
fn first_less_visible(snapshot: &Snapshot, ty: Ty, to: Vis) -> Option<DefId> {
    let types = &snapshot.globals().types;
    let (own, parts): (Option<DefId>, Vec<Ty>) = match types.kind(ty) {
        TyKind::Named { def, args } => (Some(*def), args.to_vec()),
        TyKind::Dyn { iface, args } => (Some(*iface), args.to_vec()),
        TyKind::Tuple(elems) => (None, elems.to_vec()),
        TyKind::Fn { params, ret } => {
            let mut parts = params.to_vec();
            parts.push(*ret);
            (None, parts)
        }
        TyKind::Error | TyKind::Never | TyKind::Param(_) => (None, Vec::new()),
    };
    let less = |def: DefId| {
        snapshot
            .interfaces()
            .get(def.module)
            .and_then(|i| i.item(def.index))
            .is_some_and(|item| rank(item.vis) < rank(to))
    };
    if let Some(def) = own.filter(|def| less(*def)) {
        return Some(def);
    }
    parts
        .into_iter()
        .find_map(|part| first_less_visible(snapshot, part, to))
}

/// The order of visibilities, `priv` lowest.
const fn rank(vis: Vis) -> u8 {
    match vis {
        Vis::Priv => 0,
        Vis::Package => 1,
        Vis::Pub => 2,
    }
}

/// The kind of the item `def`.
fn def_kind(snapshot: &Snapshot, def: DefId) -> Option<DefKind> {
    Some(snapshot.interfaces().get(def.module)?.item(def.index)?.kind)
}

/// The item the name at exactly `at` names, from the occurrence index: a
/// field, a local or a module names none.
fn item_at(cx: &FixCx<'_>, at: TextRange) -> Option<DefId> {
    let index = cx.analyzed.index();
    let module = index.of_file(cx.view.file()?)?;
    let occurrence = module.at(at)?;
    match module.entry(occurrence.symbol)?.meaning {
        Meaning::Item(def) => Some(def),
        Meaning::Module(_)
        | Meaning::Field { .. }
        | Meaning::TypeParam { .. }
        | Meaning::Local { .. } => None,
    }
}

/// The item that the type written at exactly `at` names when the type is
/// more than a name, `tok::Tok` or `Pair<Other>`, which E0074 reports
/// whole: the item of the last name of its path (D-1745).
fn type_item_at(cx: &FixCx<'_>, at: TextRange) -> Option<DefId> {
    let root = cx.view.tree()?.root();
    let node = match root.covering_element(at)? {
        SyntaxElement::Node(node) => node,
        SyntaxElement::Token(token) => token.parent()?,
    };
    let ty = node
        .ancestors()
        .find(|n| n.kind() == K::PATH_TYPE && n.text_range() == at)?;
    let last = ty
        .first_child_of_kind(K::PATH)?
        .tokens()
        .rfind(|t| matches!(t.kind(), K::UPPER_IDENT | K::LOWER_IDENT))?;
    item_at(cx, last.text_range())
}

/// The item that the path segment at exactly `at` names after its module
/// qualifier, `shape::secret`, read from the tree and the module's scope:
/// what E0043 reports where the index places no name, since the resolver
/// gives a hidden item no meaning. A segment after a type, as a field
/// path's, names no item.
fn qualified_item(cx: &FixCx<'_>, at: TextRange) -> Option<DefId> {
    let snapshot = &cx.analyzed.snapshot;
    let root = cx.view.tree()?.root();
    let token = token_at(root, at.start()).right()?;
    if token.text_range() != at || !matches!(token.kind(), K::LOWER_IDENT | K::UPPER_IDENT) {
        return None;
    }
    let path = token.parent().filter(|p| p.kind() == K::PATH)?;
    let tokens: Vec<_> = path
        .tokens()
        .filter(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE)
        .collect();
    let here = tokens.iter().position(|t| t.text_range() == at)?;
    let [qualifier, colons] =
        [here.checked_sub(2)?, here.checked_sub(1)?].map(|n| tokens.get(n).copied());
    let (qualifier, colons) = (qualifier?, colons?);
    if colons.kind() != K::COLON2 || qualifier.kind() != K::LOWER_IDENT || here != 2 {
        return None;
    }
    let names = &snapshot.globals().names;
    let imports = cx.view.resolved()?.scope.import_names();
    let mut bound: Vec<_> = imports
        .modules
        .iter()
        .filter(|(_, name)| names.get(**name) == qualifier.text())
        .map(|(module, _)| *module)
        .collect();
    // The map's order cannot reach the answer: one module is kept, and
    // only when it is the only one bound to the name.
    bound.sort_unstable();
    let [module] = bound.as_slice() else {
        return None;
    };
    let iface = snapshot.interfaces().get(*module)?;
    let name = names.lookup(token.text())?;
    let index = iface
        .lookup(Namespace::Value, name)
        .or_else(|| iface.lookup(Namespace::Type, name))?;
    Some(DefId::new(*module, index))
}

/// The visibility the item whose signature holds `at` needs of the types
/// and interfaces it mentions (D-148, D-636): its own, except the fields of
/// a `pub` type that is not `exposed`, which need package-visible ones.
fn needed_by_signature(cx: &FixCx<'_>, at: TextRange) -> Option<Vis> {
    let root = cx.view.tree()?.root();
    let token = token_at(root, at.start()).right()?;
    let decl = token
        .parent()?
        .ancestors()
        .find(|n| n.parent().is_some_and(|p| p.parent().is_none()))?;
    let resolved = cx.view.resolved()?;
    let item = resolved.items.iter().find(|item| item.node == decl.id())?;
    let exposed = decl
        .children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .any(|t| t.kind() == K::EXPOSED_KW);
    let in_fields = decl.kind() == K::TYPE_DECL
        && decl
            .children()
            .filter(|c| matches!(c.kind(), K::VARIANT_LIST | K::RECORD_FIELD_LIST))
            .any(|body| body.text_range().contains_range(at));
    Some(match item.vis {
        Vis::Pub if in_fields && !exposed => Vis::Package,
        Vis::Pub => Vis::Pub,
        Vis::Package => Vis::Package,
        Vis::Priv => return None,
    })
}

/// The action that raises `fix.def`, for the report's `index`-th diagnostic.
fn visibility_action(cx: &FixCx<'_>, path: &Path, index: usize, fix: &VisFix) -> Option<Action> {
    let snapshot = &cx.analyzed.snapshot;
    let name = item_name(snapshot, fix.def)?;
    let module = snapshot.module(fix.def.module)?.path()?.to_string();
    let edit = visibility_edit(snapshot, fix.def, fix.to)?;
    let title = match fix.to {
        Vis::Pub => format!("Make `{name}` `pub`"),
        Vis::Package | Vis::Priv => format!("Make `{name}` package-visible"),
    };
    let mut raise = fixing(action(title, ActionKind::QuickFix), index);
    place(&mut raise, path, vec![edit], || ActionId::MakeVisible {
        module,
        item: name,
    });
    Some(raise)
}

/// The edit that gives the declaration of `def` the visibility `to`: `pub`
/// written or put for `priv`, or `priv` removed.
fn visibility_edit(snapshot: &Snapshot, def: DefId, to: Vis) -> Option<PlainEdit> {
    let view = snapshot.module(def.module)?;
    let (path, text, tree) = file_of(snapshot, view)?;
    let item = view.resolved()?.item(def.index)?;
    let decl = tree.node(item.node)?;
    let visibility = decl.children().find(|c| c.kind() == K::VISIBILITY);
    let (range, replacement) = match (visibility, to) {
        (Some(vis), Vis::Pub) => (vis.text_range(), "pub".to_owned()),
        (Some(vis), Vis::Package) => {
            let start = vis.text_range().start();
            let after = vis.text_range().end();
            let rest = TextRange::new(after, line_end(text, after)).slice(text)?;
            let spaces = rest
                .len()
                .saturating_sub(rest.trim_start_matches([' ', '\t']).len());
            let end = after.checked_add(u32::try_from(spaces).ok()?)?;
            (TextRange::new(start, end), String::new())
        }
        (None, Vis::Pub) => {
            let first = decl.children_with_tokens().find(|e| match e {
                SyntaxElement::Node(n) => n.kind() != K::ATTR,
                SyntaxElement::Token(t) => {
                    !t.kind().is_trivia() && !matches!(t.kind(), K::NEWLINE | K::DOC_COMMENT)
                }
            })?;
            let at = match first {
                SyntaxElement::Node(n) => n.text_range().start(),
                SyntaxElement::Token(t) => t.text_range().start(),
            };
            (TextRange::empty(at), "pub ".to_owned())
        }
        (None | Some(_), Vis::Package | Vis::Priv) => return None,
    };
    Some(PlainEdit {
        path: path.to_path_buf(),
        range,
        replacement,
    })
}

#[cfg(test)]
#[path = "stubs_tests.rs"]
mod tests;
