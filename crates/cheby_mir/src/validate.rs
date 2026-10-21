//! The MIR validator (D1), run after lowering and after RC
//! insertion in debug builds and tests. A failure is an internal compiler
//! error in the driver.
//!
//! Structure: one definition per local, every use dominated by its
//! definition, block parameters matching `goto` arguments, call arguments
//! matching callee signatures (direct callees from
//! [`MirModule::callees`], their borrow masks included, D-715; a borrow
//! mask only on a top-level function's counted parameters),
//! representations of every operand and result,
//! and where the C ABI and tail calls may appear. After RC insertion
//! ([`Stage::Counted`]), ownership: on every path that returns or
//! tail-calls, each counted local is consumed exactly once more than it is
//! duplicated.
//!
//! Unwinding (D-698) is checked for its shape at every stage: unwind edges
//! sorted by statement with at most one per statement and none past the
//! terminator, pads that exist, are not `bb0` and take no parameters,
//! `resume` and `clear_unwind` only in blocks that normal control flow
//! does not reach, `clear_unwind` only in a `precounted` body (D-693),
//! no entry pad in a C-ABI function, which has no entry check, and `ctx`
//! only as the first argument of a foreign call (D-692). Before
//! [`Stage::Unwound`], only a `precounted` body has edges, an entry pad or
//! `resume`; such a body is checked with its counts from
//! [`Stage::Lowered`] on.
//!
//! The CFG of dominance and ownership follows [`Function::successors`]:
//! the normal successors, the unwind edges and the entry pad. For
//! dominance each block is split at its unwind edges ([`Segments`]), so a
//! pad sees only what is defined before the instruction that unwinds to
//! it, never that instruction's result. Ownership is the simulation of
//! [`crate::ownership`]: an unwind edge carries what an unwinding leaves
//! held at its instruction, so each pad's owned set must equal the
//! ownership state at every source, and `resume`, like `return`, leaves
//! nothing held.
//!
//! At [`Stage::Unwound`] also (D-698): each edge's instruction may unwind
//! ([`crate::unwind::stmt_may_unwind`]), every instruction that may unwind
//! in a reachable block has an edge, a pad does not use the result of the
//! instruction that unwinds to it, and every Cheby-ABI function has its
//! entry pad.
//!
//! Reuse (D-716): a reuse token is a counted local that `drop_reuse`
//! defines, so the ownership simulation consumes it once on every path,
//! pads included (D-698), with no rule of its own. The validator adds
//! that the forms appear only after counting, that a token is used only by
//! `alloc_reuse` or `drop`, that both layouts are constructors with fields
//! or tuples of the same object size, and that neither a borrowed local
//! nor a tuple being filled is reused (D-715, D-810).
//!
//! Fusion (D-716, D-850): a keep list (on a `drop_reuse` or a
//! drop specialized by constructor) names counted `field` projections of
//! the released local at their index in its layout, each index once,
//! nothing borrowed; a specialized drop keeps something, is never in a
//! cleanup pad, and a read pins its constructor as reuse's proofs do. The
//! ownership simulation models each as its `dup`s, then the release.
//! With the full ownership check (debug builds), no drop moves later
//! (D-851, `FnCx::check_drop_positions`).
//!
//! A list pattern step read in one call (D-909): a `list_uncons` defines
//! its result and the next statement of its block is the
//! `list_uncons_head` that takes its element, which follows nothing else;
//! `list_uncons_owned` appears only after counting.
//!
//! Canonical pads, blocks `drop _l; goto <pad>` or `resume` that lead
//! only to `resume` and that normal control flow does not reach, are left
//! out of the dominance graph and the representation walk (D-735): what
//! they drop is checked against ownership states, which hold only counted
//! locals that every path to the edge defined.
//!
//! D-774: the pipeline
//! ([`crate::count_and_check_with`]) validates a unit after the unwind
//! pass with the states the pass's own simulation recorded at each site:
//! each edge's pad chain (its drops, then `goto`, then `resume`) must
//! drop exactly what an unwinding leaves held there, and the entry pad's
//! the counted parameters the function owns
//! ([`Function::owned_params`]: a borrowed one holds nothing, D-715). The
//! independent second simulation runs where
//! [`crate::MirOptions::full_ownership_check`] says, by default in debug
//! builds and so in every test run. [`validate`] on its own always runs
//! the full simulation.

pub(crate) mod dom;

use std::collections::BTreeSet;

use cheby_iface::{Ice, Idx};
use rustc_hash::FxHashMap;

use self::dom::Dominance;

use crate::abi::{AbiSig, fixed_sig, is_uniform_sig};
use crate::ir::{
    Abi, ArithOp, Block, BlockId, BuiltinLayout, Callee, CmpOp, Const, FnKey, Function, LayoutRef,
    Local, MirModule, Operand, RuntimeFn, Rvalue, StaticKind, StaticObject, Stmt, TagType,
    Terminator, UnitKind,
};
use crate::layout::LayoutDesc;
use crate::liveness::{Liveness, stmt_def, stmt_value_uses};
use crate::ownership::{self, Observer};
use crate::repr::{CType, ForeignSig, Repr};
use crate::unwind::{Recorded, stmt_may_unwind, terminator_may_unwind};

/// Which invariants hold at this point of the pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// After lowering: no `dup` or `drop` yet.
    Lowered,
    /// After [`crate::rc::insert`]: reference counts balance.
    Counted,
    /// After the unwind pass (D-698): counts balance, every instruction
    /// that may unwind has its edge to the pad of what it leaves held, and
    /// nothing else has one.
    Unwound,
}

/// Checks `module`. Returns every problem found, each prefixed with the
/// function it is in.
///
/// # Errors
///
/// The list of problems, if any.
pub fn validate(module: &MirModule, stage: Stage) -> Result<(), Vec<String>> {
    validate_with(module, stage, None, true)
}

/// [`validate`] at [`Stage::Unwound`] of a unit the unwind pass has just
/// run on, with what its simulation recorded for each function, in order
/// (module documentation). `simulate` adds the independent full
/// simulation.
///
/// # Errors
///
/// The list of problems, if any.
pub(crate) fn validate_recorded(
    module: &MirModule,
    recorded: &[Recorded],
    simulate: bool,
) -> Result<(), Vec<String>> {
    validate_with(module, Stage::Unwound, Some(recorded), simulate)
}

fn validate_with(
    module: &MirModule,
    stage: Stage,
    recorded: Option<&[Recorded]>,
    simulate: bool,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check_module(module, &mut errors);
    if recorded.is_some_and(|r| r.len() != module.functions.len()) {
        errors.push("internal: the unwind pass recorded another number of functions".to_owned());
    }
    // `Callee::Code` keys are looked up once per call: indexed, so a unit
    // with thousands of continuations (D-780) is not searched per call.
    let index = module.function_index();
    for (n, f) in module.functions.iter().enumerate() {
        let mut cx = FnCx {
            module,
            index: &index,
            f,
            errors: Vec::new(),
            recorded: recorded.and_then(|r| r.get(n)),
            simulate,
            pads: Vec::new(),
        };
        if let Err(ice) = cx.check(stage) {
            cx.errors.push(format!("internal: {ice}"));
        }
        if !cx.errors.is_empty() {
            let name = key_label(f.key);
            errors.extend(cx.errors.into_iter().map(|e| format!("{name}: {e}")));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn key_label(key: FnKey) -> String {
    format!("{key:?}")
}

fn check_module(m: &MirModule, errors: &mut Vec<String>) {
    if !m
        .callees
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.def < b.def))
    {
        errors.push("callees are not sorted by def without duplicates".to_owned());
    }
    if !m
        .names
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.def < b.def))
    {
        errors.push("names are not sorted by def without duplicates".to_owned());
    }
    check_layouts(m, errors);
    for (n, s) in m.statics.iter().enumerate() {
        check_static(m, n, s, errors);
    }
    let mut keys = BTreeSet::new();
    for f in &m.functions {
        if !keys.insert(f.key) {
            errors.push(format!("{}: defined twice", key_label(f.key)));
        }
        if let FnKey::Item(def) = f.key
            && let Some(decl) = m.callee(def)
        {
            match f.sig() {
                Ok(sig) if sig == decl.sig => {}
                Ok(sig) => errors.push(format!(
                    "{}: signature {sig:?} differs from its callee declaration {:?}",
                    key_label(f.key),
                    decl.sig
                )),
                Err(ice) => errors.push(format!("{}: internal: {ice}", key_label(f.key))),
            }
        }
    }
}

/// Own layouts are constructors, listed once; foreign ones are sorted,
/// listed once and not own; every layout's counts match its fields.
fn check_layouts(m: &MirModule, errors: &mut Vec<String>) {
    let mut own = BTreeSet::new();
    for d in &m.layouts {
        if !own.insert(d.layout) {
            errors.push(format!("layout {:?} is listed twice", d.layout));
        }
    }
    if !m
        .foreign_layouts
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.layout < b.layout))
    {
        errors.push("foreign layouts are not sorted without duplicates".to_owned());
    }
    for d in &m.foreign_layouts {
        if own.contains(&d.layout) {
            errors.push(format!("layout {:?} is both own and foreign", d.layout));
        }
    }
    for d in m.layouts.iter().chain(&m.foreign_layouts) {
        check_layout_desc(d, errors);
    }
}

fn check_layout_desc(d: &LayoutDesc, errors: &mut Vec<String>) {
    if !matches!(d.layout, LayoutRef::Ctor(_)) {
        errors.push(format!("layout {:?} is not a constructor", d.layout));
    }
    let values = d.fields.iter().filter(|f| f.repr == Repr::Value).count();
    let raws = d.fields.len().saturating_sub(values);
    if usize::try_from(d.value_fields).ok() != Some(values)
        || usize::try_from(d.raw_fields).ok() != Some(raws)
    {
        errors.push(format!(
            "layout {:?} counts {} value and {} raw fields, but has {values} and {raws}",
            d.layout, d.value_fields, d.raw_fields
        ));
    }
}

