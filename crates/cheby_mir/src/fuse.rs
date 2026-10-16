//! Fusion (D-716, D-850): Perceus's dup and drop fusion and
//! drops specialized by constructor, after reuse and before the unwind
//! pass (`rc::insert → reuse::insert → fuse::run → unwind::insert`,
//! architecture §9.1), on Perceus counting only
//! ([`crate::MirOptions::fuses`]); naive counting has none (D-718, D-808).
//! Per function, in three steps, each deterministic (D-264):
//!
//! 1. **Sinking.** Counting `dup`s a projection `p = field b.i` right
//!    after its definition (rule 4 of [`crate::rc`]). That `dup` moves
//!    forward to the first statement that consumes `p` or releases `b`
//!    (or, for a projection of a projection, any local up its chain of
//!    bases): its `drop`, an owned use, a `drop_reuse`. Across a `branch`
//!    or `switch` it moves into each successor, when every successor has
//!    no other predecessor; otherwise it stops before the terminator. A
//!    path that ends in a `panic` needs none. This is sound because `b`
//!    holds a reference until its release, which keeps `p` alive (the
//!    ownership simulation's `borrow_alive`); no drop moves, so the
//!    drop-position check (D-851) holds. Pads, built
//!    after this pass, then hold `b` alone where they held `p` and `b`.
//! 2. **Cancelling.** `dup x` followed by `drop x` with nothing between
//!    them but releases of other locals and `dup`s that do not need `x`
//!    alive cancel: the reference the `dup` adds is the one the `drop`
//!    gives up. A sunk `dup` meets an edge drop of a successor where `p`
//!    is dead this way.
//! 3. **Keep lists.** The `dup`s of projections of `b` right before a
//!    `t = drop_reuse b of L` become its keep list, when each projection
//!    reads `L` ([`crate::ir::Keep`]). Before a `drop b` they make a drop
//!    specialized by constructor (`drop b of L keep [...]`) only when the
//!    reads of `L` pin `b`'s constructor ([`crate::reuse::pins`]: a tuple,
//!    the type's only constructor, or an arm of a switch on `b`'s tag;
//!    never a shared field's read, D-071, which may see another
//!    constructor of another size), and only with a non-empty list: a
//!    specialized drop with nothing kept would only inline a free that is
//!    a runtime call anyway. Pads keep plain drops (D-735), and tokens are
//!    never specialized. Code generation releases at most 8 value fields
//!    inline.
//!
//! A `precounted` body (D-693) is left alone.

use std::collections::BTreeSet;

use cheby_iface::{Ice, Idx};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{
    BlockId, Function, LayoutRef, Local, MirModule, Operand, Rvalue, Stmt, Terminator, UseKind,
};
use crate::liveness::is_borrowed_local;
use crate::repr::Repr;
use crate::validate::dom::Dominance;

/// Fuses every function of `module` (module documentation). Runs after
/// [`crate::reuse::insert_module`], before the unwind pass.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn run_module(module: &mut MirModule) -> Result<(), Ice> {
    let mut functions = std::mem::take(&mut module.functions);
    let result = functions.iter_mut().try_for_each(|f| run(module, f));
    module.functions = functions;
    result
}

/// Fuses `f`, a counted function of `module` without unwind edges (module
/// documentation).
///
/// # Errors
///
/// An [`Ice`] if `f` has unwind edges already, or is malformed.
pub fn run(module: &MirModule, f: &mut Function) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    if f.entry_unwind.is_some() || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Err(Ice::new("fusion after the unwind pass"));
    }
    let facts = Facts::of(f)?;
    sink(f, &facts)?;
    cancel(f, &facts);
    keep(module, f, &facts)?;
    Ok(())
}

/// What fusion knows of a function.
struct Facts {
    /// The counted `field` results: base, layout and index.
    projections: FxHashMap<Local, (Local, LayoutRef, u32)>,
    /// By local index, whether the local is borrowed (D-715, D-807).
    borrowed: Vec<bool>,
    /// The locals a specialized drop never releases: constants' values,
    /// tuples being filled (D-810) and reuse tokens.
    plain: BTreeSet<Local>,
    /// By block index, whether the block has exactly one predecessor
    /// along normal control flow (the entry block has the function's).
    single_pred: Vec<bool>,
}

