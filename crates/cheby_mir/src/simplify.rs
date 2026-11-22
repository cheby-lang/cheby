//! The clean-up after inlining in the release profile (design T7), on
//! lowered MIR ([`crate::Stage::Lowered`]) before borrowing inference and
//! counting, one function at a time ([`run`]):
//!
//! - `unbox` of a `box` of the same representation is the boxed value;
//! - a block parameter that every jump fills with a `box` of one
//!   representation, and that only `unbox`es to it read, takes the raw
//!   value instead: what an inlined `Float` helper's result needs;
//! - a block parameter that every jump fills with the same value is that
//!   value, and a block whose only predecessor jumps to it joins that
//!   predecessor;
//! - comparisons, `not`, checked arithmetic that cannot panic and tag reads
//!   of constants fold, and so do branches and switches on constants;
//! - a jump with a static constructor into a block that only reads its
//!   tag and switches on it goes to the case directly;
//! - unreachable blocks, unused block parameters and unused definitions
//!   of pure values go.
//!
//! A value is pure when computing it can neither panic nor have an effect:
//! a move, a box or unbox, a field or layout read, a constant's slot, a
//! comparison, float or bit arithmetic, a tag read, an allocation. Checked
//! arithmetic that may panic, and every call, stay, so a program panics at
//! the same places. Moves of counted locals are never propagated, so a
//! direct kernel call still gets its owned temporaries (D-525).
//!
//! No rewrite moves the release of a value that may hold a handle, whose
//! drop function the program can see (D-101, D-205): a definition that
//! uses such a value stays even when its result is unused (the move that
//! keeps an `@external` wrapper's handle alive across its foreign call is
//! one), a parameter whose argument may hold one stays, and a branch folds
//! or a jump is threaded only when no such value live into a dropped
//! target is dead in the taken one. Raw values, boxes of raw values and
//! constants hold no handle.
//!
//! Counting runs after this pass, so removing a definition or a parameter
//! needs no count of its own. Every rewrite keeps the statements' source
//! positions (D-938) and the call locations (D-856), and the function
//! stays lowered MIR. The pass is deterministic: it walks blocks and
//! statements in order and its maps are only looked up.

use cheby_iface::{DefId, Ice, Idx, IndexVec, builtin};
use rustc_hash::FxHashMap;

use crate::ir::{
    ArithOp, Block, BlockId, Callee, CmpOp, Const, Function, LayoutRef, Local, LocalDecl,
    MirModule, Operand, Rvalue, Stmt, TagType, Terminator,
};
use crate::liveness::Liveness;
use crate::positions::PosRef;
use crate::repr::Repr;

/// The most rounds of rewrites per function: each round can enable the
/// next, and a few reach the fixed point of an inlined body.
const ROUNDS: usize = 8;

/// The tags of the constructors a unit knows ([`MirModule::layouts`] and
/// [`MirModule::foreign_layouts`]), with their types.
#[derive(Clone, Debug, Default)]
pub struct Tags(FxHashMap<DefId, (DefId, u32)>);

impl Tags {
    /// The constructors of `unit`'s layouts.
    #[must_use]
    pub fn of(unit: &MirModule) -> Self {
        let mut out = FxHashMap::default();
        for d in unit.layouts.iter().chain(&unit.foreign_layouts) {
            if let LayoutRef::Ctor(def) = d.layout {
                out.insert(def, (d.type_def, d.tag));
            }
        }
        Self(out)
    }

    /// Adds the constructors of `layouts`, layouts of other modules that a
    /// body from another unit brought in ([`crate::inline::optimize_across`]).
    pub fn extend(&mut self, layouts: &[crate::layout::LayoutDesc]) {
        for d in layouts {
            if let LayoutRef::Ctor(def) = d.layout {
                self.0.insert(def, (d.type_def, d.tag));
            }
        }
    }

    /// The tag a [`Rvalue::ReadTag`] of `ty` reads from the static object
    /// of constructor `def`: the declaration order of its type's
    /// constructors (D-522), the runtime's layout id less the first one's
    /// for the builtin types.
    fn tag(&self, def: DefId, ty: TagType) -> Option<u32> {
        let builtin: &[DefId] = match ty {
            TagType::Adt(want) => {
                let &(type_def, tag) = self.0.get(&def)?;
                return (want == type_def).then_some(tag);
            }
            TagType::Option => &[builtin::SOME, builtin::NONE],
            TagType::Result => &[builtin::OK, builtin::ERR],
            TagType::Order => &[builtin::LESS, builtin::EQUAL, builtin::GREATER],
        };
        let at = builtin.iter().position(|&d| d == def)?;
        u32::try_from(at).ok()
    }
}

/// Cleans up `f` (module docs); returns whether it changed. A
/// `precounted` body or one with unwind edges is left alone.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn run(f: &mut Function, tags: &Tags) -> Result<bool, Ice> {
    if f.precounted || f.entry_unwind.is_some() || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Ok(false);
    }
    let mut changed = false;
    for _ in 0..ROUNDS {
        let mut round = fold(f, tags);
        round |= same_params(f);
        round |= raw_params(f)?;
        round |= thread(f, tags);
        round |= drop_unreachable(f)?;
        round |= merge(f);
        round |= dead(f);
        if !round {
            break;
        }
        changed = true;
    }
    if changed {
        drop_unreachable(f)?;
        tidy(f);
    }
    Ok(changed)
}