/// A static's id is its position, its name is unique, its layout fits
/// its kind, and its fields are uniform constants (D-523, plan §5.4).
fn check_static(m: &MirModule, n: usize, s: &StaticObject, errors: &mut Vec<String>) {
    let label = format!("static#{n}");
    if s.id.index() != n {
        errors.push(format!("{label}: has id {}", s.id.as_u32()));
    }
    // The descriptor of `dyn Show` carries the builtin `Show`'s empty
    // module path (D-603).
    let builtin_dyn = s.kind == StaticKind::Desc && s.name.starts_with("dyn$");
    if s.name.is_empty() || (s.module.is_empty() && !builtin_dyn) {
        errors.push(format!("{label}: needs a module path and a name"));
    }
    if m.statics
        .iter()
        .take(n)
        .any(|o| o.kind == s.kind && o.module == s.module && o.name == s.name)
    {
        errors.push(format!("{label}: {}${} is defined twice", s.module, s.name));
    }
    let fits = matches!(
        (s.kind, s.layout),
        (StaticKind::Dict, LayoutRef::Dict { .. })
            | (
                StaticKind::Desc,
                LayoutRef::Desc { .. } | LayoutRef::DescNominal
            )
            | (StaticKind::FnValue, LayoutRef::Closure(_))
    );
    if !fits {
        errors.push(format!(
            "{label}: a {} static with layout {:?}",
            s.kind.name(),
            s.layout
        ));
    }
    let want = s.layout.uniform_fields().unwrap_or(0);
    if u64::try_from(s.fields.len()).ok() != Some(want) {
        errors.push(format!(
            "{label}: {} fields, its layout has {want}",
            s.fields.len()
        ));
    }
    for (i, c) in s.fields.iter().enumerate() {
        let code_slot = i == 0 && s.kind == StaticKind::FnValue;
        match c {
            Const::Code(key) if code_slot && key.is_uniform() => {}
            Const::Code(key) => {
                errors.push(format!("{label}: code {key:?} as field {i}"));
            }
            _ if code_slot => errors.push(format!("{label}: a function value without its code")),
            Const::Static(id) if id.index() == n || m.static_object(*id).is_none() => {
                errors.push(format!("{label}: field {i} is static#{}", id.as_u32()));
            }
            Const::Str(id) if s.strings.get(*id).is_none() => {
                errors.push(format!("{label}: unknown string str#{}", id.as_u32()));
            }
            c if c.repr() != Repr::Value => {
                errors.push(format!("{label}: field {i} is a raw {}", c.repr()));
            }
            _ => {}
        }
    }
}

struct FnCx<'a> {
    module: &'a MirModule,
    /// [`MirModule::function_index`] of `module`.
    index: &'a FxHashMap<FnKey, usize>,
    f: &'a Function,
    errors: Vec<String>,
    /// What the unwind pass's simulation recorded, when the pipeline
    /// validates right after it: the pad chains are checked against it.
    recorded: Option<&'a Recorded>,
    /// Whether to run the full ownership simulation.
    simulate: bool,
    /// By block index: a canonical pad (module documentation), left out
    /// of the dominance graph and the representation walk.
    pads: Vec<bool>,
}

/// Where a local is defined: its block and position (`None` for a
/// parameter, defined before the first statement).
type DefSite = (BlockId, Option<usize>);

/// A release with a keep list ([`crate::ir::Keep`]): its block, the local
/// it releases, its layout and the list.
type KeepSite<'a> = (BlockId, Local, LayoutRef, &'a [(u32, Local)]);

