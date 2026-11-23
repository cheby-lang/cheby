//! The clean-up after inlining in the release profile (design T7), on
//! lowered MIR ([`crate::Stage::Lowered`]) before borrowing inference and
//! counting, one function at a time ([`run`]):
//!
//! - `unbox` of a `box` of the same representation is the boxed value,
//!   and `unbox` of a constant's slot word is the constant's value when
//!   the build folds it ([`Known::set_consts`], [`crate::fold_consts`]);
//! - a block parameter that every jump fills with a `box` of one
//!   representation, and that only `unbox`es to it read, takes the raw
//!   value instead: what an inlined `Float` helper's result needs;
//! - a block parameter that every jump fills with the same value is that
//!   value, and a block whose only predecessor jumps to it joins that
//!   predecessor;
//! - comparisons, `not`, checked arithmetic that cannot panic and tag reads
//!   of constants fold, and so do branches and switches on constants;
//! - the `Option` that a direct `map::upsert` gives an inlined callback
//!   that only takes it apart is no `Some`: the arm is chosen by the
//!   address the upsert kept, and the `Some` arm moves the stored value
//!   out (`crate::lookup::fold_upsert`);
//! - a constructor or closure allocated in the function whose only uses
//!   read its tag and its fields is taken apart: a tag read is its tag, a
//!   field read the operand it was built from, and the allocation goes,
//!   when every field that may hold a handle stays live after each read,
//!   so that the object's release never released such a value
//!   ([`scalar_ctors`]);
//! - a jump with a static constructor into a block that only reads its
//!   tag and switches on it goes to the case directly;
//! - with the `spec` lever (design T11), what a local is known to hold
//!   ([`Shape`]) settles calls: a field of a static dictionary or static
//!   closure is the field's constant, a capture of a closure allocated in
//!   the function that is a box or a constant is that box or constant
//!   again, a tag read of a static constructor is its tag, an indirect call
//!   of a function value is a direct call with the unboxes its value shim
//!   makes, and an indirect call of a static closure or of a closure of
//!   known code calls that code by key with the closure as `self`, which
//!   the inliner may then copy. The calls of shared functions (value
//!   shims, tuple functions, `dyn` forwarders) never change (D-918);
//! - a jump to a block that holds only a jump goes to that jump's target;
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
//! one), a parameter whose argument may hold one stays, a branch folds
//! only when no such value live into a dropped target is dead in the taken
//! one, and a jump is threaded only from a block of pure statements and
//! only when every other block keeps the values it holds live as before,
//! so the values the skipped switch's other cases need die where the jump
//! starts, with nothing a program sees in between. Raw values, boxes of raw
//! values and constants hold no handle.
//!
//! Counting runs after this pass, so removing a definition or a parameter
//! needs no count of its own. Every rewrite keeps the statements' source
//! positions (D-938) and the call locations (D-856), and the function
//! stays lowered MIR. The pass is deterministic: it walks blocks and
//! statements in order and its maps are only looked up.

use cheby_iface::{DefId, Ice, Idx, IndexVec, builtin};
use rustc_hash::FxHashMap;

use crate::abi::AbiSig;
use crate::ir::{
    ArithOp, Block, BlockId, Callee, CalleeDecl, CmpOp, Const, FnKey, Function, LayoutRef, Local,
    LocalDecl, MirModule, Operand, Rvalue, StaticId, StaticKind, StaticObject, Stmt, TagType,
    Terminator,
};
use crate::liveness::Liveness;
use crate::positions::PosRef;
use crate::repr::Repr;

/// The most rounds of rewrites per function: each round can enable the
/// next, and a few reach the fixed point of an inlined body.
const ROUNDS: usize = 8;

/// What the clean-up knows of a unit beyond the function it cleans up:
/// the tags of the constructors the unit knows ([`MirModule::layouts`] and
/// [`MirModule::foreign_layouts`]) with their types, the fields of its
/// static objects, the signatures of its direct callees, the number of
/// parameters of each function it defines, and what the parameters of its
/// clones are known to hold ([`crate::specialize`]).
#[derive(Clone, Debug, Default)]
pub struct Known {
    tags: FxHashMap<DefId, (DefId, u32)>,
    statics: FxHashMap<StaticId, (StaticKind, Box<[Const]>)>,
    sigs: FxHashMap<DefId, AbiSig>,
    arity: FxHashMap<FnKey, usize>,
    shapes: FxHashMap<FnKey, Vec<Option<Shape>>>,
    /// Whether calls of known values become direct calls ([`devirt`]):
    /// the release profile's `spec` lever.
    devirt: bool,
    /// The values of the constants the build folds
    /// ([`crate::fold_consts`]): an `unbox` of such a constant's slot word
    /// is its raw value. Empty unless the release profile's `consts`
    /// lever gives them.
    consts: crate::fold_consts::ConstValues,
}

impl Known {
    /// What `unit` holds: its layouts' constructors, its statics, its
    /// callees and its functions.
    #[must_use]
    pub fn of(unit: &MirModule) -> Self {
        let mut out = Self {
            devirt: true,
            ..Self::default()
        };
        out.extend(&unit.layouts);
        out.extend(&unit.foreign_layouts);
        out.add_statics(&unit.statics);
        out.add_callees(&unit.callees);
        out.add_functions(&unit.functions);
        out
    }

