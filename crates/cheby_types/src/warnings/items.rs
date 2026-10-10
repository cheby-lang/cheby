//! W0004 and W0005: unused `priv` and package-visible items (§13.8,
//! D-080, D-152).
//!
//! The candidates are top-level functions, constants, types, aliases and
//! interfaces. `pub` items, tests and every `main` are never reported.
//! An item is used when another item, tests included, refers to it:
//! through a path or a name in a signature or a body, or through THIR,
//! which also holds the implicit references of operators, interpolation,
//! `dyn` conversions and dictionaries (satisfying functions). An item's
//! uses of itself, such as recursion, do not count.
//!
//! GAP(unused-item-granularity): a type counts as used when its name or
//! any of its constructors is used, in an expression or a pattern, and an
//! interface when its name or any of its functions is; constructors and
//! interface functions are not reported on their own (D-152 speaks of
//! items). A type uses the functions its `@std_hooks` names (D-342).
//!
//! GAP(unused-self-use): uses of an item from its own signature or body
//! (recursion, a recursive type) do not make it used, so an item that only
//! refers to itself is reported. An item used only by another unused item
//! is not reported: there is no transitive dead-code analysis.
//!
//! GAP(unused-item-underscore): §2.3 exempts bindings whose names start
//! with `_`, not items, so such items are reported like others.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{DefIndex, DefKind, ItemSig, ModuleId, ModuleInterface, PackageId, Vis};
use cheby_resolve::Res;
use rustc_hash::FxHashSet;

use super::uses::{PathUse, ThirRef, top_level};
use crate::TypesInput;

/// One reference from item `owner` of module `from` to item `def`.
#[derive(Clone, Copy, Debug)]
pub(super) struct ItemRef {
    pub from: ModuleId,
    pub owner: Option<DefIndex>,
    pub def: cheby_iface::DefId,
}

/// Every reference of module `input.module` to an item: its paths and
/// names, its THIR and its types' hooks.
pub(super) fn item_refs(
    input: &TypesInput<'_>,
    paths: &[PathUse],
    thir: &[ThirRef],
    hooks: &[ThirRef],
) -> Vec<ItemRef> {
    let from = input.module;
    let mut out = Vec::new();
    for path in paths {
        if let Res::Def(def) = path.res {
            out.push(ItemRef {
                from,
                owner: path.owner,
                def,
            });
        }
    }
    for r in thir.iter().chain(hooks) {
        out.push(ItemRef {
            from,
            owner: Some(r.owner),
            def: r.def,
        });
    }
    out
}

/// The top-level items of module `input.module` that something other than
/// themselves uses, among `refs`.
pub(super) fn used_items(input: &TypesInput<'_>, refs: &[ItemRef]) -> FxHashSet<DefIndex> {
    refs.iter()
        .filter(|r| r.def.module == input.module)
        .filter_map(|r| {
            let top = top_level(input, r.def.index);
            let other = r.from != input.module || r.owner != Some(top);
            other.then_some(top)
        })
        .collect()
}

/// The items of the module that may be reported with visibility `vis`.
fn candidates(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    vis: Vis,
) -> Vec<(DefIndex, &'static str)> {
    input
        .resolved
        .items
        .iter_enumerated()
        .filter(|(_, item)| item.parent.is_none() && item.vis == vis)
        .filter(|(_, item)| {
            matches!(
                item.kind,
                DefKind::Fn | DefKind::Const | DefKind::Type | DefKind::Alias | DefKind::Interface
            )
        })
        .filter(|(_, item)| {
            !(item.kind == DefKind::Fn && input.globals.names.get(item.name) == "main")
        })
        .filter(|(index, _)| {
            own.item(*index)
                .is_some_and(|i| !matches!(i.sig, ItemSig::Error))
        })
        .map(|(index, item)| (index, item.kind.describe()))
        .collect()
}

/// W0004: the module's `priv` items that no other item of the module
/// uses.
pub(super) fn unused_private(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    refs: &[ItemRef],
) -> Vec<Diagnostic> {
    let used = used_items(input, refs);
    let mut out = Vec::new();
    for (index, kind) in candidates(input, own, Vis::Priv) {
        if used.contains(&index) {
            continue;
        }
        let Some(item) = input.resolved.item(index) else {
            continue;
        };
        let name = input.globals.names.get(item.name);
        out.push(
            Diagnostic::new(
                codes::UNUSED_PRIVATE_ITEM,
                format!("the `priv` {kind} `{name}` is never used"),
            )
            .with_primary(item.name_span, "never used in this module")
            .with_note("a `priv` item is visible only in its own module")
            .with_help(format!("remove the {kind}, or use it")),
        );
    }
    out
}

/// W0005: the package-visible items of module `input.module` (a module of
/// the local package) that no other item of the package uses, given every
/// reference of every module of the package.
pub(super) fn unused_in_package(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    refs: &[ItemRef],
) -> Vec<Diagnostic> {
    if input.package != PackageId::LOCAL {
        return Vec::new();
    }
    let used = used_items(input, refs);
    let mut out = Vec::new();
    for (index, kind) in candidates(input, own, Vis::Package) {
        if used.contains(&index) {
            continue;
        }
        let Some(item) = input.resolved.item(index) else {
            continue;
        };
        let name = input.globals.names.get(item.name);
        out.push(
            Diagnostic::new(
                codes::UNUSED_PACKAGE_ITEM,
                format!("the {kind} `{name}` is never used in this package"),
            )
            .with_primary(item.name_span, "no module of the package uses it")
            .with_note(format!(
                "without `pub`, only modules of this package can use the {kind}"
            ))
            .with_help(format!(
                "remove the {kind}, use it, or make it `pub` if other packages should use it"
            )),
        );
    }
    out
}