impl Facts {
    fn of(f: &Function) -> Result<Self, Ice> {
        let mut projections = FxHashMap::default();
        let mut plain = BTreeSet::new();
        if let (crate::ir::FnKey::Continuation { .. }, Some(&p)) = (f.key, f.params.first()) {
            plain.insert(p);
        }
        for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
            match stmt {
                Stmt::Assign {
                    dst,
                    value:
                        Rvalue::Field {
                            base,
                            layout,
                            field,
                        },
                } if f.local(*dst)?.repr == Repr::Value => {
                    projections.insert(*dst, (*base, *layout, *field));
                }
                Stmt::Assign {
                    dst,
                    value:
                        Rvalue::LoadConst(_) | Rvalue::Use(Operand::Const(_)) | Rvalue::DropReuse { .. },
                } => {
                    plain.insert(*dst);
                }
                Stmt::Eval(Rvalue::InitField { obj, .. }) => {
                    plain.insert(*obj);
                }
                _ => {}
            }
        }
        let n = f.blocks.len();
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        for (id, block) in f.blocks.iter_enumerated() {
            for succ in block.term.successors() {
                if let Some(list) = preds.get_mut(succ.index())
                    && !list.contains(&id)
                {
                    list.push(id);
                }
            }
        }
        let single_pred = preds
            .iter()
            .enumerate()
            .map(|(b, p)| b != 0 && p.len() == 1)
            .collect();
        Ok(Self {
            projections,
            borrowed: f.borrowed_locals()?,
            plain,
            single_pred,
        })
    }

    fn is_borrowed(&self, l: Local) -> bool {
        is_borrowed_local(&self.borrowed, l)
    }

    /// `p` and the locals up its chain of bases while each is a counted,
    /// owned projection, then the last base: the locals whose release
    /// ends what keeps `p` alive.
    fn chain(&self, p: Local) -> Vec<Local> {
        let mut out = vec![p];
        let mut at = p;
        while let Some(&(base, _, _)) = self.projections.get(&at) {
            if out.contains(&base) || out.len() > self.projections.len() {
                break;
            }
            out.push(base);
            if self.is_borrowed(base) {
                break;
            }
            at = base;
        }
        out
    }

    /// Whether `y` is `x` or a projection of it, directly or through other
    /// projections.
    fn needs(&self, y: Local, x: Local) -> bool {
        let mut at = y;
        for _ in 0..=self.projections.len() {
            if at == x {
                return true;
            }
            match self.projections.get(&at) {
                Some(&(base, _, _)) => at = base,
                None => return false,
            }
        }
        false
    }
}

/// The locals `stmt` consumes or releases: its owned uses, and the local a
/// `drop` or `drop_reuse` gives up.
fn consumed(stmt: &Stmt) -> Vec<Local> {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => value
            .uses()
            .into_iter()
            .filter(|&(_, k)| k == UseKind::Owned)
            .map(|(l, _)| l)
            .collect(),
        Stmt::Drop(x) | Stmt::DropKeep { local: x, .. } => vec![*x],
        Stmt::Dup(_) | Stmt::ClearUnwind => Vec::new(),
    }
}

/// Where one sunk `dup` goes: before statement `at` of `block` (its
/// terminator when `at` is the number of statements).
type Point = (BlockId, usize);