    /// Adds the constructors of `layouts`, layouts of other modules that a
    /// body from another unit brought in ([`crate::inline::optimize_across`]).
    pub fn extend(&mut self, layouts: &[crate::layout::LayoutDesc]) {
        for d in layouts {
            if let LayoutRef::Ctor(def) = d.layout {
                self.tags.insert(def, (d.type_def, d.tag));
            }
        }
    }

    /// Folds the constants of `values` ([`crate::fold_consts`]).
    pub fn set_consts(&mut self, values: &crate::fold_consts::ConstValues) {
        self.consts.clone_from(values);
    }

    /// Adds static objects that joined the unit.
    pub(crate) fn add_statics(&mut self, statics: &[StaticObject]) {
        for s in statics {
            self.statics.insert(s.id, (s.kind, s.fields.clone()));
        }
    }

    /// Adds the signatures of direct callees.
    pub(crate) fn add_callees(&mut self, callees: &[CalleeDecl]) {
        for c in callees {
            self.sigs.insert(c.def, c.sig.clone());
        }
    }

    /// Adds functions that joined the unit, by key.
    pub(crate) fn add_functions(&mut self, functions: &[Function]) {
        for f in functions {
            self.arity.insert(f.key, f.params.len());
        }
    }

    /// Records what the parameters of the clone `key` hold, in parameter
    /// order ([`crate::specialize`]).
    pub(crate) fn set_shapes(&mut self, key: FnKey, shapes: Vec<Option<Shape>>) {
        self.shapes.insert(key, shapes);
    }

    /// The kind of the static object `id`.
    pub(crate) fn static_kind(&self, id: StaticId) -> Option<StaticKind> {
        self.statics.get(&id).map(|&(kind, _)| kind)
    }

    /// Turns devirtualization ([`devirt`]) on or off.
    pub(crate) const fn set_devirt(&mut self, on: bool) {
        self.devirt = on;
    }