/// Calls `op` on every operand of `value` and `local` on every local where
/// only a local may stand.
fn rvalue_mut(
    value: &mut Rvalue,
    op: &mut dyn FnMut(&mut Operand),
    local: &mut dyn FnMut(&mut Local),
) {
    match value {
        Rvalue::Use(o)
        | Rvalue::Box { value: o, .. }
        | Rvalue::Unbox { value: o, .. }
        | Rvalue::FloatNeg { value: o, .. }
        | Rvalue::Convert { value: o, .. }
        | Rvalue::FloatSqrt { value: o, .. }
        | Rvalue::Not(o)
        | Rvalue::ReadTag { base: o, .. } => op(o),
        Rvalue::Call { callee, args } => {
            if let Callee::Indirect(l) = callee {
                local(l);
            }
            args.iter_mut().for_each(&mut *op);
        }
        Rvalue::CallForeign { args, .. } | Rvalue::CallRuntime { args, .. } => {
            args.iter_mut().for_each(&mut *op);
        }
        Rvalue::Alloc { fields, .. } => fields.iter_mut().for_each(&mut *op),
        Rvalue::AllocPlaceholder { .. } | Rvalue::LoadConst(_) => {}
        Rvalue::InitField { obj, value, .. } => {
            local(obj);
            op(value);
        }
        Rvalue::Field { base, .. } | Rvalue::ReadLayout { base } => local(base),
        Rvalue::Checked { lhs, rhs, .. }
        | Rvalue::Compare { lhs, rhs, .. }
        | Rvalue::FloatArith { lhs, rhs, .. }
        | Rvalue::Bitwise { lhs, rhs, .. }
        | Rvalue::Wrapping { lhs, rhs, .. } => {
            op(lhs);
            op(rhs);
        }
        Rvalue::DropReuse { local: l, keep, .. } => {
            local(l);
            for (_, p) in keep {
                local(p);
            }
        }
        Rvalue::AllocReuse { token, fields, .. } => {
            local(token);
            fields.iter_mut().for_each(&mut *op);
        }
    }
}

/// [`rvalue_mut`] over a statement's uses (not its definition).
fn stmt_mut(s: &mut Stmt, op: &mut dyn FnMut(&mut Operand), local: &mut dyn FnMut(&mut Local)) {
    match s {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => rvalue_mut(value, op, local),
        Stmt::Dup(l) | Stmt::Drop(l) => local(l),
        Stmt::DropKeep { local: l, keep, .. } => {
            local(l);
            for (_, p) in keep {
                local(p);
            }
        }
        Stmt::ClearUnwind => {}
    }
}

/// [`rvalue_mut`] over a terminator.
fn term_mut(
    t: &mut Terminator,
    op: &mut dyn FnMut(&mut Operand),
    local: &mut dyn FnMut(&mut Local),
) {
    match t {
        Terminator::Return(o) => op(o),
        Terminator::TailCall { callee, args } => {
            if let Callee::Indirect(l) = callee {
                local(l);
            }
            args.iter_mut().for_each(&mut *op);
        }
        Terminator::Goto { args, .. } => args.iter_mut().for_each(&mut *op),
        Terminator::Branch { cond, .. } => op(cond),
        Terminator::Switch { on, .. } => op(on),
        Terminator::Panic { message, .. } => {
            if let Some(m) = message {
                op(m);
            }
        }
        Terminator::Unreachable | Terminator::Resume => {}
    }
}

/// `op` with each local of `subst` replaced by its value, following chains.
fn resolve(subst: &FxHashMap<Local, Operand>, op: &Operand) -> Operand {
    let mut out = op.clone();
    for _ in 0..=subst.len() {
        match &out {
            Operand::Local(l) => match subst.get(l) {
                Some(next) => out = next.clone(),
                None => break,
            },
            Operand::Const(_) => break,
        }
    }
    out
}

/// Replaces each local of `subst` by its value wherever an operand stands,
/// and where only a local may stand when the value is a local; returns
/// whether anything changed.
fn substitute(f: &mut Function, subst: &FxHashMap<Local, Operand>) -> bool {
    if subst.is_empty() {
        return false;
    }
    let mut changed = false;
    let mut op = |o: &mut Operand| {
        if let Operand::Local(l) = o
            && subst.contains_key(l)
        {
            *o = resolve(subst, o);
            changed = true;
        }
    };
    let mut local_changed = false;
    let mut local = |l: &mut Local| {
        if subst.contains_key(l)
            && let Operand::Local(to) = resolve(subst, &Operand::Local(*l))
            && to != *l
        {
            *l = to;
            local_changed = true;
        }
    };
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            stmt_mut(s, &mut op, &mut local);
        }
        term_mut(&mut b.term, &mut op, &mut local);
    }
    changed || local_changed
}