impl FnCx<'_> {
    fn err(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    fn check(&mut self, stage: Stage) -> Result<(), Ice> {
        self.check_abi()?;
        let Some(defs) = self.check_definitions() else {
            return Ok(());
        };
        self.check_call_locs(&defs);
        if !self.check_cfg() {
            return Ok(());
        }
        let normal = normally_reachable(self.f);
        self.check_unwind_shape(stage, &normal);
        if self.counted(stage) {
            self.pads = canonical_pads(self.f, &normal)?;
        }
        let reachable = (stage == Stage::Unwound).then(|| reachable_with_unwinding(self.f));
        if let Some(reachable) = &reachable {
            self.check_unwind_rules(reachable);
        }
        self.check_dominance(&defs)?;
        self.check_reprs(stage)?;
        self.check_init_fields();
        self.check_uncons(stage);
        self.check_reuse(stage)?;
        if self.counted(stage) && self.errors.is_empty() {
            if let (Some(recorded), Some(reachable)) = (self.recorded, &reachable) {
                self.check_pad_chains(recorded, reachable)?;
            }
            if self.simulate {
                self.check_ownership()?;
                self.check_drop_positions()?;
            }
        }
        Ok(())
    }

    /// No drop moves later (D-716, D-205, D-851): in a
    /// block normal control flow reaches, between a counted local's last
    /// use (or its definition) and each statement that releases it (`drop`,
    /// `drop_reuse`, a specialized drop), nothing runs but `dup`, `drop`,
    /// `drop_reuse`, specialized drops and block entry. Naive placement
    /// satisfies it (each drop follows its local's last instruction, or
    /// opens the successor of an edge), and so must every pass after
    /// counting. It covers every counted local, not only those that may
    /// hold a handle, so it needs no type facts. A reuse token's own `drop`
    /// is exempt: the token holds no reference and no handle (a childless
    /// block, or 0), and `reuse` holds it across calls into the arms that
    /// rebuild (D-839); the drop of the matched value is
    /// its `drop_reuse`, which the rule covers. Cleanup pads are exempt:
    /// they release what an unwinding leaves held (D-698). Run only with
    /// the full ownership check, so in debug builds (D-774): release
    /// builds do not pay for it (D-776).
    fn check_drop_positions(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let normal = normally_reachable(f);
        let n = f.blocks.len();
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        let mut tokens: BTreeSet<Local> = BTreeSet::new();
        // By block: the nearest statement before each position that runs
        // code, and where each local is used or defined.
        let mut runs_before: Vec<Vec<Option<usize>>> = Vec::with_capacity(n);
        let mut events: Vec<FxHashMap<Local, Vec<usize>>> = Vec::with_capacity(n);
        for (id, block) in f.blocks.iter_enumerated() {
            if normal.get(id.index()).copied().unwrap_or(false) {
                for succ in block.term.successors() {
                    let list = preds
                        .get_mut(succ.index())
                        .ok_or_else(|| Ice::new("a successor out of range"))?;
                    if !list.contains(&id) {
                        list.push(id);
                    }
                }
            }
            let mut before = Vec::with_capacity(block.stmts.len().saturating_add(1));
            let mut last = None;
            let mut here: FxHashMap<Local, Vec<usize>> = FxHashMap::default();
            for (j, stmt) in block.stmts.iter().enumerate() {
                before.push(last);
                if !releases_only(stmt) {
                    last = Some(j);
                }
                if let Stmt::Assign {
                    dst,
                    value: Rvalue::DropReuse { .. },
                } = stmt
                {
                    tokens.insert(*dst);
                }
                for l in stmt_value_uses(f, stmt)?.into_iter().chain(stmt_def(stmt)) {
                    here.entry(l).or_default().push(j);
                }
            }
            before.push(last);
            for (l, _) in block.term.uses() {
                here.entry(l).or_default().push(block.stmts.len());
            }
            runs_before.push(before);
            events.push(here);
        }
        // Where a local's value is still read later, a path that leaves it
        // dead on one edge (a branch whose other successor reads it) drops
        // it at the start of that edge's block: naive placement's edge
        // drops (D-205). So a walk that reaches the end of a predecessor
        // where the value is live stops there.
        let values = Liveness::compute_values(f)?;
        let cx = DropWalk {
            f,
            preds: &preds,
            runs_before: &runs_before,
            events: &events,
        };
        // Block ends already walked for a local, so each is walked once.
        let mut walked: BTreeSet<(BlockId, Local)> = BTreeSet::new();
        for (id, block) in f.blocks.iter_enumerated() {
            if !normal.get(id.index()).copied().unwrap_or(false) {
                continue;
            }
            for (j, stmt) in block.stmts.iter().enumerate() {
                let Some(x) = released(stmt) else {
                    continue;
                };
                if tokens.contains(&x) {
                    continue;
                }
                let mut work = Vec::new();
                match cx.back(id, j, x) {
                    Back::Ok => {}
                    Back::Ran(s) => self.err(format!(
                        "bb{} statement {j} releases _{} after bb{} statement {s} runs past its last use (D-716, D-205)",
                        id.as_u32(),
                        x.as_u32(),
                        id.as_u32()
                    )),
                    Back::Preds(b) => work.push(b),
                }
                while let Some(b) = work.pop() {
                    for &p in cx.preds.get(b.index()).into_iter().flatten() {
                        if !walked.insert((p, x))
                            || values.live_out.get(p).is_some_and(|out| out.contains(&x))
                        {
                            continue;
                        }
                        // Past the end: the terminator's uses count.
                        let end = f.block(p)?.stmts.len().saturating_add(1);
                        match cx.back(p, end, x) {
                            Back::Ok => {}
                            Back::Ran(s) => self.err(format!(
                                "bb{} statement {j} releases _{}, but on the path from bb{} statement {s} runs past its last use (D-716, D-205)",
                                id.as_u32(),
                                x.as_u32(),
                                p.as_u32()
                            )),
                            Back::Preds(up) => work.push(up),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether block `id` is a canonical pad.
    fn is_pad(&self, id: BlockId) -> bool {
        self.pads.get(id.index()).copied().unwrap_or(false)
    }

    /// The pad check of D-774 (module
    /// documentation): every edge of a block the simulation reached, and
    /// the entry pad, against the recorded states. A pad of a generated
    /// body (D-744) that is not canonical was checked by the pass's own
    /// simulation, which followed that body's edges.
    fn check_pad_chains(&mut self, recorded: &Recorded, reachable: &[bool]) -> Result<(), Ice> {
        let f = self.f;
        if let Some(pad) = f.entry_unwind {
            // Borrowed parameters hold nothing to release (D-715).
            let params = f.owned_params()?;
            self.check_pad_chain(pad, &params, || "the entry check".to_owned());
        }
        for (id, block) in f.blocks.iter_enumerated() {
            if !reachable.get(id.index()).copied().unwrap_or(false) {
                continue;
            }
            for &(at, pad) in &block.unwind {
                let Ok(i) = usize::try_from(at) else {
                    continue;
                };
                let what = || format!("bb{} statement {i}", id.as_u32());
                match recorded.state(id, i) {
                    Some(held) => self.check_pad_chain(pad, held, what),
                    None => self.err(format!(
                        "{} has an unwind edge the unwind pass did not record (D-698)",
                        what()
                    )),
                }
            }
        }
        Ok(())
    }

    /// The chain from `pad` drops exactly the multiset `held` (ascending,
    /// with repeats) and resumes, in any order.
    fn check_pad_chain(&mut self, pad: BlockId, held: &[Local], what: impl Fn() -> String) {
        let f = self.f;
        if !self.is_pad(pad) {
            if !f.precounted {
                self.err(format!(
                    "{} unwinds to bb{}, which is not a chain of drops ending in resume (D-698)",
                    what(),
                    pad.as_u32()
                ));
            }
            return;
        }
        let mut dropped = Vec::new();
        let mut at = pad;
        // A canonical pad leads to `resume` without a cycle.
        while let Some(block) = f.blocks.get(at) {
            match (block.stmts.as_slice(), &block.term) {
                ([Stmt::Drop(l)], Terminator::Goto { target, .. }) => {
                    dropped.push(*l);
                    at = *target;
                }
                _ => break,
            }
        }
        // The order is the function's pad direction (D-777): an ascending
        // function's chains drop the newest local first and come out
        // descending, so they are reversed; a descending function's drop the
        // oldest first and come out ascending, so they are kept. Only a
        // chain in neither order (a nested function's, or a generated
        // body's, D-744) is sorted.
        if !dropped.is_sorted() {
            dropped.reverse();
            if !dropped.is_sorted() {
                dropped.sort_unstable();
            }
        }
        if dropped != held {
            self.err(format!(
                "{} unwinds to bb{}, which drops {}, but an unwinding there leaves {} held (D-698)",
                what(),
                pad.as_u32(),
                ownership::set_text(&dropped),
                ownership::set_text(held)
            ));
        }
    }

    /// The rules of [`Stage::Unwound`] beyond the shape (D-698): see the
    /// module documentation. Pad sets are checked with the ownership.
    fn check_unwind_rules(&mut self, reachable: &[bool]) {
        let f = self.f;
        if f.abi == Abi::Cheby && f.entry_unwind.is_none() {
            self.err("a Cheby-ABI function needs its entry pad after the unwind pass (D-698)");
        }
        for (id, block) in f.blocks.iter_enumerated() {
            let len = block.stmts.len();
            for &(at, pad) in &block.unwind {
                let Ok(i) = usize::try_from(at) else {
                    continue;
                };
                let what = match block.stmts.get(i) {
                    Some(stmt) if !stmt_may_unwind(stmt) => Some("statement"),
                    None if i == len && !terminator_may_unwind(&block.term) => Some("terminator"),
                    _ => None,
                };
                if let Some(what) = what {
                    self.err(format!(
                        "bb{} statement {i} has an unwind edge, but this {what} cannot unwind (D-698)",
                        id.as_u32()
                    ));
                }
                if let Some(Stmt::Assign { dst, .. }) = block.stmts.get(i)
                    && f.blocks.get(pad).is_some_and(|p| block_uses(p, *dst))
                {
                    self.err(format!(
                        "bb{} uses _{}, the result of bb{} statement {i}, which an unwinding leaves undefined (D-698)",
                        pad.as_u32(),
                        dst.as_u32(),
                        id.as_u32()
                    ));
                }
            }
            if !reachable.get(id.index()).copied().unwrap_or(false) {
                continue;
            }
            let mut edges = block.unwind.iter().map(|&(at, _)| at).peekable();
            for (n, stmt) in block.stmts.iter().enumerate() {
                while edges
                    .next_if(|&at| usize::try_from(at).is_ok_and(|at| at < n))
                    .is_some()
                {}
                let has_edge = edges.peek().and_then(|&at| usize::try_from(at).ok()) == Some(n);
                if stmt_may_unwind(stmt) && !has_edge {
                    self.err(format!(
                        "bb{} statement {n} may unwind but has no unwind edge (D-698)",
                        id.as_u32()
                    ));
                }
            }
            if terminator_may_unwind(&block.term) && block.unwind_target(len).is_none() {
                self.err(format!(
                    "bb{} terminator may unwind but has no unwind edge (D-698)",
                    id.as_u32()
                ));
            }
        }
    }

    /// Whether the function has its counts at `stage`: after RC, or a
    /// `precounted` body from lowering on (D-693).
    fn counted(&self, stage: Stage) -> bool {
        stage != Stage::Lowered || self.f.precounted
    }

    /// The shape of unwinding (D-698): see the module documentation.
    fn check_unwind_shape(&mut self, stage: Stage, normal: &[bool]) {
        let f = self.f;
        let early = stage != Stage::Unwound && !f.precounted;
        // `what` is formatted only for a message.
        let pad_ok = |cx: &mut Self, what: &dyn Fn() -> String, pad: BlockId| {
            let problem = match f.blocks.get(pad) {
                None => format!("unknown bb{}", pad.as_u32()),
                Some(_) if pad.as_u32() == 0 => "the entry block".to_owned(),
                Some(target) if !target.params.is_empty() => {
                    format!("bb{}, which takes parameters", pad.as_u32())
                }
                Some(_) => return,
            };
            cx.err(format!("{} unwinds to {problem}", what()));
        };
        if let Some(pad) = f.entry_unwind {
            if early {
                self.err("an entry pad before the unwind pass");
            }
            // Entry thunks and trampolines have no entry check to unwind
            // from (D-752).
            if f.abi == Abi::C {
                self.err("a C-ABI function has no entry check, so no entry pad");
            }
            pad_ok(self, &|| "the entry check".to_owned(), pad);
        }
        for (id, block) in f.blocks.iter_enumerated() {
            if early && !block.unwind.is_empty() {
                self.err(format!(
                    "bb{} has unwind edges before the unwind pass",
                    id.as_u32()
                ));
            }
            if !block
                .unwind
                .windows(2)
                .all(|w| matches!(w, [a, b] if a.0 < b.0))
            {
                self.err(format!(
                    "the unwind edges of bb{} are not sorted by statement without duplicates",
                    id.as_u32()
                ));
            }
            for &(at, pad) in &block.unwind {
                if usize::try_from(at).map_or(true, |at| at > block.stmts.len()) {
                    self.err(format!(
                        "bb{} has an unwind edge for statement {at}, past its terminator",
                        id.as_u32()
                    ));
                }
                pad_ok(self, &|| format!("bb{} statement {at}", id.as_u32()), pad);
            }
            if early && block.term == Terminator::Resume {
                self.err(format!("bb{} resumes before the unwind pass", id.as_u32()));
            }
        }
        // `resume` and `clear_unwind` run only while unwinding: in blocks
        // that only pads lead to.
        for (id, block) in f.blocks.iter_enumerated() {
            if !normal.get(id.index()).copied().unwrap_or(false) {
                continue;
            }
            if block.term == Terminator::Resume {
                self.err(format!(
                    "bb{} resumes, but normal control flow reaches it",
                    id.as_u32()
                ));
            }
            if block.stmts.contains(&Stmt::ClearUnwind) {
                self.err(format!(
                    "bb{} clears the unwind flag, but normal control flow reaches it",
                    id.as_u32()
                ));
            }
        }
    }

    fn check_abi(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let entries = self.module.unit == UnitKind::Entries;
        match (f.key, f.abi) {
            (FnKey::Entry(_), Abi::C) if !entries => {
                self.err("entry thunks belong in an entries unit");
            }
            // In an entries unit, the static dictionary of an error
            // type's `show` brings value shims, tuple functions and `dyn`
            // forwarders, which depend on nothing but their key (D-523).
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::C)
            | (
                FnKey::ValueShim(_) | FnKey::TupleDictFn { .. } | FnKey::DynForward { .. },
                Abi::Cheby,
            ) => {}
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::Cheby) => {
                self.err("entry thunks and trampolines use the C ABI");
            }
            (_, Abi::C) => self.err("only entry thunks and trampolines use the C ABI"),
            (_, Abi::Cheby) if entries => {
                self.err(
                    "an entries unit holds only entry thunks, trampolines, value shims, tuple functions and `dyn` forwarders",
                );
            }
            (_, Abi::Cheby) => {}
        }
        match f.abi {
            Abi::Cheby => {
                if f.ret != Repr::Value {
                    self.err("a Cheby-ABI function returns a uniform value");
                }
                if f.entry_loc.is_none() {
                    self.err("a Cheby-ABI function needs its entry-check location");
                }
            }
            Abi::C => {
                if f.entry_loc.is_some() {
                    self.err("a C-ABI function has no entry check");
                }
            }
        }
        if let Some(loc) = f.entry_loc
            && f.strings.get(loc).is_none()
        {
            self.err(format!("unknown string str#{}", loc.as_u32()));
        }
        self.check_borrow()?;
        if let FnKey::Continuation { .. } = f.key {
            match f.params.first() {
                Some(&p) if f.local(p)?.repr == Repr::Value => {}
                _ => self.err("a continuation takes the value being built first (D-780)"),
            }
        }
        if let Some(fixed) = fixed_sig(f.key) {
            let sig = f.sig()?;
            if sig != fixed {
                self.err(format!("signature {sig:?}, expected {fixed:?}"));
            }
        }
        if f.key.is_uniform() {
            let sig = f.sig()?;
            if !is_uniform_sig(&sig) {
                self.err(format!(
                    "signature {sig:?}, expected the uniform convention (D-453)"
                ));
            }
        }
        Ok(())
    }

    /// The function's borrow mask (D-715): only a top-level Cheby function
    /// that is not `precounted` borrows, and only counted parameters it
    /// has.
    fn check_borrow(&mut self) -> Result<(), Ice> {
        let f = self.f;
        if f.borrow.is_owned() {
            return Ok(());
        }
        if !matches!(f.key, FnKey::Item(_)) || f.abi != Abi::Cheby || f.precounted {
            self.err(format!(
                "borrows {:?}, but only a top-level function that is not precounted may borrow (D-715)",
                f.borrow
            ));
        }
        for i in f.borrow.indices() {
            match f.params.get(i) {
                Some(&p) if f.local(p)?.repr == Repr::Value => {}
                Some(&p) => self.err(format!(
                    "borrows parameter {i} (_{}), which is not counted (D-715)",
                    p.as_u32()
                )),
                None => self.err(format!(
                    "borrows parameter {i}, but has {} (D-715)",
                    f.params.len()
                )),
            }
        }
        Ok(())
    }

    /// The tuples filled one field at a time (D-720,
    /// D-810): each `init_field` stores into a fresh
    /// object, one this function made with `alloc_placeholder` of the same
    /// layout, or the first parameter of a continuation (D-780), which
    /// takes the partly filled tuple, always with one layout; each field of
    /// an object is stored once; and no `dup` ever shares an object that is
    /// being filled, so no other reference sees a store.
    fn check_init_fields(&mut self) {
        let f = self.f;
        let mut fresh: FxHashMap<Local, LayoutRef> = FxHashMap::default();
        for block in &f.blocks {
            for stmt in &block.stmts {
                if let Stmt::Assign {
                    dst,
                    value: Rvalue::AllocPlaceholder { layout },
                } = stmt
                {
                    fresh.insert(*dst, *layout);
                }
            }
        }
        let carried = match (f.key, f.params.first()) {
            (FnKey::Continuation { .. }, Some(&p)) => Some(p),
            _ => None,
        };
        let mut carried_layout: Option<LayoutRef> = None;
        let mut stored: BTreeSet<(Local, u32)> = BTreeSet::new();
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                let Stmt::Eval(Rvalue::InitField {
                    obj, layout, field, ..
                }) = stmt
                else {
                    continue;
                };
                let fresh_here = match fresh.get(obj) {
                    Some(made) => made == layout,
                    None if Some(*obj) == carried => {
                        *carried_layout.get_or_insert(*layout) == *layout
                    }
                    None => false,
                };
                if !fresh_here {
                    self.err(format!(
                        "bb{} stores field {field} of _{}, which is not a fresh {layout:?} being filled (D-720)",
                        id.as_u32(),
                        obj.as_u32()
                    ));
                }
                if !stored.insert((*obj, *field)) {
                    self.err(format!(
                        "bb{} stores field {field} of _{} a second time (D-720)",
                        id.as_u32(),
                        obj.as_u32()
                    ));
                }
            }
        }
        if stored.is_empty() {
            return;
        }
        let filled: BTreeSet<Local> = stored.iter().map(|&(obj, _)| obj).collect();
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                if let Stmt::Dup(l) = stmt
                    && filled.contains(l)
                {
                    self.err(format!(
                        "bb{} shares _{}, a tuple being filled field by field (D-720)",
                        id.as_u32(),
                        l.as_u32()
                    ));
                }
            }
        }
    }

    /// A list pattern step read in one call (D-909): each `list_uncons`
    /// and `list_uncons_owned` defines its result and is followed at once,
    /// in its block, by the `list_uncons_head` that takes the element it
    /// kept, and each `list_uncons_head` follows one; `list_uncons_owned`,
    /// which the RC pass emits, only once counts are placed.
    fn check_uncons(&mut self, stage: Stage) {
        let f = self.f;
        let counted = self.counted(stage);
        for (id, block) in f.blocks.iter_enumerated() {
            let funcs: Vec<Option<(RuntimeFn, bool)>> = block
                .stmts
                .iter()
                .map(|stmt| match stmt {
                    Stmt::Assign {
                        value: Rvalue::CallRuntime { func, .. },
                        ..
                    } => Some((*func, true)),
                    Stmt::Eval(Rvalue::CallRuntime { func, .. }) => Some((*func, false)),
                    _ => None,
                })
                .collect();
            for (j, &call) in funcs.iter().enumerate() {
                let Some((func, assigned)) = call else {
                    continue;
                };
                let uncons = matches!(func, RuntimeFn::ListUncons | RuntimeFn::ListUnconsOwned);
                if !uncons && func != RuntimeFn::ListUnconsHead {
                    continue;
                }
                if !assigned {
                    self.err(format!(
                        "bb{} statement {j}: call_runtime {} without its result (D-909)",
                        id.as_u32(),
                        func.name()
                    ));
                }
                if func == RuntimeFn::ListUnconsOwned && !counted {
                    self.err(format!(
                        "bb{} statement {j}: list_uncons_owned before counting (D-909)",
                        id.as_u32()
                    ));
                }
                let paired = if uncons {
                    matches!(
                        j.checked_add(1).and_then(|k| funcs.get(k)),
                        Some(Some((RuntimeFn::ListUnconsHead, _)))
                    )
                } else {
                    matches!(
                        j.checked_sub(1).and_then(|k| funcs.get(k)),
                        Some(Some((
                            RuntimeFn::ListUncons | RuntimeFn::ListUnconsOwned,
                            _
                        )))
                    )
                };
                if !paired {
                    let what = if uncons {
                        "is not followed at once by list_uncons_head"
                    } else {
                        "does not follow a list_uncons at once"
                    };
                    self.err(format!(
                        "bb{} statement {j}: call_runtime {} {what} (D-909)",
                        id.as_u32(),
                        func.name()
                    ));
                }
            }
        }
    }

    /// Whether `layout` may be reused (D-716): a constructor with fields
    /// whose layout the unit knows, or a tuple, with a fixed object size
    /// ([`crate::layout::object_size`]). Reports it otherwise.
    fn check_reuse_layout(&mut self, what: &str, layout: LayoutRef) -> bool {
        let shape = match layout {
            LayoutRef::Ctor(_) => !self
                .module
                .layout(layout)
                .is_some_and(|d| d.fields.is_empty()),
            LayoutRef::Tuple(n) => n > 0,
            _ => false,
        };
        if shape && crate::layout::object_size(self.module, layout).is_some() {
            return true;
        }
        self.err(format!(
            "{what} of {layout:?}, which is not a constructor with fields or a tuple (D-716)"
        ));
        false
    }

    /// The reuse tokens (D-716, D-698, D-838): only after
    /// reference counting; a `drop_reuse` gives up an owned local that is
    /// not borrowed (D-715, D-807) and not a tuple being filled (D-810),
    /// and defines a token, a local without a source type; a token is used
    /// only as the token of an `alloc_reuse` or by a `drop`, never
    /// `dup`'d, passed, returned, stored or a `goto` argument; an
    /// `alloc_reuse` takes a token whose layout has the same object size
    /// as its own. That each token is consumed once on every path, pads
    /// included, is the ownership simulation's rule for every counted
    /// local.
    fn check_reuse(&mut self, stage: Stage) -> Result<(), Ice> {
        let f = self.f;
        let mut tokens: FxHashMap<Local, LayoutRef> = FxHashMap::default();
        let mut sinks: Vec<(BlockId, Local, LayoutRef)> = Vec::new();
        let mut sources: Vec<(BlockId, Local)> = Vec::new();
        let mut keeps: Vec<KeepSite<'_>> = Vec::new();
        let mut specialized: Vec<(BlockId, Local, LayoutRef)> = Vec::new();
        let mut normal: Option<Vec<bool>> = None;
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign {
                        dst,
                        value:
                            Rvalue::DropReuse {
                                local,
                                layout,
                                keep,
                            },
                    } => {
                        tokens.insert(*dst, *layout);
                        sources.push((id, *local));
                        if !keep.is_empty() {
                            keeps.push((id, *local, *layout, keep.as_slice()));
                        }
                        if f.local(*dst)?.ty.is_some() {
                            self.err(format!(
                                "bb{} defines the reuse token _{} with a source type (D-716)",
                                id.as_u32(),
                                dst.as_u32()
                            ));
                        }
                    }
                    Stmt::Assign {
                        value: Rvalue::AllocReuse { token, layout, .. },
                        ..
                    } => sinks.push((id, *token, *layout)),
                    // A specialized drop (D-850): what a source
                    // may not be, a non-empty keep list, and never in a
                    // cleanup pad (D-735).
                    Stmt::DropKeep {
                        local,
                        layout,
                        keep,
                    } => {
                        sources.push((id, *local));
                        keeps.push((id, *local, *layout, keep.as_slice()));
                        specialized.push((id, *local, *layout));
                        let normal = normal.get_or_insert_with(|| normally_reachable(f));
                        self.check_specialized_site(id, *local, keep, normal);
                    }
                    _ => continue,
                }
                if !self.counted(stage) {
                    self.err(format!(
                        "bb{} reuses before reference-count insertion (D-716)",
                        id.as_u32()
                    ));
                }
            }
        }
        if tokens.is_empty() && sinks.is_empty() && keeps.is_empty() {
            return Ok(());
        }
        let borrowed = f.borrowed_locals()?;
        self.check_keeps(&keeps, &borrowed)?;
        self.check_pinned(&specialized)?;
        let filled: BTreeSet<Local> = f
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter_map(|s| match s {
                Stmt::Eval(Rvalue::InitField { obj, .. }) => Some(*obj),
                _ => None,
            })
            .chain(match (f.key, f.params.first()) {
                (FnKey::Continuation { .. }, Some(&p)) => Some(p),
                _ => None,
            })
            .collect();
        for (id, local) in sources {
            if crate::liveness::is_borrowed_local(&borrowed, local) {
                self.err(format!(
                    "bb{} reuses _{}, which is borrowed (D-715, D-716)",
                    id.as_u32(),
                    local.as_u32()
                ));
            }
            if filled.contains(&local) {
                self.err(format!(
                    "bb{} reuses _{}, a tuple being filled field by field (D-810, D-716)",
                    id.as_u32(),
                    local.as_u32()
                ));
            }
        }
        self.check_sinks(&tokens, &sinks);
        self.check_token_uses(&tokens);
        Ok(())
    }

    /// Each `alloc_reuse` of `sinks` (block, token, layout) takes a token
    /// (a key of `tokens`, with its `drop_reuse`'s layout) of its own
    /// object size (D-716).
    fn check_sinks(
        &mut self,
        tokens: &FxHashMap<Local, LayoutRef>,
        sinks: &[(BlockId, Local, LayoutRef)],
    ) {
        for &(id, token, layout) in sinks {
            let Some(&from) = tokens.get(&token) else {
                self.err(format!(
                    "bb{} allocates with _{}, which is not a reuse token (D-716)",
                    id.as_u32(),
                    token.as_u32()
                ));
                continue;
            };
            let old = crate::layout::object_size(self.module, from);
            let new = crate::layout::object_size(self.module, layout);
            if old.is_none() || old != new {
                self.err(format!(
                    "bb{} reuses a {from:?} of {old:?} bytes for a {layout:?} of {new:?} bytes (D-716)",
                    id.as_u32()
                ));
            }
        }
    }

    /// A specialized drop of `local` in block `id` keeps something and is
    /// in a block normal control flow reaches (`normal`, by block index):
    /// never in a cleanup pad (D-735, D-850).
    fn check_specialized_site(
        &mut self,
        id: BlockId,
        local: Local,
        keep: &[(u32, Local)],
        normal: &[bool],
    ) {
        if keep.is_empty() {
            self.err(format!(
                "bb{} specializes a drop of _{} that keeps nothing (D-716)",
                id.as_u32(),
                local.as_u32()
            ));
        }
        if !normal.get(id.index()).copied().unwrap_or(false) {
            self.err(format!(
                "bb{} specializes a drop in a cleanup pad (D-735, D-716)",
                id.as_u32()
            ));
        }
    }

    /// Each specialized drop in `specialized` (block, local, layout)
    /// releases the fields of its layout, so a read must pin the
    /// constructor there, as for reuse ([`crate::fuse::Pins`]): never a
    /// shared field's read (D-071, D-850).
    fn check_pinned(&mut self, specialized: &[(BlockId, Local, LayoutRef)]) -> Result<(), Ice> {
        if specialized.is_empty() {
            return Ok(());
        }
        let pins = crate::fuse::Pins::of(self.module, self.f)?;
        for &(id, x, layout) in specialized {
            if !pins.pins(self.module, x, layout, id) {
                self.err(format!(
                    "bb{} specializes a drop of _{} as {layout:?}, which no read pins (D-071, D-716)",
                    id.as_u32(),
                    x.as_u32()
                ));
            }
        }
        Ok(())
    }

    /// The keep lists (fusion, D-716, D-850) of the releases
    /// in `keeps`: each kept local is a counted `field x.i of L` projection
    /// of the released local `x` at its index `i` in the release's layout
    /// `L`, the indices are distinct, and no kept local is borrowed (D-715,
    /// D-807: a borrowed projection holds no reference to move).
    fn check_keeps(&mut self, keeps: &[KeepSite<'_>], borrowed: &[bool]) -> Result<(), Ice> {
        if keeps.is_empty() {
            return Ok(());
        }
        let f = self.f;
        let mut projections: FxHashMap<Local, (Local, LayoutRef, u32)> = FxHashMap::default();
        for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
            if let Stmt::Assign {
                dst,
                value:
                    Rvalue::Field {
                        base,
                        layout,
                        field,
                    },
            } = stmt
            {
                projections.insert(*dst, (*base, *layout, *field));
            }
        }
        for &(id, x, layout, keep) in keeps {
            let mut seen = BTreeSet::new();
            for &(i, p) in keep {
                if !seen.insert(i) {
                    self.err(format!(
                        "bb{} keeps field {i} of _{} twice (D-716)",
                        id.as_u32(),
                        x.as_u32()
                    ));
                }
                if projections.get(&p) != Some(&(x, layout, i)) {
                    self.err(format!(
                        "bb{} keeps _{} as field {i} of _{}, but it is not that field of {layout:?} (D-716)",
                        id.as_u32(),
                        p.as_u32(),
                        x.as_u32()
                    ));
                }
                if f.local(p)?.repr != Repr::Value {
                    self.err(format!(
                        "bb{} keeps the uncounted _{} (D-716)",
                        id.as_u32(),
                        p.as_u32()
                    ));
                }
                if crate::liveness::is_borrowed_local(borrowed, p) {
                    self.err(format!(
                        "bb{} keeps _{}, which is borrowed (D-715, D-716)",
                        id.as_u32(),
                        p.as_u32()
                    ));
                }
            }
        }
        Ok(())
    }

    /// A reuse token (a key of `tokens`) is used only as the token of an
    /// `alloc_reuse` or by a `drop` ([`FnCx::check_reuse`]).
    fn check_token_uses(&mut self, tokens: &FxHashMap<Local, LayoutRef>) {
        let f = self.f;
        let is_token = |l: &Local| tokens.contains_key(l);
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                let misused: Vec<Local> = match stmt {
                    Stmt::Drop(_) | Stmt::ClearUnwind => Vec::new(),
                    Stmt::Dup(l) => vec![*l],
                    Stmt::DropKeep { local, keep, .. } => std::iter::once(*local)
                        .chain(keep.iter().map(|&(_, p)| p))
                        .collect(),
                    Stmt::Assign {
                        value: Rvalue::AllocReuse { fields, .. },
                        ..
                    } => fields.iter().filter_map(Operand::local).collect(),
                    Stmt::Assign { value, .. } | Stmt::Eval(value) => value
                        .uses()
                        .into_iter()
                        .map(|(l, _)| l)
                        .chain(value.kept().iter().map(|&(_, p)| p))
                        .collect(),
                };
                for l in misused.into_iter().filter(is_token) {
                    self.err(format!(
                        "bb{} uses the reuse token _{} other than by alloc_reuse or drop (D-716)",
                        id.as_u32(),
                        l.as_u32()
                    ));
                }
            }
            for (l, _) in block.term.uses().into_iter().filter(|(l, _)| is_token(l)) {
                self.err(format!(
                    "bb{} passes on the reuse token _{} (D-716, D-733)",
                    id.as_u32(),
                    l.as_u32()
                ));
            }
        }
    }

    /// Every local defined exactly once; returns the definition sites, by
    /// local index.
    fn check_definitions(&mut self) -> Option<Vec<Option<DefSite>>> {
        let f = self.f;
        let mut sites: Vec<Option<DefSite>> = vec![None; f.locals.len()];
        let mut ok = true;
        let entry = BlockId::from_u32(0);
        let mut define = |cx: &mut Self, l: Local, site: DefSite| match sites.get_mut(l.index()) {
            None => {
                cx.err(format!("unknown local _{}", l.as_u32()));
                ok = false;
            }
            Some(Some(_)) => {
                cx.err(format!("_{} is defined more than once", l.as_u32()));
                ok = false;
            }
            Some(slot) => *slot = Some(site),
        };
        for &p in &f.params {
            define(self, p, (entry, None));
        }
        for (id, block) in f.blocks.iter_enumerated() {
            for &p in &block.params {
                define(self, p, (id, None));
            }
            for (n, stmt) in block.stmts.iter().enumerate() {
                if let Stmt::Assign { dst, .. } = stmt {
                    define(self, *dst, (id, Some(n)));
                }
            }
        }
        ok.then_some(sites)
    }

    /// Every recorded call location (D-856) names a local that a call
    /// defines and a string of [`Function::call_loc_strings`], and the
    /// table is sorted by local, as code generation searches it.
    fn check_call_locs(&mut self, defs: &[Option<DefSite>]) {
        let f = self.f;
        if !f
            .call_locs
            .windows(2)
            .all(|w| matches!(w, [a, b] if a.0 < b.0))
        {
            self.err("the call locations are not sorted by local, each once");
        }
        for &(local, loc) in &f.call_locs {
            let by_call = defs
                .get(local.index())
                .copied()
                .flatten()
                .and_then(|(block, at)| f.blocks.get(block)?.stmts.get(at?))
                .is_some_and(|stmt| {
                    matches!(
                        stmt,
                        Stmt::Assign {
                            value: Rvalue::Call { .. },
                            ..
                        }
                    )
                });
            if !by_call {
                self.err(format!(
                    "the call location of _{} names a local no call defines",
                    local.as_u32()
                ));
            }
            if f.call_loc_strings.get(loc).is_none() {
                self.err(format!("unknown call location str#{}", loc.as_u32()));
            }
        }
    }

    /// Block references exist, `bb0` is not a target and has no
    /// parameters, and `branch`/`switch` targets take no parameters.
    fn check_cfg(&mut self) -> bool {
        let f = self.f;
        let before = self.errors.len();
        let Some(entry) = f.blocks.get(BlockId::from_u32(0)) else {
            self.err("a function needs an entry block");
            return false;
        };
        if !entry.params.is_empty() {
            self.err("bb0 takes no parameters");
        }
        for (id, block) in f.blocks.iter_enumerated() {
            for succ in block.term.successors() {
                let Some(target) = f.blocks.get(succ) else {
                    self.err(format!(
                        "bb{} jumps to unknown bb{}",
                        id.as_u32(),
                        succ.as_u32()
                    ));
                    continue;
                };
                if succ.as_u32() == 0 {
                    self.err(format!("bb{} jumps to the entry block", id.as_u32()));
                }
                let branches = matches!(
                    block.term,
                    Terminator::Branch { .. } | Terminator::Switch { .. }
                );
                if branches && !target.params.is_empty() {
                    self.err(format!(
                        "bb{} branches to bb{}, which takes parameters",
                        id.as_u32(),
                        succ.as_u32()
                    ));
                }
            }
        }
        self.errors.len() == before
    }

    /// Every use dominated by its definition, over the blocks split at
    /// their unwind edges ([`Segments`]), canonical pads left out.
    fn check_dominance(&mut self, defs: &[Option<DefSite>]) -> Result<(), Ice> {
        let f = self.f;
        let segments = Segments::new(f, &self.pads)?;
        let dom = Dominance::from_successors(&segments.succs)?;
        for (id, block) in f.blocks.iter_enumerated() {
            if self.is_pad(id) {
                continue;
            }
            let check = |cx: &mut Self, l: Local, at: usize| {
                let use_node = segments.use_node(id, at);
                // Unreachable code has no dominators; its uses are not
                // checked.
                if !dom.is_reachable(use_node) {
                    return;
                }
                let Some(&Some((def_block, def_pos))) = defs.get(l.index()) else {
                    cx.err(format!("_{} is used but never defined", l.as_u32()));
                    return;
                };
                let def_node = segments.def_node(def_block, def_pos);
                let ok = if def_node == use_node {
                    def_pos.is_none_or(|d| d < at)
                } else {
                    dom.dominates(def_node, use_node)
                };
                if !ok {
                    cx.err(format!(
                        "_{} is used in bb{} where its definition does not dominate",
                        l.as_u32(),
                        id.as_u32()
                    ));
                }
            };
            for (n, stmt) in block.stmts.iter().enumerate() {
                for l in stmt_locals(stmt) {
                    check(self, l, n);
                }
            }
            for (l, _) in block.term.uses() {
                check(self, l, block.stmts.len());
            }
        }
        Ok(())
    }

    fn repr(&mut self, op: &Operand) -> Option<Repr> {
        let repr = self.f.operand_repr(op).ok();
        if repr.is_none() {
            self.err(format!("unknown operand {op:?}"));
        }
        repr
    }

    fn expect_repr(&mut self, op: &Operand, want: Repr, what: &str) {
        if let Some(got) = self.repr(op)
            && got != want
        {
            self.err(format!("{what}: expected {want}, found {got}"));
        }
        self.check_const(op, false);
    }

    fn check_const(&mut self, op: &Operand, code_allowed: bool) {
        match op {
            Operand::Const(Const::Code(_)) if !code_allowed => {
                self.err("a code word appears only as a closure's first field");
            }
            Operand::Const(Const::Str(id)) if self.f.strings.get(*id).is_none() => {
                self.err(format!("unknown string str#{}", id.as_u32()));
            }
            Operand::Const(Const::Sized { repr, .. }) if !repr.is_sized() => {
                self.err(format!("a sized constant of representation {repr}"));
            }
            Operand::Const(Const::Static(id)) if self.module.static_object(*id).is_none() => {
                self.err(format!("unknown static#{}", id.as_u32()));
            }
            Operand::Const(Const::Ctx) => {
                self.err("ctx appears only as the first argument of a foreign call (D-692)");
            }
            _ => {}
        }
    }

    fn callee_sig(&mut self, callee: &Callee) -> Option<AbiSig> {
        let sig = match callee {
            Callee::Direct(def) | Callee::Borrowing(def, _) => {
                let sig = self.module.callee(*def).map(|d| d.sig.clone());
                let mask = callee.borrow_mask();
                match &sig {
                    None => self.err(format!("direct callee {def:?} has no declaration")),
                    // The mask is part of the ABI signature (D-715): a call
                    // with another one would release what the callee holds,
                    // or leak what it does not.
                    Some(decl) if decl.borrow != mask => self.err(format!(
                        "call of {def:?} borrows {mask:?}, but its declaration borrows {:?} (D-715)",
                        decl.borrow
                    )),
                    Some(_) => {}
                }
                if matches!(callee, Callee::Borrowing(..)) && mask.is_owned() {
                    self.err(format!(
                        "a borrowing call of {def:?} that borrows nothing (D-715)"
                    ));
                }
                sig
            }
            Callee::Code(key) => {
                let sig = self
                    .index
                    .get(key)
                    .and_then(|&n| self.module.functions.get(n))
                    .and_then(|f| f.sig().ok())
                    .or_else(|| fixed_sig(*key));
                if sig.is_none() {
                    self.err(format!("callee {key:?} is not in this unit"));
                }
                sig
            }
            Callee::Indirect(l) => {
                let repr = self.f.local(*l).map(|d| d.repr).ok();
                if repr != Some(Repr::Value) {
                    self.err(format!("indirect callee _{} is not a value", l.as_u32()));
                }
                None
            }
        };
        if let Some(sig) = &sig
            && sig.abi != Abi::Cheby
        {
            self.err(format!("call of a C-ABI function {callee:?}"));
        }
        sig
    }

    fn check_call(&mut self, callee: &Callee, args: &[Operand]) {
        if let Callee::Indirect(_) = callee {
            self.callee_sig(callee);
            for arg in args {
                self.expect_repr(arg, Repr::Value, "indirect call argument");
            }
            return;
        }
        let Some(sig) = self.callee_sig(callee) else {
            return;
        };
        if sig.params.len() != args.len() {
            self.err(format!(
                "call of {callee:?} with {} arguments, expected {}",
                args.len(),
                sig.params.len()
            ));
            return;
        }
        for (arg, &want) in args.iter().zip(&sig.params) {
            self.expect_repr(arg, want, "call argument");
        }
    }

    /// The representation a foreign call with the C signature `sig`
    /// produces, after checking its arguments.
    fn check_foreign(
        &mut self,
        sig: &ForeignSig,
        args: &[Operand],
        blocking: bool,
    ) -> Option<Repr> {
        if sig.params.len() != args.len() {
            self.err("foreign call with the wrong number of arguments");
        }
        // A pool thread never receives the fiber's context (D-191, D-819).
        if blocking && args.contains(&Operand::Const(Const::Ctx)) {
            self.err("a blocking foreign call passes ctx");
        }
        if sig.params.contains(&CType::Void) {
            self.err("a foreign parameter of type void");
        }
        for (n, (arg, &ctype)) in args.iter().zip(&sig.params).enumerate() {
            // The fiber's context of a `@std_context` kernel (D-692, D-745).
            if n == 0 && *arg == Operand::Const(Const::Ctx) {
                if !ctype.accepts_ctx() {
                    self.err(format!("ctx passed as {ctype}"));
                }
                continue;
            }
            self.check_const(arg, false);
            if let Some(r) = self.repr(arg)
                && !ctype.accepts(r)
            {
                self.err(format!("foreign argument {r} passed as {ctype}"));
            }
        }
        sig.ret.repr()
    }

    /// The representation the rvalue produces, after checking its operands.
    fn check_rvalue(&mut self, value: &Rvalue) -> Option<Repr> {
        match value {
            Rvalue::Use(op) => {
                self.check_const(op, false);
                self.repr(op)
            }
            Rvalue::Call { callee, args } => {
                self.check_call(callee, args);
                Some(Repr::Value)
            }
            Rvalue::CallForeign {
                sig,
                args,
                blocking,
                ..
            } => self.check_foreign(sig, args, *blocking),
            Rvalue::CallRuntime { func, args } => self.check_runtime(*func, args),
            Rvalue::Alloc { layout, fields } => {
                self.check_alloc(*layout, fields);
                Some(Repr::Value)
            }
            Rvalue::AllocPlaceholder { layout } => {
                if !matches!(layout, LayoutRef::Tuple(n) if *n > 0) {
                    self.err(format!(
                        "alloc_placeholder of {layout:?}, which is not a tuple (D-720)"
                    ));
                }
                Some(Repr::Value)
            }
            // Which locals are tokens, how they are used and the sizes:
            // `check_reuse`.
            Rvalue::DropReuse { local, layout, .. } => {
                self.expect_repr(&Operand::Local(*local), Repr::Value, "drop_reuse operand");
                self.check_reuse_layout("drop_reuse", *layout);
                Some(Repr::Value)
            }
            Rvalue::AllocReuse {
                token,
                layout,
                fields,
            } => {
                self.expect_repr(&Operand::Local(*token), Repr::Value, "alloc_reuse token");
                if self.check_reuse_layout("alloc_reuse", *layout) {
                    self.check_alloc(*layout, fields);
                }
                Some(Repr::Value)
            }
            Rvalue::InitField {
                obj,
                layout,
                field,
                value,
            } => {
                // Where `obj` comes from and that each field is stored
                // once: `check_init_fields`.
                self.expect_repr(&Operand::Local(*obj), Repr::Value, "init_field object");
                match layout {
                    LayoutRef::Tuple(n) if field < n => {}
                    _ => self.err(format!(
                        "init_field {field} of {layout:?}, which is not a tuple field (D-720)"
                    )),
                }
                self.expect_repr(value, Repr::Value, "init_field value");
                None
            }
            Rvalue::Field {
                base,
                layout,
                field,
            } => {
                self.expect_repr(&Operand::Local(*base), Repr::Value, "field base");
                self.field_repr(*layout, *field)
            }
            Rvalue::ReadLayout { base } => {
                self.expect_repr(&Operand::Local(*base), Repr::Value, "read_layout base");
                Some(Repr::U32)
            }
            Rvalue::LoadConst(_) => Some(Repr::Value),
            Rvalue::Box { value, from } => {
                if *from == Repr::Value {
                    self.err("boxing a value that is already uniform");
                }
                self.expect_repr(value, *from, "box operand");
                Some(Repr::Value)
            }
            Rvalue::Unbox { value, to } => {
                if *to == Repr::Value {
                    self.err("unboxing into a uniform value");
                }
                self.expect_repr(value, Repr::Value, "unbox operand");
                Some(*to)
            }
            Rvalue::ReadTag { base, ty } => {
                self.expect_repr(base, Repr::Value, "read_tag base");
                self.check_tag_type(*ty);
                Some(Repr::Int)
            }
            Rvalue::Checked { .. }
            | Rvalue::Compare { .. }
            | Rvalue::FloatArith { .. }
            | Rvalue::Bitwise { .. }
            | Rvalue::FloatNeg { .. }
            | Rvalue::Not(_) => self.check_arith(value),
        }
    }

    /// Arithmetic, comparisons and the other operators on raw values
    /// (plan §4.1).
    fn check_arith(&mut self, value: &Rvalue) -> Option<Repr> {
        let integer = |r: Repr| r.is_signed_int() || r.is_unsigned_int();
        let float = |r: Repr| matches!(r, Repr::Float | Repr::F32);
        match value {
            Rvalue::Checked {
                op,
                lhs,
                rhs,
                repr,
                loc,
            } => {
                if self.f.strings.get(*loc).is_none() {
                    self.err(format!("unknown string str#{}", loc.as_u32()));
                }
                if !integer(*repr) {
                    self.err(format!("checked {} on {repr}", op.name()));
                }
                self.expect_repr(lhs, *repr, "checked operand");
                self.expect_repr(rhs, *repr, "checked operand");
                Some(*repr)
            }
            Rvalue::Compare { op, lhs, rhs, repr } => {
                if *repr == Repr::Value {
                    self.err("compare on uniform values");
                }
                if *repr == Repr::Bool && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                    self.err(format!("compare {} on bool", op.name()));
                }
                self.expect_repr(lhs, *repr, "compare operand");
                self.expect_repr(rhs, *repr, "compare operand");
                Some(Repr::Bool)
            }
            Rvalue::FloatArith { op, lhs, rhs, repr } => {
                if !float(*repr) {
                    self.err(format!("float_arith {} on {repr}", op.name()));
                }
                if matches!(op, ArithOp::Shl | ArithOp::Shr) {
                    self.err(format!("float_arith {}", op.name()));
                }
                self.expect_repr(lhs, *repr, "float operand");
                self.expect_repr(rhs, *repr, "float operand");
                Some(*repr)
            }
            Rvalue::Bitwise { op, lhs, rhs, repr } => {
                if !integer(*repr) {
                    self.err(format!("bitwise {} on {repr}", op.name()));
                }
                self.expect_repr(lhs, *repr, "bitwise operand");
                self.expect_repr(rhs, *repr, "bitwise operand");
                Some(*repr)
            }
            Rvalue::FloatNeg { value, repr } => {
                if !float(*repr) {
                    self.err(format!("float_neg on {repr}"));
                }
                self.expect_repr(value, *repr, "float_neg operand");
                Some(*repr)
            }
            Rvalue::Not(value) => {
                self.expect_repr(value, Repr::Bool, "not operand");
                Some(Repr::Bool)
            }
            _ => None,
        }
    }

    fn check_runtime(&mut self, func: RuntimeFn, args: &[Operand]) -> Option<Repr> {
        let params = func.params();
        if params.len() != args.len() {
            self.err(format!(
                "call_runtime {} with {} arguments, expected {}",
                func.name(),
                args.len(),
                params.len()
            ));
        }
        for (arg, &(want, _)) in args.iter().zip(params) {
            self.expect_repr(arg, want, "runtime argument");
        }
        // The location of a structural `==` is a static string (D-556).
        if func == RuntimeFn::Equal
            && let Some(location) = args.get(2)
            && !matches!(location, Operand::Const(Const::Str(_)))
        {
            self.err("call_runtime equal: the location is not a string constant".to_owned());
        }
        func.ret()
    }

    /// A tag is read only from a type with several constructors (D-522),
    /// whose layouts the unit holds in tag order.
    fn check_tag_type(&mut self, ty: TagType) {
        let TagType::Adt(def) = ty else {
            return;
        };
        let tags: Vec<u32> = self
            .module
            .type_layouts(def)
            .iter()
            .map(|d| d.tag)
            .collect();
        if tags.len() < 2 {
            self.err(format!(
                "read_tag of {def:?}, which has {} known constructor(s)",
                tags.len()
            ));
        } else if !tags
            .iter()
            .copied()
            .eq(0..u32::try_from(tags.len()).unwrap_or(0))
        {
            self.err(format!("read_tag of {def:?} without all its constructors"));
        }
    }

    /// The representation of field `field` of `layout`.
    fn field_repr(&mut self, layout: LayoutRef, field: u32) -> Option<Repr> {
        match layout {
            LayoutRef::Ctor(def) => {
                let Some(desc) = self.module.layout(layout) else {
                    self.err(format!("no layout for constructor {def:?}"));
                    return None;
                };
                let found = usize::try_from(field)
                    .ok()
                    .and_then(|n| desc.fields.get(n))
                    .map(|f| f.repr);
                if found.is_none() {
                    self.err(format!(
                        "field {field} of {def:?}, which has {}",
                        desc.fields.len()
                    ));
                }
                found
            }
            LayoutRef::Closure(_) if field == 0 => {
                self.err("field 0 of a closure is its code word, not a value");
                None
            }
            LayoutRef::Builtin(_) => {
                self.err(format!("field of a runtime-defined object {layout:?}"));
                None
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Closure(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Dyn
            | LayoutRef::Desc { .. }
            | LayoutRef::DescNominal => {
                let count = layout.uniform_fields().unwrap_or(0);
                if u64::from(field) >= count {
                    self.err(format!("field {field} of {layout:?}, which has {count}"));
                }
                Some(Repr::Value)
            }
        }
    }

    fn check_alloc(&mut self, layout: LayoutRef, fields: &[Operand]) {
        match layout {
            LayoutRef::Closure(n) => {
                let Some((code, captures)) = fields.split_first() else {
                    self.err("a closure needs its code word");
                    return;
                };
                match code {
                    Operand::Const(Const::Code(key)) if key.is_uniform() => {}
                    Operand::Const(Const::Code(key)) => {
                        self.err(format!("a closure's code {key:?} is not uniform"));
                    }
                    _ => self.err("a closure's first field is its code word"),
                }
                if usize::try_from(n).ok() != Some(captures.len()) {
                    self.err("closure capture count differs from its layout");
                }
                for c in captures {
                    self.expect_repr(c, Repr::Value, "closure capture");
                }
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Dyn
            | LayoutRef::Desc { .. } => {
                let want = layout.uniform_fields().unwrap_or(0);
                if u64::try_from(fields.len()).ok() != Some(want) {
                    self.err(format!(
                        "alloc {layout:?} with {} fields, expected {want}",
                        fields.len()
                    ));
                }
                for c in fields {
                    self.expect_repr(c, Repr::Value, "field");
                }
            }
            LayoutRef::DescNominal => {
                self.err("a nominal descriptor is only a static object (D-524)");
            }
            LayoutRef::Ctor(def) => {
                let Some(desc) = self.module.layout(layout) else {
                    self.err(format!("no layout for constructor {def:?}"));
                    return;
                };
                let want: Vec<Repr> = desc.fields.iter().map(|f| f.repr).collect();
                if want.is_empty() {
                    self.err(format!(
                        "alloc of the unit constructor {def:?}, which is a static object"
                    ));
                }
                if want.len() != fields.len() {
                    self.err(format!(
                        "alloc of {def:?} with {} fields, expected {}",
                        fields.len(),
                        want.len()
                    ));
                }
                for (c, r) in fields.iter().zip(want) {
                    self.expect_repr(c, r, "constructor field");
                }
            }
            LayoutRef::Builtin(b) => {
                let want = match b {
                    BuiltinLayout::BoxedInt => Repr::Int,
                    BuiltinLayout::BoxedFloat => Repr::Float,
                    BuiltinLayout::BoxedF32 => Repr::F32,
                    BuiltinLayout::BoxedU64 => Repr::U64,
                    BuiltinLayout::String | BuiltinLayout::Bytes => {
                        self.err(format!("alloc of {b:?}, which the runtime builds"));
                        return;
                    }
                };
                if fields.len() != 1 {
                    self.err(format!("alloc of {b:?} with {} fields", fields.len()));
                }
                for c in fields {
                    self.expect_repr(c, want, "boxed number");
                }
            }
        }
    }

    fn check_reprs(&mut self, stage: Stage) -> Result<(), Ice> {
        let f = self.f;
        for (id, block) in f.blocks.iter_enumerated() {
            // A canonical pad drops counted locals and jumps without
            // arguments ([`canonical_pads`]).
            if self.is_pad(id) {
                continue;
            }
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign { dst, value } => {
                        if is_void_call(value) {
                            self.err("a void call cannot define a local");
                        }
                        let dst_repr = f.local(*dst)?.repr;
                        if let Some(r) = self.check_rvalue(value)
                            && r != dst_repr
                            && !foreign_i64(value, dst_repr)
                        {
                            self.err(format!(
                                "_{} has representation {dst_repr} but is assigned a {r}",
                                dst.as_u32()
                            ));
                        }
                    }
                    Stmt::Eval(value) => {
                        if !is_void_call(value) {
                            self.err("only a void call is evaluated for effect");
                        }
                        self.check_rvalue(value);
                    }
                    Stmt::Dup(l) | Stmt::Drop(l) => {
                        if !self.counted(stage) {
                            self.err("dup or drop before reference-count insertion");
                        }
                        if f.local(*l)?.repr != Repr::Value {
                            self.err(format!("dup or drop of uncounted _{}", l.as_u32()));
                        }
                    }
                    // Which locals it keeps, its stage and what it may
                    // release: `check_reuse`.
                    Stmt::DropKeep { local, layout, .. } => {
                        self.expect_repr(&Operand::Local(*local), Repr::Value, "specialized drop");
                        self.check_reuse_layout("a specialized drop", *layout);
                    }
                    Stmt::ClearUnwind => {
                        if !f.precounted {
                            self.err("clear_unwind outside a generated intrinsic body (D-693)");
                        }
                    }
                }
            }
            self.check_terminator(id, &block.term)?;
        }
        Ok(())
    }

    fn check_terminator(&mut self, id: BlockId, term: &Terminator) -> Result<(), Ice> {
        let f = self.f;
        match term {
            Terminator::Return(op) => self.expect_repr(op, f.ret, "returned value"),
            Terminator::TailCall { callee, args } => {
                if f.abi != Abi::Cheby {
                    self.err("a tail call in a C-ABI function");
                }
                self.check_call(callee, args);
            }
            Terminator::Goto { target, args } => {
                let params = f.block(*target)?.params.clone();
                if params.len() != args.len() {
                    self.err(format!(
                        "bb{} passes {} arguments to bb{}, which takes {}",
                        id.as_u32(),
                        args.len(),
                        target.as_u32(),
                        params.len()
                    ));
                }
                for (arg, p) in args.iter().zip(params) {
                    let want = f.local(p)?.repr;
                    self.expect_repr(arg, want, "goto argument");
                }
            }
            Terminator::Branch { cond, .. } => {
                self.expect_repr(cond, Repr::Bool, "branch condition");
            }
            Terminator::Switch { on, cases, .. } => {
                self.check_const(on, false);
                if let Some(r) = self.repr(on)
                    && !(r == Repr::U32 || r.is_signed_int() || r.is_unsigned_int())
                {
                    self.err(format!("switch on {r}"));
                }
                let mut seen = BTreeSet::new();
                for (v, _) in cases {
                    if !seen.insert(*v) {
                        self.err(format!("switch case {v} appears twice"));
                    }
                }
            }
            Terminator::Panic { message, loc, .. } => {
                if f.strings.get(*loc).is_none() {
                    self.err(format!("unknown string str#{}", loc.as_u32()));
                }
                if let Some(m) = message {
                    self.expect_repr(m, Repr::Value, "panic message");
                }
            }
            // Where it may appear is part of the unwinding shape.
            Terminator::Unreachable | Terminator::Resume => {}
        }
        Ok(())
    }

    /// Reference counts balance on every path, pads included (D7, D-698):
    /// the simulation of [`crate::ownership`].
    fn check_ownership(&mut self) -> Result<(), Ice> {
        ownership::simulate(self.f, &mut ErrorSink(&mut self.errors))
    }
}

