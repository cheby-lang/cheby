//! The unwind pass (D-698): unwind edges to shared, chained cleanup pads.
//!
//! The pass runs last, after reference counting (D-262, D-273): what a pad
//! releases depends on the final `dup` and `drop` placement. Per function:
//!
//! 1. Every instruction that may start or pass on an unwinding
//!    ([`stmt_may_unwind`], [`terminator_may_unwind`]) gets an edge to the
//!    pad of what an unwinding leaves held there, computed by the
//!    ownership simulation the validator uses ([`crate::ownership`]).
//! 2. Pads are chained and shared: `pad({})` is a block `resume`, and
//!    `pad(S)` is `drop newest(S); goto pad(S minus newest(S))`, where the
//!    newest local is the one with the highest index (a local holding two
//!    references is in `S` twice). Pads are memoized by set, so calls that
//!    leave the same values share one pad and neighbouring sets share
//!    their tails: one landing pad per set of live values (D-262).
//! 3. A function with a function-entry check (every [`Abi::Cheby`]
//!    function) gets `entry_unwind = pad(its counted parameters)`. C-ABI
//!    functions (entry thunks and trampolines) have no entry check, so no
//!    entry pad, but their calls get edges like any other.
//!    GAP(c-abi-entry-pad)
//! 4. An instruction that already has an edge, and an existing entry pad,
//!    are kept: the generated bodies of std intrinsics (D-693) come with
//!    their own (GAP(intrinsic-precounted)). Running the pass twice changes
//!    nothing.
//!
//! The pass is per function, deterministic (D-264: pads are created in
//! block and statement order, memoized in ordered maps) and linear in the
//! size of the function times the size of its pad sets.
//!
//! The MIR is the same for every unwinding backend (D-696 is open): the
//! code generator ignores the edges with the default backend, and
//! `CHEBY_UNWIND_EDGES=off` skips this pass for D-702's measurement.

use std::collections::BTreeMap;

use cheby_iface::{Ice, Idx};

use crate::ir::{
    Abi, Block, BlockId, Const, Function, Local, MirModule, Operand, Rvalue, Stmt, Terminator,
};
use crate::ownership::{self, Counts, Observer};
use crate::repr::Repr;

/// Whether `stmt` may start or pass on an unwinding (D-698): a Cheby
/// call; every foreign call (a kernel can run out of memory; a table of
/// kernels that never raise is a later optimization); a runtime helper
/// with [`crate::RuntimeFn::may_unwind`]; an `alloc`; a `box` that may
/// allocate; a `load_const` (its cold path forces the constant); checked
/// arithmetic. Never `dup` or `drop` (a drop aborts on a panic, D-171; a
/// count overflow makes the object static, D-454), field reads, compares
/// and the other pure operations.
#[must_use]
pub fn stmt_may_unwind(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => rvalue_may_unwind(value),
        Stmt::Dup(_) | Stmt::Drop(_) | Stmt::ClearUnwind => false,
    }
}

