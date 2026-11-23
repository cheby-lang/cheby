//! Lookups whose `Option` is taken apart at once, without the `Some`.
//!
//! A direct call of `list::get` or `map::get` is the kernel's foreign
//! call (`lower/external.rs`), and the kernel allocates a `Some` cell
//! around the element it found. Most programs match that result at once
//! and never use it as a value: `let assert Some(x) = list::get(xs, i)`,
//! `let guard Some(v) = map::get(m, k) else { … }` or a `case` with
//! `Some(…)` and `None` arms. Lowering then reads the result's tag right
//! after the call and switches on it, and reads the element as field 0 of
//! `Some` in the `Some` arm. For such a call this pass tests for the
//! element and reads it without the cell:
//!
//! - `list::get(xs, i)`: the arm is chosen by `0 <= i < length`, with
//!   the length the root holds (`list_length`, read inline, D-910), and the
//!   element is `list_get`'s, dup'd, as a list pattern reads one.
//! - `map::get(m, k)`: `map_find` looks the key up once and gives the
//!   address of the stored value or 0; the arm is chosen by that address,
//!   and the element is `map_value_at`'s, the word at the address, dup'd.
//!
//! Each field read of the `Some` becomes the element's read, so the
//! element's count ends where the cell's `dup` of its field left it. In
//! the `None` arm the result, if anything still uses it (a `let assert`'s
//! failure prints it), is the static `None` that the kernel returns.
//!
//! The rewrite is taken only when the shape is exactly that: the tag is
//! read in the call's block and the block ends in a switch on it; the
//! `Some` and the `None` target are distinct blocks that only this block
//! reaches; every other use of the result is a field read of `Some` in a
//! block the `Some` target dominates, or any use in a block the `None`
//! target dominates. Anything else, a result kept as a value or passed on,
//! keeps the call. The values are the kernel's: a negative index, one past
//! the end and an index no list reaches give `None`, and the key is hashed
//! and compared as the kernel does, so a key that holds a function value
//! panics the same way (D-233). Neither the bounds test nor `list_get` and
//! `map_value_at` can raise, where the kernel could run out of memory for
//! the cell.
//!
//! The pass runs at the end of lowering, before the field reads move
//! (`crate::hoist`, D-911), so every counting mode and borrowing
//! inference see its result. A `precounted` body (D-693) is left alone.
//!
//! The release clean-up takes the `Option` that a direct `map::upsert`
//! gives its callback apart in the same shape, once the callback is
//! inlined ([`fold_upsert`], `crate::simplify`).

use cheby_iface::{Ice, Idx, builtin};

use crate::ir::{
    BlockId, CmpOp, Const, Function, LayoutRef, Local, LocalDecl, Operand, RuntimeFn, Rvalue, Stmt,
    TagType, Terminator,
};
use crate::repr::Repr;
use crate::validate::dom::Dominance;

/// The kernel of `list::get`.
pub const LIST_GET: &str = "cheby_list_get";

/// The kernel of `map::get`.
pub const MAP_GET: &str = "cheby_map_get";

/// Which lookup a call is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    List,
    Map,
}

/// A call the pass rewrites, as the analysis found it.
struct Plan {
    /// The call's result, the `Option`.
    result: Local,
    /// The tag read of the result.
    tag: Local,
    /// The call's block.
    block: BlockId,
    /// The switch's target for `Some`.
    some: BlockId,
    /// The switch's target for `None`.
    none: BlockId,
    kind: Kind,
    /// The call's arguments: the list and the index, or the map and the
    /// key.
    args: Vec<Operand>,
    /// Whether a block the `None` target dominates uses the result.
    none_used: bool,
}

/// Rewrites every lookup of `f` whose `Option` is taken apart at once
/// (module documentation).
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn run(f: &mut Function) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    let calls: Vec<(BlockId, Local, Kind, Vec<Operand>)> = f
        .blocks
        .iter_enumerated()
        .flat_map(|(id, b)| b.stmts.iter().map(move |s| (id, s)))
        .filter_map(|(id, stmt)| {
            let (result, kind, args) = lookup(stmt)?;
            Some((id, result, kind, args))
        })
        .collect();
    if calls.is_empty() {
        return Ok(());
    }
    let graph = Graph::of(f)?;
    let uses = Uses::of(f);
    let mut plans = Vec::new();
    for (block, result, kind, args) in calls {
        if let Some(plan) = plan(f, &graph, &uses, block, result, kind, args)? {
            plans.push(plan);
        }
    }
    for plan in plans {
        apply(f, &plan)?;
    }
    Ok(())
}

