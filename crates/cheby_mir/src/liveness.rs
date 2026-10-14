//! Backward liveness of counted ([`Repr::Value`]) locals.

use std::collections::BTreeSet;

use cheby_iface::{Ice, Idx, IndexVec};

use crate::ir::{Block, BlockId, Function, Local, Stmt};
use crate::repr::Repr;

/// A set of locals, ordered so iteration is deterministic.
pub(crate) type LocalSet = BTreeSet<Local>;

/// Live-in and live-out sets per block.
#[derive(Debug)]
pub(crate) struct Liveness {
    pub live_in: IndexVec<BlockId, LocalSet>,
    pub live_out: IndexVec<BlockId, LocalSet>,
}

/// Whether `local` is counted. A borrowed local (D-715) is counted too:
/// it is live where it is used, but holds no reference of its own
/// ([`is_borrowed_local`]).
pub(crate) fn counted(f: &Function, local: Local) -> Result<bool, Ice> {
    Ok(f.local(local)?.repr == Repr::Value)
}

/// Whether `local` is borrowed in `borrowed`, a
/// [`Function::borrowed_locals`] result (D-715).
#[must_use]
pub(crate) fn is_borrowed_local(borrowed: &[bool], local: Local) -> bool {
    borrowed.get(local.index()).copied().unwrap_or(false)
}

/// The local a statement defines.
pub(crate) const fn stmt_def(stmt: &Stmt) -> Option<Local> {
    match stmt {
        Stmt::Assign { dst, .. } => Some(*dst),
        Stmt::Eval(_) | Stmt::Dup(_) | Stmt::Drop(_) | Stmt::ClearUnwind => None,
    }
}

/// The counted locals a statement reads (Dup and Drop included).
pub(crate) fn stmt_uses(f: &Function, stmt: &Stmt) -> Result<Vec<Local>, Ice> {
    let all = match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => {
            value.uses().into_iter().map(|(l, _)| l).collect()
        }
        Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
        Stmt::ClearUnwind => Vec::new(),
    };
    filter_counted(f, all)
}

pub(crate) fn filter_counted(f: &Function, locals: Vec<Local>) -> Result<Vec<Local>, Ice> {
    let mut out = Vec::with_capacity(locals.len());
    for l in locals {
        if counted(f, l)? {
            out.push(l);
        }
    }
    Ok(out)
}

/// The counted locals the terminator of `block` reads.
pub(crate) fn term_uses(f: &Function, block: &Block) -> Result<Vec<Local>, Ice> {
    filter_counted(f, block.term.uses().into_iter().map(|(l, _)| l).collect())
}

impl Liveness {
    /// Computes liveness to a fixed point.
    pub(crate) fn compute(f: &Function) -> Result<Self, Ice> {
        let mut gen_sets = IndexVec::<BlockId, LocalSet>::new();
        let mut kill_sets = IndexVec::<BlockId, LocalSet>::new();
        for block in &f.blocks {
            let mut generated = LocalSet::new();
            let mut killed: LocalSet = block.params.iter().copied().collect();
            for stmt in &block.stmts {
                for l in stmt_uses(f, stmt)? {
                    if !killed.contains(&l) {
                        generated.insert(l);
                    }
                }
                if let Some(d) = stmt_def(stmt) {
                    killed.insert(d);
                }
            }
            for l in term_uses(f, block)? {
                if !killed.contains(&l) {
                    generated.insert(l);
                }
            }
            gen_sets.push(generated)?;
            kill_sets.push(killed)?;
        }
        let empty = || -> Result<IndexVec<BlockId, LocalSet>, Ice> {
            IndexVec::from_raw(vec![LocalSet::new(); f.blocks.len()])
        };
        let mut live_in = empty()?;
        let mut live_out = empty()?;
        let mut changed = true;
        while changed {
            changed = false;
            for (id, block) in f.blocks.iter_enumerated().rev() {
                let mut out = LocalSet::new();
                for succ in block.term.successors() {
                    out.extend(live_in.try_get(succ, "successor block")?.iter().copied());
                }
                let killed = kill_sets.try_get(id, "block")?;
                let mut inn = gen_sets.try_get(id, "block")?.clone();
                inn.extend(out.iter().filter(|l| !killed.contains(l)).copied());
                let slot_in = live_in
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("liveness block missing"))?;
                if *slot_in != inn {
                    *slot_in = inn;
                    changed = true;
                }
                let slot_out = live_out
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("liveness block missing"))?;
                if *slot_out != out {
                    *slot_out = out;
                    changed = true;
                }
            }
        }
        Ok(Self { live_in, live_out })
    }

    /// The counted locals live at each point of `block`.
    pub(crate) fn block(&self, f: &Function, id: BlockId) -> Result<BlockLive, Ice> {
        let block = f.block(id)?;
        let out = self.live_out.try_get(id, "block")?.clone();
        let mut live = out.clone();
        live.extend(term_uses(f, block)?);
        let before_term = live.clone();
        let mut after = vec![LocalSet::new(); block.stmts.len()];
        for (slot, stmt) in after.iter_mut().zip(&block.stmts).rev() {
            slot.clone_from(&live);
            if let Some(d) = stmt_def(stmt) {
                live.remove(&d);
            }
            live.extend(stmt_uses(f, stmt)?);
        }
        Ok(BlockLive {
            start: live,
            after,
            before_term,
            out,
        })
    }
}

/// Liveness inside one block.
#[derive(Debug)]
pub(crate) struct BlockLive {
    /// Live right after the block's parameters are defined: the used
    /// parameters and the live-in set.
    pub start: LocalSet,
    /// Live after each statement.
    pub after: Vec<LocalSet>,
    /// Live right before the terminator (its uses included).
    pub before_term: LocalSet,
    /// Live after the terminator: the union of the successors' live-in.
    pub out: LocalSet,
}