/// How often each local is used, by index: every operand, base, callee and
/// kept projection.
pub(crate) fn use_counts(f: &Function) -> Vec<u32> {
    let mut counts = vec![0_u32; f.locals.len()];
    let mut add = |l: Local| {
        if let Some(c) = counts.get_mut(l.index()) {
            *c = c.saturating_add(1);
        }
    };
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                    for (l, _) in value.uses() {
                        add(l);
                    }
                    for &(_, p) in value.kept() {
                        add(p);
                    }
                }
                Stmt::Dup(l) | Stmt::Drop(l) => add(*l),
                Stmt::DropKeep { local, keep, .. } => {
                    add(*local);
                    for &(_, p) in keep {
                        add(p);
                    }
                }
                Stmt::ClearUnwind => {}
            }
        }
        for (l, _) in b.term.uses() {
            add(l);
        }
    }
    counts
}

/// Which locals of `f` hold no handle whose release could be seen, by
/// index: raw values, and values defined as a box of a raw value or as a
/// constant. Moving their last use moves no drop function (D-101,
/// D-205), so the pass may remove or reroute their uses; any other
/// counted local may hold a handle, whose release must stay where it was.
fn plain_locals(f: &Function) -> Vec<bool> {
    let mut plain: Vec<bool> = f.locals.iter().map(|d| d.repr != Repr::Value).collect();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                dst,
                value: Rvalue::Box { .. } | Rvalue::Use(Operand::Const(_)),
            } = s
                && let Some(p) = plain.get_mut(dst.index())
            {
                *p = true;
            }
        }
    }
    plain
}

/// Whether `op` is a constant or a plain local ([`plain_locals`]).
fn is_plain(plain: &[bool], op: &Operand) -> bool {
    match op {
        Operand::Const(_) => true,
        Operand::Local(l) => plain.get(l.index()) == Some(&true),
    }
}

/// Whether the counted locals live into `dropped` are all live into
/// `kept` too, plain ones aside: then a jump that goes only to `kept`
/// moves no release (D-205).
fn moves_no_release(live: &Liveness, plain: &[bool], dropped: BlockId, kept: BlockId) -> bool {
    let (Some(gone), Some(stay)) = (live.live_in.get(dropped), live.live_in.get(kept)) else {
        return false;
    };
    gone.iter()
        .all(|l| stay.contains(l) || plain.get(l.index()) == Some(&true))
}

/// The count of `l` in `counts`.
fn count(counts: &[u32], l: Local) -> u32 {
    counts.get(l.index()).copied().unwrap_or(0)
}

