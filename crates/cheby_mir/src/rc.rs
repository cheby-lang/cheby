//! Naive reference counting: explicit `dup`/`drop` by the last-use rule
//! (D7, D-273, D-205). Perceus (M2) replaces only this pass.
//!
//! Lowering emits ownership-agnostic MIR. This pass places:
//!
//! 1. `dup x` before an owned use of `x` when `x` is still live afterwards
//!    (or used again by the same instruction);
//! 2. `drop x` after a borrowed use that is the last use;
//! 3. `drop x` after a definition that is never used;
//! 4. `dup x` right after `x = field …` or `x = load_const …`, before
//!    anything can drop the base;
//! 5. on every edge of a `branch` or `switch`, `drop x` at the start of the
//!    successor for each `x` alive before the terminator but dead in the
//!    successor (D-205); an edge into a block with other predecessors is
//!    split first.
//!
//! A `list_slice` whose list rule 2 would drop right after consumes the
//! list instead (`list_slice_owned`), so a unique list is sliced in place.
//!
//! Every counted local is therefore consumed exactly once on every path
//! that returns or tail-calls, so nothing is pending at a tail call.
//! What is live at a `panic`, or at any instruction that may unwind, is
//! released by the cleanup pads of the unwind pass, which runs after this
//! one ([`crate::unwind`], D-698); without it (`CHEBY_UNWIND_EDGES=off`)
//! it leaks, as in M1.

use std::collections::BTreeMap;

use cheby_iface::{Ice, Idx};

use crate::ir::{
    Block, BlockId, Function, Local, MirModule, Operand, RuntimeFn, Rvalue, Stmt, Terminator,
    UseKind,
};
use crate::liveness::{Liveness, LocalSet, counted, stmt_def};

/// Inserts `dup` and `drop` into every function of `module`.
///
/// # Errors
///
/// An [`Ice`] if a function already has `dup` or `drop`, or is malformed.
pub fn insert_module(module: &mut MirModule) -> Result<(), Ice> {
    for f in &mut module.functions {
        insert(f)?;
    }
    Ok(())
}

/// Inserts `dup` and `drop` into `f`. A `precounted` body (D-693) already
/// has its counts and is left alone (D-744).
///
/// # Errors
///
/// An [`Ice`] if `f` already has `dup` or `drop`, has unwind edges
/// (they come after this pass, D-698), or is malformed.
pub fn insert(f: &mut Function) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    if f.entry_unwind.is_some() || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Err(Ice::new("unwind edges before reference counting"));
    }
    if f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .any(|s| matches!(s, Stmt::Dup(_) | Stmt::Drop(_)))
    {
        return Err(Ice::new("reference counts inserted twice"));
    }
    let live = Liveness::compute(f)?;
    let mut new_blocks = Vec::with_capacity(f.blocks.len());
    let mut edge_drops: Vec<(BlockId, BlockId, Vec<Local>)> = Vec::new();
    for (id, block) in f.blocks.iter_enumerated() {
        let bl = live.block(f, id)?;
        let mut stmts = Vec::new();
        // Unused parameters: the function's in the entry block, the
        // block's own everywhere.
        let mut params: Vec<Local> = block.params.clone();
        if id.as_u32() == 0 {
            params.extend(f.params.iter().copied());
        }
        for p in params {
            if counted(f, p)? && !bl.start.contains(&p) {
                stmts.push(Stmt::Drop(p));
            }
        }
        for (stmt, after) in block.stmts.iter().zip(&bl.after) {
            let uses = match stmt {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => value.uses(),
                Stmt::Dup(_) | Stmt::Drop(_) => {
                    return Err(Ice::new("reference counts inserted twice"));
                }
                // Touches no local (D-693).
                Stmt::ClearUnwind => Vec::new(),
            };
            let (before, mut post) = place(f, &uses, after)?;
            stmts.extend(before);
            stmts.push(own_dying_list(stmt, &mut post));
            if let (Some(dst), Stmt::Assign { value, .. }) = (stmt_def(stmt), stmt)
                && counted(f, dst)?
            {
                if value.result_is_borrowed() {
                    if after.contains(&dst) {
                        post.insert(0, Stmt::Dup(dst));
                    }
                } else if !after.contains(&dst) {
                    post.push(Stmt::Drop(dst));
                }
            }
            stmts.extend(post);
        }
        match &block.term {
            Terminator::Return(_) | Terminator::TailCall { .. } | Terminator::Goto { .. } => {
                let (before, post) = place(f, &block.term.uses(), &bl.out)?;
                if !post.is_empty() {
                    return Err(Ice::new("a borrowed use in a moving terminator"));
                }
                stmts.extend(before);
            }
            Terminator::Branch { .. } | Terminator::Switch { .. } => {
                let mut succs = block.term.successors();
                succs.sort_unstable();
                succs.dedup();
                for succ in succs {
                    let succ_in = live.live_in.try_get(succ, "successor block")?;
                    let drops: Vec<Local> = bl
                        .before_term
                        .iter()
                        .filter(|l| !succ_in.contains(l))
                        .copied()
                        .collect();
                    if !drops.is_empty() {
                        edge_drops.push((id, succ, drops));
                    }
                }
            }
            // Like a panic: what is live leaks until the unwind pass adds
            // pads (D-698).
            Terminator::Panic { .. } | Terminator::Unreachable | Terminator::Resume => {}
        }
        new_blocks.push(Block {
            params: block.params.clone(),
            stmts,
            term: block.term.clone(),
            unwind: Vec::new(),
        });
    }
    place_edge_drops(f, new_blocks, edge_drops)
}