/// The result, the kind and the arguments of `stmt` when it is a call of
/// a lookup kernel with a result.
fn lookup(stmt: &Stmt) -> Option<(Local, Kind, Vec<Operand>)> {
    let Stmt::Assign {
        dst,
        value:
            Rvalue::CallForeign {
                symbol,
                args,
                blocking: false,
                ..
            },
    } = stmt
    else {
        return None;
    };
    let kind = match &**symbol {
        LIST_GET => Kind::List,
        MAP_GET => Kind::Map,
        _ => return None,
    };
    (args.len() == 2).then(|| (*dst, kind, args.clone()))
}

/// The blocks' dominance and each block's predecessors.
struct Graph {
    dom: Dominance,
    /// Each block's distinct reachable predecessors.
    preds: Vec<Vec<usize>>,
}

impl Graph {
    fn of(f: &Function) -> Result<Self, Ice> {
        let mut succs: Vec<Vec<usize>> = Vec::with_capacity(f.blocks.len());
        for (id, _) in f.blocks.iter_enumerated() {
            let mut out: Vec<usize> = f.successors(id)?.into_iter().map(Idx::index).collect();
            out.sort_unstable();
            out.dedup();
            succs.push(out);
        }
        let dom = Dominance::from_successors(&succs)?;
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); f.blocks.len()];
        for (b, out) in succs.iter().enumerate() {
            if !dom.is_reachable(b) {
                continue;
            }
            for &s in out {
                if let Some(p) = preds.get_mut(s) {
                    p.push(b);
                }
            }
        }
        Ok(Self { dom, preds })
    }

    /// Whether `target` is reached from `from` alone.
    fn only_from(&self, target: BlockId, from: BlockId) -> bool {
        self.preds
            .get(target.index())
            .is_some_and(|p| p.as_slice() == [from.index()])
    }
}

/// Where a local is used: a statement, or the terminator when the index is
/// the number of statements.
type At = (BlockId, usize);

/// The uses of every local, in block and statement order.
struct Uses {
    by_local: Vec<Vec<At>>,
}

impl Uses {
    fn of(f: &Function) -> Self {
        let mut by_local: Vec<Vec<At>> = vec![Vec::new(); f.locals.len()];
        let mut add = |local: Local, at: At| {
            if let Some(list) = by_local.get_mut(local.index()) {
                list.push(at);
            }
        };
        for (id, block) in f.blocks.iter_enumerated() {
            for (i, stmt) in block.stmts.iter().enumerate() {
                let locals: Vec<Local> = match stmt {
                    Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                        value.uses().into_iter().map(|(l, _)| l).collect()
                    }
                    Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
                    Stmt::DropKeep { local, keep, .. } => std::iter::once(*local)
                        .chain(keep.iter().map(|&(_, p)| p))
                        .collect(),
                    Stmt::ClearUnwind => Vec::new(),
                };
                for l in locals {
                    add(l, (id, i));
                }
            }
            for (l, _) in block.term.uses() {
                add(l, (id, block.stmts.len()));
            }
        }
        Self { by_local }
    }

    fn of_local(&self, local: Local) -> &[At] {
        self.by_local.get(local.index()).map_or(&[], Vec::as_slice)
    }
}

/// The plan for the lookup `result` defined in `block`, or `None` when
/// its result is not only taken apart at once.
fn plan(
    f: &Function,
    graph: &Graph,
    uses: &Uses,
    block: BlockId,
    result: Local,
    kind: Kind,
    args: Vec<Operand>,
) -> Result<Option<Plan>, Ice> {
    let Some(apart) = taken_apart(f, graph, uses, block, result)? else {
        return Ok(None);
    };
    Ok(Some(Plan {
        result,
        tag: apart.tag,
        block,
        some: apart.some,
        none: apart.none,
        kind,
        args,
        none_used: apart.none_used,
    }))
}

/// How an `Option` defined in a block is taken apart at once: the tag read
/// that the block's switch reads, the switch's targets for `Some` and
/// `None`, and whether a block the `None` target dominates uses it.
struct Apart {
    tag: Local,
    some: BlockId,
    none: BlockId,
    none_used: bool,
}

