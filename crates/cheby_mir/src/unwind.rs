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
//!    `pad(S)` is `drop x; goto pad(S minus x)` for one local `x` of `S`
//!    (a local holding two references is in `S` twice). Pads are memoized
//!    as a trie keyed by `(the pad below, the local dropped)`, so calls
//!    that leave the same values share one pad and neighbouring sets share
//!    their tails: one landing pad per set of live values (D-262).
//! 3. Which local `x` is depends on the function's [`Direction`] (D-777),
//!    chosen once per function from every set its pads release, before
//!    any pad is built. [`Direction::Ascending`], D-698's shape, drops the
//!    newest local (the highest index) first: the trie grows from `resume`
//!    through the locals oldest first, and sets that share their oldest
//!    locals share pads, as when a function releases its locals newest
//!    first. [`Direction::Descending`] drops the oldest local first, for
//!    the mirror case. A function that makes its locals in order and
//!    releases them in the same order holds growing prefixes and then
//!    shrinking suffixes, which neither direction shares;
//!    [`Direction::Nested`] builds each set on the pad of another set that
//!    holds one local fewer (GAP(pad-chain-nested)), so it also shares a
//!    function that releases its locals one at a time in any order.
//!    The direction is the first of ascending, descending and nested
//!    with the fewest pads, by an exact count of the trie nodes each would
//!    create; the others are counted only when ascending creates more than
//!    a linear bound ([`plan_pads`]), so the common case costs one walk.
//! 4. A function with a function-entry check (every [`Abi::Cheby`]
//!    function) gets `entry_unwind = pad(its counted parameters)`, built
//!    first and in the function's direction. C-ABI functions (entry thunks
//!    and trampolines) have no entry check, so no entry pad, but their
//!    calls get edges like any other (D-752).
//! 5. An instruction that already has an edge, and an existing entry pad,
//!    are kept: the generated bodies of std intrinsics (D-693) come with
//!    their own (D-744). Running the pass twice changes
//!    nothing.
//!
//! The order of the drops inside a pad is not observable: the spec does
//! not order releases during unwinding.
//!
//! The pass is per function, deterministic (D-264: the direction is a
//! function of the sets, pads are created in block and statement order,
//! from sites sorted that way, and their memo is never iterated) and
//! linear in the size of the function times the size of its pad sets. A
//! function that releases its locals from the middle (each set a sliding
//! window of the locals) still gets a pad count quadratic in its locals
//! in every direction. What its simulation found at each site is kept
//! for the validation that follows (`Recorded`, D-774).
//!
//! The code generator lowers every edge with the return flag (D-733), and
//! `CHEBY_UNWIND_EDGES=off` skips this pass for D-702's measurement
//! (D-742).

use cheby_iface::{Ice, Idx};
use rustc_hash::FxHashMap;

use crate::ir::{
    Abi, Block, BlockId, Const, Function, Local, MirModule, Operand, Rvalue, Stmt, Terminator,
};
use crate::ownership::{self, Counts, Observer};
use crate::repr::Repr;

/// Whether `stmt` may start or pass on an unwinding (D-698): a Cheby
/// call; a foreign call (a kernel can run out of memory), except one of
/// the kernels that never raise ([`crate::never_raise`], D-767); a runtime
/// helper with [`crate::RuntimeFn::may_unwind`]; an `alloc`; a `box` that may
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
        | Rvalue::Alloc { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::Checked { .. } => true,
        Rvalue::CallForeign { symbol, .. } => !crate::never_raise::never_raises(symbol),
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
    match insert_recording(f)? {
        Ok(_) => Ok(()),
        Err(errors) => Err(Ice::new(format!(
            "the unwind pass needs balanced reference counts:\n  {}",
            errors.join("\n  ")
        ))),
    }
}

