//! Clones of a function for what one of its calls knows (design T12,
//! T13), in the release profile's `spec` lever, on lowered MIR between
//! inlining and the clean-up ([`crate::inline`]).
//!
//! # When a call gets a clone ([`specialize_calls`])
//!
//! A direct call of a top-level function `g` gets a clone of `g` in the
//! caller's unit when one of its arguments has a known shape
//! ([`crate::simplify::Shape`]) that `g`'s body can use: a static
//! dictionary or a static closure, a function value, or a closure
//! allocated with a code word whose captures may be known in turn; and
//! the body reads a field of that parameter, calls it, or passes it to
//! another call. The body is `g`'s own when `g` is a function of the
//! unit, or its export ([`crate::export::Exported`]), which a function
//! in a cycle of its unit's call graph has too, so that a loop gets its
//! clone. `g` must be a callee the inliner could copy (no `@external`
//! wrapper's shape, no wait that keeps its frame, D-861), and it may cost
//! at most [`crate::inline::InlineBudget::clone`].
//!
//! # The clone
//!
//! - Its key is [`FnKey::Specialized`], numbered in the unit in creation
//!   order, owned by the closure owner of the function whose call made
//!   it. It is called only through [`Callee::Code`] from its unit, never
//!   borrows, and a backtrace names it as `g`.
//! - A parameter whose argument is a constant is no parameter of the
//!   clone: the clone's entry defines it as that constant. A parameter
//!   whose argument is a closure stays, since the object holds the
//!   captures; [`Known`] records its shape, so the clean-up calls the
//!   closure's code directly and the inliner copies it.
//! - A call of `g` in the clone that passes those parameters through
//!   unchanged calls the clone instead, so a loop stays a loop and its
//!   tail calls stay tail calls (D-015, D-1761).
//! - The call becomes a call of the clone with the constant arguments
//!   dropped.
//!
//! One clone serves every call of `g` with the same shapes: the unit
//! keeps a memo by `g` and the shapes. The clones of a unit cost at most
//! [`crate::inline::InlineBudget::clone_unit`] or
//! [`crate::inline::InlineBudget::clone_unit_percent`] of the unit,
//! whichever is more, and a clone made from inside a clone is made only to
//! the depth [`crate::inline::InlineBudget::clone_depth`].
//!
//! A clone copies `g`'s body, so a program does what it did: the same
//! panics with the same messages and locations, the same releases. A
//! clone of a body of another file has no source positions (D-1170), and
//! records no site for `--reuse-report` (D-717), as an inlined body
//! does.

use std::sync::Arc;

use cheby_iface::{DefId, Ice, Idx};
use rustc_hash::FxHashMap;

use crate::defs::Defs;
use crate::export::Exported;
use crate::inline::{Bodies, CallSite, InlineBudget, SiteAt};
use crate::ir::{
    BlockId, Callee, Const, FnKey, Function, MirModule, Operand, Rvalue, StaticKind, Stmt,
    Terminator,
};
use crate::simplify::{Known, Shape};

/// One clone a unit made: what it was made for, and its key.
#[derive(Clone, Debug)]
struct Made {
    origin: DefId,
    /// The shape each parameter of the origin was specialized for.
    shapes: Vec<Option<Shape>>,
    key: FnKey,
    /// 1 for a clone made for a call outside clones, one more for each
    /// clone it was made from inside.
    depth: u32,
}

/// The clones of one unit and what they may still cost (module docs).
#[derive(Debug)]
pub(crate) struct Clones {
    made: Vec<Made>,
    left: usize,
}

impl Clones {
    /// No clone yet, for a unit of cost `unit_cost`.
    pub(crate) fn new(unit_cost: usize, budget: &InlineBudget) -> Self {
        let share = unit_cost
            .saturating_mul(budget.clone_unit_percent)
            .checked_div(100)
            .unwrap_or(0);
        Self {
            made: Vec::new(),
            left: share.max(budget.clone_unit),
        }
    }

    /// The keys of the clones, in creation order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = FnKey> + '_ {
        self.made.iter().map(|m| m.key)
    }

    /// The depth of the function `key`: its clone's, or 0.
    fn depth(&self, key: FnKey) -> u32 {
        self.made
            .iter()
            .find(|m| m.key == key)
            .map_or(0, |m| m.depth)
    }
}

/// The index the unit's next clone takes: one more than the highest of its
/// clones and helpers ([`crate::export::Exported::helpers`]).
pub(crate) fn next_index(unit: &MirModule) -> u32 {
    unit.functions
        .iter()
        .filter_map(|f| match f.key {
            FnKey::Specialized { index, .. } => index.checked_add(1),
            FnKey::Item(_)
            | FnKey::ValueShim(_)
            | FnKey::Closure { .. }
            | FnKey::ConstInit(_)
            | FnKey::Test(_)
            | FnKey::Entry(_)
            | FnKey::Trampoline(_)
            | FnKey::TupleDictFn { .. }
            | FnKey::DynForward { .. }
            | FnKey::Continuation { .. } => None,
        })
        .max()
        .unwrap_or(0)
}

