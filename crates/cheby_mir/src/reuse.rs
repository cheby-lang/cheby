//! Reuse (D-716, D-005): a `drop` of an object whose layout is known pairs
//! with a later allocation of the same size in the same block, so a unique
//! object's block is written over instead of freed and allocated again.
//!
//! The pass runs after reference counting and before the unwind pass
//! (`rc::insert → reuse::insert → unwind::insert`, architecture §9.1), on
//! Perceus counting only ([`crate::MirOptions::reuses`]); naive counting
//! has no reuse (D-718, D-808). It rewrites a paired `drop x` into
//! `t = drop_reuse x of L` and the allocation into
//! `alloc_reuse t, L'(…)` ([`Rvalue::DropReuse`], [`Rvalue::AllocReuse`]).
//! The token `t` is an ordinary counted local: the unwind pass releases it
//! in the pads of what may unwind between the two, and the ownership
//! simulation checks it like any other local (GAP(reuse-token-form)).
//!
//! # Pairing (GAP(reuse-pairing-scope))
//!
//! Per function, block by block in statement order (D-264):
//!
//! - A **source** is a `drop x` of a counted local that is not borrowed
//!   (D-715, D-807), not a constant's value (a `load_const` result or a
//!   constant operand, which is static and never unique), and not a tuple
//!   being filled field by field (D-810), whose layout `L` a `field … of L`
//!   read of `x` proves: one in the same block before the drop, or in a
//!   block that dominates it. `L` is a constructor with fields or a tuple.
//!   A read proves `L` only when it pins the constructor: `L` is a tuple
//!   or its type's only constructor, or the read is in an arm of a switch
//!   on `x`'s tag that only `L`'s tag enters. A field every constructor
//!   holds at the same place is read with the first constructor's layout
//!   and no test of the tag (D-071): that read proves nothing, since `x`
//!   may be a constructor of another size (the fix review's TOK-1). The
//!   decisions' candidates and [`candidate_params`] use the same proofs.
//! - A **sink** is an `alloc` of a constructor with fields or a tuple whose
//!   object size ([`crate::layout::object_size`]) equals the source's. A
//!   placeholder tuple (`alloc_placeholder`, D-810) is never a sink
//!   (GAP(reuse-and-large-tuples)).
//! - **In the source's block**, each sink takes the nearest unpaired source
//!   of its size before it; each such source pairs once.
//! - **Across blocks**, a source still unpaired at the end of its block
//!   holds its token into the blocks that only its block enters (the
//!   single-predecessor tree below it, which it dominates). On each path
//!   down that tree, the first block with an unpaired allocation of its
//!   size takes the token in its first such allocation; a path that reaches
//!   no such allocation drops the token as it leaves the source's tree: at
//!   the start of a block of the tree with no sink below it, or on a new
//!   block splitting an edge into a block with other predecessors. So the
//!   token is consumed once on every path, and the source's `drop` stays
//!   where counting put it. This is the matched constructor rebuilt in a
//!   nested arm when no arm uses the matched value: counting drops it in
//!   the block that reads its fields, before the inner `case`'s switch
//!   (`std::sorted_map::insert`, gate 6's ROB6-1). Sources nearest their
//!   sinks (deepest in the tree) are routed first.
//!
//! A `precounted` body (D-693) is left alone.
//!
//! # Borrowing (GAP(reuse-owns-parameters))
//!
//! A borrowed parameter is never dropped, so it can never be reused.
//! [`candidate_params`] names the parameters whose drop would pair if they
//! were owned; the borrowing inference counts them as consumed (D-806), so
//! they stay owned. It runs the counting and this pairing on a copy of the
//! lowered function: the one predicate both use.
//!
//! # Decisions
//!
//! [`decisions`] reads one counted function and says, for each allocation
//! that could reuse, whether it does and why not: the reuse report's input
//! (D-717).

use std::collections::{BTreeMap, BTreeSet};

use cheby_iface::{DefId, Ice, Idx, builtin};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{
    Block, BlockId, FnKey, Function, LayoutRef, Local, LocalDecl, MirModule, Operand, Rvalue, Stmt,
    TagType, Terminator,
};
use crate::layout::object_size;
use crate::liveness::is_borrowed_local;
use crate::repr::Repr;
use crate::validate::dom::Dominance;