/// Whether `stmt` only releases or adds references, running nothing a
/// handle could observe (D-851): `dup`, `drop`,
/// `drop_reuse` and specialized drops, and the `list_uncons_head` that
/// completes the instruction before it (D-909).
fn releases_only(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Dup(_)
            | Stmt::Drop(_)
            | Stmt::DropKeep { .. }
            | Stmt::Assign {
                value: Rvalue::DropReuse { .. }
                    // Completes the `list_uncons` before it and runs no
                    // code: what counting releases after that instruction
                    // follows it (D-909).
                    | Rvalue::CallRuntime {
                        func: RuntimeFn::ListUnconsHead,
                        ..
                    },
                ..
            }
    )
}

/// The local `stmt` releases: a `drop`'s, a `drop_reuse`'s or a
/// specialized drop's.
fn released(stmt: &Stmt) -> Option<Local> {
    match stmt {
        Stmt::Drop(x)
        | Stmt::DropKeep { local: x, .. }
        | Stmt::Assign {
            value: Rvalue::DropReuse { local: x, .. },
            ..
        } => Some(*x),
        _ => None,
    }
}

/// What walking back from a release finds ([`DropWalk::back`]).
enum Back {
    /// The local's last use or definition, with nothing run since.
    Ok,
    /// This statement ran after the local's last use.
    Ran(usize),
    /// Nothing ran and the local is not used in the block: its
    /// predecessors decide.
    Preds(BlockId),
}