/// [`insert`], returning what the pass's ownership simulation found: the
/// state at every unwind site, which `validate`'s pad check compares each
/// pad chain with, or the broken ownership rules, with `f` left as it was
/// (D-774).
///
/// # Errors
///
/// An [`Ice`] if `f` is malformed.
pub(crate) fn insert_recording(f: &mut Function) -> Result<Result<Recorded, Vec<String>>, Ice> {
    let mut sites = Sites::default();
    ownership::simulate(f, &mut sites)?;
    if !sites.errors.is_empty() {
        return Ok(Err(sites.errors));
    }
    let mut recorded = Recorded {
        sites: sites.found,
        locals: sites.locals,
    };
    // Each instruction is reported once, so the keys are unique.
    recorded
        .sites
        .sort_unstable_by_key(|site| (site.block, site.at));
    // Every set a new pad releases, the entry pad's first, then the sites'
    // in block and statement order, so that pads are numbered
    // deterministically (D-264). The direction is chosen from all of them
    // before any pad is built (D-777).
    let entry = f.entry_unwind.is_none() && f.abi == Abi::Cheby;
    let mut params = Vec::new();
    if entry {
        for &p in &f.params {
            if f.local(p)?.repr == Repr::Value {
                params.push(p);
            }
        }
        params.sort_unstable();
    }
    let mut sets: Vec<&[Local]> = Vec::with_capacity(recorded.sites.len().saturating_add(1));
    if entry {
        sets.push(&params);
    }
    let mut edges: Vec<(BlockId, u32)> = Vec::new();
    for site in &recorded.sites {
        let b = f.block(site.block)?;
        if b.unwind_target(site.at).is_some() {
            continue;
        }
        let at = u32::try_from(site.at).map_err(|_| Ice::new("a block too large to number"))?;
        edges.push((site.block, at));
        sets.push(recorded.locals_of(site)?);
    }
    let plan = plan_pads(f.blocks.len(), f.locals.len(), &sets)?;
    let mut pads = plan.pads.into_iter();
    if entry {
        f.entry_unwind = Some(
            pads.next()
                .ok_or_else(|| Ice::new("the unwind pass lost the entry pad"))?,
        );
    }
    if pads.len() != edges.len() {
        return Err(Ice::new("the unwind pass lost a pad"));
    }
    // The sites are sorted, so each block's new edges are consecutive.
    let new_edges: Vec<(BlockId, u32, BlockId)> = edges
        .into_iter()
        .zip(pads)
        .map(|((block, at), pad)| (block, at, pad))
        .collect();
    for group in new_edges.chunk_by(|a, b| a.0 == b.0) {
        let Some(&(block, _, _)) = group.first() else {
            continue;
        };
        let b = f
            .blocks
            .get_mut(block)
            .ok_or_else(|| Ice::new("an unwind site in an unknown block"))?;
        b.unwind.extend(group.iter().map(|&(_, at, pad)| (at, pad)));
        b.unwind.sort_unstable_by_key(|&(at, _)| at);
    }
    let first = f.blocks.len();
    for (n, block) in plan.blocks.into_iter().enumerate() {
        let id = f.blocks.push(block)?;
        if id.index() != first.saturating_add(n) {
            return Err(Ice::new("the unwind pass numbered a pad wrongly"));
        }
    }
    Ok(Ok(recorded))
}

/// One unwind site the simulation reported: an instruction (the
/// terminator when `at` is the number of statements) and the range of
/// [`Recorded::locals`] it leaves held.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SiteState {
    block: BlockId,
    at: usize,
    start: usize,
    end: usize,
}

/// What the unwind pass's ownership simulation found in one function
/// (D-774): what an unwinding leaves held at
/// every instruction that may unwind or has an edge, from the state
/// before the instruction. `validate`'s pad check compares each pad chain
/// with it instead of simulating the counts a second time.
#[derive(Debug, Default)]
pub(crate) struct Recorded {
    /// Sorted by block and instruction.
    sites: Vec<SiteState>,
    /// Every site's owned multiset, ascending, one after the other.
    locals: Vec<Local>,
}

impl Recorded {
    /// What an unwinding out of instruction `at` of `block` leaves held,
    /// ascending with repeats, if the simulation reported that
    /// instruction.
    pub(crate) fn state(&self, block: BlockId, at: usize) -> Option<&[Local]> {
        let i = self
            .sites
            .binary_search_by_key(&(block, at), |site| (site.block, site.at))
            .ok()?;
        self.locals_of(self.sites.get(i)?).ok()
    }

    fn locals_of(&self, site: &SiteState) -> Result<&[Local], Ice> {
        self.locals
            .get(site.start..site.end)
            .ok_or_else(|| Ice::new("an unwind site's state out of range"))
    }
}