/// Why an allocation reuses a dying object, or why it does not.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ReuseReason {
    /// It writes over the block of the source, when that is unique at run
    /// time.
    Reused,
    /// An object of its size whose layout is known here (the candidate) is
    /// still used after the allocation, or dies elsewhere.
    UsedLater,
    /// The candidate is borrowed from the caller (D-715, D-807), so this
    /// function never releases it.
    Borrowed,
    /// The candidate is a constant's value, which is static and never
    /// unique.
    Constant,
    /// No object of its size whose layout is known dies before it in its
    /// block.
    NoCandidate,
}

/// One allocation's reuse decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReuseDecision {
    /// The local the allocation (`alloc` or `alloc_reuse`) defines.
    pub sink: Local,
    /// The object it reuses ([`ReuseReason::Reused`]) or the candidate it
    /// does not reuse; `None` with [`ReuseReason::NoCandidate`].
    pub source: Option<Local>,
    pub reason: ReuseReason,
}

/// Pairs drops with allocations in every function of `module` (module
/// documentation). Runs after [`crate::rc::insert_module`], before the
/// unwind pass.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn insert_module(module: &mut MirModule) -> Result<(), Ice> {
    let mut functions = std::mem::take(&mut module.functions);
    let result = functions.iter_mut().try_for_each(|f| insert(module, f));
    module.functions = functions;
    result
}

/// Pairs drops with allocations in `f`, a function of `module` after
/// reference counting (module documentation).
///
/// # Errors
///
/// An [`Ice`] if `f` has unwind edges already, or is malformed.
pub fn insert(module: &MirModule, f: &mut Function) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    if f.entry_unwind.is_some() || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Err(Ice::new("reuse after the unwind pass"));
    }
    // The tokens dropped at the start of a block, and on a split edge, in
    // the order of the pairs (D-264).
    let mut starts: BTreeMap<BlockId, Vec<Local>> = BTreeMap::new();
    let mut edges: Vec<((BlockId, BlockId), Vec<Local>)> = Vec::new();
    let mut edge_at: BTreeMap<(BlockId, BlockId), usize> = BTreeMap::new();
    for pair in pairs(module, f)? {
        let token = f.locals.push(LocalDecl {
            repr: Repr::Value,
            name: None,
            ty: None,
        })?;
        let source = f
            .blocks
            .get_mut(pair.block)
            .ok_or_else(|| Ice::new("reuse: no such block"))?
            .stmts
            .get_mut(pair.source_at)
            .ok_or_else(|| Ice::new("reuse: no such source"))?;
        *source = Stmt::Assign {
            dst: token,
            value: Rvalue::DropReuse {
                local: pair.local,
                layout: pair.layout,
            },
        };
        for &(block, at) in &pair.sinks {
            make_sink(f, block, at, token)?;
        }
        for drop in pair.drops {
            match drop {
                TokenDrop::Start(block) => starts.entry(block).or_default().push(token),
                TokenDrop::Edge(from, to) => {
                    let i = *edge_at.entry((from, to)).or_insert_with(|| {
                        edges.push(((from, to), Vec::new()));
                        edges.len().saturating_sub(1)
                    });
                    edges
                        .get_mut(i)
                        .ok_or_else(|| Ice::new("reuse: no such edge"))?
                        .1
                        .push(token);
                }
            }
        }
    }
    // Statement positions are no longer needed: insert the drops.
    for (block, tokens) in starts {
        f.blocks
            .get_mut(block)
            .ok_or_else(|| Ice::new("reuse: no such block"))?
            .stmts
            .splice(0..0, tokens.into_iter().map(Stmt::Drop));
    }
    for ((from, to), tokens) in edges {
        let split = f.blocks.push(Block {
            params: Vec::new(),
            stmts: tokens.into_iter().map(Stmt::Drop).collect(),
            term: Terminator::Goto {
                target: to,
                args: Vec::new(),
            },
            unwind: Vec::new(),
        })?;
        let term = &mut f
            .blocks
            .get_mut(from)
            .ok_or_else(|| Ice::new("reuse: no such block"))?
            .term;
        // Only a branch or a switch has an edge into a block with other
        // predecessors while a block below it holds a sink; their targets
        // take no parameters.
        if !matches!(term, Terminator::Branch { .. } | Terminator::Switch { .. }) {
            return Err(Ice::new("reuse: a token's edge to split is not a branch"));
        }
        term.retarget(to, split);
    }
    Ok(())
}