/// How the `Option` `result`, defined in `block`, is taken apart there,
/// or `None` when it is not only taken apart at once (module
/// documentation).
fn taken_apart(
    f: &Function,
    graph: &Graph,
    uses: &Uses,
    block: BlockId,
    result: Local,
) -> Result<Option<Apart>, Ice> {
    let b = f.block(block)?;
    let Terminator::Switch {
        on: Operand::Local(tag),
        cases,
        default,
    } = &b.term
    else {
        return Ok(None);
    };
    let tag = *tag;
    // The tag is read in this block, and only the switch reads it.
    let reads_tag = |at: &At| {
        at.0 == block
            && matches!(
                b.stmts.get(at.1),
                Some(Stmt::Assign {
                    dst,
                    value: Rvalue::ReadTag {
                        base: Operand::Local(base),
                        ty: TagType::Option,
                    },
                }) if *dst == tag && *base == result
            )
    };
    if uses.of_local(tag) != [(block, b.stmts.len())] {
        return Ok(None);
    }
    // `Option`'s tags: `Some` 0, `None` 1 (D-522).
    let target = |bits: u64| {
        cases
            .iter()
            .find(|&&(c, _)| c == bits)
            .map(|&(_, t)| t)
            .or(*default)
    };
    let (Some(some), Some(none)) = (target(0), target(1)) else {
        return Ok(None);
    };
    if some == none
        || cases.iter().any(|&(c, _)| c > 1)
        || !graph.only_from(some, block)
        || !graph.only_from(none, block)
    {
        return Ok(None);
    }
    let mut none_used = false;
    let mut tag_reads = 0_usize;
    for at in uses.of_local(result) {
        if reads_tag(at) {
            tag_reads = tag_reads.saturating_add(1);
            continue;
        }
        let (u, i) = *at;
        if graph.dom.dominates(some.index(), u.index()) {
            if !is_some_field(f.block(u)?.stmts.get(i), result) {
                return Ok(None);
            }
        } else if graph.dom.dominates(none.index(), u.index()) {
            none_used = true;
        } else {
            return Ok(None);
        }
    }
    if tag_reads != 1 {
        return Ok(None);
    }
    Ok(Some(Apart {
        tag,
        some,
        none,
        none_used,
    }))
}

/// Whether `stmt` reads field 0 of `result` as a `Some`.
fn is_some_field(stmt: Option<&Stmt>, result: Local) -> bool {
    matches!(
        stmt,
        Some(Stmt::Assign {
            value: Rvalue::Field {
                base,
                layout: LayoutRef::Ctor(ctor),
                field: 0,
            },
            ..
        }) if *base == result && *ctor == builtin::SOME
    )
}

/// A new local of `repr` without a name or a type.
fn temp(f: &mut Function, repr: Repr) -> Result<Local, Ice> {
    f.locals.push(LocalDecl {
        repr,
        name: None,
        ty: None,
    })
}

/// The test that replaces `plan`'s call, the local it defines as its
/// answer, and the element's read that replaces each field read of the
/// `Some`.
fn test_and_element(f: &mut Function, plan: &Plan) -> Result<(Local, Vec<Stmt>, Rvalue), Ice> {
    let missing = || Ice::new("lookup: the planned call is gone");
    Ok(match plan.kind {
        Kind::List => {
            let [list, index] = plan.args.as_slice() else {
                return Err(missing());
            };
            let len = temp(f, Repr::Int)?;
            let from_start = temp(f, Repr::Bool)?;
            let below_end = temp(f, Repr::Bool)?;
            let found = temp(f, Repr::Bool)?;
            // `0 <= i` and `i < length` cannot both fail, since the length
            // is never negative: the two tests are equal exactly when both
            // hold.
            let test = vec![
                assign(
                    len,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::ListLength,
                        args: vec![list.clone()],
                    },
                ),
                assign(
                    from_start,
                    compare(
                        CmpOp::Ge,
                        index.clone(),
                        Operand::Const(Const::Int(0)),
                        Repr::Int,
                    ),
                ),
                assign(
                    below_end,
                    compare(CmpOp::Lt, index.clone(), Operand::Local(len), Repr::Int),
                ),
                assign(
                    found,
                    compare(
                        CmpOp::Eq,
                        Operand::Local(from_start),
                        Operand::Local(below_end),
                        Repr::Bool,
                    ),
                ),
            ];
            let element = Rvalue::CallRuntime {
                func: RuntimeFn::ListGet,
                args: vec![list.clone(), index.clone()],
            };
            (found, test, element)
        }
        Kind::Map => {
            let [map, key] = plan.args.as_slice() else {
                return Err(missing());
            };
            let address = temp(f, Repr::U64)?;
            let found = temp(f, Repr::Bool)?;
            let test = vec![
                assign(
                    address,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::MapFind,
                        args: vec![map.clone(), key.clone()],
                    },
                ),
                assign(
                    found,
                    compare(
                        CmpOp::Ne,
                        Operand::Local(address),
                        Operand::Const(Const::Sized {
                            repr: Repr::U64,
                            bits: 0,
                        }),
                        Repr::U64,
                    ),
                ),
            ];
            let element = Rvalue::CallRuntime {
                func: RuntimeFn::MapValueAt,
                args: vec![map.clone(), Operand::Local(address)],
            };
            (found, test, element)
        }
    })
}