/// The unwind sites of a function and what an unwinding leaves held at
/// each, from the ownership simulation, in the order the simulation
/// reaches them.
#[derive(Default)]
struct Sites {
    found: Vec<SiteState>,
    locals: Vec<Local>,
    errors: Vec<String>,
}

impl Observer for Sites {
    fn error(&mut self, message: String) {
        self.errors.push(message);
    }

    fn unwinds(&mut self, block: BlockId, at: usize, state: &Counts) {
        let start = self.locals.len();
        state.extend_owned(&mut self.locals);
        self.found.push(SiteState {
            block,
            at,
            start,
            end: self.locals.len(),
        });
    }

    fn wants_unwinds(&self) -> bool {
        true
    }
}

/// The order in which a function's pad chains drop what they release
/// (D-777), chosen once per function by [`plan_pads`] (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    /// D-698's shape: `pad(S)` drops the newest local of `S` first, and the
    /// trie grows from `resume` through the locals oldest first.
    Ascending,
    /// `pad(S)` drops the oldest local of `S` first, and the trie grows
    /// from `resume` through the locals newest first.
    Descending,
    /// GAP(pad-chain-nested): equal sets share one pad, and each set is
    /// built on the pad of a smaller set: one that holds one local fewer
    /// (tried for its newest local, its oldest, then each newest first),
    /// else the larger of the sets of the sites before and after it that
    /// it contains (the one before on a tie), else `resume`. What it holds
    /// beyond that set is dropped newest first. Neither direction shares
    /// the prefixes a function holds while it makes its locals in order
    /// together with the suffixes it holds while it releases them in the
    /// same order; this builds one pad per distinct set there, and
    /// likewise when a function releases its locals one at a time in any
    /// order.
    Nested,
}

/// The pads of one function, from [`plan_pads`].
#[derive(Debug)]
pub(crate) struct PadPlan {
    /// The direction chosen.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests of the choice (D-777) read it")
    )]
    pub(crate) direction: Direction,
    /// The pad of each set, in the order of the sets.
    pub(crate) pads: Vec<BlockId>,
    /// The new blocks, in id order from the `first` given.
    pub(crate) blocks: Vec<Block>,
}

/// Chooses the direction of a function's pad chains (D-777) and builds
/// its pads: the pad of each of `sets` (each ascending, with repeats; the
/// entry pad's first, then the sites' in block and statement order), as
/// new blocks numbered from `first`, in a function of `locals` locals.
///
/// The choice counts exactly the trie nodes, that is the pads, each
/// direction creates. Ascending is counted first and is kept, without
/// counting the others, when it creates at most `2 × (sets + locals)`
/// pads, a bound linear in the function. Otherwise descending and then
/// nested are counted, each stopping once it reaches the fewest pads
/// so far, and the first with the fewest wins: ascending wins every tie.
/// Only the winner's pads become blocks.
///
/// # Errors
///
/// An [`Ice`] if the blocks cannot be numbered.
pub(crate) fn plan_pads(first: usize, locals: usize, sets: &[&[Local]]) -> Result<PadPlan, Ice> {
    let mut best = Pads::new(first, Direction::Ascending);
    let mut pads = Vec::with_capacity(sets.len());
    for set in sets {
        pads.push(best.pad(set)?);
    }
    let bound = sets.len().saturating_add(locals).saturating_mul(2);
    if best.len() > bound {
        let mut descending = Pads::new(first, Direction::Descending);
        let mut found = Vec::with_capacity(sets.len());
        for set in sets {
            if descending.len() >= best.len() {
                break;
            }
            found.push(descending.pad(set)?);
        }
        if found.len() == sets.len() && descending.len() < best.len() {
            best = descending;
            pads = found;
        }
        if let Some((nested, found)) = Pads::nested(first, sets, best.len())? {
            best = nested;
            pads = found;
        }
    }
    Ok(PadPlan {
        direction: best.direction,
        pads,
        blocks: best.into_blocks(),
    })
}

/// The pads of one function as a trie: the pad that drops `l` and
/// continues to `below` is created once per `(below, l)`. The trie is a
/// hash map, never iterated: pad ids come from the order of creation
/// (D-264). The pads stay nodes until [`Pads::into_blocks`], so that
/// counting a direction builds no block.
struct Pads {
    direction: Direction,
    /// The id of the first new block: the function's blocks come first.
    first: usize,
    /// `pad({})`, once created.
    resume: Option<BlockId>,
    chain: FxHashMap<(BlockId, Local), BlockId>,
    /// The new pads, in id order from `first`: the local each drops and
    /// the pad it continues to, or `None` for `resume`.
    nodes: Vec<Option<(Local, BlockId)>>,
}