/// Turns the `alloc` at position `at` of `block` into an `alloc_reuse` of
/// `token`.
fn make_sink(f: &mut Function, block: BlockId, at: usize, token: Local) -> Result<(), Ice> {
    let sink = f
        .blocks
        .get_mut(block)
        .ok_or_else(|| Ice::new("reuse: no such block"))?
        .stmts
        .get_mut(at)
        .ok_or_else(|| Ice::new("reuse: no such sink"))?;
    let Stmt::Assign {
        value: Rvalue::Alloc { layout, fields },
        ..
    } = sink
    else {
        return Err(Ice::new("reuse: the sink is not an allocation"));
    };
    let value = Rvalue::AllocReuse {
        token,
        layout: *layout,
        fields: std::mem::take(fields),
    };
    if let Stmt::Assign { value: slot, .. } = sink {
        *slot = value;
    }
    Ok(())
}

/// The counted parameters of `f`, a lowered function of `module` that
/// borrows nothing yet, whose `drop` the counting and this pass would pair
/// with an allocation if they were owned (GAP(reuse-owns-parameters),
/// refining D-806). Sorted.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn candidate_params(module: &MirModule, f: &Function) -> Result<Vec<Local>, Ice> {
    if f.precounted || !f.borrow.is_owned() || !may_pair(module, f) {
        return Ok(Vec::new());
    }
    let mut counted = f.clone();
    crate::rc::insert(&mut counted)?;
    let params: BTreeSet<Local> = f.params.iter().copied().collect();
    let mut out: Vec<Local> = pairs(module, &counted)?
        .into_iter()
        .map(|p| p.local)
        .filter(|l| params.contains(l))
        .collect();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// A quick test before [`candidate_params`] counts a copy: a parameter
/// has a field read of a layout whose size some allocation of `f` has.
fn may_pair(module: &MirModule, f: &Function) -> bool {
    let params: BTreeSet<Local> = f.params.iter().copied().collect();
    let mut read = BTreeSet::new();
    let mut made = BTreeSet::new();
    for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
        match stmt {
            Stmt::Assign {
                value: Rvalue::Field { base, layout, .. },
                ..
            } if params.contains(base) => read.extend(reusable_size(module, *layout)),
            Stmt::Assign {
                value: Rvalue::Alloc { layout, .. },
                ..
            } => made.extend(reusable_size(module, *layout)),
            _ => {}
        }
    }
    read.intersection(&made).next().is_some()
}

/// The object size of `layout` if an object of it may be reused: a
/// constructor with fields or a tuple (D-716).
fn reusable_size(module: &MirModule, layout: LayoutRef) -> Option<u64> {
    let shape = match layout {
        LayoutRef::Ctor(_) => !module.layout(layout).is_some_and(|d| d.fields.is_empty()),
        LayoutRef::Tuple(n) => n > 0,
        _ => false,
    };
    shape.then(|| object_size(module, layout)).flatten()
}

/// The arms of `f` that test a constructor, by the local whose tag they
/// test: each block that a `switch` on a `read_tag` of that local in the
/// same block enters for exactly one tag, and that has no other
/// predecessor, with the constructor of that tag. The default arm takes
/// the tags no case names (pattern lowering makes the last constructor of
/// an exhaustive `case` the default). Such a block dominates every block
/// it reaches without passing the switch again, and the local is defined
/// once, before its tag is read, so a read there sees that constructor.
fn tested_variants(
    module: &MirModule,
    f: &Function,
    dom: &Dominance,
    preds: &[Vec<usize>],
) -> Result<FxHashMap<Local, Vec<(usize, Variant)>>, Ice> {
    let mut out: FxHashMap<Local, Vec<(usize, Variant)>> = FxHashMap::default();
    for (id, block) in f.blocks.iter_enumerated() {
        let Terminator::Switch {
            on: Operand::Local(on),
            cases,
            default,
        } = &block.term
        else {
            continue;
        };
        if !dom.is_reachable(id.index()) {
            continue;
        }
        let Some((base, ty)) = block.stmts.iter().find_map(|s| match s {
            Stmt::Assign {
                dst,
                value:
                    Rvalue::ReadTag {
                        base: Operand::Local(base),
                        ty,
                    },
            } if dst == on => Some((*base, *ty)),
            _ => None,
        }) else {
            continue;
        };
        // The builtin types' constructors and tags are fixed (D-454).
        let (type_def, count) = match ty {
            TagType::Option => (builtin::OPTION, 2),
            TagType::Result => (builtin::RESULT, 2),
            TagType::Order => (builtin::ORDER, 3),
            TagType::Adt(def) => (def, module.type_layouts(def).len()),
        };
        let count = u64::try_from(count).map_err(|_| Ice::new("too many constructors"))?;
        let named: BTreeSet<u64> = cases.iter().map(|&(tag, _)| tag).collect();
        let mut targets: Vec<BlockId> = cases.iter().map(|&(_, b)| b).chain(*default).collect();
        targets.sort_unstable();
        targets.dedup();
        for target in targets {
            let mut tags: BTreeSet<u64> = cases
                .iter()
                .filter(|&&(_, b)| b == target)
                .map(|&(tag, _)| tag)
                .collect();
            if *default == Some(target) {
                tags.extend((0..count).filter(|t| !named.contains(t)));
            }
            let s = target.index();
            let only_from_here = s != 0
                && preds
                    .get(s)
                    .is_some_and(|p| p.as_slice() == [id.index()].as_slice());
            if only_from_here
                && tags.len() == 1
                && let Some(&tag) = tags.first()
                && let Ok(tag) = u32::try_from(tag)
            {
                out.entry(base).or_default().push((s, (type_def, tag)));
            }
        }
    }
    Ok(out)
}