/// Carries out `plan` (module documentation).
fn apply(f: &mut Function, plan: &Plan) -> Result<(), Ice> {
    let missing = || Ice::new("lookup: the planned call is gone");
    let (found, test, element) = test_and_element(f, plan)?;
    let block = f
        .blocks
        .get_mut(plan.block)
        .ok_or_else(|| Ice::new("lookup: no such block"))?;
    let defines =
        |dst: Local| move |s: &Stmt| matches!(s, Stmt::Assign { dst: d, .. } if *d == dst);
    let at = block
        .stmts
        .iter()
        .position(defines(plan.result))
        .ok_or_else(missing)?;
    let pos = block.pos_at(at);
    block.remove_stmt(at);
    block.insert_stmts(at, test, pos);
    let tag_at = block
        .stmts
        .iter()
        .position(defines(plan.tag))
        .ok_or_else(missing)?;
    block.remove_stmt(tag_at);
    block.term = Terminator::Branch {
        cond: Operand::Local(found),
        then_: plan.some,
        else_: plan.none,
    };
    // Each read of the `Some`'s field is a read of the element.
    for b in &mut f.blocks {
        for stmt in &mut b.stmts {
            if let Stmt::Assign { value, .. } = stmt
                && is_some_field_value(value, plan.result)
            {
                *value = element.clone();
            }
        }
    }
    if plan.none_used {
        let none = f
            .blocks
            .get_mut(plan.none)
            .ok_or_else(|| Ice::new("lookup: no such block"))?;
        let pos = none.first_pos();
        none.insert_stmts(
            0,
            [assign(
                plan.result,
                Rvalue::Use(Operand::Const(Const::StaticCtor(builtin::NONE))),
            )],
            pos,
        );
    }
    // The result is no call's any more (D-852).
    f.sites.retain(|&(l, _)| l != plan.result);
    Ok(())
}

/// Whether `value` reads field 0 of `result` as a `Some`.
fn is_some_field_value(value: &Rvalue, result: Local) -> bool {
    matches!(
        value,
        Rvalue::Field {
            base,
            layout: LayoutRef::Ctor(ctor),
            field: 0,
        } if *base == result && *ctor == builtin::SOME
    )
}

fn assign(dst: Local, value: Rvalue) -> Stmt {
    Stmt::Assign { dst, value }
}

fn compare(op: CmpOp, lhs: Operand, rhs: Operand, repr: Repr) -> Rvalue {
    Rvalue::Compare { op, lhs, rhs, repr }
}