/// Where a clone's body comes from.
enum Origin {
    /// A function of the unit, by index.
    Own(usize),
    /// Another unit's export.
    Other(Arc<Exported>),
}

/// Whether a constant is one a clone is made for: a static dictionary or
/// closure, or a function value. Other constants are left to the
/// clean-up, so a call with a number gets no clone of its own.
const fn known_constant(c: &Const) -> bool {
    matches!(c, Const::Static(_) | Const::FnValue(_))
}

/// The shape a clone takes for an argument of shape `shape`, when it is
/// one a clone is made for.
fn clone_shape(shape: Option<&Shape>, known: &Known) -> Option<Shape> {
    match shape? {
        // A static descriptor is no dictionary and no closure.
        Shape::Const(c @ Const::Static(id)) => matches!(
            known.static_kind(*id),
            Some(StaticKind::Dict | StaticKind::FnValue)
        )
        .then(|| Shape::Const(c.clone())),
        Shape::Const(c) if known_constant(c) => Some(Shape::Const(c.clone())),
        s @ Shape::Closure { .. } => Some(portable(s)),
        Shape::Const(_) => None,
    }
}

/// `shape` without what only its own function can read: a string
/// constant indexes the function's string table, so a clone, another
/// function, does not know it.
fn portable(shape: &Shape) -> Shape {
    match shape {
        Shape::Const(_) => shape.clone(),
        Shape::Closure { code, captures } => Shape::Closure {
            code: *code,
            captures: captures
                .iter()
                .map(|c| match c {
                    Some(Shape::Const(Const::Str(_))) | None => None,
                    Some(s) => Some(portable(s)),
                })
                .collect(),
        },
    }
}

/// How the body `f` uses each of its parameters, by position: whether it
/// reads a field of it, calls it or passes it to a call other than its own
/// (`origin`'s), which a clone can use.
fn useful_params(f: &Function, origin: DefId, nested: bool) -> Vec<bool> {
    let mut out = vec![false; f.params.len()];
    let position = |l: crate::ir::Local| f.params.iter().position(|&p| p == l);
    let mark = |out: &mut Vec<bool>, l: crate::ir::Local| {
        if let Some(slot) = position(l).and_then(|k| out.get_mut(k)) {
            *slot = true;
        }
    };
    let call = |out: &mut Vec<bool>, callee: &Callee, args: &[Operand]| {
        if let Callee::Indirect(l) = callee {
            mark(out, *l);
        }
        let own = matches!(callee, Callee::Direct(d) | Callee::Borrowing(d, _) if *d == origin);
        if nested && !own {
            for a in args {
                if let Operand::Local(l) = a {
                    mark(out, *l);
                }
            }
        }
    };
    for b in &f.blocks {
        for s in &b.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = s else {
                continue;
            };
            match value {
                Rvalue::Field { base, .. } => mark(&mut out, *base),
                Rvalue::Call { callee, args } => call(&mut out, callee, args),
                Rvalue::Use(_)
                | Rvalue::CallForeign { .. }
                | Rvalue::CallRuntime { .. }
                | Rvalue::Alloc { .. }
                | Rvalue::AllocPlaceholder { .. }
                | Rvalue::InitField { .. }
                | Rvalue::ReadLayout { .. }
                | Rvalue::LoadConst(_)
                | Rvalue::Box { .. }
                | Rvalue::Unbox { .. }
                | Rvalue::Checked { .. }
                | Rvalue::Compare { .. }
                | Rvalue::FloatArith { .. }
                | Rvalue::Bitwise { .. }
                | Rvalue::FloatNeg { .. }
                | Rvalue::Convert { .. }
                | Rvalue::FloatSqrt { .. }
                | Rvalue::Wrapping { .. }
                | Rvalue::Not(_)
                | Rvalue::ReadTag { .. }
                | Rvalue::DropReuse { .. }
                | Rvalue::AllocReuse { .. } => {}
            }
        }
        if let Terminator::TailCall { callee, args } = &b.term {
            call(&mut out, callee, args);
        }
    }
    out
}

/// A direct call of `host` that may get a clone: where it is, its callee
/// and its arguments.
struct Site {
    at: CallSite,
    callee: DefId,
    args: Vec<Operand>,
}