/// Whether evaluating `value` may start or pass on an unwinding
/// ([`stmt_may_unwind`]).
#[must_use]
pub fn rvalue_may_unwind(value: &Rvalue) -> bool {
    match value {
        Rvalue::Call { .. }
        | Rvalue::CallForeign { .. }
        | Rvalue::Alloc { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::Checked { .. } => true,
        Rvalue::CallRuntime { func, .. } => func.may_unwind(),
        Rvalue::Box { value, from } => box_may_allocate(value, *from),
        Rvalue::Use(_)
        | Rvalue::Field { .. }
        | Rvalue::ReadLayout { .. }
        | Rvalue::Unbox { .. }
        | Rvalue::Compare { .. }
        | Rvalue::FloatArith { .. }
        | Rvalue::Bitwise { .. }
        | Rvalue::FloatNeg { .. }
        | Rvalue::Not(_)
        | Rvalue::ReadTag { .. } => false,
    }
}

/// Whether `terminator` may start an unwinding: only `panic`. A tail call
/// leaves the frame before its callee runs, and `resume` is the unwinding
/// leaving.
#[must_use]
pub const fn terminator_may_unwind(terminator: &Terminator) -> bool {
    matches!(terminator, Terminator::Panic { .. })
}

/// Whether boxing `value` from `from` may allocate (D-278, D-521): a
/// 64-bit integer that does not fit a tagged word, and every float. Small
/// sized integers are always tagged and a `Bool` is a static object; a
/// constant that fits is folded to its tagged word. Must agree with the
/// code generator's `box_value`.
fn box_may_allocate(value: &Operand, from: Repr) -> bool {
    match from {
        Repr::Int | Repr::I64 | Repr::U64 => !constant_fits_tagged(value, from),
        Repr::Float | Repr::F32 => true,
        Repr::I8
        | Repr::I16
        | Repr::I32
        | Repr::U8
        | Repr::U16
        | Repr::U32
        | Repr::Bool
        | Repr::Value => false,
    }
}

/// Whether `value` is a 64-bit integer constant that a tagged word holds:
/// a signed value of 63 bits, or an unsigned one below 2^62.
fn constant_fits_tagged(value: &Operand, from: Repr) -> bool {
    let fits_signed = |n: i64| n.wrapping_shl(1) >> 1 == n;
    match (value, from) {
        (Operand::Const(Const::Int(n)), Repr::Int) => fits_signed(*n),
        (
            Operand::Const(Const::Sized {
                repr: Repr::I64,
                bits,
            }),
            Repr::I64,
        ) => fits_signed(bits.cast_signed()),
        (
            Operand::Const(Const::Sized {
                repr: Repr::U64,
                bits,
            }),
            Repr::U64,
        ) => *bits < 1 << 62,
        _ => false,
    }
}

/// Adds unwind edges and pads to every function of `module`.
///
/// # Errors
///
/// An [`Ice`] if a function's reference counts do not balance, or it is
/// malformed.
pub fn insert_module(module: &mut MirModule) -> Result<(), Ice> {
    for f in &mut module.functions {
        insert(f)?;
    }
    Ok(())
}

/// Adds unwind edges and pads to `f`, after reference counting (module
/// docs).
///
/// # Errors
///
/// An [`Ice`] if the reference counts of `f` do not balance, or `f` is
/// malformed.
pub fn insert(f: &mut Function) -> Result<(), Ice> {
    let mut sites = Sites::default();
    ownership::simulate(f, &mut sites)?;
    if !sites.errors.is_empty() {
        return Err(Ice::new(format!(
            "the unwind pass needs balanced reference counts:\n  {}",
            sites.errors.join("\n  ")
        )));
    }
    let mut pads = Pads::new(f.blocks.len());
    if f.entry_unwind.is_none() && f.abi == Abi::Cheby {
        let mut params = Vec::new();
        for &p in &f.params {
            if f.local(p)?.repr == Repr::Value {
                params.push(p);
            }
        }
        params.sort_unstable();
        f.entry_unwind = Some(pads.pad(&params)?);
    }
    // In block and statement order, so pads are numbered deterministically.
    let mut new_edges: BTreeMap<BlockId, Vec<(u32, BlockId)>> = BTreeMap::new();
    for ((block, at), owned) in sites.found {
        let b = f.block(block)?;
        if b.unwind_target(at).is_some() {
            continue;
        }
        let at = u32::try_from(at).map_err(|_| Ice::new("a block too large to number"))?;
        let pad = pads.pad(&owned)?;
        new_edges.entry(block).or_default().push((at, pad));
    }
    for (block, edges) in new_edges {
        let b = f
            .blocks
            .get_mut(block)
            .ok_or_else(|| Ice::new("an unwind site in an unknown block"))?;
        b.unwind.extend(edges);
        b.unwind.sort_unstable_by_key(|&(at, _)| at);
    }
    for (n, block) in pads.blocks.into_iter().enumerate() {
        let id = f.blocks.push(block)?;
        if id.index() != pads.first.saturating_add(n) {
            return Err(Ice::new("the unwind pass numbered a pad wrongly"));
        }
    }
    Ok(())
}

/// The unwind sites of a function and what an unwinding leaves held at
/// each, from the ownership simulation.
#[derive(Default)]
struct Sites {
    /// By block and instruction, so pads are created in that order.
    found: BTreeMap<(BlockId, usize), Vec<Local>>,
    errors: Vec<String>,
}

impl Observer for Sites {
    fn error(&mut self, message: String) {
        self.errors.push(message);
    }

    fn unwinds(&mut self, block: BlockId, at: usize, state: &Counts) {
        self.found.insert((block, at), state.owned());
    }

    fn wants_unwinds(&self) -> bool {
        true
    }
}

/// The pads created for one function, memoized as a trie: the pad that
/// drops `l` and continues to `below` is created once per `(below, l)`.
struct Pads {
    /// The id of the first new block: the function's blocks come first.
    first: usize,
    /// `pad({})`, once created.
    resume: Option<BlockId>,
    chain: BTreeMap<(BlockId, Local), BlockId>,
    /// The new blocks, in id order from `first`.
    blocks: Vec<Block>,
}

impl Pads {
    const fn new(first: usize) -> Self {
        Self {
            first,
            resume: None,
            chain: BTreeMap::new(),
            blocks: Vec::new(),
        }
    }

    fn push(&mut self, stmts: Vec<Stmt>, term: Terminator) -> Result<BlockId, Ice> {
        let next = self.first.saturating_add(self.blocks.len());
        let id =
            BlockId::from_u32(u32::try_from(next).map_err(|_| Ice::new("too many MIR blocks"))?);
        self.blocks.push(Block {
            params: Vec::new(),
            stmts,
            term,
            unwind: Vec::new(),
        });
        Ok(id)
    }

    /// The pad that releases `owned` (ascending, with repeats) and
    /// resumes.
    fn pad(&mut self, owned: &[Local]) -> Result<BlockId, Ice> {
        let mut below = if let Some(pad) = self.resume {
            pad
        } else {
            let pad = self.push(Vec::new(), Terminator::Resume)?;
            self.resume = Some(pad);
            pad
        };
        // `pad(S)` drops the newest local and continues to the pad of the
        // rest, so the chain is built from the oldest local up.
        for &l in owned {
            below = if let Some(&pad) = self.chain.get(&(below, l)) {
                pad
            } else {
                let pad = self.push(
                    vec![Stmt::Drop(l)],
                    Terminator::Goto {
                        target: below,
                        args: Vec::new(),
                    },
                )?;
                self.chain.insert((below, l), pad);
                pad
            };
        }
        Ok(below)
    }
}