/// Whether a read of a field of `base` as `layout` in `block` proves that
/// `base` is an object of `layout` (GAP(reuse-pairing-scope)). A tuple's
/// read does; a constructor's does when it is its type's only constructor
/// ([`crate::LayoutDesc::only_ctor`]) or when an arm that tests `base` for
/// it dominates `block` ([`tested_variants`]). A field every constructor
/// holds at the same place is read once, with the layout of the first
/// constructor and no test of the tag (D-071), and constructors that hold
/// a field alike share one arm of the tag's switch: neither read proves a
/// layout, since `base` may be a constructor of another size.
fn pins(
    module: &MirModule,
    tested: &FxHashMap<Local, Vec<(usize, Variant)>>,
    dom: &Dominance,
    base: Local,
    layout: LayoutRef,
    block: BlockId,
) -> bool {
    match layout {
        LayoutRef::Tuple(_) => true,
        LayoutRef::Ctor(_) => {
            module.layout(layout).is_some_and(|d| d.only_ctor)
                || variant_of(module, layout).is_some_and(|v| {
                    tested.get(&base).is_some_and(|arms| {
                        arms.iter()
                            .any(|&(s, w)| w == v && dom.dominates(s, block.index()))
                    })
                })
        }
        _ => false,
    }
}

/// The type and tag of the constructor `layout`: from its layout, or, for
/// the builtin one-field constructors a unit may allocate without listing
/// their layout, the fixed ones (D-454): `Some` and `Ok` have tag 0, `Err`
/// tag 1.
fn variant_of(module: &MirModule, layout: LayoutRef) -> Option<Variant> {
    let LayoutRef::Ctor(def) = layout else {
        return None;
    };
    if let Some(d) = module.layout(layout) {
        return Some((d.type_def, d.tag));
    }
    match def {
        d if d == builtin::SOME => Some((builtin::OPTION, 0)),
        d if d == builtin::OK => Some((builtin::RESULT, 0)),
        d if d == builtin::ERR => Some((builtin::RESULT, 1)),
        _ => None,
    }
}

/// A drop paired with allocations: one later in its own block, or, held
/// across the end of its block, the first one on each path that reaches
/// one through the blocks below it (module documentation).
#[derive(Clone, Debug)]
struct Pair {
    block: BlockId,
    /// The position of the `drop`.
    source_at: usize,
    local: Local,
    /// The layout the field read proves.
    layout: LayoutRef,
    /// The `alloc`s that take the token, by block and position: at most
    /// one on any path.
    sinks: Vec<(BlockId, usize)>,
    /// Where the paths that reach no sink drop the token.
    drops: Vec<TokenDrop>,
}

/// Where a token held across the end of its block is dropped on a path
/// that reaches no sink.
#[derive(Clone, Copy, Debug)]
enum TokenDrop {
    /// At the start of the block, whose only predecessor holds the token.
    Start(BlockId),
    /// In a new block on the edge from the first block to the second,
    /// which has other predecessors.
    Edge(BlockId, BlockId),
}

/// A source still unpaired at the end of its block.
#[derive(Clone, Copy, Debug)]
struct Open {
    block: BlockId,
    at: usize,
    local: Local,
    layout: LayoutRef,
    size: u64,
}

/// A field read that proves its base's layout: its position in its block,
/// the layout and its object size.
type Proof = (usize, LayoutRef, u64);