/// Whether computing `value` can neither panic nor have an effect (module
/// docs), so an unused one may go.
const fn is_pure(value: &Rvalue) -> bool {
    match value {
        Rvalue::Use(_)
        | Rvalue::Box { .. }
        | Rvalue::Unbox { .. }
        | Rvalue::Field { .. }
        | Rvalue::ReadLayout { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::Compare { .. }
        | Rvalue::FloatArith { .. }
        | Rvalue::Bitwise { .. }
        | Rvalue::FloatNeg { .. }
        | Rvalue::Convert { .. }
        | Rvalue::FloatSqrt { .. }
        | Rvalue::Wrapping { .. }
        | Rvalue::Not(_)
        | Rvalue::ReadTag { .. }
        | Rvalue::Alloc { .. }
        | Rvalue::AllocPlaceholder { .. } => true,
        Rvalue::Call { .. }
        | Rvalue::CallForeign { .. }
        | Rvalue::CallRuntime { .. }
        | Rvalue::InitField { .. }
        | Rvalue::Checked { .. }
        | Rvalue::DropReuse { .. }
        | Rvalue::AllocReuse { .. } => false,
    }
}

/// The value of a raw constant that may stand for its local everywhere.
const fn raw_constant(c: &Const) -> bool {
    match c {
        Const::Int(_) | Const::Float(_) | Const::Bool(_) | Const::Sized { .. } => true,
        Const::Str(_)
        | Const::Nil
        | Const::StaticCtor(_)
        | Const::FnValue(_)
        | Const::Code(_)
        | Const::EmptyList
        | Const::Static(_)
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => false,
    }
}

/// The value `value` folds to, its operands read through `subst`:
/// `unbox` of a `box` (`boxes`), a raw constant moved, a comparison,
/// `not`, checked `Int` arithmetic that does not panic, a tag read of a
/// static constructor.
fn fold_rvalue(
    value: &Rvalue,
    boxes: &FxHashMap<Local, (Operand, Repr)>,
    subst: &FxHashMap<Local, Operand>,
    tags: &Tags,
) -> Option<Operand> {
    let konst = |c: Const| Some(Operand::Const(c));
    match value {
        Rvalue::Unbox {
            value: Operand::Local(b),
            to,
        } => {
            let target = match resolve(subst, &Operand::Local(*b)) {
                Operand::Local(l) => l,
                Operand::Const(_) => return None,
            };
            let (x, from) = boxes.get(&target)?;
            (from == to).then(|| resolve(subst, x))
        }
        Rvalue::Use(Operand::Const(c)) if raw_constant(c) => konst(c.clone()),
        Rvalue::Compare { op, lhs, rhs, .. } => match (resolve(subst, lhs), resolve(subst, rhs)) {
            (Operand::Const(Const::Int(a)), Operand::Const(Const::Int(b))) => {
                konst(Const::Bool(compare(*op, a.cmp(&b))))
            }
            (Operand::Const(Const::Bool(a)), Operand::Const(Const::Bool(b))) => match op {
                CmpOp::Eq => konst(Const::Bool(a == b)),
                CmpOp::Ne => konst(Const::Bool(a != b)),
                CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => None,
            },
            _ => None,
        },
        Rvalue::Not(o) => match resolve(subst, o) {
            Operand::Const(Const::Bool(b)) => konst(Const::Bool(!b)),
            Operand::Const(_) | Operand::Local(_) => None,
        },
        Rvalue::Checked {
            op,
            lhs,
            rhs,
            repr: Repr::Int,
            ..
        } => match (resolve(subst, lhs), resolve(subst, rhs)) {
            (Operand::Const(Const::Int(a)), Operand::Const(Const::Int(b))) => {
                let folded = match op {
                    ArithOp::Add => a.checked_add(b),
                    ArithOp::Sub => a.checked_sub(b),
                    ArithOp::Mul => a.checked_mul(b),
                    ArithOp::Div => a.checked_div(b),
                    ArithOp::Rem => a.checked_rem(b),
                    ArithOp::Shl | ArithOp::Shr => None,
                };
                folded.map(|v| Operand::Const(Const::Int(v)))
            }
            _ => None,
        },
        Rvalue::ReadTag { base, ty } => match resolve(subst, base) {
            Operand::Const(Const::StaticCtor(def)) => {
                let tag = tags.tag(def, *ty)?;
                konst(Const::Int(i64::from(tag)))
            }
            Operand::Const(_) | Operand::Local(_) => None,
        },
        Rvalue::Use(_)
        | Rvalue::Call { .. }
        | Rvalue::CallForeign { .. }
        | Rvalue::CallRuntime { .. }
        | Rvalue::Alloc { .. }
        | Rvalue::AllocPlaceholder { .. }
        | Rvalue::InitField { .. }
        | Rvalue::Field { .. }
        | Rvalue::ReadLayout { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::Box { .. }
        | Rvalue::Unbox { .. }
        | Rvalue::Checked { .. }
        | Rvalue::FloatArith { .. }
        | Rvalue::Bitwise { .. }
        | Rvalue::FloatNeg { .. }
        | Rvalue::Convert { .. }
        | Rvalue::FloatSqrt { .. }
        | Rvalue::Wrapping { .. }
        | Rvalue::DropReuse { .. }
        | Rvalue::AllocReuse { .. } => None,
    }
}

/// Whether `ordering` satisfies `op`.
const fn compare(op: CmpOp, ordering: std::cmp::Ordering) -> bool {
    match op {
        CmpOp::Eq => ordering.is_eq(),
        CmpOp::Ne => ordering.is_ne(),
        CmpOp::Lt => ordering.is_lt(),
        CmpOp::Le => ordering.is_le(),
        CmpOp::Gt => ordering.is_gt(),
        CmpOp::Ge => ordering.is_ge(),
    }
}

/// The bits a [`Terminator::Switch`] case holds for constant `c`: a
/// sized integer's bits within its width, as lowering writes the cases
/// (a constant's bits are sign-extended, a case's are not).
const fn switch_bits(c: &Const) -> Option<u64> {
    match c {
        Const::Int(v) => Some(v.cast_unsigned()),
        Const::Sized { repr, bits } => Some(match repr {
            Repr::I8 | Repr::U8 => *bits & 0xff,
            Repr::I16 | Repr::U16 => *bits & 0xffff,
            Repr::I32 | Repr::U32 => *bits & 0xffff_ffff,
            Repr::Int
            | Repr::I64
            | Repr::U64
            | Repr::Value
            | Repr::Float
            | Repr::F32
            | Repr::Bool => *bits,
        }),
        Const::Float(_)
        | Const::Bool(_)
        | Const::Str(_)
        | Const::Nil
        | Const::StaticCtor(_)
        | Const::FnValue(_)
        | Const::Code(_)
        | Const::EmptyList
        | Const::Static(_)
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => None,
    }
}

/// The target a switch takes for the bits `bits`.
fn switch_target(cases: &[(u64, BlockId)], default: Option<BlockId>, bits: u64) -> Option<BlockId> {
    cases
        .iter()
        .find(|&&(v, _)| v == bits)
        .map(|&(_, t)| t)
        .or(default)
}

/// The boxes of `f`: each local defined by a `box`, with the boxed value
/// and its representation.
fn boxes(f: &Function) -> FxHashMap<Local, (Operand, Repr)> {
    let mut out = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                dst,
                value: Rvalue::Box { value, from },
            } = s
            {
                out.insert(*dst, (value.clone(), *from));
            }
        }
    }
    out
}

