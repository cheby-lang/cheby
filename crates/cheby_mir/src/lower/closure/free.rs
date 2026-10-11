//! Free-variable analysis over THIR (D4).
//!
//! THIR has no capture lists, and [`thir::Body::locals`] is shared by a
//! body and every closure and local function nested in it. A closure
//! therefore captures exactly the locals it uses (directly or through a
//! closure nested in it) whose binding is outside it. No shadowing
//! (D-051) makes every local's binding site unique, so "bound inside"
//! is a set of local ids, not a scope chain.
//!
//! A local function's own name is never one of its captures: inside its
//! body every reference to it is [`ExprKind::SelfRef`], which lowering
//! turns into the closure's `self` (D-453, D-499). A closure nested in the
//! local function that names it does capture it, like any outer local.

use std::collections::BTreeSet;

use cheby_iface::Ice;
use cheby_resolve::LocalId;
use cheby_types::thir::{self, ExprId, ExprKind, Param, PatKind};

/// The outer locals that a closure or local function with `params` and
/// body `value` uses, in first-use order (evaluation order, §5.15.1, then
/// nested bodies in place), so capture order never depends on hashing
/// (D-264). `own` is a local function's own name, which is never a
/// capture.
///
/// # Errors
///
/// An [`Ice`] for an expression id outside `body`.
pub(crate) fn free_locals(
    body: &thir::Body,
    params: &[Param],
    value: ExprId,
    own: Option<LocalId>,
) -> Result<Vec<LocalId>, Ice> {
    let mut bound: BTreeSet<LocalId> = BTreeSet::new();
    bound.extend(own);
    for p in params {
        bind_pat(&p.pat, &mut bound);
    }
    // First pass: every local bound inside the closure.
    for id in subtree(body, value)? {
        let e = expr(body, id)?;
        for (_, pat) in e.kind.pats() {
            bind_pat(pat, &mut bound);
        }
        if let ExprKind::LocalFn { local, .. } = &e.kind {
            bound.insert(*local);
        }
    }
    // Second pass: uses of the others, in order.
    let mut seen: BTreeSet<LocalId> = BTreeSet::new();
    let mut out = Vec::new();
    for id in subtree(body, value)? {
        let used = match &expr(body, id)?.kind {
            ExprKind::Local(l) | ExprKind::SelfRef { local: l } => *l,
            _ => continue,
        };
        if !bound.contains(&used) && seen.insert(used) {
            out.push(used);
        }
    }
    Ok(out)
}

/// The expressions of the tree rooted at `root` in pre-order, children in
/// evaluation order, nested closure bodies included. An explicit stack
/// keeps deep expressions off the Rust stack.
pub(crate) fn subtree(body: &thir::Body, root: ExprId) -> Result<Vec<ExprId>, Ice> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        out.push(id);
        let mut kids = expr(body, id)?.kind.children();
        kids.reverse();
        stack.extend(kids);
    }
    Ok(out)
}

fn expr(body: &thir::Body, id: ExprId) -> Result<&thir::Expr, Ice> {
    body.expr(id)
        .ok_or_else(|| Ice::new(format!("no THIR expression {id:?}")))
}

fn bind_pat(pat: &thir::Pat, bound: &mut BTreeSet<LocalId>) {
    pat.walk(&mut |p| match &p.kind {
        PatKind::Bind(l) | PatKind::As { local: l, .. } => {
            bound.insert(*l);
        }
        _ => {}
    });
}
