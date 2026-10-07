//! What is left of the support gate (D-451) in name resolution: E0038 for
//! a function without a body whose `@external` attributes do not cover
//! every target its module is built for, until the cross-target check
//! (C12) lands. The two other E0038 rows of name resolution are imports of
//! planned standard-library modules ([`crate::resolve_import_targets`]) and
//! names missing from a partly shipped standard-library module
//! ([`crate::paths`]).

use cheby_diag::{Diagnostic, Span, TextRange, codes};
use cheby_iface::{DefIndex, DefKind, Ice, IndexVec};
use cheby_syntax::ast::{AstNode, FnDecl, Item};

use crate::{ItemDef, ItemStatus, ResolveInput, items::Collected};

/// The E0038 diagnostic: "`<what>` are not supported yet".
pub(crate) fn not_supported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("{what} are not supported yet"),
    )
    .with_primary(span, "")
}

pub(crate) fn gate_module(
    input: &ResolveInput<'_>,
    collected: &mut Collected,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    // What the module's code runs on: its package's targets, narrowed by
    // `@!target` (§12.6).
    let required = input.package_targets.intersection(collected.module_targets);
    let items = &mut collected.items;
    for index in items.indices().collect::<Vec<_>>() {
        let item = items.try_get(index, "item")?;
        if item.kind != DefKind::Fn || item.attrs.externals.is_empty() {
            continue;
        }
        let Some(decl) = input.tree.node(item.node).and_then(FnDecl::cast) else {
            return Err(Ice::new(
                "a function item's node is not a function declaration",
            ));
        };
        // A bodiless function exists only on the targets of its externals
        // (§12.5), and code that uses it must be restricted to them (§12.6,
        // D-056, D-190). Until the cross-target check (C12) lands, such a
        // function is rejected unless it exists on every target its module
        // is built for, so no unchecked use can get through.
        if decl.body().is_some() || item.attrs.targets.intersection(required) == required {
            continue;
        }
        diags.push(not_supported(
            head_span(decl, item.name_span),
            "functions that exist only on some of the package's targets",
        ));
        mark(items, index)?;
    }
    Ok(())
}

/// Marks a function whose body alone holds unsupported constructs, unless
/// its declaration is already unsupported.
pub(crate) fn mark_body(
    items: &mut IndexVec<DefIndex, ItemDef>,
    index: DefIndex,
) -> Result<(), Ice> {
    let item = items
        .get_mut(index)
        .ok_or_else(|| Ice::new("gated item is missing"))?;
    if item.status == ItemStatus::Ok {
        item.status = ItemStatus::BodyUnsupported;
    }
    Ok(())
}

/// Marks an item unsupported.
fn mark(items: &mut IndexVec<DefIndex, ItemDef>, index: DefIndex) -> Result<(), Ice> {
    items
        .get_mut(index)
        .ok_or_else(|| Ice::new("gated item is missing"))?
        .status = ItemStatus::Unsupported;
    Ok(())
}

/// From the start of the declaration to the end of its name: `pub fn now`.
fn head_span(decl: FnDecl<'_>, name: Span) -> Span {
    let item = Item::Fn(decl);
    let full = item.significant_span();
    let start = item
        .visibility()
        .map(|v| v.significant_span().range.start())
        .or_else(|| item.keyword().map(|t| t.span().range.start()))
        .unwrap_or(full.range.start());
    let end = name.range.end();
    Span::new(full.file, TextRange::new(start, end.max(start)))
}