/// Folds values and branches on constants (module docs).
fn fold(f: &mut Function, tags: &Tags) -> bool {
    let boxes = boxes(f);
    let mut subst: FxHashMap<Local, Operand> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign { dst, value } = s
                && let Some(v) = fold_rvalue(value, &boxes, &subst, tags)
                && v != Operand::Local(*dst)
            {
                subst.insert(*dst, v);
            }
        }
    }
    let mut changed = substitute(f, &subst);
    // One branch at a time, each on fresh liveness, so no fold moves a
    // release (D-205): the targets it drops may need nothing the taken one
    // does not.
    for _ in 0..f.blocks.len() {
        if !fold_one_branch(f) {
            break;
        }
        changed = true;
    }
    changed
}

/// Folds the first branch or switch on a constant whose dropped targets
/// need no counted local that the taken one does not ([`moves_no_release`]);
/// returns whether there was one.
fn fold_one_branch(f: &mut Function) -> bool {
    let constant = f.blocks.iter().any(|b| {
        matches!(
            b.term,
            Terminator::Branch {
                cond: Operand::Const(_),
                ..
            } | Terminator::Switch {
                on: Operand::Const(_),
                ..
            }
        )
    });
    if !constant {
        return false;
    }
    let Ok(live) = Liveness::compute(f) else {
        return false;
    };
    let plain = plain_locals(f);
    for b in &mut f.blocks {
        let target = match &b.term {
            Terminator::Branch {
                cond: Operand::Const(Const::Bool(c)),
                then_,
                else_,
            } => Some(if *c { *then_ } else { *else_ }),
            Terminator::Switch {
                on: Operand::Const(c),
                cases,
                default,
            } => switch_bits(c).and_then(|bits| switch_target(cases, *default, bits)),
            Terminator::Return(_)
            | Terminator::TailCall { .. }
            | Terminator::Goto { .. }
            | Terminator::Branch { .. }
            | Terminator::Switch { .. }
            | Terminator::Panic { .. }
            | Terminator::Unreachable
            | Terminator::Resume => None,
        };
        let Some(target) = target else {
            continue;
        };
        let safe = b
            .term
            .successors()
            .into_iter()
            .all(|s| s == target || moves_no_release(&live, &plain, s, target));
        if safe {
            b.term = Terminator::Goto {
                target,
                args: Vec::new(),
            };
            return true;
        }
    }
    false
}

/// The predecessors of each block, by index: the blocks that jump there
/// with a `goto`, in order, and the number of edges from any terminator.
fn predecessors(f: &Function) -> (Vec<Vec<BlockId>>, Vec<usize>) {
    let n = f.blocks.len();
    let mut gotos: Vec<Vec<BlockId>> = vec![Vec::new(); n];
    let mut edges = vec![0_usize; n];
    for (id, b) in f.blocks.iter_enumerated() {
        if let Terminator::Goto { target, .. } = &b.term
            && let Some(list) = gotos.get_mut(target.index())
        {
            list.push(id);
        }
        for succ in b.term.successors() {
            if let Some(e) = edges.get_mut(succ.index()) {
                *e = e.saturating_add(1);
            }
        }
    }
    (gotos, edges)
}

/// The arguments block `from` passes to its `goto`'s target.
fn goto_args(f: &Function, from: BlockId) -> Option<&[Operand]> {
    if let Terminator::Goto { args, .. } = &f.block(from).ok()?.term {
        Some(args)
    } else {
        None
    }
}

/// Removes parameter `k` of block `target` and the argument `k` of every
/// `goto` of `preds` there.
fn remove_param(f: &mut Function, target: BlockId, k: usize, preds: &[BlockId]) {
    if let Some(b) = f.blocks.get_mut(target)
        && k < b.params.len()
    {
        b.params.remove(k);
    }
    for &p in preds {
        if let Some(b) = f.blocks.get_mut(p)
            && let Terminator::Goto { target: t, args } = &mut b.term
            && *t == target
            && k < args.len()
        {
            args.remove(k);
        }
    }
}