/// Step 1 (module documentation): moves each projection's `dup` from
/// after its definition to the points where it is first needed. The
/// points are found on the function as counting left it, so the moves do
/// not interact: a `dup` is never a stopping point.
fn sink(f: &mut Function, facts: &Facts) -> Result<(), Ice> {
    // (projection, its block, the position of its `dup`, where it goes).
    let mut moves: Vec<(Local, BlockId, usize, Vec<Point>)> = Vec::new();
    for (id, block) in f.blocks.iter_enumerated() {
        for (j, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign { dst: p, .. } = stmt else {
                continue;
            };
            let Some(&(base, _, _)) = facts.projections.get(p) else {
                continue;
            };
            let at = j.saturating_add(1);
            if facts.is_borrowed(*p)
                || facts.is_borrowed(base)
                || block.stmts.get(at) != Some(&Stmt::Dup(*p))
            {
                continue;
            }
            let start = at.saturating_add(1);
            if let Some(points) = sink_points(f, facts, *p, id, start)?
                && points != [(id, start)]
            {
                moves.push((*p, id, at, points));
            }
        }
    }
    if moves.is_empty() {
        return Ok(());
    }
    // Per block: the `dup`s removed, and those inserted before each
    // position, in the order of the moves (definition order).
    let mut removed: FxHashSet<(BlockId, usize)> = FxHashSet::default();
    let mut inserted: FxHashMap<BlockId, Vec<(usize, Local)>> = FxHashMap::default();
    for (p, block, at, points) in moves {
        removed.insert((block, at));
        for (b, before) in points {
            inserted.entry(b).or_default().push((before, p));
        }
    }
    let mut touched: Vec<BlockId> = removed
        .iter()
        .map(|&(b, _)| b)
        .chain(inserted.keys().copied())
        .collect();
    touched.sort_unstable();
    touched.dedup();
    for id in touched {
        let block = f
            .blocks
            .get_mut(id)
            .ok_or_else(|| Ice::new("fusion: no such block"))?;
        let mut here = inserted.remove(&id).unwrap_or_default();
        // Stable: dups for one position keep the order of the moves.
        here.sort_by_key(|&(before, _)| before);
        let old = std::mem::take(&mut block.stmts);
        let mut stmts = Vec::with_capacity(old.len().saturating_add(here.len()));
        let mut next = here.iter().peekable();
        for (i, stmt) in old.into_iter().enumerate() {
            while let Some(&&(_, p)) = next.peek().filter(|&&&(before, _)| before == i) {
                stmts.push(Stmt::Dup(p));
                next.next();
            }
            if !removed.contains(&(id, i)) {
                stmts.push(stmt);
            }
        }
        for &(_, p) in next {
            stmts.push(Stmt::Dup(p));
        }
        block.stmts = stmts;
    }
    Ok(())
}

/// The points where the `dup` of projection `p` goes, walking forward from
/// position `start` of `block` (module documentation), or `None` when a
/// path reaches something that would leave it pending: then it stays.
fn sink_points(
    f: &Function,
    facts: &Facts,
    p: Local,
    block: BlockId,
    start: usize,
) -> Result<Option<Vec<Point>>, Ice> {
    let stops = facts.chain(p);
    let stops_at = |l: &Local| stops.contains(l);
    let mut points = Vec::new();
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut work = vec![(block, start)];
    while let Some((b, from)) = work.pop() {
        let blk = f.block(b)?;
        if let Some(k) = blk
            .stmts
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, s)| consumed(s).iter().any(stops_at))
            .map(|(k, _)| k)
        {
            points.push((b, k));
            continue;
        }
        let end = blk.stmts.len();
        let term_uses = blk.term.uses().iter().any(|(l, _)| stops_at(l));
        match &blk.term {
            Terminator::Return(_) | Terminator::TailCall { .. } if term_uses => {
                points.push((b, end));
            }
            // Nothing may be held at a return; the base's release was
            // missed, so the `dup` stays.
            Terminator::Return(_) | Terminator::TailCall { .. } | Terminator::Resume => {
                return Ok(None);
            }
            // The path ends; what is held leaks to the unwind pass's pad,
            // which releases the base, and the base its field.
            Terminator::Panic { .. } | Terminator::Unreachable => {}
            Terminator::Goto { .. } | Terminator::Branch { .. } | Terminator::Switch { .. } => {
                let mut succs = blk.term.successors();
                succs.sort_unstable();
                succs.dedup();
                let flows = !term_uses
                    && succs.iter().all(|s| {
                        facts.single_pred.get(s.index()).copied().unwrap_or(false)
                            && !seen.contains(s)
                    });
                if flows {
                    for s in succs.into_iter().rev() {
                        seen.insert(s);
                        work.push((s, 0));
                    }
                } else {
                    points.push((b, end));
                }
            }
        }
    }
    points.sort_unstable();
    Ok(Some(points))
}

