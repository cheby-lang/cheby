//! Field reads move up to the consumption of a sibling projection
//! (D-911).
//!
//! In `State { items: list::push(s.items, n), count: s.count + 1 }`,
//! lowering reads `s.count` after the push, so `s` is live across the
//! push and still holds `items`, and the push copies a list that would be
//! unique once `s` is gone. Within one block, this pass takes a statement
//! `k` that consumes a counted projection `p = field b.i` at `p`'s last
//! use. If every later use of `b` in the block is a field read and `b` is
//! dead at the block's end, those reads move, in their order, to just
//! before `k`. Counting then releases `b` before `k`, and fusion folds
//! `p`'s `dup` into that release as a keep list (D-850), so `p` is unique
//! at `k` whenever `b` was. Naive counting, which drops `b` right after
//! its last use, gets the same effect from the same MIR.
//!
//! A field read raises nothing and reads an immutable object, so moving
//! it earlier in its block changes no result. Only a base whose type
//! cannot hold a handle moves (D-715's handle-free types): the fields of
//! `b` that nothing reads are released before the source's last use of
//! `b`, which only a handle could observe (D-205).
//!
//! A moved read's result lives across every statement between its new
//! and its old place, and one that may unwind lengthens that statement's
//! cleanup pad and the register allocator's work. So a base moves only
//! while its reads cross at most [`CROSSING_BUDGET`] statements that may
//! unwind in all, counting each read's crossings: otherwise the extra
//! compile time would grow with the number of reads times the number of
//! calls between them, quadratically in one function.
//!
//! The pass runs at the end of lowering, so every pipeline, naive
//! counting included, and borrowing inference (D-779, D-806) see the
//! moved reads. It is one backward scan per block over liveness computed
//! once: moves inside a block change no block's live-in or live-out set.

use cheby_iface::{Ice, Idx, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{
    Block, BlockId, Callee, Function, Local, Operand, RuntimeFn, Rvalue, Stmt, UseKind,
};
use crate::liveness::{Liveness, counted};
use crate::positions::{PosRef, StmtsOut};
use crate::unwind::stmt_may_unwind;

/// The most statements that may unwind which the moved reads of one base
/// cross, summed over the reads (D-911). A record update that pushes onto
/// one field and computes a dozen others with checked arithmetic stays
/// within it.
pub const CROSSING_BUDGET: usize = 128;

/// The base a statement reads a field of, if it is a field read.
const fn field_base(stmt: &Stmt) -> Option<Local> {
    match stmt {
        Stmt::Assign {
            value: Rvalue::Field { base, .. },
            ..
        } => Some(*base),
        _ => None,
    }
}

/// The locals a statement uses, in any way.
fn uses_of(stmt: &Stmt) -> Vec<Local> {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => {
            value.uses().into_iter().map(|(l, _)| l).collect()
        }
        Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
        Stmt::DropKeep { local, keep, .. } => std::iter::once(*local)
            .chain(keep.iter().map(|&(_, p)| p))
            .collect(),
        Stmt::ClearUnwind => Vec::new(),
    }
}

/// The locals a statement may consume: its owned uses, except an
/// indirect callee, which a call never updates; and the list of a
/// `list_slice` or `list_uncons`, which counting makes the owned form at
/// the list's last use (D-634, D-909).
fn consumed_by(stmt: &Stmt) -> Vec<Local> {
    let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = stmt else {
        return Vec::new();
    };
    match value {
        Rvalue::CallRuntime {
            func: RuntimeFn::ListSlice | RuntimeFn::ListUncons,
            args,
        } => match args.first() {
            Some(&Operand::Local(list)) => vec![list],
            _ => Vec::new(),
        },
        Rvalue::Call { callee, .. } => {
            let indirect = match callee {
                Callee::Indirect(l) => Some(*l),
                _ => None,
            };
            value
                .uses()
                .into_iter()
                .filter(|&(l, kind)| kind == UseKind::Owned && Some(l) != indirect)
                .map(|(l, _)| l)
                .collect()
        }
        _ => value
            .uses()
            .into_iter()
            .filter(|&(_, kind)| kind == UseKind::Owned)
            .map(|(l, _)| l)
            .collect(),
    }
}

/// Runs the pass over `f`, a lowered function without counts.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn run(f: &mut Function, handle_free: &dyn Fn(Ty) -> bool) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    // Each local is defined once, so a projection has one base.
    let mut base_of: FxHashMap<Local, Local> = FxHashMap::default();
    for block in &f.blocks {
        for stmt in &block.stmts {
            if let (Some(base), Stmt::Assign { dst, .. }) = (field_base(stmt), stmt) {
                base_of.insert(*dst, base);
            }
        }
    }
    if base_of.is_empty() {
        return Ok(());
    }
    let mut cx = Cx {
        live: Liveness::compute(f)?,
        base_of,
        movable: FxHashMap::default(),
        handle_free,
    };
    for index in 0..f.blocks.len() {
        let id = BlockId::from_usize(index).ok_or_else(|| Ice::new("hoist: too many blocks"))?;
        let moves = cx.plan(f, id)?;
        if moves.is_empty() {
            continue;
        }
        let block = f
            .blocks
            .get_mut(id)
            .ok_or_else(|| Ice::new("hoist: block missing"))?;
        apply(block, &moves)?;
    }
    Ok(())
}