/// A block parameter that every jump fills with the same value becomes
/// that value (module docs).
fn same_params(f: &mut Function) -> bool {
    let (gotos, _) = predecessors(f);
    let used = use_counts(f);
    let local_only = crate::inline::local_only_uses(f);
    let mut subst: FxHashMap<Local, Operand> = FxHashMap::default();
    // (block, parameter index), in order.
    let mut removed: Vec<(BlockId, usize)> = Vec::new();
    for (id, b) in f.blocks.iter_enumerated() {
        let preds = gotos.get(id.index()).map_or(&[][..], Vec::as_slice);
        if id.index() == 0 || preds.is_empty() {
            continue;
        }
        for (k, &p) in b.params.iter().enumerate() {
            let mut value: Option<&Operand> = None;
            let mut same = true;
            for &pred in preds {
                let Some(arg) = goto_args(f, pred).and_then(|args| args.get(k)) else {
                    same = false;
                    break;
                };
                if *arg == Operand::Local(p) {
                    continue;
                }
                match value {
                    None => value = Some(arg),
                    Some(v) if v == arg => {}
                    Some(_) => {
                        same = false;
                        break;
                    }
                }
            }
            let Some(v) = value.filter(|_| same) else {
                continue;
            };
            // An unused one is left to `dead`, which keeps the release
            // of an argument that may hold a handle where it was.
            if used.get(p.index()).is_none_or(|&n| n == 0) {
                continue;
            }
            if matches!(v, Operand::Const(_)) && local_only.contains(&p) {
                continue;
            }
            subst.insert(p, v.clone());
            removed.push((id, k));
        }
    }
    if removed.is_empty() {
        return false;
    }
    for &(id, k) in removed.iter().rev() {
        let preds = gotos.get(id.index()).cloned().unwrap_or_default();
        remove_param(f, id, k, &preds);
    }
    substitute(f, &subst);
    true
}

/// A block parameter that every jump fills with a `box` of one
/// representation, and that only `unbox`es to it read, takes the raw
/// value (module docs).
#[expect(
    clippy::too_many_lines,
    reason = "the plan, the new parameters and the unboxes they replace, in order"
)]
fn raw_params(f: &mut Function) -> Result<bool, Ice> {
    let boxes = boxes(f);
    let counts = use_counts(f);
    // The unboxes of each local: their results and representations.
    let mut unboxes: FxHashMap<Local, Vec<(Local, Repr)>> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                dst,
                value:
                    Rvalue::Unbox {
                        value: Operand::Local(x),
                        to,
                    },
            } = s
            {
                unboxes.entry(*x).or_default().push((*dst, *to));
            }
        }
    }
    let (gotos, _) = predecessors(f);
    // (block, parameter index, raw representation, raw arguments by
    // predecessor), in order.
    let mut plans: Vec<(BlockId, usize, Repr, Vec<Operand>)> = Vec::new();
    for (id, b) in f.blocks.iter_enumerated() {
        let preds = gotos.get(id.index()).map_or(&[][..], Vec::as_slice);
        if id.index() == 0 || preds.is_empty() {
            continue;
        }
        for (k, &p) in b.params.iter().enumerate() {
            if f.local(p)?.repr != Repr::Value {
                continue;
            }
            let Some(uses) = unboxes.get(&p) else {
                continue;
            };
            let Some(&(_, to)) = uses.first() else {
                continue;
            };
            let all_unboxes = u32::try_from(uses.len()).is_ok_and(|n| n == count(&counts, p));
            if !all_unboxes || uses.iter().any(|&(_, r)| r != to) {
                continue;
            }
            let mut raw = Vec::with_capacity(preds.len());
            for &pred in preds {
                let fed = goto_args(f, pred)
                    .and_then(|args| args.get(k))
                    .and_then(Operand::local)
                    .and_then(|l| boxes.get(&l))
                    .filter(|&&(_, from)| from == to);
                match fed {
                    Some((x, _)) => raw.push(x.clone()),
                    None => break,
                }
            }
            if raw.len() == preds.len() {
                plans.push((id, k, to, raw));
            }
        }
    }
    if plans.is_empty() {
        return Ok(false);
    }
    let mut subst: FxHashMap<Local, Operand> = FxHashMap::default();
    // The parameters made raw, whose unboxes go.
    let mut raw: Vec<Local> = Vec::new();
    for (id, k, repr, fed) in plans {
        let preds = gotos.get(id.index()).cloned().unwrap_or_default();
        let Some(&p) = f.block(id)?.params.get(k) else {
            continue;
        };
        let decl = f.local(p)?;
        let new = f.locals.push(LocalDecl {
            repr,
            name: decl.name,
            ty: decl.ty,
        })?;
        if let Some(b) = f.blocks.get_mut(id)
            && let Some(slot) = b.params.get_mut(k)
        {
            *slot = new;
        }
        for (&pred, x) in preds.iter().zip(fed) {
            if let Some(b) = f.blocks.get_mut(pred)
                && let Terminator::Goto { args, .. } = &mut b.term
                && let Some(slot) = args.get_mut(k)
            {
                *slot = x;
            }
        }
        for &(u, _) in unboxes.get(&p).map_or(&[][..], Vec::as_slice) {
            subst.insert(u, Operand::Local(new));
        }
        raw.push(p);
    }
    // The unboxes of a parameter that is raw now read nothing: each one's
    // result is the raw parameter, so it goes, whatever `dead` keeps.
    for b in &mut f.blocks {
        b.retain_stmts(|s| {
            !matches!(
                s,
                Stmt::Assign {
                    value: Rvalue::Unbox {
                        value: Operand::Local(x),
                        ..
                    },
                    ..
                } if raw.contains(x)
            )
        });
    }
    substitute(f, &subst);
    Ok(true)
}