/// The facts [`FnCx::check_drop_positions`] walks back over.
struct DropWalk<'a> {
    f: &'a Function,
    /// By block index: its predecessors along normal control flow.
    preds: &'a [Vec<BlockId>],
    /// By block index, then position (the terminator's last): the nearest
    /// earlier statement that runs code ([`releases_only`]).
    runs_before: &'a [Vec<Option<usize>>],
    /// By block index: the positions where each local is used or defined
    /// (the terminator at the number of statements).
    events: &'a [FxHashMap<Local, Vec<usize>>],
}

impl DropWalk<'_> {
    /// Walks back from position `at` of block `b` (past its end, so its
    /// terminator included, when `at` exceeds the number of statements) to
    /// the last use or definition of `x`.
    fn back(&self, b: BlockId, at: usize, x: Local) -> Back {
        let before = self.runs_before.get(b.index());
        let last = before.map_or(0, |v| v.len().saturating_sub(1));
        let ran = before.and_then(|v| v.get(at.min(last))).copied().flatten();
        let used = self
            .events
            .get(b.index())
            .and_then(|e| e.get(&x))
            .is_some_and(|ps| ps.iter().any(|&p| p < at && ran.is_none_or(|s| p >= s)));
        if used {
            return Back::Ok;
        }
        if let Some(s) = ran {
            return Back::Ran(s);
        }
        let param = self
            .f
            .blocks
            .get(b)
            .is_some_and(|block| block.params.contains(&x))
            || (b.index() == 0 && self.f.params.contains(&x));
        if param { Back::Ok } else { Back::Preds(b) }
    }
}