/// `stmt`, except that a `list_slice` whose list `post` drops right after
/// (its last use) becomes `list_slice_owned`, which consumes the list
/// instead, and that drop goes: a unique list then becomes its own slice
/// in place, so walking a list with `[x, ..rest]` allocates nothing per
/// element.
fn own_dying_list(stmt: &Stmt, post: &mut Vec<Stmt>) -> Stmt {
    let mut stmt = stmt.clone();
    if let Stmt::Assign {
        value: Rvalue::CallRuntime { func, args },
        ..
    } = &mut stmt
        && *func == RuntimeFn::ListSlice
        && let Some(&Operand::Local(list)) = args.first()
        && let Some(at) = post.iter().position(|s| *s == Stmt::Drop(list))
    {
        post.remove(at);
        *func = RuntimeFn::ListSliceOwned;
    }
    stmt
}

/// The statements to put before and after one instruction with `uses`,
/// given the counted locals live after it.
fn place(
    f: &Function,
    uses: &[(Local, UseKind)],
    live_after: &LocalSet,
) -> Result<(Vec<Stmt>, Vec<Stmt>), Ice> {
    // Per local, in first-use order: (owned uses, borrowed uses).
    let mut order: Vec<Local> = Vec::new();
    let mut counts: BTreeMap<Local, (u32, u32)> = BTreeMap::new();
    for &(l, kind) in uses {
        if !counted(f, l)? {
            continue;
        }
        let entry = counts.entry(l).or_insert_with(|| {
            order.push(l);
            (0, 0)
        });
        match kind {
            UseKind::Owned => entry.0 = entry.0.saturating_add(1),
            UseKind::Borrowed => entry.1 = entry.1.saturating_add(1),
        }
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    for l in order {
        let (owned, borrowed) = counts.get(&l).copied().unwrap_or_default();
        let live = live_after.contains(&l);
        // The last owned use takes the local's own reference, unless the
        // local is still needed: later, or borrowed by this instruction.
        let dups = if live || borrowed > 0 {
            owned
        } else {
            owned.saturating_sub(1)
        };
        for _ in 0..dups {
            before.push(Stmt::Dup(l));
        }
        if !live && borrowed > 0 {
            after.push(Stmt::Drop(l));
        }
    }
    Ok((before, after))
}

/// Puts the drops of `branch`/`switch` edges at the start of their
/// successors, splitting edges into blocks with other predecessors.
fn place_edge_drops(
    f: &mut Function,
    mut blocks: Vec<Block>,
    edge_drops: Vec<(BlockId, BlockId, Vec<Local>)>,
) -> Result<(), Ice> {
    let mut preds: BTreeMap<BlockId, Vec<BlockId>> = BTreeMap::new();
    for (id, block) in f.blocks.iter_enumerated() {
        for succ in block.term.successors() {
            let list = preds.entry(succ).or_default();
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    for (from, to, drops) in edge_drops {
        let only_pred = preds.get(&to).is_some_and(|p| p.as_slice() == [from]);
        let drops = drops.into_iter().map(Stmt::Drop);
        if only_pred {
            let target = blocks
                .get_mut(to.index())
                .ok_or_else(|| Ice::new("edge into an unknown block"))?;
            target.stmts.splice(0..0, drops);
        } else {
            let split = BlockId::from_u32(
                u32::try_from(blocks.len()).map_err(|_| Ice::new("too many MIR blocks"))?,
            );
            blocks.push(Block {
                params: Vec::new(),
                stmts: drops.collect(),
                term: Terminator::Goto {
                    target: to,
                    args: Vec::new(),
                },
                unwind: Vec::new(),
            });
            blocks
                .get_mut(from.index())
                .ok_or_else(|| Ice::new("edge from an unknown block"))?
                .term
                .retarget(to, split);
        }
    }
    f.blocks = cheby_iface::IndexVec::from_raw(blocks)?;
    Ok(())
}
