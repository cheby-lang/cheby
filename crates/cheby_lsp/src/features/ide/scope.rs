//! Names in scope at a cursor (X §1.5, F §4): the body at an offset, the
//! locals visible there, and the free-name tests that new code needs,
//! since Cheby has no shadowing (D-051, E0044).
//!
//! A local is named by its body's item and its slot, its position in the
//! body's locals, as [`crate::features::index::Meaning::Local`] names it:
//! the same position holds it in the body's facts (F §1.3).

use std::cmp::Reverse;

use cheby_driver::analysis::{
    DefIndex, LocalDecl, ModuleView, Name, Namespace, Res, ScopeSource, Snapshot, TextSize, thir,
};
use cheby_syntax::SyntaxKind;
use cheby_syntax::names::{NameCase, check_candidate};

/// One local visible at a cursor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalAt<'s> {
    /// The item whose body declares it.
    pub item: DefIndex,
    /// Its position in the body's locals.
    pub slot: u32,
    /// The resolver's record of it.
    pub decl: &'s LocalDecl,
}

/// The item whose body holds `offset` (F §4.1): the one whose node holds
/// it, else the last one before it whose text does not end with `}`, since
/// a cursor after an unclosed body's last token is outside its node.
/// `None` outside every body, or in a module with no resolution.
pub(crate) fn body_at(view: ModuleView<'_>, offset: TextSize) -> Option<DefIndex> {
    let (tree, resolved) = (view.tree()?, view.resolved()?);
    let mut bodies: Vec<_> = resolved
        .bodies
        .keys()
        .filter_map(|&index| {
            let node = tree.node(resolved.item(index)?.node)?;
            Some((node.text_range(), index, node))
        })
        .collect();
    bodies.sort_by_key(|&(range, _, _)| range.start());
    if let Some(&(_, index, _)) = bodies.iter().find(|(range, _, _)| range.contains(offset)) {
        return Some(index);
    }
    bodies
        .iter()
        .rev()
        .find(|(range, _, _)| range.end() <= offset)
        .filter(|(_, _, node)| {
            node.tokens()
                .rev()
                .find(|t| !t.kind().is_trivia() && t.kind() != SyntaxKind::NEWLINE)
                .is_some_and(|t| t.kind() != SyntaxKind::R_BRACE)
        })
        .map(|&(_, index, _)| index)
}

/// The locals visible at `offset`, innermost first: the order of
/// `BodyRes::locals_at` (F §4.1). Names are unique among them (D-051),
/// except where E0044 was reported.
pub(crate) fn locals_at(view: ModuleView<'_>, offset: TextSize) -> Vec<LocalAt<'_>> {
    let Some(item) = body_at(view, offset) else {
        return Vec::new();
    };
    let Some(body) = view.resolved().and_then(|r| r.bodies.get(&item)) else {
        return Vec::new();
    };
    let mut locals: Vec<LocalAt<'_>> = body
        .locals
        .iter()
        .zip(0_u32..)
        .filter(|(decl, _)| decl.visible_at(offset))
        .map(|(decl, slot)| LocalAt { item, slot, decl })
        .collect();
    locals.sort_by_key(|local| Reverse((local.decl.scope.start(), local.slot)));
    locals
}

/// The resolver's record of local `slot` of the body of `item`.
pub(crate) fn local_decl(view: ModuleView<'_>, item: DefIndex, slot: u32) -> Option<&LocalDecl> {
    let body = view.resolved()?.bodies.get(&item)?;
    body.locals.iter().nth(usize::try_from(slot).ok()?)
}

/// The checker's record of local `slot` of the body of `item`: its closed
/// type. `None` when the body has no facts.
pub(crate) fn local_fact(view: ModuleView<'_>, item: DefIndex, slot: u32) -> Option<&thir::Local> {
    let body = view.facts()?.body(item)?;
    body.locals.iter().nth(usize::try_from(slot).ok()?)
}

/// Whether a new local `name` at `offset` would reuse no name (E0044,
/// D-051): no local of the body at `offset` has it, wherever its scope
/// is, so a binding after the new one cannot clash either; and it is no
/// function or constant of the module and no imported module or value.
/// The prelude does not count, since a local may hide it. `view` is a
/// module of `snapshot`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "W10's rename and W11's fixes check names")
)]
pub(crate) fn is_free(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    offset: TextSize,
    name: &str,
) -> bool {
    // A text no name has interned is no name of the run.
    let Some(name) = snapshot.globals().names.lookup(name) else {
        return true;
    };
    let in_body = body_at(view, offset)
        .and_then(|item| view.resolved()?.bodies.get(&item))
        .is_some_and(|body| body.locals.iter().any(|decl| decl.name == name));
    !in_body && !in_module_scope(view, name)
}

/// Whether `name` is free at every point of the module: no value of the
/// module scope but the prelude's, and no local of any body, has it. An
/// import's name must be this, or the import needs an alias (D-1395).
/// `view` is a module of `snapshot`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "W10's rename and W11's fixes check names")
)]
pub(crate) fn is_free_in_module(snapshot: &Snapshot, view: ModuleView<'_>, name: &str) -> bool {
    let Some(name) = snapshot.globals().names.lookup(name) else {
        return true;
    };
    let in_bodies = view.resolved().is_some_and(|resolved| {
        resolved
            .bodies
            .values()
            .any(|body| body.locals.iter().any(|decl| decl.name == name))
    });
    !in_bodies && !in_module_scope(view, name)
}

/// A name for a new local at `offset`: `wanted`, else `wanted_value`, else
/// `wanted_value_2`, `_3` and so on, the first that [`is_free`] accepts,
/// that is a lower-case name E0006 accepts (X §3.3), and that is not in
/// `taken`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "W10's rename and W11's fixes check names")
)]
pub(crate) fn fresh(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    offset: TextSize,
    wanted: &str,
    taken: &[String],
) -> String {
    let fits = |name: &str| {
        check_candidate(name, NameCase::Lower).is_ok()
            && !taken.iter().any(|t| t == name)
            && is_free(snapshot, view, offset, name)
    };
    let value = format!("{wanted}_value");
    std::iter::once(wanted.to_owned())
        .chain(std::iter::once(value.clone()))
        .chain((2..=u32::MAX).map(|n| format!("{value}_{n}")))
        .find(|name| fits(name))
        .unwrap_or(value)
}

/// Whether the module scope binds `name` as a value from one of the
/// module's items or imports, which a local must not reuse (E0044).
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "W10's rename and W11's fixes check names")
)]
fn in_module_scope(view: ModuleView<'_>, name: Name) -> bool {
    view.resolved()
        .and_then(|resolved| resolved.scope.get(Namespace::Value, name))
        .is_some_and(|entry| {
            matches!(entry.source, ScopeSource::Item | ScopeSource::Import(_))
                && !matches!(entry.res, Res::Err)
        })
}