/// Collects the ownership simulation's findings.
struct ErrorSink<'a>(&'a mut Vec<String>);

impl Observer for ErrorSink<'_> {
    fn error(&mut self, message: String) {
        self.0.push(message);
    }
}

/// The CFG of a function with each block split at its unwind edges
/// (D-698), for dominance. Node 0 is the function's entry, where its
/// parameters are defined; it goes to `bb0` and to the entry pad. A block
/// with edges at instructions `i1 < … < ik` has `k + 1` nodes: node `j`
/// holds the result of instruction `ij` (none for `j = 0`), the
/// instructions after it, and the operands of `i(j+1)`, whose edge leaves
/// from it; the last node ends with the terminator's normal successors. So
/// a pad sees what is defined before the instruction that unwinds to it,
/// not that instruction's result.
struct Segments {
    /// Each block's first node, by block index.
    first: Vec<usize>,
    /// Each block's edge positions, sorted.
    edges: Vec<Vec<usize>>,
    /// Each node's successors.
    succs: Vec<Vec<usize>>,
}

impl Segments {
    /// The segments of `f`, without the canonical pads `pads` marks by
    /// block index: they get no node, and an edge to one does not split
    /// its block.
    fn new(f: &Function, pads: &[bool]) -> Result<Self, Ice> {
        let n = f.blocks.len();
        let is_pad = |b: BlockId| pads.get(b.index()).copied().unwrap_or(false);
        let mut first = Vec::with_capacity(n);
        let mut edges = Vec::with_capacity(n);
        let mut splits: Vec<Vec<(usize, BlockId)>> = Vec::with_capacity(n);
        let mut next = 1_usize;
        for (id, block) in f.blocks.iter_enumerated() {
            if is_pad(id) {
                first.push(NO_NODE);
                edges.push(Vec::new());
                splits.push(Vec::new());
                continue;
            }
            first.push(next);
            // Out-of-range pads are shape errors, reported elsewhere.
            let mut here: Vec<(usize, BlockId)> = block
                .unwind
                .iter()
                .filter(|&&(_, pad)| pad.index() < n && !is_pad(pad))
                .filter_map(|&(i, pad)| Some((usize::try_from(i).ok()?, pad)))
                .collect();
            here.sort_unstable_by_key(|&(i, _)| i);
            next = next.saturating_add(here.len()).saturating_add(1);
            edges.push(here.iter().map(|&(i, _)| i).collect());
            splits.push(here);
        }
        let node = |b: BlockId| first.get(b.index()).copied().filter(|&n| n != NO_NODE);
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); next];
        let entry = succs
            .first_mut()
            .ok_or_else(|| Ice::new("segments without an entry node"))?;
        entry.extend(node(BlockId::from_u32(0)));
        entry.extend(f.entry_unwind.and_then(node));
        for ((id, block), pads) in f.blocks.iter_enumerated().zip(&splits) {
            if is_pad(id) {
                continue;
            }
            let base = node(id).ok_or_else(|| Ice::new("a block without a node"))?;
            for (j, &(_, pad)) in pads.iter().enumerate() {
                let out = succs
                    .get_mut(base.saturating_add(j))
                    .ok_or_else(|| Ice::new("a segment out of range"))?;
                out.extend(node(pad));
                out.push(base.saturating_add(j).saturating_add(1));
            }
            let last = succs
                .get_mut(base.saturating_add(pads.len()))
                .ok_or_else(|| Ice::new("a segment out of range"))?;
            // Unknown targets are CFG errors, reported before.
            last.extend(block.term.successors().into_iter().filter_map(node));
        }
        Ok(Self {
            first,
            edges,
            succs,
        })
    }

    fn base(&self, b: BlockId) -> (usize, &[usize]) {
        (
            self.first.get(b.index()).copied().unwrap_or(NO_NODE),
            self.edges.get(b.index()).map_or(&[], Vec::as_slice),
        )
    }

    /// The node where instruction `at` of `b` reads its operands.
    fn use_node(&self, b: BlockId, at: usize) -> usize {
        let (base, edges) = self.base(b);
        base.saturating_add(edges.partition_point(|&e| e < at))
    }

    /// The node where a local defined at `site` becomes defined: node 0
    /// for a function parameter, a block's first node for its parameters.
    fn def_node(&self, b: BlockId, at: Option<usize>) -> usize {
        let (base, edges) = self.base(b);
        match at {
            None if b.index() == 0 => 0,
            None => base,
            Some(at) => base.saturating_add(edges.partition_point(|&e| e <= at)),
        }
    }
}