/// The direct calls of `f` of functions other than itself, in block and
/// statement order.
fn sites(f: &Function) -> Vec<Site> {
    let mut out = Vec::new();
    let other = |d: &DefId| f.key != FnKey::Item(*d);
    for (id, b) in f.blocks.iter_enumerated() {
        for (i, s) in b.stmts.iter().enumerate() {
            if let Stmt::Assign {
                value:
                    Rvalue::Call {
                        callee: Callee::Direct(d),
                        args,
                    },
                ..
            } = s
                && other(d)
            {
                out.push(Site {
                    at: CallSite {
                        block: id,
                        at: SiteAt::Stmt(i),
                    },
                    callee: *d,
                    args: args.clone(),
                });
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Direct(d),
            args,
        } = &b.term
            && other(d)
        {
            out.push(Site {
                at: CallSite {
                    block: id,
                    at: SiteAt::Tail,
                },
                callee: *d,
                args: args.clone(),
            });
        }
    }
    out
}

/// What [`specialize_calls`] reads of the unit and the build.
pub(crate) struct Sources<'a, 'b> {
    /// The unit's functions by key ([`MirModule::function_index`]).
    pub index: &'a FxHashMap<FnKey, usize>,
    /// Whether each function of the unit may be cloned, by index: a
    /// callee the inliner could copy, cycles aside.
    pub cloneable: &'a [bool],
    /// The bodies of other units, with the build's interfaces.
    pub across: Option<(&'b dyn Bodies, &'b Defs<'b>)>,
    pub budget: &'a InlineBudget,
}

/// Gives the calls of function `host` of `unit` their clones (module
/// docs): makes the clones the memo lacks, appended to the unit's
/// functions, and points each such call at its clone. Returns the indexes
/// of the clones made, in creation order, and whether `host` changed.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR, or a def the build's interfaces cannot
/// see.
#[expect(
    clippy::too_many_lines,
    reason = "the steps of the module docs, for each call in order"
)]
pub(crate) fn specialize_calls(
    unit: &mut MirModule,
    host: usize,
    clones: &mut Clones,
    src: &Sources<'_, '_>,
    known: &mut Known,
) -> Result<(Vec<usize>, bool), Ice> {
    let mut made = Vec::new();
    let Some(f) = unit.functions.get(host) else {
        return Ok((made, false));
    };
    let depth = clones.depth(f.key);
    if !crate::inline::is_host(f) || depth >= src.budget.clone_depth {
        return Ok((made, false));
    }
    let Some(owner) = crate::inline::closure_owner(f.key) else {
        return Ok((made, false));
    };
    let nested = depth.saturating_add(1) < src.budget.clone_depth;
    let shapes = crate::simplify::shapes(f, known);
    let shape_of = |op: &Operand| match op {
        Operand::Const(c) => Some(Shape::Const(c.clone())),
        Operand::Local(l) => shapes.get(l.index()).cloned().flatten(),
    };
    let mut changed = false;
    for site in sites(f) {
        let arg_shapes: Vec<Option<Shape>> = site
            .args
            .iter()
            .map(|a| clone_shape(shape_of(a).as_ref(), known))
            .collect();
        if arg_shapes.iter().all(Option::is_none) {
            continue;
        }
        let origin = if let Some(&at) = src.index.get(&FnKey::Item(site.callee)) {
            if src.cloneable.get(at) != Some(&true) {
                continue;
            }
            Origin::Own(at)
        } else if let Some((bodies, _)) = src.across
            && let Some(export) = bodies.body(site.callee)
        {
            Origin::Other(export)
        } else {
            continue;
        };
        let body: &Function = match &origin {
            Origin::Own(at) => match unit.functions.get(*at) {
                Some(g) => g,
                None => continue,
            },
            Origin::Other(export) => &export.function,
        };
        let size = crate::inline::cost(body);
        if size > src.budget.clone || body.params.len() != site.args.len() {
            continue;
        }
        let useful = useful_params(body, site.callee, nested);
        let wanted: Vec<Option<Shape>> = arg_shapes
            .into_iter()
            .zip(&useful)
            .map(|(s, &u)| s.filter(|_| u))
            .collect();
        if wanted.iter().all(Option::is_none) {
            continue;
        }
        let existing = clones
            .made
            .iter()
            .find(|m| m.origin == site.callee && m.shapes == wanted)
            .map(|m| m.key);
        let key = if let Some(key) = existing {
            key
        } else {
            if size > clones.left {
                continue;
            }
            let Some(clone) = make_clone(unit, host, &site, origin, owner, &wanted, src, known)?
            else {
                continue;
            };
            clones.left = clones.left.saturating_sub(size);
            let key = clone.key;
            clones.made.push(Made {
                origin: site.callee,
                shapes: wanted.clone(),
                key,
                depth: depth.saturating_add(1),
            });
            known.add_functions(std::slice::from_ref(&clone));
            made.push(unit.functions.len());
            unit.functions.push(clone);
            key
        };
        let args: Vec<Operand> = site
            .args
            .iter()
            .zip(&wanted)
            .filter(|(_, s)| !matches!(s, Some(Shape::Const(_))))
            .map(|(a, _)| a.clone())
            .collect();
        let f = unit
            .functions
            .get_mut(host)
            .ok_or_else(|| Ice::new("specialize: the host"))?;
        let b = f
            .blocks
            .get_mut(site.at.block)
            .ok_or_else(|| Ice::new("specialize: the call's block"))?;
        match site.at.at {
            SiteAt::Stmt(i) => {
                if let Some(Stmt::Assign {
                    value: Rvalue::Call { callee, args: a },
                    ..
                }) = b.stmts.get_mut(i)
                {
                    *callee = Callee::Code(key);
                    *a = args;
                }
            }
            SiteAt::Tail => {
                b.term = Terminator::TailCall {
                    callee: Callee::Code(key),
                    args,
                };
            }
        }
        changed = true;
    }
    Ok((made, changed))
}