/// The release clean-up's rewrite of the `Option` that a direct
/// `map::upsert` gives its callback (`lower/upsert.rs`), once the inlined
/// callback only takes it apart at once, in the shape of the module
/// documentation: `map_upsert_found` goes, the arm is chosen by the
/// address's lowest bit, which is 0 for a present key, the `Some` arm
/// starts by moving the stored value out with `map_slot_take`, and each
/// read of the `Some`'s field is that value. In the `None` arm the
/// `Option`, if anything still uses it, is the static `None`. So nothing
/// is allocated. The value is taken where the `Some` arm starts, right
/// after the switch, where the `Some` would have released its reference
/// when no read follows, so no release moves past anything a program sees
/// (D-205). Returns whether anything changed.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub(crate) fn fold_upsert(f: &mut Function) -> Result<bool, Ice> {
    if f.precounted {
        return Ok(false);
    }
    let calls: Vec<(BlockId, Local, Local, Local)> = f
        .blocks
        .iter_enumerated()
        .flat_map(|(id, b)| b.stmts.iter().map(move |s| (id, s)))
        .filter_map(|(id, stmt)| {
            let Stmt::Assign {
                dst,
                value:
                    Rvalue::CallRuntime {
                        func: RuntimeFn::MapUpsertFound,
                        args,
                    },
            } = stmt
            else {
                return None;
            };
            match args.as_slice() {
                [Operand::Local(map), Operand::Local(address)] => Some((id, *dst, *map, *address)),
                _ => None,
            }
        })
        .collect();
    if calls.is_empty() {
        return Ok(false);
    }
    let graph = Graph::of(f)?;
    let uses = Uses::of(f);
    let mut plans = Vec::new();
    for (block, result, map, address) in calls {
        if let Some(apart) = taken_apart(f, &graph, &uses, block, result)? {
            plans.push((block, result, map, address, apart));
        }
    }
    let changed = !plans.is_empty();
    for (block, result, map, address, apart) in plans {
        apply_upsert(f, block, result, map, address, &apart)?;
    }
    Ok(changed)
}

/// Carries out [`fold_upsert`] for the `Option` `result` of the upsert's
/// `map` and `address`, defined in `block` and taken apart as `apart`.
fn apply_upsert(
    f: &mut Function,
    block: BlockId,
    result: Local,
    map: Local,
    address: Local,
    apart: &Apart,
) -> Result<(), Ice> {
    let missing = || Ice::new("lookup: the planned upsert is gone");
    let low = temp(f, Repr::U64)?;
    let present = temp(f, Repr::Bool)?;
    let value = temp(f, Repr::Value)?;
    let u64_const = |bits| {
        Operand::Const(Const::Sized {
            repr: Repr::U64,
            bits,
        })
    };
    let test = vec![
        assign(
            low,
            Rvalue::Bitwise {
                op: crate::ir::BitOp::And,
                lhs: Operand::Local(address),
                rhs: u64_const(1),
                repr: Repr::U64,
            },
        ),
        assign(
            present,
            compare(CmpOp::Eq, Operand::Local(low), u64_const(0), Repr::U64),
        ),
    ];
    let b = f
        .blocks
        .get_mut(block)
        .ok_or_else(|| Ice::new("lookup: no such block"))?;
    let defines =
        |dst: Local| move |s: &Stmt| matches!(s, Stmt::Assign { dst: d, .. } if *d == dst);
    let at = b
        .stmts
        .iter()
        .position(defines(result))
        .ok_or_else(missing)?;
    let pos = b.pos_at(at);
    b.remove_stmt(at);
    b.insert_stmts(at, test, pos);
    let tag_at = b
        .stmts
        .iter()
        .position(defines(apart.tag))
        .ok_or_else(missing)?;
    b.remove_stmt(tag_at);
    b.term = Terminator::Branch {
        cond: Operand::Local(present),
        then_: apart.some,
        else_: apart.none,
    };
    let some = f
        .blocks
        .get_mut(apart.some)
        .ok_or_else(|| Ice::new("lookup: no such block"))?;
    let pos = some.first_pos();
    some.insert_stmts(
        0,
        [assign(
            value,
            Rvalue::CallRuntime {
                func: RuntimeFn::MapSlotTake,
                args: vec![Operand::Local(map), Operand::Local(address)],
            },
        )],
        pos,
    );
    // Each read of the `Some`'s field is the value taken.
    for b in &mut f.blocks {
        for stmt in &mut b.stmts {
            if let Stmt::Assign { value: v, .. } = stmt
                && is_some_field_value(v, result)
            {
                *v = Rvalue::Use(Operand::Local(value));
            }
        }
    }
    if apart.none_used {
        let none = f
            .blocks
            .get_mut(apart.none)
            .ok_or_else(|| Ice::new("lookup: no such block"))?;
        let pos = none.first_pos();
        none.insert_stmts(
            0,
            [assign(
                result,
                Rvalue::Use(Operand::Const(Const::StaticCtor(builtin::NONE))),
            )],
            pos,
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