/// Whether a statement or the terminator of `block` reads `local`.
fn block_uses(block: &Block, local: Local) -> bool {
    block.stmts.iter().any(|s| stmt_locals(s).contains(&local))
        || block.term.uses().iter().any(|&(l, _)| l == local)
}

/// No node in [`Segments`]: a canonical pad's.
const NO_NODE: usize = usize::MAX;

/// The canonical pads of `f`, by block index (module documentation): a
/// block without parameters or unwind edges that normal control flow
/// (`normal`, by block index) does not reach, and that is either `resume`
/// or `drop _l; goto <canonical pad>` without arguments, `_l` counted.
/// The chain below one ends in `resume`.
///
/// # Errors
///
/// An [`Ice`] for an unknown local.
fn canonical_pads(f: &Function, normal: &[bool]) -> Result<Vec<bool>, Ice> {
    /// The state of a block in the walk.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Unknown,
        OnPath,
        Pad,
        Other,
    }
    let n = f.blocks.len();
    let mut mark = vec![Mark::Unknown; n];
    let mut path = Vec::new();
    for start in 0..n {
        let mut at = start;
        // Follow the chain below `start` until its end is known, then
        // mark the path walked by that end; no recursion over long
        // chains.
        let end = loop {
            match mark.get(at).copied() {
                Some(Mark::Pad) => break Mark::Pad,
                // `OnPath` is a cycle: no `resume` below.
                Some(Mark::Other | Mark::OnPath) | None => break Mark::Other,
                Some(Mark::Unknown) => {}
            }
            let Some(block) = BlockId::from_usize(at).and_then(|id| f.blocks.get(id)) else {
                break Mark::Other;
            };
            let candidate = block.params.is_empty()
                && block.unwind.is_empty()
                && !normal.get(at).copied().unwrap_or(true);
            let next = match (block.stmts.as_slice(), &block.term) {
                _ if !candidate => None,
                ([], Terminator::Resume) => {
                    if let Some(m) = mark.get_mut(at) {
                        *m = Mark::Pad;
                    }
                    break Mark::Pad;
                }
                ([Stmt::Drop(l)], Terminator::Goto { target, args })
                    if args.is_empty() && f.local(*l)?.repr == Repr::Value =>
                {
                    Some(target.index())
                }
                _ => None,
            };
            let Some(next) = next else {
                if let Some(m) = mark.get_mut(at) {
                    *m = Mark::Other;
                }
                break Mark::Other;
            };
            if let Some(m) = mark.get_mut(at) {
                *m = Mark::OnPath;
            }
            path.push(at);
            at = next;
        };
        for b in path.drain(..) {
            if let Some(m) = mark.get_mut(b) {
                *m = end;
            }
        }
    }
    Ok(mark.into_iter().map(|m| m == Mark::Pad).collect())
}