/// What the pairing and the decisions know of a function.
struct Facts {
    /// By local index, whether the local is borrowed (D-715, D-807).
    borrowed: Vec<bool>,
    /// The values of constants: `load_const` results and locals defined as
    /// a constant operand, static and never unique.
    constants: BTreeSet<Local>,
    /// Tuples being filled field by field (D-810): an `init_field`'s object
    /// or a continuation's first parameter.
    filled: BTreeSet<Local>,
    /// The layouts field reads prove, by base, then by block: each read's
    /// position, layout and object size, in statement order.
    proofs: FxHashMap<Local, FxHashMap<BlockId, Vec<Proof>>>,
    /// The same reads by block: position, base, layout and size, in
    /// statement order.
    by_block: FxHashMap<BlockId, Vec<(usize, Local, LayoutRef, u64)>>,
    /// Each block's distinct successors, in terminator order, by block
    /// index.
    succs: Vec<Vec<usize>>,
    /// Each block's distinct reachable predecessors, by block index.
    preds: Vec<Vec<usize>>,
    /// Dominance over the blocks.
    dom: Dominance,
}

/// A constructor by its type and tag: [`LayoutDesc::tag`] of
/// [`LayoutDesc::type_def`].
///
/// [`LayoutDesc::tag`]: crate::LayoutDesc::tag
/// [`LayoutDesc::type_def`]: crate::LayoutDesc::type_def
type Variant = (DefId, u32);

impl Facts {
    fn of(module: &MirModule, f: &Function) -> Result<Self, Ice> {
        let mut constants = BTreeSet::new();
        let mut filled = BTreeSet::new();
        let mut proofs: FxHashMap<Local, FxHashMap<BlockId, Vec<Proof>>> = FxHashMap::default();
        let mut by_block: FxHashMap<BlockId, Vec<(usize, Local, LayoutRef, u64)>> =
            FxHashMap::default();
        if let (FnKey::Continuation { .. }, Some(&p)) = (f.key, f.params.first()) {
            filled.insert(p);
        }
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
                    .ok_or_else(|| Ice::new("reuse: no such successor"))?
                    .push(b);
            }
        }
        let tested = tested_variants(module, f, &dom, &preds)?;
        for (id, block) in f.blocks.iter_enumerated() {
            for (k, stmt) in block.stmts.iter().enumerate() {
                match stmt {
                    Stmt::Assign {
                        dst,
                        value: Rvalue::LoadConst(_) | Rvalue::Use(Operand::Const(_)),
                    } => {
                        constants.insert(*dst);
                    }
                    Stmt::Assign {
                        value: Rvalue::Field { base, layout, .. },
                        ..
                    } => {
                        if let Some(size) = reusable_size(module, *layout)
                            && pins(module, &tested, &dom, *base, *layout, id)
                        {
                            proofs
                                .entry(*base)
                                .or_default()
                                .entry(id)
                                .or_default()
                                .push((k, *layout, size));
                            by_block
                                .entry(id)
                                .or_default()
                                .push((k, *base, *layout, size));
                        }
                    }
                    Stmt::Eval(Rvalue::InitField { obj, .. }) => {
                        filled.insert(*obj);
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            borrowed: f.borrowed_locals()?,
            constants,
            filled,
            proofs,
            by_block,
            dom,
            succs,
            preds,
        })
    }

    /// Whether a drop of `x` may be a source, its layout aside.
    fn may_reuse(&self, f: &Function, x: Local) -> Result<bool, Ice> {
        Ok(f.local(x)?.repr == Repr::Value
            && !is_borrowed_local(&self.borrowed, x)
            && !self.constants.contains(&x)
            && !self.filled.contains(&x))
    }

    /// The layout and size a field read of `x` proves at position `at` of
    /// `block`: the first read in the block before `at`, else the first in
    /// the nearest block that dominates it. Walks up the dominator tree
    /// from `block` and looks only at `x`'s reads in each block, so a local
    /// read and dropped in each of many arms costs O(depth) per drop, not
    /// O(reads) (gate 6's ROB6-2).
    fn proof_at(&self, x: Local, block: BlockId, at: usize) -> Option<(LayoutRef, u64)> {
        let reads = self.proofs.get(&x)?;
        if let Some(&(k, layout, size)) = reads.get(&block).and_then(|v| v.first())
            && k < at
        {
            return Some((layout, size));
        }
        let mut up = self.dom.immediate_dominator(block.index());
        while let Some(d) = up {
            let id = BlockId::from_usize(d)?;
            if let Some(&(_, layout, size)) = reads.get(&id).and_then(|v| v.first()) {
                return Some((layout, size));
            }
            up = self.dom.immediate_dominator(d);
        }
        None
    }
}