/// A jump with a static constructor into a block that only reads its tag
/// and switches on it goes to the case directly (module docs).
fn thread(f: &mut Function, tags: &Tags) -> bool {
    let candidate = f.blocks.iter().any(|b| {
        matches!(
            b.stmts.as_slice(),
            [Stmt::Assign {
                value: Rvalue::ReadTag {
                    base: Operand::Local(_),
                    ..
                },
                ..
            }]
        ) && matches!(b.term, Terminator::Switch { .. })
    });
    if !candidate {
        return false;
    }
    let counts = use_counts(f);
    let Ok(live) = Liveness::compute(f) else {
        return false;
    };
    let plain = plain_locals(f);
    let (gotos, _) = predecessors(f);
    // (predecessor, case), in order.
    let mut jumps: Vec<(BlockId, BlockId)> = Vec::new();
    for (id, b) in f.blocks.iter_enumerated() {
        let [
            Stmt::Assign {
                dst: tag,
                value:
                    Rvalue::ReadTag {
                        base: Operand::Local(p),
                        ty,
                    },
            },
        ] = b.stmts.as_slice()
        else {
            continue;
        };
        let Terminator::Switch {
            on: Operand::Local(on),
            cases,
            default,
        } = &b.term
        else {
            continue;
        };
        let Some(k) = b.params.iter().position(|q| q == p) else {
            continue;
        };
        let only_here = on == tag
            && count(&counts, *tag) == 1
            && count(&counts, *p) == 1
            && b.params.iter().all(|&q| q == *p || count(&counts, q) == 0);
        if !only_here {
            continue;
        }
        for &pred in gotos.get(id.index()).map_or(&[][..], Vec::as_slice) {
            let Some(args) = goto_args(f, pred) else {
                continue;
            };
            let Some(Operand::Const(Const::StaticCtor(def))) = args.get(k) else {
                continue;
            };
            // The other parameters are unused: their arguments must be
            // plain, or skipping the jump would move their release.
            if !args.iter().all(|a| is_plain(&plain, a)) {
                continue;
            }
            if let Some(case) = tags
                .tag(*def, *ty)
                .and_then(|t| switch_target(cases, *default, u64::from(t)))
                && moves_no_release(&live, &plain, id, case)
            {
                jumps.push((pred, case));
            }
        }
    }
    let changed = !jumps.is_empty();
    for (pred, case) in jumps {
        if let Some(b) = f.blocks.get_mut(pred) {
            b.term = Terminator::Goto {
                target: case,
                args: Vec::new(),
            };
        }
    }
    changed
}

/// Removes the blocks the entry does not reach, renumbering the others in
/// order.
fn drop_unreachable(f: &mut Function) -> Result<bool, Ice> {
    let n = f.blocks.len();
    let mut reached = vec![false; n];
    let mut work = vec![BlockId::from_u32(0)];
    while let Some(id) = work.pop() {
        match reached.get_mut(id.index()) {
            Some(r) if !*r => *r = true,
            Some(_) | None => continue,
        }
        work.extend(f.block(id)?.term.successors());
    }
    if reached.iter().all(|&r| r) {
        return Ok(false);
    }
    let mut map: Vec<Option<BlockId>> = vec![None; n];
    let mut next = 0_usize;
    for (i, &r) in reached.iter().enumerate() {
        if r && let Some(slot) = map.get_mut(i) {
            *slot = BlockId::from_usize(next);
            next = next.saturating_add(1);
        }
    }
    let old = std::mem::take(&mut f.blocks).into_raw();
    let mut kept = Vec::with_capacity(next);
    for (b, r) in old.into_iter().zip(reached) {
        if r {
            kept.push(b);
        }
    }
    for b in &mut kept {
        renumber(&mut b.term, &map)?;
    }
    f.blocks = IndexVec::from_raw(kept)?;
    Ok(true)
}

/// Gives every target of `t` its new number in `map`.
fn renumber(t: &mut Terminator, map: &[Option<BlockId>]) -> Result<(), Ice> {
    let new = |b: &mut BlockId| -> Result<(), Ice> {
        *b = map
            .get(b.index())
            .copied()
            .flatten()
            .ok_or_else(|| Ice::new("simplify: a jump into an unreachable block"))?;
        Ok(())
    };
    match t {
        Terminator::Goto { target, .. } => new(target),
        Terminator::Branch { then_, else_, .. } => {
            new(then_)?;
            new(else_)
        }
        Terminator::Switch { cases, default, .. } => {
            for (_, b) in cases {
                new(b)?;
            }
            if let Some(b) = default {
                new(b)?;
            }
            Ok(())
        }
        Terminator::Return(_)
        | Terminator::TailCall { .. }
        | Terminator::Panic { .. }
        | Terminator::Unreachable
        | Terminator::Resume => Ok(()),
    }
}