impl Pads {
    fn new(first: usize, direction: Direction) -> Self {
        Self {
            direction,
            first,
            resume: None,
            chain: FxHashMap::default(),
            nodes: Vec::new(),
        }
    }

    /// The number of pads created.
    const fn len(&self) -> usize {
        self.nodes.len()
    }

    fn push(&mut self, node: Option<(Local, BlockId)>) -> Result<BlockId, Ice> {
        let next = self.first.saturating_add(self.nodes.len());
        let id =
            BlockId::from_u32(u32::try_from(next).map_err(|_| Ice::new("too many MIR blocks"))?);
        self.nodes.push(node);
        Ok(id)
    }

    /// `pad({})`.
    fn resume(&mut self) -> Result<BlockId, Ice> {
        if let Some(pad) = self.resume {
            return Ok(pad);
        }
        let pad = self.push(None)?;
        self.resume = Some(pad);
        Ok(pad)
    }

    /// The pad that drops `locals` in reverse order, newest given last
    /// dropped first, and continues to `below`.
    fn extend(
        &mut self,
        mut below: BlockId,
        locals: impl Iterator<Item = Local>,
    ) -> Result<BlockId, Ice> {
        for l in locals {
            below = if let Some(&pad) = self.chain.get(&(below, l)) {
                pad
            } else {
                let pad = self.push(Some((l, below)))?;
                self.chain.insert((below, l), pad);
                pad
            };
        }
        Ok(below)
    }

    /// The pad that releases `owned` (ascending, with repeats) and
    /// resumes, in the direction of the trie: the chain is built from
    /// `resume` up through `owned` oldest first when ascending, so `pad(S)`
    /// drops the newest local first, and newest first when descending, so
    /// `pad(S)` drops the oldest local first. Not for [`Direction::Nested`].
    fn pad(&mut self, owned: &[Local]) -> Result<BlockId, Ice> {
        let resume = self.resume()?;
        match self.direction {
            Direction::Ascending => self.extend(resume, owned.iter().copied()),
            Direction::Descending => self.extend(resume, owned.iter().rev().copied()),
            Direction::Nested => Err(Ice::new("a nested pad built from `resume`")),
        }
    }

