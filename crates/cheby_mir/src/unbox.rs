//! Raw parameters and results of clones (design T14), in the release
//! profile's `unbox` lever, after specialization ([`crate::specialize`]).
//!
//! A clone is private to its unit and called only through
//! [`crate::ir::Callee::Code`], so every call of it is known, and its
//! signature is its own (D-453 allows such an entry). So:
//!
//! - a uniform parameter becomes raw in one representation `R` when the
//!   clone unboxes it to `R` and otherwise only passes it to itself, at a
//!   position that becomes raw in `R` too, or returns it. The clone boxes
//!   the raw value at its entry where a uniform one is still read, and each
//!   call unboxes its argument; the clean-up then cancels the pairs, so a
//!   loop that carries a `Float` boxes nothing per step;
//! - the result becomes raw in `R` (`Int` or `Float`) when every value
//!   the clone returns is a box of `R`, and every tail call it makes is a
//!   call of itself. Each return unboxes, and each call boxes the raw
//!   result where a uniform one is used; a tail call of the clone from
//!   another function becomes a call, a box and a return.
//!
//! Every value that reaches such a parameter has the representation the
//! clone unboxes it to: the unbox is the code of the type the clone was
//! made for, a dictionary's or a closure's, and every call of one clone
//! has the same shapes ([`crate::specialize`]). A box of a raw value is
//! no handle and is never observable as an object (D-278), so no release
//! a program can see moves (D-101, D-205). Code generation tests the
//! unwind flag after a call with a raw result (D-734).

use cheby_iface::{Ice, Idx};
use rustc_hash::FxHashMap;

use crate::ir::{
    Block, Callee, FnKey, Function, Local, LocalDecl, MirModule, Operand, Rvalue, Stmt, Terminator,
};
use crate::repr::Repr;

/// Whether `r` is a representation a clone's result may take.
const fn raw_result(r: Repr) -> bool {
    matches!(r, Repr::Int | Repr::Float)
}

/// The positions of the calls of `key` in `f`: statement `i` of a block,
/// or its terminator (`None`).
fn calls_of(f: &Function, key: FnKey) -> Vec<(crate::ir::BlockId, Option<usize>)> {
    let mut out = Vec::new();
    for (id, b) in f.blocks.iter_enumerated() {
        for (i, s) in b.stmts.iter().enumerate() {
            if let Stmt::Assign {
                value:
                    Rvalue::Call {
                        callee: Callee::Code(k),
                        ..
                    },
                ..
            } = s
                && *k == key
            {
                out.push((id, Some(i)));
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Code(k),
            ..
        } = &b.term
            && *k == key
        {
            out.push((id, None));
        }
    }
    out
}

/// The parameters of `f` that may become raw, by position, with their
/// representation (module docs).
#[expect(
    clippy::too_many_lines,
    reason = "each kind of use of a parameter, then the fixed point"
)]
fn raw_params(f: &Function) -> Result<FxHashMap<usize, Repr>, Ice> {
    // Each parameter's unboxes, the self-call positions it is passed at,
    // and whether it has any other use.
    let n = f.params.len();
    let mut to: Vec<Option<Repr>> = vec![None; n];
    let mut mixed = vec![false; n];
    let mut passed: Vec<Vec<usize>> = vec![Vec::new(); n];
    let position = |l: Local| f.params.iter().position(|&p| p == l);
    let mut other = vec![false; n];
    for (k, &p) in f.params.iter().enumerate() {
        if f.local(p)?.repr != Repr::Value
            && let Some(o) = other.get_mut(k)
        {
            *o = true;
        }
    }
    let use_other = |other: &mut Vec<bool>, l: Local| {
        if let Some(k) = position(l)
            && let Some(o) = other.get_mut(k)
        {
            *o = true;
        }
    };
    let self_call = |c: &Callee| matches!(c, Callee::Code(k) if *k == f.key);
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Assign {
                    value:
                        Rvalue::Unbox {
                            value: Operand::Local(l),
                            to: r,
                        },
                    ..
                } => {
                    if let Some(k) = position(*l) {
                        match to.get(k).copied().flatten() {
                            None => {
                                if let Some(t) = to.get_mut(k) {
                                    *t = Some(*r);
                                }
                            }
                            Some(seen) if seen == *r => {}
                            Some(_) => {
                                if let Some(m) = mixed.get_mut(k) {
                                    *m = true;
                                }
                            }
                        }
                    }
                }
                Stmt::Assign {
                    value: Rvalue::Call { callee, args },
                    ..
                } if self_call(callee) => {
                    for (j, a) in args.iter().enumerate() {
                        if let Operand::Local(l) = a
                            && let Some(k) = position(*l)
                            && let Some(list) = passed.get_mut(k)
                        {
                            list.push(j);
                        }
                    }
                }
                Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                    for (l, _) in value.uses() {
                        use_other(&mut other, l);
                    }
                }
                Stmt::Dup(l) | Stmt::Drop(l) => use_other(&mut other, *l),
                Stmt::DropKeep { local, .. } => use_other(&mut other, *local),
                Stmt::ClearUnwind => {}
            }
        }
        match &b.term {
            Terminator::TailCall { callee, args } if self_call(callee) => {
                for (j, a) in args.iter().enumerate() {
                    if let Operand::Local(l) = a
                        && let Some(k) = position(*l)
                        && let Some(list) = passed.get_mut(k)
                    {
                        list.push(j);
                    }
                }
            }
            Terminator::Return(Operand::Local(_)) => {}
            term @ (Terminator::Return(_)
            | Terminator::TailCall { .. }
            | Terminator::Goto { .. }
            | Terminator::Branch { .. }
            | Terminator::Switch { .. }
            | Terminator::Panic { .. }
            | Terminator::Unreachable
            | Terminator::Resume) => {
                for (l, _) in term.uses() {
                    use_other(&mut other, l);
                }
            }
        }
    }
    let mut out: FxHashMap<usize, Repr> = FxHashMap::default();
    for k in 0..n {
        if let (Some(Some(r)), Some(false), Some(false)) = (to.get(k), mixed.get(k), other.get(k)) {
            out.insert(k, *r);
        }
    }
    // A parameter passed to itself at a position that does not become raw
    // in the same representation stays uniform, until nothing changes.
    loop {
        let drop: Vec<usize> = out
            .iter()
            .filter(|&(&k, &r)| {
                passed
                    .get(k)
                    .is_some_and(|js| js.iter().any(|j| out.get(j) != Some(&r)))
            })
            .map(|(&k, _)| k)
            .collect();
        if drop.is_empty() {
            break;
        }
        for k in drop {
            out.remove(&k);
        }
    }
    Ok(out)
}