/// The clone of `origin`'s body for the call `site` of function `host`
/// (module docs), not yet in the unit; `None` when another unit's body
/// does not join this one (a static of the same symbol with other
/// contents).
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "what the call, the unit and the build give the clone, and its steps in order"
)]
fn make_clone(
    unit: &mut MirModule,
    host: usize,
    site: &Site,
    origin: Origin,
    owner: DefId,
    wanted: &[Option<Shape>],
    src: &Sources<'_, '_>,
    known: &mut Known,
) -> Result<Option<Function>, Ice> {
    let host_span = unit
        .functions
        .get(host)
        .ok_or_else(|| Ice::new("specialize: the host"))?
        .span;
    let (mut body, same_file) = match origin {
        Origin::Own(at) => (
            unit.functions
                .get(at)
                .ok_or_else(|| Ice::new("specialize: the origin"))?
                .clone(),
            true,
        ),
        Origin::Other(export) => {
            let Some((_, defs)) = src.across else {
                return Ok(None);
            };
            let Some((body, positions)) =
                crate::inline::import(unit, host, site.at.block, site.at.at, &export, defs, known)?
            else {
                return Ok(None);
            };
            (body, positions == crate::inline::CopiedPositions::Own)
        }
    };
    let index = next_index(unit);
    let key = FnKey::Specialized {
        owner,
        index,
        origin: site.callee,
    };
    let params = std::mem::take(&mut body.params);
    let mut kept = Vec::with_capacity(params.len());
    let mut kept_shapes = Vec::with_capacity(params.len());
    let mut defs = Vec::new();
    for (&p, shape) in params.iter().zip(wanted) {
        match shape {
            Some(Shape::Const(c)) => defs.push(Stmt::Assign {
                dst: p,
                value: Rvalue::Use(Operand::Const(c.clone())),
            }),
            Some(Shape::Closure { .. }) | None => {
                kept.push(p);
                kept_shapes.push(shape.clone());
            }
        }
    }
    body.params = kept;
    body.key = key;
    body.borrow = crate::borrow::BorrowMask::OWNED;
    body.sites.clear();
    if !same_file {
        body.span = host_span;
        crate::inline::clear_positions(&mut body);
    }
    // The calls of the origin that pass the known parameters through call
    // the clone.
    let through = |args: &[Operand]| {
        args.len() == params.len()
            && params
                .iter()
                .zip(wanted)
                .zip(args)
                .all(|((&p, s), a)| match s {
                    None => true,
                    Some(Shape::Const(c)) => {
                        *a == Operand::Local(p) || *a == Operand::Const(c.clone())
                    }
                    Some(Shape::Closure { .. }) => *a == Operand::Local(p),
                })
    };
    let drop_known = |args: &[Operand]| -> Vec<Operand> {
        args.iter()
            .zip(wanted)
            .filter(|(_, s)| !matches!(s, Some(Shape::Const(_))))
            .map(|(a, _)| a.clone())
            .collect()
    };
    let own = |c: &Callee| matches!(c, Callee::Direct(d) if *d == site.callee);
    for b in &mut body.blocks {
        for s in &mut b.stmts {
            if let Stmt::Assign {
                value: Rvalue::Call { callee, args },
                ..
            } = s
                && own(callee)
                && through(args)
            {
                *args = drop_known(args);
                *callee = Callee::Code(key);
            }
        }
        if let Terminator::TailCall { callee, args } = &mut b.term
            && own(callee)
            && through(args)
        {
            *args = drop_known(args);
            *callee = Callee::Code(key);
        }
    }
    if let Some(entry) = body.blocks.get_mut(BlockId::from_u32(0)) {
        let pos = entry.pos_at(0);
        entry.insert_stmts(0, defs, pos);
    }
    known.set_shapes(key, kept_shapes);
    Ok(Some(body))
}