/// The blocks reached from `bb0` and the entry pad through
/// [`Function::successors`], normal flow and unwinding, by block index.
fn reachable_with_unwinding(f: &Function) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![BlockId::from_u32(0)];
    work.extend(f.entry_unwind);
    while let Some(id) = work.pop() {
        match seen.get_mut(id.index()) {
            Some(s) if !*s => *s = true,
            _ => continue,
        }
        work.extend(f.successors(id).unwrap_or_default());
    }
    seen
}

/// Whether `value` is evaluated only for effect: a call without a result
/// (a foreign function returning `void`, a runtime helper returning
/// nothing) or a field store ([`Rvalue::InitField`], D-720).
fn is_void_call(value: &Rvalue) -> bool {
    match value {
        Rvalue::CallForeign { sig, .. } => sig.ret == CType::Void,
        Rvalue::CallRuntime { func, .. } => func.ret().is_none(),
        Rvalue::InitField { .. } => true,
        _ => false,
    }
}

/// Whether a foreign `int64_t` result lands in an `I64` local.
fn foreign_i64(value: &Rvalue, dst: Repr) -> bool {
    matches!(value, Rvalue::CallForeign { sig, .. } if sig.ret == CType::I64 && dst == Repr::I64)
}

/// The locals a statement reads, Dup and Drop included, and the locals of
/// a keep list.
fn stmt_locals(stmt: &Stmt) -> Vec<Local> {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => value
            .uses()
            .into_iter()
            .map(|(l, _)| l)
            .chain(value.kept().iter().map(|&(_, p)| p))
            .collect(),
        Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
        Stmt::DropKeep { local, keep, .. } => std::iter::once(*local)
            .chain(keep.iter().map(|&(_, p)| p))
            .collect(),
        Stmt::ClearUnwind => Vec::new(),
    }
}

/// The blocks reached from `bb0` through normal successors only, without
/// taking an unwind edge or the entry pad.
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