/// What the pass knows of one function.
struct Cx<'h> {
    /// Liveness before any move: moves inside a block change no block's
    /// live-in or live-out set.
    live: Liveness,
    /// The base of each field read's result.
    base_of: FxHashMap<Local, Local>,
    /// Whether a base may move: counted, typed and handle-free. Memoized,
    /// since the type walk is not free.
    movable: FxHashMap<Local, bool>,
    handle_free: &'h dyn Fn(Ty) -> bool,
}

impl Cx<'_> {
    /// Whether `base` may move.
    fn movable(&mut self, f: &Function, base: Local) -> Result<bool, Ice> {
        if let Some(&ok) = self.movable.get(&base) {
            return Ok(ok);
        }
        let ok = counted(f, base)? && f.local(base)?.ty.is_some_and(self.handle_free);
        self.movable.insert(base, ok);
        Ok(ok)
    }

    /// The moves of block `id`: for each statement index, the indices of
    /// the field reads that go right before it, ascending.
    ///
    /// One backward scan. `reads` holds, per base, the indices of its
    /// field reads after the current statement; `blocked` the locals that
    /// a statement after it, other than a field read, uses; `target` the
    /// earliest consumer found so far for each base whose reads stay
    /// within [`CROSSING_BUDGET`] when they move before it.
    ///
    /// `unwound` counts the statements after the current one that may
    /// unwind, and `marks` sums, per base, that count as each of its reads
    /// was seen. A read at `j` moved before `k` crosses `u(k) - u(j)` such
    /// statements, `u(i)` counting those at `i` and after, so the reads of
    /// `b` cross `reads[b].len() * u(k) - marks[b]` in all.
    fn plan(&mut self, f: &Function, id: BlockId) -> Result<FxHashMap<usize, Vec<usize>>, Ice> {
        let bl = self.live.block(f, id)?;
        let block = f.block(id)?;
        let mut reads: FxHashMap<Local, Vec<usize>> = FxHashMap::default();
        let mut marks: FxHashMap<Local, usize> = FxHashMap::default();
        let mut unwound = 0usize;
        let mut blocked: FxHashSet<Local> = FxHashSet::default();
        let mut target: FxHashMap<Local, usize> = FxHashMap::default();
        for (k, stmt) in block.stmts.iter().enumerate().rev() {
            if let Some(b) = field_base(stmt) {
                reads.entry(b).or_default().push(k);
                let mark = marks.entry(b).or_default();
                *mark = mark.saturating_add(unwound);
                continue;
            }
            unwound = unwound.saturating_add(usize::from(stmt_may_unwind(stmt)));
            let used = uses_of(stmt);
            let after = bl
                .after
                .get(k)
                .ok_or_else(|| Ice::new("hoist: liveness out of step"))?;
            for p in consumed_by(stmt) {
                let Some(&b) = self.base_of.get(&p) else {
                    continue;
                };
                let later = reads.get(&b).map_or(0, Vec::len);
                let crossed = later
                    .saturating_mul(unwound)
                    .saturating_sub(marks.get(&b).copied().unwrap_or(0));
                if blocked.contains(&b)
                    || used.contains(&b)
                    || after.contains(&p)
                    || bl.before_term.contains(&b)
                    || later == 0
                    || crossed > CROSSING_BUDGET
                    || !counted(f, p)?
                {
                    continue;
                }
                if self.movable(f, b)? {
                    target.insert(b, k);
                }
            }
            blocked.extend(used);
        }
        let mut before: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        for (b, k) in target {
            let later = reads.get(&b).map_or(&[][..], Vec::as_slice);
            before
                .entry(k)
                .or_default()
                .extend(later.iter().filter(|&&j| j > k));
        }
        for list in before.values_mut() {
            list.sort_unstable();
        }
        Ok(before)
    }
}

/// Moves the statements of `block` as `plan` says; a moved field read
/// keeps its own position (D-938).
fn apply(block: &mut Block, plan: &FxHashMap<usize, Vec<usize>>) -> Result<(), Ice> {
    let moved: FxHashSet<usize> = plan.values().flatten().copied().collect();
    let len = block.stmts.len();
    let term = block.term_pos();
    let pos: Vec<PosRef> = (0..len).map(|i| block.pos_at(i)).collect();
    let mut old: Vec<Option<Stmt>> = std::mem::take(&mut block.stmts)
        .into_iter()
        .map(Some)
        .collect();
    let mut take = |k: usize| {
        let stmt = old
            .get_mut(k)
            .and_then(Option::take)
            .ok_or_else(|| Ice::new("hoist: a statement moved twice"))?;
        Ok::<_, Ice>((stmt, pos.get(k).copied().unwrap_or(PosRef::NONE)))
    };
    let mut out = StmtsOut::new(block.has_positions(), len);
    for k in 0..len {
        for &j in plan.get(&k).map_or(&[][..], Vec::as_slice) {
            let (stmt, at) = take(j)?;
            out.push(stmt, at);
        }
        if !moved.contains(&k) {
            let (stmt, at) = take(k)?;
            out.push(stmt, at);
        }
    }
    block.set_stmts(out.finish(term));
    Ok(())
}