    /// The tag a [`Rvalue::ReadTag`] of `ty` reads from the static object
    /// of constructor `def`: the declaration order of its type's
    /// constructors (D-522), the runtime's layout id less the first one's
    /// for the builtin types.
    fn tag(&self, def: DefId, ty: TagType) -> Option<u32> {
        let builtin: &[DefId] = match ty {
            TagType::Adt(want) => {
                let &(type_def, tag) = self.tags.get(&def)?;
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
pub fn run(f: &mut Function, tags: &Known) -> Result<bool, Ice> {
    if f.precounted || f.entry_unwind.is_some() || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Ok(false);
    }
    let mut changed = false;
    for _ in 0..ROUNDS {
        let mut round = devirt(f, tags)?;
        round |= fold(f, tags);
        round |= crate::lookup::fold_upsert(f)?;
        round |= scalar_ctors(f, tags)?;
        round |= same_params(f);
        round |= raw_params(f)?;
        round |= thread(f, tags);
        round |= forward_empty(f);
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

/// Whether `value` is checked arithmetic on constants that does not
/// overflow: [`fold_rvalue`] folds its uses, and it may go like a pure
/// value.
fn cannot_panic(value: &Rvalue) -> bool {
    let none = FxHashMap::default();
    matches!(
        value,
        Rvalue::Checked {
            lhs: Operand::Const(_),
            rhs: Operand::Const(_),
            ..
        }
    ) && fold_rvalue(
        value,
        &FxHashMap::default(),
        &none,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &Known::default(),
    )
    .is_some()
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
/// `unbox` of a `box` (`boxes`) or of the slot word of a constant
/// whose value the build folds (`loads`, [`Known::set_consts`]), a raw
/// constant moved, a comparison, `not`, checked `Int` arithmetic that
/// does not panic, a tag read of a static constructor.
fn fold_rvalue(
    value: &Rvalue,
    boxes: &FxHashMap<Local, (Operand, Repr)>,
    unboxes: &FxHashMap<Local, (Local, Repr)>,
    loads: &FxHashMap<Local, DefId>,
    subst: &FxHashMap<Local, Operand>,
    tags: &Known,
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
            if let Some(&def) = loads.get(&target) {
                return crate::fold_consts::unboxed(&tags.consts, def, *to).map(Operand::Const);
            }
            let (x, from) = boxes.get(&target)?;
            (from == to).then(|| resolve(subst, x))
        }
        Rvalue::Use(Operand::Const(c)) if raw_constant(c) => konst(c.clone()),
        // A box of a value unboxed from a uniform word in the same
        // representation is that word: boxing is canonical (D-278).
        Rvalue::Box {
            value: Operand::Local(u),
            from,
        } => {
            let (x, to) = unboxes.get(u)?;
            (to == from).then(|| resolve(subst, &Operand::Local(*x)))
        }
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
fn fold(f: &mut Function, tags: &Known) -> bool {
    let boxes = boxes(f);
    let mut unboxes: FxHashMap<Local, (Local, Repr)> = FxHashMap::default();
    // The slot words of the constants the build folds.
    let mut loads: FxHashMap<Local, DefId> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Assign {
                    dst,
                    value:
                        Rvalue::Unbox {
                            value: Operand::Local(x),
                            to,
                        },
                } => {
                    unboxes.insert(*dst, (*x, *to));
                }
                Stmt::Assign {
                    dst,
                    value: Rvalue::LoadConst(def),
                } if tags.consts.contains_key(def) => {
                    loads.insert(*dst, *def);
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
    let mut subst: FxHashMap<Local, Operand> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign { dst, value } = s
                && let Some(v) = fold_rvalue(value, &boxes, &unboxes, &loads, &subst, tags)
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
/// and switches on it goes to the case directly (module docs), one jump at
/// a time ([`thread_one`]).
fn thread(f: &mut Function, tags: &Known) -> bool {
    let mut changed = false;
    for _ in 0..f.blocks.len() {
        if !thread_one(f, tags) {
            break;
        }
        changed = true;
    }
    changed
}

/// Whether every statement of `b` is pure ([`is_pure`], [`cannot_panic`]).
fn pure_block(b: &Block) -> bool {
    b.stmts.iter().all(|s| match s {
        Stmt::Assign { value, .. } => is_pure(value) || cannot_panic(value),
        Stmt::Eval(_)
        | Stmt::Dup(_)
        | Stmt::Drop(_)
        | Stmt::DropKeep { .. }
        | Stmt::ClearUnwind => false,
    })
}

/// Whether the counted locals that may hold a handle are live into and out
/// of every block but `except` alike in `old` and `new`: then a change
/// that makes `new` moves no release outside `except` (D-205).
fn same_releases(old: &Liveness, new: &Liveness, plain: &[bool], except: BlockId) -> bool {
    let held = |set: &crate::liveness::LocalSet| -> Vec<Local> {
        set.iter()
            .copied()
            .filter(|l| plain.get(l.index()) != Some(&true))
            .collect()
    };
    old.live_in
        .iter_enumerated()
        .zip(new.live_in.iter())
        .zip(old.live_out.iter().zip(new.live_out.iter()))
        .all(|(((id, oi), ni), (oo, no))| {
            id == except || (held(oi) == held(ni) && held(oo) == held(no))
        })
}

/// Threads the first jump with a static constructor into a block that only
/// reads its tag and switches on it, straight to the case (module docs);
/// returns whether there was one. The jumping block must hold only pure
/// statements, and the jump is kept when the locals that may hold a
/// handle are live anywhere else otherwise than before: the values the
/// skipped switch's other cases need die at the jumping block instead of
/// at the case, with nothing a program can see between (D-205).
fn thread_one(f: &mut Function, tags: &Known) -> bool {
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
            if !f.blocks.get(pred).is_some_and(pure_block) {
                continue;
            }
            if let Some(case) = tags
                .tag(*def, *ty)
                .and_then(|t| switch_target(cases, *default, u64::from(t)))
            {
                jumps.push((pred, case));
            }
        }
    }
    for (pred, case) in jumps {
        let Some(b) = f.blocks.get_mut(pred) else {
            continue;
        };
        let saved = std::mem::replace(
            &mut b.term,
            Terminator::Goto {
                target: case,
                args: Vec::new(),
            },
        );
        let kept = Liveness::compute(f).is_ok_and(|new| same_releases(&live, &new, &plain, pred));
        if kept {
            return true;
        }
        if let Some(b) = f.blocks.get_mut(pred) {
            b.term = saved;
        }
    }
    false
}

/// What the clean-up knows a local holds (design T11, T13): a constant,
/// or a closure object of known code whose captures may be known too.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Shape {
    /// The constant itself: a static dictionary, a function value, a static
    /// closure or any other constant.
    Const(Const),
    /// A closure allocated with the code word `code`, its captures (fields
    /// 1 on) as far as they are known.
    Closure {
        code: FnKey,
        captures: Vec<Option<Shape>>,
    },
}

/// How deep a shape's captures are followed: a clone's key holds its
/// arguments' shapes ([`crate::specialize`]), and a deeper one would only
/// make keys larger.
const SHAPE_DEPTH: usize = 3;

impl Shape {
    /// The shape with the captures below `depth` levels forgotten.
    fn bounded(self, depth: usize) -> Self {
        match self {
            Self::Const(_) => self,
            Self::Closure { code, captures } => {
                let captures = if depth == 0 {
                    vec![None; captures.len()]
                } else {
                    captures
                        .into_iter()
                        .map(|c| c.map(|c| c.bounded(depth.saturating_sub(1))))
                        .collect()
                };
                Self::Closure { code, captures }
            }
        }
    }
}

/// The shape of field `field` of a value of shape `base`: a static
/// object's constant field, or a closure's known capture.
fn field_shape(base: &Shape, field: u32, known: &Known) -> Option<Shape> {
    let at = usize::try_from(field).ok()?;
    match base {
        // A static's string indexes the static's own table, and a code word
        // stands only in a closure's first field: neither is an operand.
        Shape::Const(Const::Static(id)) => match known.statics.get(id)?.1.get(at)? {
            Const::Str(_) | Const::Code(_) => None,
            c @ (Const::Int(_)
            | Const::Float(_)
            | Const::Bool(_)
            | Const::Sized { .. }
            | Const::Nil
            | Const::StaticCtor(_)
            | Const::FnValue(_)
            | Const::EmptyList
            | Const::Static(_)
            | Const::BuiltinDesc(_)
            | Const::Ctx
            | Const::ErrNil) => Some(Shape::Const(c.clone())),
        },
        Shape::Closure { captures, .. } => captures.get(at.checked_sub(1)?).cloned().flatten(),
        Shape::Const(_) => None,
    }
}

/// The shape of each local of `f`, by index (module docs): a clone's
/// parameters as [`Known`] records them, constants moved into locals,
/// closures allocated with a code word, and fields of values of known
/// shape. Block parameters stay unknown.
pub(crate) fn shapes(f: &Function, known: &Known) -> Vec<Option<Shape>> {
    let mut out: Vec<Option<Shape>> = vec![None; f.locals.len()];
    if let Some(params) = known.shapes.get(&f.key) {
        for (&p, s) in f.params.iter().zip(params) {
            if let Some(slot) = out.get_mut(p.index()) {
                slot.clone_from(s);
            }
        }
    }
    let operand = |out: &[Option<Shape>], op: &Operand| match op {
        Operand::Const(c) => Some(Shape::Const(c.clone())),
        Operand::Local(l) => out.get(l.index()).cloned().flatten(),
    };
    // Definitions in block order; a block may read what a later one
    // defines, so a few passes reach every definition of an inlined body.
    for _ in 0..4 {
        let mut changed = false;
        for b in &f.blocks {
            for s in &b.stmts {
                let Stmt::Assign { dst, value } = s else {
                    continue;
                };
                if out.get(dst.index()).is_none_or(Option::is_some) {
                    continue;
                }
                let shape = match value {
                    Rvalue::Use(op) => operand(&out, op),
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(_),
                        fields,
                    } => match fields.split_first() {
                        Some((Operand::Const(Const::Code(code)), captures)) => {
                            Some(Shape::Closure {
                                code: *code,
                                captures: captures.iter().map(|op| operand(&out, op)).collect(),
                            })
                        }
                        _ => None,
                    },
                    Rvalue::Field { base, field, .. } => out
                        .get(base.index())
                        .and_then(Option::as_ref)
                        .and_then(|s| field_shape(s, *field, known)),
                    Rvalue::Call { .. }
                    | Rvalue::CallForeign { .. }
                    | Rvalue::CallRuntime { .. }
                    | Rvalue::Alloc { .. }
                    | Rvalue::AllocPlaceholder { .. }
                    | Rvalue::InitField { .. }
                    | Rvalue::ReadLayout { .. }
                    | Rvalue::LoadConst(_)
                    | Rvalue::Box { .. }
                    | Rvalue::Unbox { .. }
                    | Rvalue::Checked { .. }
                    | Rvalue::Compare { .. }
                    | Rvalue::FloatArith { .. }
                    | Rvalue::Bitwise { .. }
                    | Rvalue::FloatNeg { .. }
                    | Rvalue::Convert { .. }
                    | Rvalue::FloatSqrt { .. }
                    | Rvalue::Wrapping { .. }
                    | Rvalue::Not(_)
                    | Rvalue::ReadTag { .. }
                    | Rvalue::DropReuse { .. }
                    | Rvalue::AllocReuse { .. } => None,
                };
                if let Some(shape) = shape.map(|s| s.bounded(SHAPE_DEPTH))
                    && let Some(slot) = out.get_mut(dst.index())
                {
                    *slot = Some(shape);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// Whether the clean-up may change the calls of `f`: not a function every
/// unit that needs it defines the same way (D-918).
const fn rewrites_calls(key: FnKey) -> bool {
    !matches!(
        key,
        FnKey::ValueShim(_)
            | FnKey::TupleDictFn { .. }
            | FnKey::DynForward { .. }
            | FnKey::Trampoline(_)
            | FnKey::Entry(_)
    )
}

/// An argument of a devirtualized call: the operand, and the
/// representation it is unboxed to first when the callee takes it raw.
type DevirtArg = (Operand, Option<Repr>);

/// The call an indirect call of a value of shape `shape` with `args`
/// becomes (module docs): a function value's function, directly, with the
/// unboxes its value shim makes; a static closure's code or a known
/// closure's code, by key, with the closure as `self`. `callee` is the
/// operand that holds the value.
fn devirt_call(
    shape: &Shape,
    callee: &Operand,
    args: &[Operand],
    known: &Known,
) -> Option<(Callee, Vec<DevirtArg>)> {
    let arity = args.len().checked_add(1)?;
    let with_self = |code: FnKey| -> Option<(Callee, Vec<DevirtArg>)> {
        (known.arity.get(&code) == Some(&arity)).then(|| {
            let all = std::iter::once((callee.clone(), None))
                .chain(args.iter().map(|a| (a.clone(), None)))
                .collect();
            (Callee::Code(code), all)
        })
    };
    match shape {
        Shape::Const(Const::FnValue(def)) => {
            let sig = known.sigs.get(def)?;
            let fits = sig.params.len() == args.len()
                && sig.ret == Repr::Value
                && sig.borrow.is_owned()
                && sig.abi == crate::ir::Abi::Cheby;
            fits.then(|| {
                let all = args
                    .iter()
                    .zip(&sig.params)
                    .map(|(a, &r)| (a.clone(), (r != Repr::Value).then_some(r)))
                    .collect();
                (Callee::Direct(*def), all)
            })
        }
        Shape::Const(Const::Static(id)) => match known.statics.get(id) {
            Some((StaticKind::FnValue, fields)) => match fields.first() {
                Some(Const::Code(code)) => with_self(*code),
                _ => None,
            },
            _ => None,
        },
        Shape::Closure { code, .. } => with_self(*code),
        Shape::Const(_) => None,
    }
}

/// A rewrite [`devirt`] found.
enum Devirt {
    /// Statement `stmt` of `block` defines its local as `value`.
    Value {
        block: BlockId,
        stmt: usize,
        value: Rvalue,
    },
    /// The call in statement `at` of `block`, or its tail call when `at`
    /// is `None`, becomes a call of `callee` with `args`.
    Call {
        block: BlockId,
        at: Option<usize>,
        callee: Callee,
        args: Vec<DevirtArg>,
    },
}

/// What the shapes of locals settle (design T11; module docs): a field of
/// a static object, or a closure's capture that is a constant, is that
/// constant, and a capture of a closure allocated here that is a box or a
/// constant is that box or constant again; a tag read of a static
/// constructor is its tag; an indirect call of a value of known shape is
/// a call of its code ([`devirt_call`]). Returns whether anything changed.
///
/// # Errors
///
/// An [`Ice`] for an unknown local.
#[expect(
    clippy::too_many_lines,
    reason = "the rewrites of the module docs, found and then applied in order"
)]
fn devirt(f: &mut Function, known: &Known) -> Result<bool, Ice> {
    if !known.devirt || !rewrites_calls(f.key) {
        return Ok(false);
    }
    let candidate = f.blocks.iter().any(|b| {
        matches!(
            b.term,
            Terminator::TailCall {
                callee: Callee::Indirect(_),
                ..
            }
        ) || b.stmts.iter().any(|s| {
            matches!(
                s,
                Stmt::Assign {
                    value: Rvalue::Field { .. }
                        | Rvalue::ReadTag { .. }
                        | Rvalue::Call {
                            callee: Callee::Indirect(_),
                            ..
                        },
                    ..
                }
            )
        })
    });
    if !candidate {
        return Ok(false);
    }
    let shapes = shapes(f, known);
    let shape_of = |l: Local| shapes.get(l.index()).and_then(Option::as_ref);
    // The closures allocated here and the definitions of boxes and
    // constants, which a capture may be forwarded to.
    let mut allocs: FxHashMap<Local, &[Operand]> = FxHashMap::default();
    let mut defs: FxHashMap<Local, &Rvalue> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign { dst, value } = s {
                match value {
                    Rvalue::Alloc {
                        layout: LayoutRef::Closure(_),
                        fields,
                    } => {
                        allocs.insert(*dst, fields);
                    }
                    Rvalue::Box { .. } | Rvalue::Use(Operand::Const(_)) => {
                        defs.insert(*dst, value);
                    }
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
                    | Rvalue::Unbox { .. }
                    | Rvalue::Checked { .. }
                    | Rvalue::Compare { .. }
                    | Rvalue::FloatArith { .. }
                    | Rvalue::Bitwise { .. }
                    | Rvalue::FloatNeg { .. }
                    | Rvalue::Convert { .. }
                    | Rvalue::FloatSqrt { .. }
                    | Rvalue::Wrapping { .. }
                    | Rvalue::Not(_)
                    | Rvalue::ReadTag { .. }
                    | Rvalue::DropReuse { .. }
                    | Rvalue::AllocReuse { .. } => {}
                }
            }
        }
    }
    let mut found: Vec<Devirt> = Vec::new();
    for (id, b) in f.blocks.iter_enumerated() {
        for (i, s) in b.stmts.iter().enumerate() {
            let Stmt::Assign { dst, value } = s else {
                continue;
            };
            match value {
                Rvalue::Field { base, field, .. } => {
                    if f.local(*dst)?.repr != Repr::Value {
                        continue;
                    }
                    let forwarded = allocs
                        .get(base)
                        .and_then(|fields| fields.get(usize::try_from(*field).ok()?))
                        .and_then(|op| match op {
                            Operand::Const(c) => Some(Rvalue::Use(Operand::Const(c.clone()))),
                            Operand::Local(l) => defs.get(l).map(|&v| v.clone()),
                        });
                    let constant =
                        || match shape_of(*base).and_then(|s| field_shape(s, *field, known)) {
                            Some(Shape::Const(c)) => Some(Rvalue::Use(Operand::Const(c))),
                            Some(Shape::Closure { .. }) | None => None,
                        };
                    if let Some(value) = forwarded.or_else(constant) {
                        found.push(Devirt::Value {
                            block: id,
                            stmt: i,
                            value,
                        });
                    }
                }
                Rvalue::ReadTag {
                    base: Operand::Local(base),
                    ty,
                } => {
                    if let Some(Shape::Const(Const::StaticCtor(def))) = shape_of(*base)
                        && let Some(tag) = known.tag(*def, *ty)
                    {
                        found.push(Devirt::Value {
                            block: id,
                            stmt: i,
                            value: Rvalue::Use(Operand::Const(Const::Int(i64::from(tag)))),
                        });
                    }
                }
                Rvalue::Call {
                    callee: Callee::Indirect(l),
                    args,
                } => {
                    if let Some(shape) = shape_of(*l)
                        && let Some((callee, args)) =
                            devirt_call(shape, &Operand::Local(*l), args, known)
                    {
                        found.push(Devirt::Call {
                            block: id,
                            at: Some(i),
                            callee,
                            args,
                        });
                    }
                }
                Rvalue::Use(_)
                | Rvalue::Call { .. }
                | Rvalue::CallForeign { .. }
                | Rvalue::CallRuntime { .. }
                | Rvalue::Alloc { .. }
                | Rvalue::AllocPlaceholder { .. }
                | Rvalue::InitField { .. }
                | Rvalue::ReadLayout { .. }
                | Rvalue::LoadConst(_)
                | Rvalue::Box { .. }
                | Rvalue::Unbox { .. }
                | Rvalue::Checked { .. }
                | Rvalue::Compare { .. }
                | Rvalue::FloatArith { .. }
                | Rvalue::Bitwise { .. }
                | Rvalue::FloatNeg { .. }
                | Rvalue::Convert { .. }
                | Rvalue::FloatSqrt { .. }
                | Rvalue::Wrapping { .. }
                | Rvalue::Not(_)
                | Rvalue::ReadTag { .. }
                | Rvalue::DropReuse { .. }
                | Rvalue::AllocReuse { .. } => {}
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Indirect(l),
            args,
        } = &b.term
            && let Some(shape) = shape_of(*l)
            && let Some((callee, args)) = devirt_call(shape, &Operand::Local(*l), args, known)
        {
            found.push(Devirt::Call {
                block: id,
                at: None,
                callee,
                args,
            });
        }
    }
    if found.is_empty() {
        return Ok(false);
    }
    // Later sites first within a block, so earlier indexes stay right.
    found.reverse();
    for rewrite in found {
        match rewrite {
            Devirt::Value { block, stmt, value } => {
                if let Some(Stmt::Assign { value: v, .. }) =
                    f.blocks.get_mut(block).and_then(|b| b.stmts.get_mut(stmt))
                {
                    *v = value;
                }
            }
            Devirt::Call {
                block,
                at,
                callee,
                args,
            } => {
                let mut unboxes = Vec::new();
                let mut operands = Vec::with_capacity(args.len());
                for (op, raw) in args {
                    match raw {
                        Some(to) => {
                            let u = f.locals.push(LocalDecl {
                                repr: to,
                                name: None,
                                ty: None,
                            })?;
                            unboxes.push(Stmt::Assign {
                                dst: u,
                                value: Rvalue::Unbox { value: op, to },
                            });
                            operands.push(Operand::Local(u));
                        }
                        None => operands.push(op),
                    }
                }
                let Some(b) = f.blocks.get_mut(block) else {
                    continue;
                };
                let at_index = at.unwrap_or(b.stmts.len());
                let pos = b.pos_at(at_index);
                match at {
                    Some(i) => {
                        if let Some(Stmt::Assign {
                            value: Rvalue::Call { callee: c, args: a },
                            ..
                        }) = b.stmts.get_mut(i)
                        {
                            *c = callee;
                            *a = operands;
                        }
                    }
                    None => {
                        b.term = Terminator::TailCall {
                            callee,
                            args: operands,
                        };
                    }
                }
                b.insert_stmts(at_index, unboxes, pos);
            }
        }
    }
    Ok(true)
}

/// Whether `f` makes an indirect call: what [`devirt`] may settle before
/// the inliner looks for direct calls.
#[must_use]
pub(crate) fn has_indirect_call(f: &Function) -> bool {
    indirect_calls(f) != 0
}

/// How many indirect calls `f` makes.
#[must_use]
pub(crate) fn indirect_calls(f: &Function) -> usize {
    f.blocks
        .iter()
        .map(|b| {
            let tail = usize::from(matches!(
                b.term,
                Terminator::TailCall {
                    callee: Callee::Indirect(_),
                    ..
                }
            ));
            b.stmts
                .iter()
                .filter(|s| {
                    matches!(
                        s,
                        Stmt::Assign {
                            value: Rvalue::Call {
                                callee: Callee::Indirect(_),
                                ..
                            },
                            ..
                        }
                    )
                })
                .count()
                .saturating_add(tail)
        })
        .fold(0, usize::saturating_add)
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

/// A jump to a block that holds nothing but a jump without arguments to a
/// block without parameters goes to that block directly: the values live
/// into both are the same, so no release moves.
fn forward_empty(f: &mut Function) -> bool {
    let n = f.blocks.len();
    let mut to: Vec<Option<BlockId>> = vec![None; n];
    for (id, b) in f.blocks.iter_enumerated() {
        if id.index() == 0 || !b.params.is_empty() || !b.stmts.is_empty() {
            continue;
        }
        if let Terminator::Goto { target, args } = &b.term
            && args.is_empty()
            && *target != id
            && f.blocks.get(*target).is_some_and(|t| t.params.is_empty())
            && let Some(slot) = to.get_mut(id.index())
        {
            *slot = Some(*target);
        }
    }
    // Follow chains of such blocks, never round a cycle of them.
    let last = |mut b: BlockId| {
        for _ in 0..n {
            match to.get(b.index()).copied().flatten() {
                Some(next) => b = next,
                None => break,
            }
        }
        b
    };
    let mut changed = false;
    for index in 0..n {
        let Some(id) = BlockId::from_usize(index) else {
            break;
        };
        let Some(b) = f.blocks.get(id) else {
            break;
        };
        let succs = b.term.successors();
        let mut term = b.term.clone();
        for s in succs {
            if to.get(s.index()).copied().flatten().is_some() {
                let end = last(s);
                if end != s && end != id {
                    term.retarget(s, end);
                    changed = true;
                }
            }
        }
        if let Some(b) = f.blocks.get_mut(id) {
            b.term = term;
        }
    }
    changed
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

/// Where a constructor allocated in the function is read: the tag reads
/// and the field reads, each as its block, its statement and, for a field
/// read, the local it defines and the operand it reads.
struct CtorReads {
    tags: Vec<(BlockId, usize, i64)>,
    fields: Vec<(BlockId, usize, Local, Operand)>,
}

/// Takes apart the constructors and closures allocated in `f` whose only
/// uses read their tag or their fields (module docs): a tag read is the
/// constructor's tag, a field read is the operand the field was built
/// from, and the allocation goes, as the `Some` an upsert builds for its
/// inlined callback and the callback's closure (`lower/upsert.rs`). A constructor is taken apart only
/// when no release moves (D-205): every field that may hold a handle stays
/// live after each read, so the object it holds is released where it was,
/// at the last use of the field read's result. Returns whether anything
/// changed.
///
/// # Errors
///
/// An [`Ice`] for an unknown local or malformed blocks.
fn scalar_ctors(f: &mut Function, tags: &Known) -> Result<bool, Ice> {
    let candidates: Vec<(Local, LayoutRef, Vec<Operand>)> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Stmt::Assign {
                dst,
                value:
                    Rvalue::Alloc {
                        layout: layout @ (LayoutRef::Ctor(_) | LayoutRef::Closure(_)),
                        fields,
                    },
            } => Some((*dst, *layout, fields.clone())),
            Stmt::Assign { .. }
            | Stmt::Eval(_)
            | Stmt::Dup(_)
            | Stmt::Drop(_)
            | Stmt::DropKeep { .. }
            | Stmt::ClearUnwind => None,
        })
        .collect();
    if candidates.is_empty() {
        return Ok(false);
    }
    let defs = definitions(f);
    let mut changed = false;
    for (alloc, layout, fields) in candidates {
        if defs.get(alloc.index()).copied() != Some(1) {
            continue;
        }
        let Some(reads) = ctor_reads(f, alloc, layout, &fields, tags)? else {
            continue;
        };
        let mut trial = f.clone();
        let mut subst: FxHashMap<Local, Operand> = FxHashMap::default();
        for b in &mut trial.blocks {
            for s in &mut b.stmts {
                if let Stmt::Assign {
                    dst,
                    value: v @ Rvalue::Alloc { .. },
                } = s
                    && *dst == alloc
                {
                    *v = Rvalue::Use(Operand::Const(Const::Nil));
                }
            }
        }
        for &(block, at, tag) in &reads.tags {
            set_value(
                &mut trial,
                block,
                at,
                Rvalue::Use(Operand::Const(Const::Int(tag))),
            );
        }
        for (block, at, dst, op) in &reads.fields {
            // The definition stays a move, so a use where only a local may
            // stand still reads the field's value. A local stands for the
            // field's local everywhere; a constant is left to the passes
            // that fold a constant's uses, which know where one may stand.
            set_value(&mut trial, *block, *at, Rvalue::Use(op.clone()));
            if let Operand::Local(_) = op {
                subst.insert(*dst, op.clone());
            }
        }
        substitute(&mut trial, &subst);
        if !keeps_releases(&trial, &fields, &reads)? {
            continue;
        }
        *f = trial;
        changed = true;
    }
    Ok(changed)
}

/// How often each local of `f` is defined: as a parameter of the function
/// or of a block, or by a statement.
fn definitions(f: &Function) -> Vec<u32> {
    let mut defs = vec![0_u32; f.locals.len()];
    let mut add = |l: Local| {
        if let Some(n) = defs.get_mut(l.index()) {
            *n = n.saturating_add(1);
        }
    };
    f.params.iter().copied().for_each(&mut add);
    for b in &f.blocks {
        b.params.iter().copied().for_each(&mut add);
        for s in &b.stmts {
            if let Stmt::Assign { dst, .. } = s {
                add(*dst);
            }
        }
    }
    defs
}

/// The reads of `alloc`, an allocation of `layout`, a constructor or a
/// closure, with `fields`, when every use of it reads a constructor's
/// tag, which `tags` knows, or one of its fields into a local of the
/// field's representation; `None` when anything else uses it, or a read
/// would take a closure's code word.
fn ctor_reads(
    f: &Function,
    alloc: Local,
    layout: LayoutRef,
    fields: &[Operand],
    tags: &Known,
) -> Result<Option<CtorReads>, Ice> {
    let mut reads = CtorReads {
        tags: Vec::new(),
        fields: Vec::new(),
    };
    for (id, b) in f.blocks.iter_enumerated() {
        if b.term.uses().iter().any(|&(l, _)| l == alloc) {
            return Ok(None);
        }
        for (at, s) in b.stmts.iter().enumerate() {
            let uses = match s {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => value.uses(),
                Stmt::Dup(l) | Stmt::Drop(l) => vec![(*l, crate::ir::UseKind::Owned)],
                Stmt::DropKeep { local, .. } => vec![(*local, crate::ir::UseKind::Owned)],
                Stmt::ClearUnwind => Vec::new(),
            };
            if !uses.iter().any(|&(l, _)| l == alloc) {
                continue;
            }
            match s {
                Stmt::Assign {
                    value:
                        Rvalue::ReadTag {
                            base: Operand::Local(base),
                            ty,
                        },
                    ..
                } if *base == alloc => {
                    let LayoutRef::Ctor(ctor) = layout else {
                        return Ok(None);
                    };
                    let Some(tag) = tags.tag(ctor, *ty) else {
                        return Ok(None);
                    };
                    reads.tags.push((id, at, i64::from(tag)));
                }
                Stmt::Assign {
                    dst,
                    value:
                        Rvalue::Field {
                            base,
                            layout: read,
                            field,
                        },
                } if *base == alloc && *read == layout => {
                    let Some(op) = usize::try_from(*field).ok().and_then(|i| fields.get(i)) else {
                        return Ok(None);
                    };
                    if matches!(op, Operand::Const(Const::Code(_))) {
                        return Ok(None);
                    }
                    let want = f.local(*dst)?.repr;
                    let repr = match op {
                        Operand::Local(l) => f.local(*l)?.repr,
                        Operand::Const(c) => const_repr(c),
                    };
                    if repr != want {
                        return Ok(None);
                    }
                    reads.fields.push((id, at, *dst, op.clone()));
                }
                Stmt::Assign { .. }
                | Stmt::Eval(_)
                | Stmt::Dup(_)
                | Stmt::Drop(_)
                | Stmt::DropKeep { .. }
                | Stmt::ClearUnwind => return Ok(None),
            }
        }
    }
    Ok(Some(reads))
}

/// The representation of constant `c` as an operand.
const fn const_repr(c: &Const) -> Repr {
    match c {
        Const::Int(_) => Repr::Int,
        Const::Float(_) => Repr::Float,
        Const::Bool(_) => Repr::Bool,
        Const::Sized { repr, .. } => *repr,
        Const::Str(_)
        | Const::Nil
        | Const::StaticCtor(_)
        | Const::FnValue(_)
        | Const::Code(_)
        | Const::EmptyList
        | Const::Static(_)
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => Repr::Value,
    }
}

/// Replaces the value of statement `at` of `block`, an assignment.
fn set_value(f: &mut Function, block: BlockId, at: usize, value: Rvalue) {
    if let Some(Stmt::Assign { value: v, .. }) =
        f.blocks.get_mut(block).and_then(|b| b.stmts.get_mut(at))
    {
        *v = value;
    }
}

/// Whether `trial`, a function whose constructor of `fields` was taken
/// apart at `reads`, keeps every field that may hold a handle live after
/// each read: then the constructor's release, after its last read, never
/// released the last reference of such a field, and nothing moves.
fn keeps_releases(trial: &Function, fields: &[Operand], reads: &CtorReads) -> Result<bool, Ice> {
    let plain = plain_locals(trial);
    let held: Vec<Local> = fields
        .iter()
        .filter_map(Operand::local)
        .filter(|l| plain.get(l.index()) != Some(&true))
        .collect();
    if held.is_empty() {
        return Ok(true);
    }
    let live = Liveness::compute(trial)?;
    let points = reads
        .tags
        .iter()
        .map(|&(b, at, _)| (b, at))
        .chain(reads.fields.iter().map(|&(b, at, _, _)| (b, at)));
    for (block, at) in points {
        let b = trial.block(block)?;
        let mut after: rustc_hash::FxHashSet<Local> = live
            .live_out
            .get(block)
            .map_or_else(Default::default, |s| s.iter().copied().collect());
        after.extend(b.term.uses().into_iter().map(|(l, _)| l));
        for s in b.stmts.iter().skip(at.saturating_add(1)) {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = s {
                after.extend(value.uses().into_iter().map(|(l, _)| l));
            }
        }
        if !held.iter().all(|l| after.contains(l)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Removes unused definitions of pure values and unused block parameters
/// (module docs).
fn dead(f: &mut Function) -> bool {
    let mut counts = use_counts(f);
    let plain = plain_locals(f);
    let mut changed = false;
    loop {
        let mut round = false;
        let live = Liveness::compute(f).ok();
        for index in 0..f.blocks.len() {
            let Some(id) = BlockId::from_usize(index) else {
                break;
            };
            let Some(b) = f.blocks.get_mut(id) else {
                break;
            };
            // The locals used after each statement: later in the block or
            // live out of it.
            let mut after: rustc_hash::FxHashSet<Local> = live
                .as_ref()
                .and_then(|l| l.live_out.get(id))
                .map_or_else(Default::default, |s| s.iter().copied().collect());
            after.extend(b.term.uses().into_iter().map(|(l, _)| l));
            let mut keep = vec![true; b.stmts.len()];
            for (i, s) in b.stmts.iter().enumerate().rev() {
                let Stmt::Assign { dst, value } = s else {
                    if let Stmt::Eval(v) = s {
                        after.extend(v.uses().into_iter().map(|(l, _)| l));
                    }
                    continue;
                };
                // A use of a local that may hold a handle keeps it alive
                // to here (D-205): such a definition stays, as the move
                // that keeps an `@external` wrapper's handle alive across
                // its foreign call does, unless the local is used after it
                // anyway.
                let keeps_alive = value.uses().iter().any(|&(l, _)| {
                    plain.get(l.index()) != Some(&true) && (live.is_none() || !after.contains(&l))
                });
                let removable = count(&counts, *dst) == 0
                    && (is_pure(value) || cannot_panic(value))
                    && !keeps_alive;
                if removable {
                    if let Some(k) = keep.get_mut(i) {
                        *k = false;
                    }
                    for (l, _) in value.uses() {
                        if let Some(c) = counts.get_mut(l.index()) {
                            *c = c.saturating_sub(1);
                        }
                    }
                } else {
                    after.extend(value.uses().into_iter().map(|(l, _)| l));
                    after.extend(value.kept().iter().map(|&(_, p)| p));
                }
            }
            if keep.iter().all(|&k| k) {
                continue;
            }
            let mut n = 0_usize;
            b.retain_stmts(|_| {
                let k = keep.get(n).copied().unwrap_or(true);
                n = n.saturating_add(1);
                k
            });
            round = true;
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