/// A block whose only predecessor jumps to it without arguments joins that
/// predecessor (module docs).
fn merge(f: &mut Function) -> bool {
    let (_, edges) = predecessors(f);
    let mut changed = false;
    for index in 0..f.blocks.len() {
        let Some(p) = BlockId::from_usize(index) else {
            break;
        };
        while let Some(Terminator::Goto { target, args }) = f.blocks.get(p).map(|b| &b.term) {
            let c = *target;
            let joins = args.is_empty()
                && c != p
                && c.index() != 0
                && edges.get(c.index()) == Some(&1)
                && f.blocks.get(c).is_some_and(|b| b.params.is_empty());
            if !joins {
                break;
            }
            let Some(cb) = f.blocks.get_mut(c) else {
                break;
            };
            let moved = std::mem::replace(
                cb,
                Block {
                    params: Vec::new(),
                    stmts: Vec::new(),
                    term: Terminator::Unreachable,
                    unwind: Vec::new(),
                    pos: Vec::new(),
                },
            );
            let Some(pb) = f.blocks.get_mut(p) else {
                break;
            };
            join(pb, moved);
            changed = true;
        }
    }
    changed
}

/// Appends `c`'s statements and terminator to `p`, whose `goto` to `c`
/// they replace, with their positions.
fn join(p: &mut Block, c: Block) {
    let p_has = p.has_positions();
    let c_has = !c.pos.is_empty();
    if p_has || c_has {
        let mut pos = if p_has {
            let mut own = std::mem::take(&mut p.pos);
            own.pop();
            own
        } else {
            vec![PosRef::NONE; p.stmts.len()]
        };
        if c_has {
            pos.extend(c.pos);
        } else {
            pos.extend(std::iter::repeat_n(
                PosRef::NONE,
                c.stmts.len().saturating_add(1),
            ));
        }
        p.pos = pos;
    }
    p.stmts.extend(c.stmts);
    p.term = c.term;
}

/// Removes unused definitions of pure values and unused block parameters
/// (module docs).
fn dead(f: &mut Function) -> bool {
    let mut counts = use_counts(f);
    let plain = plain_locals(f);
    let mut changed = false;
    loop {
        let mut round = false;
        for b in &mut f.blocks {
            let before = b.stmts.len();
            b.retain_stmts(|s| {
                let Stmt::Assign { dst, value } = s else {
                    return true;
                };
                // A use of a local that may hold a handle keeps it alive
                // to here (D-205): such a definition stays, as the move
                // that keeps an `@external` wrapper's handle alive across
                // its foreign call does.
                let keeps_alive = value
                    .uses()
                    .iter()
                    .any(|&(l, _)| plain.get(l.index()) != Some(&true));
                if count(&counts, *dst) != 0 || !is_pure(value) || keeps_alive {
                    return true;
                }
                for (l, _) in value.uses() {
                    if let Some(c) = counts.get_mut(l.index()) {
                        *c = c.saturating_sub(1);
                    }
                }
                false
            });
            round |= b.stmts.len() != before;
        }
        let (gotos, _) = predecessors(f);
        let mut removed: Vec<(BlockId, usize)> = Vec::new();
        for (id, b) in f.blocks.iter_enumerated() {
            if id.index() == 0 {
                continue;
            }
            let preds = gotos.get(id.index()).map_or(&[][..], Vec::as_slice);
            for (k, &p) in b.params.iter().enumerate() {
                // An argument that may hold a handle is released at the
                // block's start; without the parameter it would be
                // released earlier (D-205).
                let plain_args = preds.iter().all(|&pred| {
                    goto_args(f, pred)
                        .and_then(|a| a.get(k))
                        .is_some_and(|a| is_plain(&plain, a))
                });
                if count(&counts, p) == 0 && plain_args {
                    removed.push((id, k));
                }
            }
        }
        for &(id, k) in removed.iter().rev() {
            let preds = gotos.get(id.index()).cloned().unwrap_or_default();
            for &pred in &preds {
                if let Some(Operand::Local(l)) = goto_args(f, pred).and_then(|a| a.get(k))
                    && let Some(c) = counts.get_mut(l.index())
                {
                    *c = c.saturating_sub(1);
                }
            }
            remove_param(f, id, k, &preds);
            round = true;
        }
        if !round {
            break;
        }
        changed = true;
    }
    changed
}

/// Keeps the call locations and sites of the locals the function still
/// defines: a call removed with an unreachable block takes its location
/// with it.
fn tidy(f: &mut Function) {
    let mut by_call = vec![false; f.locals.len()];
    let mut defined = vec![false; f.locals.len()];
    let mark = |v: &mut Vec<bool>, l: Local| {
        if let Some(slot) = v.get_mut(l.index()) {
            *slot = true;
        }
    };
    for &p in &f.params {
        mark(&mut defined, p);
    }
    for b in &f.blocks {
        for &p in &b.params {
            mark(&mut defined, p);
        }
        for s in &b.stmts {
            if let Stmt::Assign { dst, value } = s {
                mark(&mut defined, *dst);
                if matches!(value, Rvalue::Call { .. }) {
                    mark(&mut by_call, *dst);
                }
            }
        }
    }
    f.call_locs
        .retain(|&(l, _)| by_call.get(l.index()) == Some(&true));
    f.sites
        .retain(|&(l, _)| defined.get(l.index()) == Some(&true));
}