/// The pairs of `f`, a counted function of `module` without unwind edges
/// (module documentation).
fn pairs(module: &MirModule, f: &Function) -> Result<Vec<Pair>, Ice> {
    let facts = Facts::of(module, f)?;
    let mut out = Vec::new();
    // The allocations paired in their block, and the sources left open at
    // the end of theirs.
    let mut taken: FxHashSet<(BlockId, usize)> = FxHashSet::default();
    let mut open_at_end: Vec<Open> = Vec::new();
    for (id, block) in f.blocks.iter_enumerated() {
        if !facts.dom.is_reachable(id.index()) {
            continue;
        }
        let mut open: Vec<Open> = Vec::new();
        for (j, stmt) in block.stmts.iter().enumerate() {
            match stmt {
                Stmt::Drop(x) if facts.may_reuse(f, *x)? => {
                    if let Some((layout, size)) = facts.proof_at(*x, id, j) {
                        open.push(Open {
                            block: id,
                            at: j,
                            local: *x,
                            layout,
                            size,
                        });
                    }
                }
                Stmt::Assign {
                    value: Rvalue::Alloc { layout, .. },
                    ..
                } => {
                    let Some(size) = reusable_size(module, *layout) else {
                        continue;
                    };
                    if let Some(pos) = open.iter().rposition(|o| o.size == size) {
                        let source = open.remove(pos);
                        taken.insert((id, j));
                        out.push(Pair {
                            block: id,
                            source_at: source.at,
                            local: source.local,
                            layout: source.layout,
                            sinks: vec![(id, j)],
                            drops: Vec::new(),
                        });
                    }
                }
                _ => {}
            }
        }
        open_at_end.extend(open);
    }
    if !open_at_end.is_empty() {
        out.extend(Across::new(module, f, &facts, &taken).route_all(open_at_end)?);
    }
    Ok(out)
}

/// The pairing across blocks (module documentation): the
/// single-predecessor tree, and per object size the unpaired allocations
/// below each block.
struct Across<'a> {
    module: &'a MirModule,
    f: &'a Function,
    facts: &'a Facts,
    taken: &'a FxHashSet<(BlockId, usize)>,
    /// By block index: the block's only predecessor, when it has one and
    /// is not the entry (its parent in the tree).
    parent: Vec<Option<usize>>,
    /// By block index: its depth in the tree (a root's is 0).
    depth: Vec<usize>,
    /// The blocks with each parent before its children.
    order: Vec<usize>,
    /// Per object size: the allocations still free in each block and
    /// below it.
    sizes: BTreeMap<u64, Free>,
}

/// The allocations of one object size not yet paired.
struct Free {
    /// By block index: the positions of its own, in statement order.
    own: Vec<Vec<usize>>,
    /// By block index: how many of `own` are taken (from the front).
    used: Vec<usize>,
    /// By block index: how many are free in it and in the blocks below it.
    below: Vec<usize>,
}

impl<'a> Across<'a> {
    fn new(
        module: &'a MirModule,
        f: &'a Function,
        facts: &'a Facts,
        taken: &'a FxHashSet<(BlockId, usize)>,
    ) -> Self {
        let n = f.blocks.len();
        let parent: Vec<Option<usize>> = facts
            .preds
            .iter()
            .enumerate()
            .map(|(s, p)| match p.as_slice() {
                [only] if s != 0 => Some(*only),
                _ => None,
            })
            .collect();
        // Roots first, then each block's children: parents come first.
        let mut order: Vec<usize> = (0..n)
            .filter(|&b| facts.dom.is_reachable(b) && parent.get(b) == Some(&None))
            .collect();
        let mut depth: Vec<usize> = vec![0; n];
        let mut next = 0;
        while let Some(&b) = order.get(next) {
            next = next.saturating_add(1);
            let d = depth.get(b).copied().unwrap_or(0);
            for &c in facts.succs.get(b).into_iter().flatten() {
                if parent.get(c) == Some(&Some(b)) {
                    if let Some(slot) = depth.get_mut(c) {
                        *slot = d.saturating_add(1);
                    }
                    order.push(c);
                }
            }
        }
        Self {
            module,
            f,
            facts,
            taken,
            parent,
            depth,
            order,
            sizes: BTreeMap::new(),
        }
    }

    /// Whether `c` is a child of `b` in the tree.
    fn is_child(&self, b: usize, c: usize) -> bool {
        self.parent.get(c) == Some(&Some(b))
    }