/// Step 2 (module documentation): cancels each `dup x` against the first
/// later `drop x` when only releases of other locals and `dup`s that do
/// not need `x` alive come between, until none is left.
fn cancel(f: &mut Function, facts: &Facts) {
    for block in &mut f.blocks {
        let mut i = 0;
        while i < block.stmts.len() {
            let Some(Stmt::Dup(x)) = block.stmts.get(i) else {
                i = i.saturating_add(1);
                continue;
            };
            let x = *x;
            let mut pair = None;
            for (k, stmt) in block.stmts.iter().enumerate().skip(i.saturating_add(1)) {
                match stmt {
                    Stmt::Drop(y) if *y == x => {
                        pair = Some(k);
                        break;
                    }
                    Stmt::Drop(_) => {}
                    Stmt::Dup(y) if !facts.needs(*y, x) => {}
                    Stmt::Assign {
                        value: Rvalue::DropReuse { local, keep, .. },
                        ..
                    } if *local != x && keep.iter().all(|&(_, p)| !facts.needs(p, x)) => {}
                    _ => break,
                }
            }
            if let Some(k) = pair {
                block.stmts.remove(k);
                block.stmts.remove(i);
            } else {
                i = i.saturating_add(1);
            }
        }
    }
}

/// What proves a constructor for a specialized drop: the same proofs as
/// reuse's ([`crate::reuse::pins`], [`crate::reuse::tested_variants`]).
/// The validator checks every specialized drop with it too.
pub(crate) struct Pins {
    dom: Dominance,
    tested: FxHashMap<Local, Vec<(usize, crate::reuse::Variant)>>,
}

impl Pins {
    /// Whether a read of `x` as `layout` in `block` pins `x`'s
    /// constructor ([`crate::reuse::pins`]).
    pub(crate) fn pins(
        &self,
        module: &MirModule,
        x: Local,
        layout: LayoutRef,
        block: BlockId,
    ) -> bool {
        crate::reuse::pins(module, &self.tested, &self.dom, x, layout, block)
    }

    pub(crate) fn of(module: &MirModule, f: &Function) -> Result<Self, Ice> {
        let succs: Vec<Vec<usize>> = (0..f.blocks.len())
            .map(|n| {
                let id = BlockId::from_usize(n).ok_or_else(|| Ice::new("too many blocks"))?;
                let mut seen: FxHashSet<usize> = FxHashSet::default();
                Ok(f.successors(id)?
                    .into_iter()
                    .map(Idx::index)
                    .filter(|&s| seen.insert(s))
                    .collect())
            })
            .collect::<Result<_, Ice>>()?;
        let dom = Dominance::from_successors(&succs)?;
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); f.blocks.len()];
        for (b, out) in succs.iter().enumerate() {
            if !dom.is_reachable(b) {
                continue;
            }
            for &s in out {
                preds
                    .get_mut(s)
                    .ok_or_else(|| Ice::new("fusion: no such successor"))?
                    .push(b);
            }
        }
        let tested = crate::reuse::tested_variants(module, f, &dom, &preds)?;
        Ok(Self { dom, tested })
    }
}

/// Whether a drop of an object of `layout` may be specialized: a
/// constructor with fields or a tuple, with a fixed object size, as reuse
/// requires of its layouts.
fn specializable(module: &MirModule, layout: LayoutRef) -> bool {
    let shape = match layout {
        LayoutRef::Ctor(_) => !module.layout(layout).is_some_and(|d| d.fields.is_empty()),
        LayoutRef::Tuple(n) => n > 0,
        _ => false,
    };
    shape && crate::layout::object_size(module, layout).is_some()
}