/// The representation the result of `f` may take (module docs), its raw
/// parameters `raw` given.
fn raw_ret(f: &Function, raw: &FxHashMap<usize, Repr>) -> Option<Repr> {
    // What each local is known to be a box of.
    let mut boxed: FxHashMap<Local, Repr> = FxHashMap::default();
    for (k, &p) in f.params.iter().enumerate() {
        if let Some(&r) = raw.get(&k) {
            boxed.insert(p, r);
        }
    }
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Assign {
                    dst,
                    value: Rvalue::Box { from, .. },
                } => {
                    boxed.insert(*dst, *from);
                }
                Stmt::Assign {
                    dst,
                    value: Rvalue::Use(Operand::Local(l)),
                } => {
                    if let Some(&r) = boxed.get(l) {
                        boxed.insert(*dst, r);
                    }
                }
                Stmt::Assign { .. }
                | Stmt::Eval(_)
                | Stmt::Dup(_)
                | Stmt::Drop(_)
                | Stmt::DropKeep { .. }
                | Stmt::ClearUnwind => {}
            }
        }
    }
    // Block parameters every jump fills with boxes of one representation,
    // to a fixed point.
    // By local, so the walk below goes in one order.
    let mut preds: std::collections::BTreeMap<Local, Vec<Operand>> =
        std::collections::BTreeMap::new();
    for b in &f.blocks {
        if let Terminator::Goto { target, args } = &b.term
            && let Ok(t) = f.block(*target)
        {
            for (p, a) in t.params.iter().zip(args) {
                preds.entry(*p).or_default().push(a.clone());
            }
        }
    }
    for _ in 0..f.blocks.len().min(16) {
        let mut changed = false;
        for (p, args) in &preds {
            if boxed.contains_key(p) {
                continue;
            }
            let mut seen = None;
            let all = args.iter().all(|a| match a {
                Operand::Local(l) => match (boxed.get(l), seen) {
                    (Some(&r), None) => {
                        seen = Some(r);
                        true
                    }
                    (Some(&r), Some(s)) => r == s,
                    (None, _) => false,
                },
                Operand::Const(_) => false,
            });
            if all && let Some(r) = seen {
                boxed.insert(*p, r);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut ret = None;
    for b in &f.blocks {
        match &b.term {
            Terminator::Return(Operand::Local(l)) => {
                let r = *boxed.get(l)?;
                match ret {
                    None => ret = Some(r),
                    Some(seen) if seen == r => {}
                    Some(_) => return None,
                }
            }
            Terminator::Return(Operand::Const(_)) => return None,
            Terminator::TailCall { callee, .. } => {
                if !matches!(callee, Callee::Code(k) if *k == f.key) {
                    return None;
                }
            }
            Terminator::Goto { .. }
            | Terminator::Branch { .. }
            | Terminator::Switch { .. }
            | Terminator::Panic { .. }
            | Terminator::Unreachable
            | Terminator::Resume => {}
        }
    }
    ret.filter(|&r| raw_result(r))
}

/// Makes the parameters and results of the clones `keys` of `unit` raw
/// where they may be (module docs), in the order of `keys`. Returns the
/// indexes of the functions that changed, sorted, for the clean-up to
/// cancel the boxes and unboxes it put in.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub(crate) fn run(unit: &mut MirModule, keys: &[FnKey]) -> Result<Vec<usize>, Ice> {
    let index = unit.function_index();
    let mut changed: Vec<usize> = Vec::new();
    for &key in keys {
        let Some(&at) = index.get(&key) else {
            continue;
        };
        let f = unit
            .functions
            .get(at)
            .ok_or_else(|| Ice::new("unbox: a clone"))?;
        let params = raw_params(f)?;
        let ret = raw_ret(f, &params);
        if params.is_empty() && ret.is_none() {
            continue;
        }
        let mut raw: Vec<(usize, Repr)> = params.into_iter().collect();
        raw.sort_unstable_by_key(|&(k, _)| k);
        let f = unit
            .functions
            .get_mut(at)
            .ok_or_else(|| Ice::new("unbox: a clone"))?;
        let mut entry = Vec::with_capacity(raw.len());
        for &(k, r) in &raw {
            let p = *f
                .params
                .get(k)
                .ok_or_else(|| Ice::new("unbox: a parameter"))?;
            let name = f.local(p)?.name;
            let new = f.locals.push(LocalDecl {
                repr: r,
                name,
                ty: None,
            })?;
            if let Some(slot) = f.params.get_mut(k) {
                *slot = new;
            }
            entry.push(Stmt::Assign {
                dst: p,
                value: Rvalue::Box {
                    value: Operand::Local(new),
                    from: r,
                },
            });
        }
        if let Some(b) = f.blocks.get_mut(crate::ir::BlockId::from_u32(0)) {
            let pos = b.pos_at(0);
            b.insert_stmts(0, entry, pos);
        }
        if let Some(r) = ret {
            f.ret = r;
            unbox_returns(f, r)?;
        }
        for (n, g) in unit.functions.iter_mut().enumerate() {
            let sites = calls_of(g, key);
            if sites.is_empty() {
                continue;
            }
            rewrite_calls(g, key, &raw, ret, &sites)?;
            changed.push(n);
        }
        changed.push(at);
    }
    changed.sort_unstable();
    changed.dedup();
    Ok(changed)
}

/// Each `return op` of `f` returns `op` unboxed to `r`.
fn unbox_returns(func: &mut Function, repr: Repr) -> Result<(), Ice> {
    let (f, r) = (func, repr);
    for index in 0..f.blocks.len() {
        let Some(id) = crate::ir::BlockId::from_usize(index) else {
            break;
        };
        let Some(Terminator::Return(op)) = f.blocks.get(id).map(|b| b.term.clone()) else {
            continue;
        };
        let u = f.locals.push(LocalDecl {
            repr: r,
            name: None,
            ty: None,
        })?;
        let Some(b) = f.blocks.get_mut(id) else {
            continue;
        };
        let pos = b.term_pos();
        let at = b.stmts.len();
        b.insert_stmts(
            at,
            [Stmt::Assign {
                dst: u,
                value: Rvalue::Unbox { value: op, to: r },
            }],
            pos,
        );
        b.term = Terminator::Return(Operand::Local(u));
    }
    Ok(())
}

/// The calls `sites` of the clone `key` in `g`: each raw parameter's
/// argument unboxed first, and with a raw result `ret`, the result boxed
/// after; a tail call of the clone from another function becomes a call,
/// a box and a return.
#[expect(
    clippy::too_many_lines,
    reason = "a call in a statement, a tail call that stays and one that does not"
)]
fn rewrite_calls(
    g: &mut Function,
    key: FnKey,
    raw: &[(usize, Repr)],
    ret: Option<Repr>,
    sites: &[(crate::ir::BlockId, Option<usize>)],
) -> Result<(), Ice> {
    let own = g.key == key;
    for &(id, at) in sites.iter().rev() {
        let mut args = {
            let b = g.block(id)?;
            match at {
                Some(i) => match b.stmts.get(i) {
                    Some(Stmt::Assign {
                        value: Rvalue::Call { args, .. },
                        ..
                    }) => args.clone(),
                    Some(_) | None => continue,
                },
                None => match &b.term {
                    Terminator::TailCall { args, .. } => args.clone(),
                    Terminator::Return(_)
                    | Terminator::Goto { .. }
                    | Terminator::Branch { .. }
                    | Terminator::Switch { .. }
                    | Terminator::Panic { .. }
                    | Terminator::Unreachable
                    | Terminator::Resume => continue,
                },
            }
        };
        let mut before = Vec::new();
        for &(k, r) in raw {
            let Some(slot) = args.get_mut(k) else {
                continue;
            };
            let u = g.locals.push(LocalDecl {
                repr: r,
                name: None,
                ty: None,
            })?;
            before.push(Stmt::Assign {
                dst: u,
                value: Rvalue::Unbox {
                    value: slot.clone(),
                    to: r,
                },
            });
            *slot = Operand::Local(u);
        }
        match (at, ret) {
            (Some(i), _) => {
                let result = match ret {
                    Some(r) => Some((
                        g.locals.push(LocalDecl {
                            repr: r,
                            name: None,
                            ty: None,
                        })?,
                        r,
                    )),
                    None => None,
                };
                let mut renamed: Option<(Local, Local)> = None;
                let b: &mut Block = g
                    .blocks
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("unbox: a call's block"))?;
                let pos = b.pos_at(i);
                let mut after = Vec::new();
                if let Some(Stmt::Assign {
                    dst,
                    value: Rvalue::Call { args: a, .. },
                }) = b.stmts.get_mut(i)
                {
                    *a = args;
                    if let Some((res, r)) = result {
                        after.push(Stmt::Assign {
                            dst: *dst,
                            value: Rvalue::Box {
                                value: Operand::Local(res),
                                from: r,
                            },
                        });
                        renamed = Some((*dst, res));
                        *dst = res;
                    }
                }
                b.insert_stmts(i.saturating_add(1), after, pos);
                b.insert_stmts(i, before, pos);
                // The call's location follows the local the call defines.
                if let Some((old, new)) = renamed {
                    for entry in &mut g.call_locs {
                        if entry.0 == old {
                            entry.0 = new;
                        }
                    }
                    g.call_locs.sort_by_key(|&(l, _)| l);
                }
            }
            // A self tail call returns the raw result itself, and any tail
            // call a uniform one.
            (None, None) => {
                let b = g
                    .blocks
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("unbox: a call's block"))?;
                let end = b.stmts.len();
                let pos = b.term_pos();
                if let Terminator::TailCall { args: a, .. } = &mut b.term {
                    *a = args;
                }
                b.insert_stmts(end, before, pos);
            }
            (None, Some(_)) if own => {
                let b = g
                    .blocks
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("unbox: a call's block"))?;
                let end = b.stmts.len();
                let pos = b.term_pos();
                if let Terminator::TailCall { args: a, .. } = &mut b.term {
                    *a = args;
                }
                b.insert_stmts(end, before, pos);
            }
            (None, Some(r)) => {
                let res = g.locals.push(LocalDecl {
                    repr: r,
                    name: None,
                    ty: None,
                })?;
                let boxed = g.locals.push(LocalDecl {
                    repr: Repr::Value,
                    name: None,
                    ty: None,
                })?;
                let b = g
                    .blocks
                    .get_mut(id)
                    .ok_or_else(|| Ice::new("unbox: a call's block"))?;
                let end = b.stmts.len();
                let pos = b.term_pos();
                before.push(Stmt::Assign {
                    dst: res,
                    value: Rvalue::Call {
                        callee: Callee::Code(key),
                        args,
                    },
                });
                before.push(Stmt::Assign {
                    dst: boxed,
                    value: Rvalue::Box {
                        value: Operand::Local(res),
                        from: r,
                    },
                });
                b.insert_stmts(end, before, pos);
                b.term = Terminator::Return(Operand::Local(boxed));
            }
        }
    }
    Ok(())
}