    /// The free allocations of `size`, counted on first use.
    fn free(&mut self, size: u64) -> Result<&mut Free, Ice> {
        if !self.sizes.contains_key(&size) {
            let n = self.f.blocks.len();
            let mut own: Vec<Vec<usize>> = vec![Vec::new(); n];
            for (id, block) in self.f.blocks.iter_enumerated() {
                if !self.facts.dom.is_reachable(id.index()) {
                    continue;
                }
                for (j, stmt) in block.stmts.iter().enumerate() {
                    if let Stmt::Assign {
                        value: Rvalue::Alloc { layout, .. },
                        ..
                    } = stmt
                        && reusable_size(self.module, *layout) == Some(size)
                        && !self.taken.contains(&(id, j))
                        && let Some(list) = own.get_mut(id.index())
                    {
                        list.push(j);
                    }
                }
            }
            let mut below: Vec<usize> = own.iter().map(Vec::len).collect();
            // Children before parents.
            for &b in self.order.iter().rev() {
                if let Some(Some(p)) = self.parent.get(b) {
                    let mine = below.get(b).copied().unwrap_or(0);
                    let up = below
                        .get_mut(*p)
                        .ok_or_else(|| Ice::new("reuse: no such parent"))?;
                    *up = up.saturating_add(mine);
                }
            }
            self.sizes.insert(
                size,
                Free {
                    own,
                    used: vec![0; n],
                    below,
                },
            );
        }
        self.sizes
            .get_mut(&size)
            .ok_or_else(|| Ice::new("reuse: no free allocations"))
    }

    /// Routes each open source, the deepest in the tree first (each nearest
    /// its sinks), then by block and the later position first.
    fn route_all(mut self, mut open: Vec<Open>) -> Result<Vec<Pair>, Ice> {
        open.sort_by_key(|o| {
            (
                std::cmp::Reverse(self.depth.get(o.block.index()).copied().unwrap_or(0)),
                o.block,
                std::cmp::Reverse(o.at),
            )
        });
        let mut out = Vec::new();
        for source in open {
            if let Some(pair) = self.route(source)? {
                out.push(pair);
            }
        }
        Ok(out)
    }

    /// The sinks and drops of `source`'s token down the tree below its
    /// block, or `None` when no free allocation of its size is there.
    fn route(&mut self, source: Open) -> Result<Option<Pair>, Ice> {
        let facts = self.facts;
        let start = source.block.index();
        let children: Vec<usize> = facts
            .succs
            .get(start)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&c| self.is_child(start, c))
            .collect();
        let free = self.free(source.size)?;
        if !children
            .iter()
            .any(|&c| free.below.get(c).is_some_and(|&k| k > 0))
        {
            return Ok(None);
        }
        let mut sinks = Vec::new();
        let mut drops = Vec::new();
        // Blocks whose successors receive the token.
        let mut work = vec![start];
        while let Some(b) = work.pop() {
            let from = BlockId::from_usize(b).ok_or_else(|| Ice::new("too many blocks"))?;
            for &c in facts.succs.get(b).into_iter().flatten() {
                let to = BlockId::from_usize(c).ok_or_else(|| Ice::new("too many blocks"))?;
                if !self.is_child(b, c) {
                    drops.push(TokenDrop::Edge(from, to));
                    continue;
                }
                let free = self.free(source.size)?;
                if free.below.get(c).is_none_or(|&k| k == 0) {
                    drops.push(TokenDrop::Start(to));
                } else if let Some(at) = free.take(c) {
                    sinks.push((to, at));
                    self.used_one(source.size, c)?;
                } else {
                    work.push(c);
                }
            }
        }
        Ok(Some(Pair {
            block: source.block,
            source_at: source.at,
            local: source.local,
            layout: source.layout,
            sinks,
            drops,
        }))
    }

    /// One allocation of `size` in block `b` is taken: one fewer free in
    /// `b` and in each block above it.
    fn used_one(&mut self, size: u64, b: usize) -> Result<(), Ice> {
        let mut at = Some(b);
        while let Some(x) = at {
            at = self.parent.get(x).copied().flatten();
            let free = self.free(size)?;
            let slot = free
                .below
                .get_mut(x)
                .ok_or_else(|| Ice::new("reuse: no such block"))?;
            *slot = slot
                .checked_sub(1)
                .ok_or_else(|| Ice::new("reuse: an allocation taken twice"))?;
        }
        Ok(())
    }
}