/// Step 3 (module documentation): folds the `dup`s of projections right
/// before a `drop_reuse` into its keep list, and before a `drop` into a
/// drop specialized by constructor where a read pins it.
fn keep(module: &MirModule, function: &mut Function, facts: &Facts) -> Result<(), Ice> {
    let mut pins: Option<Pins> = None;
    for n in 0..function.blocks.len() {
        let id = BlockId::from_usize(n).ok_or_else(|| Ice::new("too many blocks"))?;
        let mut at = 0;
        while at < function.block(id)?.stmts.len() {
            let (picked, specialized) = fold_at(module, function, facts, &mut pins, id, at)?;
            if picked.is_empty() {
                at = at.saturating_add(1);
                continue;
            }
            let list: Vec<(u32, Local)> = picked.iter().map(|&(_, i, p)| (i, p)).collect();
            let block = function
                .blocks
                .get_mut(id)
                .ok_or_else(|| Ice::new("fusion: no such block"))?;
            match (block.stmts.get_mut(at), specialized) {
                (
                    Some(Stmt::Assign {
                        value: Rvalue::DropReuse { keep, .. },
                        ..
                    }),
                    None,
                ) => *keep = list,
                (Some(stmt @ Stmt::Drop(_)), Some(layout)) => {
                    if let Stmt::Drop(dropped) = *stmt {
                        *stmt = Stmt::DropKeep {
                            local: dropped,
                            layout,
                            keep: list,
                        };
                    }
                }
                _ => return Err(Ice::new("fusion: the release moved")),
            }
            // Remove the folded `dup`s, last first.
            let mut dups: Vec<usize> = picked.iter().map(|&(k, _, _)| k).collect();
            dups.sort_unstable();
            for k in dups.into_iter().rev() {
                block.stmts.remove(k);
                at = at.saturating_sub(1);
            }
            at = at.saturating_add(1);
        }
    }
    Ok(())
}

/// The `dup`s to fold into a release, `(position, field, projection)`,
/// and the layout of a drop to specialize ([`fold_at`]).
type Fold = (Vec<(usize, u32, Local)>, Option<LayoutRef>);

/// The `dup`s to fold into the release at position `at` of block `id`
/// ([`kept_before`]), and, for a `drop` to specialize, its layout: a
/// `drop_reuse` keeps the projections that read its layout; a `drop` of a
/// local that may be specialized keeps those that read the nearest
/// projection's layout, when that layout pins the local there
/// ([`Pins`]). Nothing otherwise.
fn fold_at(
    module: &MirModule,
    function: &Function,
    facts: &Facts,
    pins: &mut Option<Pins>,
    id: BlockId,
    at: usize,
) -> Result<Fold, Ice> {
    let stmts = &function.block(id)?.stmts;
    match stmts.get(at) {
        Some(Stmt::Assign {
            value: Rvalue::DropReuse { local, layout, .. },
            ..
        }) => Ok((kept_before(stmts, at, *local, Some(*layout), facts), None)),
        Some(Stmt::Drop(dropped))
            if !facts.is_borrowed(*dropped) && !facts.plain.contains(dropped) =>
        {
            let picked = kept_before(stmts, at, *dropped, None, facts);
            let layout = picked.first().and_then(|&(k, _, _)| match stmts.get(k) {
                Some(Stmt::Dup(p)) => facts.projections.get(p).map(|&(_, read, _)| read),
                _ => None,
            });
            let Some(layout) = layout.filter(|&read| specializable(module, read)) else {
                return Ok((Vec::new(), None));
            };
            let pins = match pins {
                Some(known) => known,
                slot => slot.insert(Pins::of(module, function)?),
            };
            if pins.pins(module, *dropped, layout, id) {
                Ok((picked, Some(layout)))
            } else {
                Ok((Vec::new(), None))
            }
        }
        _ => Ok((Vec::new(), None)),
    }
}

/// The `dup`s of projections of `x` read as `layout` in the run of `dup`s
/// right before position `j` of `stmts`, at most one per field and per
/// projection: `(position, field, projection)`, in statement order.
/// Without a layout, the layout of the nearest such projection.
fn kept_before(
    stmts: &[Stmt],
    j: usize,
    x: Local,
    layout: Option<LayoutRef>,
    facts: &Facts,
) -> Vec<(usize, u32, Local)> {
    let mut layout = layout;
    let mut out: Vec<(usize, u32, Local)> = Vec::new();
    let mut k = j;
    while let Some(prev) = k.checked_sub(1) {
        let Some(Stmt::Dup(p)) = stmts.get(prev) else {
            break;
        };
        k = prev;
        if let Some(&(base, l, i)) = facts.projections.get(p)
            && base == x
            && *layout.get_or_insert(l) == l
            && !facts.is_borrowed(*p)
            && !out.iter().any(|&(_, fi, fp)| fi == i || fp == *p)
        {
            out.push((prev, i, *p));
        }
    }
    out.reverse();
    out
}
