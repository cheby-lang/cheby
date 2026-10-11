//! W0002: unused local bindings and parameters (§13.8, D-080).
//!
//! Every local the resolver records counts: parameters of the item,
//! `let` and `let assert` bindings, bindings in `case` arms, `use`
//! binders, closure and local-function parameters, and local functions.
//! A local is used when a path or a name of the body resolves to it. A
//! local function's uses inside its own body (THIR's `SelfRef`, D-110) do
//! not count, so a local function that only calls itself is unused
//! (D-516). A name that starts with `_` is never reported (§2.3). Functions without a body have no locals.

use cheby_diag::{Diagnostic, TextRange, codes};
use cheby_iface::{DefIndex, DefKind, Ice, PackageId};
use cheby_resolve::{BodyRes, LocalId, LocalKind, Res};
use cheby_syntax::SyntaxKind;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::TypesInput;

/// The W0002 warnings of every body of the module, by item, then by the
/// locals' order.
///
/// # Errors
///
/// An [`Ice`] when a recorded node is missing from the tree.
pub(super) fn unused_locals(input: &TypesInput<'_>) -> Result<Vec<Diagnostic>, Ice> {
    let mut bodies: Vec<(&DefIndex, &BodyRes)> = input.resolved.bodies.iter().collect();
    bodies.sort_by_key(|&(index, _)| *index);
    let mut out = Vec::new();
    for (&index, body) in bodies {
        // The tests of a std module are not part of user builds (they are
        // checked when std itself is tested), so they warn only then.
        let is_test = input
            .resolved
            .item(index)
            .is_some_and(|item| item.kind == DefKind::Test);
        if is_test && input.package == PackageId::STD {
            continue;
        }
        body_warnings(input, body, &mut out)?;
    }
    Ok(out)
}

fn body_warnings(
    input: &TypesInput<'_>,
    body: &BodyRes,
    out: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    // Where each use is, by local.
    let mut found: FxHashMap<LocalId, Vec<TextRange>> = FxHashMap::default();
    for (&node, &res) in body.paths.iter().chain(&body.names) {
        if let Res::Local(local) = res {
            let range = input
                .tree
                .node(node)
                .ok_or_else(|| Ice::new("a resolved path is not in the tree"))?
                .text_range();
            found.entry(local).or_default().push(range);
        }
    }
    // The declaration of each local function, and punned field patterns.
    let mut local_fns: FxHashMap<LocalId, TextRange> = FxHashMap::default();
    let mut punned: FxHashSet<LocalId> = FxHashSet::default();
    for (&node, &local) in &body.bindings {
        let node = input
            .tree
            .node(node)
            .ok_or_else(|| Ice::new("a binding is not in the tree"))?;
        match node.kind() {
            SyntaxKind::LOCAL_FN => {
                local_fns.insert(local, node.text_range());
            }
            SyntaxKind::FIELD_PAT => {
                punned.insert(local);
            }
            _ => {}
        }
    }
    for (local, decl) in body.locals.iter_enumerated() {
        let name = input.globals.names.get(decl.name);
        if name.starts_with('_') {
            continue;
        }
        let own_body = local_fns.get(&local);
        let used = found.get(&local).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&range| own_body.is_none_or(|body| !body.contains_range(range)))
        });
        if used {
            continue;
        }
        let what = match decl.kind {
            LocalKind::Param | LocalKind::ClosureParam => "parameter",
            LocalKind::LocalFn => "local function",
            LocalKind::Let | LocalKind::CaseArm | LocalKind::UseBinder => "binding",
        };
        let label = if own_body.is_some() && found.contains_key(&local) {
            "used only inside its own body"
        } else {
            "never used"
        };
        let diag = Diagnostic::new(codes::UNUSED_BINDING, format!("unused {what} `{name}`"))
            .with_primary(decl.span, label);
        let diag = if punned.contains(&local) {
            diag.with_suggestion(
                format!("if this is intentional, ignore the field: `{name}: _`"),
                decl.span,
                format!("{name}: _"),
            )
        } else {
            diag.with_suggestion(
                format!("if this is intentional, start the name with an underscore: `_{name}`"),
                decl.span,
                format!("_{name}"),
            )
        };
        out.push(diag);
    }
    Ok(())
}