impl Free {
    /// The first free allocation of block `b`, now taken.
    fn take(&mut self, b: usize) -> Option<usize> {
        let used = self.used.get_mut(b)?;
        let at = *self.own.get(b)?.get(*used)?;
        *used = used.saturating_add(1);
        Some(at)
    }
}

/// The reuse decision of every allocation of `function`, a counted
/// function of `module`, that could reuse (an `alloc` of a constructor
/// with fields or a tuple, or an `alloc_reuse`), in block and statement
/// order, outside cleanup pads (module documentation). Naive counting's
/// MIR has no `alloc_reuse`, so every decision there says why not.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn decisions(module: &MirModule, function: &Function) -> Result<Vec<ReuseDecision>, Ice> {
    let f = function;
    let facts = Facts::of(module, f)?;
    let normal = normally_reachable(f);
    let mut tokens: FxHashMap<Local, Local> = FxHashMap::default();
    let mut reused: BTreeSet<Local> = BTreeSet::new();
    for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
        if let Stmt::Assign {
            dst,
            value: Rvalue::DropReuse { local, .. },
        } = stmt
        {
            tokens.insert(*dst, *local);
            reused.insert(*local);
        }
    }
    let mut out = Vec::new();
    for (id, block) in f.blocks.iter_enumerated() {
        if !normal.get(id.index()).copied().unwrap_or(false) {
            continue;
        }
        for (i, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign { dst, value } = stmt else {
                continue;
            };
            let (layout, token) = match value {
                Rvalue::Alloc { layout, .. } => (*layout, None),
                Rvalue::AllocReuse { layout, token, .. } => (*layout, Some(*token)),
                _ => continue,
            };
            if let Some(token) = token {
                out.push(ReuseDecision {
                    sink: *dst,
                    source: tokens.get(&token).copied(),
                    reason: ReuseReason::Reused,
                });
                continue;
            }
            let Some(size) = reusable_size(module, layout) else {
                continue;
            };
            let (source, reason) = match candidate(f, &facts, id, i, layout, size) {
                None => (None, ReuseReason::NoCandidate),
                Some(x) if is_borrowed_local(&facts.borrowed, x) => {
                    (Some(x), ReuseReason::Borrowed)
                }
                Some(x) if facts.constants.contains(&x) => (Some(x), ReuseReason::Constant),
                Some(x) if facts.filled.contains(&x) || reused.contains(&x) => {
                    (None, ReuseReason::NoCandidate)
                }
                Some(x) => (Some(x), ReuseReason::UsedLater),
            };
            out.push(ReuseDecision {
                sink: *dst,
                source,
                reason,
            });
        }
    }
    Ok(out)
}

/// The object an allocation of `layout` (of `size` bytes) at position `at`
/// of `block` could have reused: among the locals a dominating field read
/// proves to have a layout of that size, one of the same layout first (a
/// record update's base), then one read in the same block, read last, then
/// the first by local number. Looks only at the reads in `block` before
/// `at` and in the blocks that dominate it, walking up the dominator tree
/// (gate 6's ROB6-2).
fn candidate(
    f: &Function,
    facts: &Facts,
    block: BlockId,
    at: usize,
    layout: LayoutRef,
    size: u64,
) -> Option<Local> {
    let mut best: Option<((bool, bool, usize), Local)> = None;
    let mut here = true;
    let mut up = Some(block.index());
    while let Some(d) = up {
        let id = BlockId::from_usize(d)?;
        for &(k, x, l, s) in facts.by_block.get(&id).into_iter().flatten() {
            if here && k >= at {
                break;
            }
            if s != size || f.local(x).map_or(true, |decl| decl.repr != Repr::Value) {
                continue;
            }
            let key = (l == layout, here, if here { k } else { 0 });
            if best.is_none_or(|(b, bx)| key > b || (key == b && x < bx)) {
                best = Some((key, x));
            }
        }
        here = false;
        up = facts.dom.immediate_dominator(d);
    }
    best.map(|(_, x)| x)
}

/// The blocks reached from `bb0` through normal successors only, by block
/// index: every block but the cleanup pads.
fn normally_reachable(f: &Function) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![BlockId::from_u32(0)];
    while let Some(id) = work.pop() {
        let Some(block) = f.blocks.get(id) else {
            continue;
        };
        match seen.get_mut(id.index()) {
            Some(s) if !*s => *s = true,
            _ => continue,
        }
        work.extend(block.term.successors());
    }
    seen
}