    /// The pads of `sets` in [`Direction::Nested`], or `None` once they
    /// reach `limit` pads.
    fn nested(
        first: usize,
        sets: &[&[Local]],
        limit: usize,
    ) -> Result<Option<(Self, Vec<BlockId>)>, Ice> {
        let set = |i: usize| {
            sets.get(i)
                .copied()
                .ok_or_else(|| Ice::new("a pad set out of range"))
        };
        // Equal sets share the pad of the first of them, found by a
        // multiset hash; on a collision of unequal sets the later one is
        // only built on its own.
        let mut first_of: FxHashMap<u64, usize> = FxHashMap::default();
        let mut hashes = Vec::with_capacity(sets.len());
        let mut same = Vec::with_capacity(sets.len());
        for (i, &s) in sets.iter().enumerate() {
            let h = multiset_hash(s);
            hashes.push(h);
            same.push(match first_of.get(&h) {
                Some(&j) if set(j)? == s => j,
                Some(_) => i,
                None => {
                    first_of.insert(h, i);
                    i
                }
            });
        }
        let first_equal = |i: usize| {
            same.get(i)
                .copied()
                .ok_or_else(|| Ice::new("a pad set out of range"))
        };
        // The set each first set is built on, always a smaller one, so the
        // sets cannot be built on each other: a set holding one local
        // fewer (its newest, its oldest, then any, newest first), else
        // the larger of the sets of the site before and after it that it
        // contains (the one before on a tie), else none.
        let mut on: Vec<Option<usize>> = vec![None; sets.len()];
        for (i, &s) in sets.iter().enumerate() {
            if first_equal(i)? != i {
                continue;
            }
            let h = hashes.get(i).copied().unwrap_or_default();
            let one_fewer = |x: &Local| {
                let &j = first_of.get(&h.wrapping_sub(mix(*x)))?;
                let t = sets.get(j)?;
                (t.len().wrapping_add(1) == s.len() && contains(s, t)).then_some(j)
            };
            let mut base = s
                .last()
                .into_iter()
                .chain(s.first())
                .chain(s.iter().rev())
                .find_map(one_fewer);
            if base.is_none() {
                let smaller = |j: usize| {
                    sets.get(j)
                        .filter(|t| t.len() < s.len() && contains(s, t))
                        .map(|t| (j, t.len()))
                };
                let before = i.checked_sub(1).and_then(smaller);
                let after = smaller(i.saturating_add(1));
                base = match (before, after) {
                    (Some((_, b)), Some((j, a))) if a > b => Some(j),
                    (Some((j, _)), _) | (None, Some((j, _))) => Some(j),
                    (None, None) => None,
                };
            }
            if let (Some(slot), Some(j)) = (on.get_mut(i), base) {
                *slot = Some(first_equal(j)?);
            }
        }
        let mut pads = Self::new(first, Direction::Nested);
        let mut built: Vec<Option<BlockId>> = vec![None; sets.len()];
        let mut path = Vec::new();
        for start in 0..sets.len() {
            // Follow the sets each is built on down to one that is built
            // or built on `resume`, then build them back up.
            path.clear();
            let mut i = first_equal(start)?;
            while built.get(i).copied().flatten().is_none() {
                path.push(i);
                if path.len() > sets.len() {
                    return Err(Ice::new("the pad sets are built on each other"));
                }
                match on.get(i).copied().flatten() {
                    Some(j) => i = j,
                    None => break,
                }
            }
            while let Some(i) = path.pop() {
                let (below, base) = match on.get(i).copied().flatten() {
                    Some(j) => (built.get(j).copied().flatten(), set(j)?),
                    None => (Some(pads.resume()?), &[][..]),
                };
                let below = below.ok_or_else(|| Ice::new("a pad set built before its base"))?;
                let pad = pads.extend(below, difference(set(i)?, base).into_iter())?;
                if let Some(slot) = built.get_mut(i) {
                    *slot = Some(pad);
                }
                if pads.len() >= limit {
                    return Ok(None);
                }
            }
            let pad = built.get(first_equal(start)?).copied().flatten();
            if let Some(slot) = built.get_mut(start) {
                *slot = pad;
            }
        }
        let found = built
            .into_iter()
            .map(|pad| pad.ok_or_else(|| Ice::new("a pad set left unbuilt")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some((pads, found)))
    }

    /// The new blocks, in id order from `first`.
    fn into_blocks(self) -> Vec<Block> {
        self.nodes
            .into_iter()
            .map(|node| {
                let (stmts, term) = match node {
                    None => (Vec::new(), Terminator::Resume),
                    Some((l, below)) => (
                        vec![Stmt::Drop(l)],
                        Terminator::Goto {
                            target: below,
                            args: Vec::new(),
                        },
                    ),
                };
                Block {
                    params: Vec::new(),
                    stmts,
                    term,
                    unwind: Vec::new(),
                }
            })
            .collect()
    }
}

/// A hash of the multiset `set` that is the sum of its locals' hashes, so
/// that removing a local subtracts that local's hash ([`mix`]).
fn multiset_hash(set: &[Local]) -> u64 {
    set.iter().fold(0, |h, &l| h.wrapping_add(mix(l)))
}

/// A hash of one local: `SplitMix64`'s finalizer, so that sums of hashes
/// of different multisets rarely collide.
fn mix(l: Local) -> u64 {
    let mut z = u64::from(l.as_u32()).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Whether the multiset `small` is contained in `big`, both ascending.
fn contains(big: &[Local], small: &[Local]) -> bool {
    if small.len() > big.len() {
        return false;
    }
    let mut big = big.iter();
    small.iter().all(|s| big.any(|b| b == s))
}

/// What the multiset `big` holds beyond `small`, ascending; `small` is
/// contained in `big` ([`contains`]), both ascending.
fn difference(big: &[Local], small: &[Local]) -> Vec<Local> {
    let mut out = Vec::with_capacity(big.len().saturating_sub(small.len()));
    let mut small = small.iter().peekable();
    for &b in big {
        if small.peek() == Some(&&b) {
            small.next();
        } else {
            out.push(b);
        }
    }
    out
}
