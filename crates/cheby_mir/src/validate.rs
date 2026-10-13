//! The MIR validator (D1), run after lowering and after RC
//! insertion in debug builds and tests. A failure is an internal compiler
//! error in the driver.
//!
//! Structure: one definition per local, every use dominated by its
//! definition, block parameters matching `goto` arguments, call arguments
//! matching callee signatures (direct callees from
//! [`MirModule::callees`]), representations of every operand and result,
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
//! entry pad. Reuse tokens do not exist yet, so the rule that each is
//! consumed once on every path, pads included, waits for them.
//!
//! Canonical pads, blocks `drop _l; goto <pad>` or `resume` that lead
//! only to `resume` and that normal control flow does not reach, are left
//! out of the dominance graph and the representation walk (D-735): what
//! they drop is checked against ownership states, which hold only counted
//! locals that every path to the edge defined.
//!
//! GAP(unwound-validation-split): the pipeline
//! ([`crate::count_and_check_with`]) validates a unit after the unwind
//! pass with the states the pass's own simulation recorded at each site:
//! each edge's pad chain (its drops, then `goto`, then `resume`) must
//! drop exactly what an unwinding leaves held there, and the entry pad's
//! the counted parameters. The independent second simulation runs where
//! [`crate::MirOptions::full_ownership_check`] says, by default in debug
//! builds and so in every test run. [`validate`] on its own always runs
//! the full simulation.

mod dom;

use std::collections::BTreeSet;

use cheby_iface::{Ice, Idx};

use self::dom::Dominance;

use crate::abi::{AbiSig, fixed_sig, is_uniform_sig};
use crate::ir::{
    Abi, ArithOp, Block, BlockId, BuiltinLayout, Callee, CmpOp, Const, FnKey, Function, LayoutRef,
    Local, MirModule, Operand, RuntimeFn, Rvalue, StaticKind, StaticObject, Stmt, TagType,
    Terminator, UnitKind,
};
use crate::layout::LayoutDesc;
use crate::ownership::{self, Observer};
use crate::repr::{CType, Repr};
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
    for (n, f) in module.functions.iter().enumerate() {
        let mut cx = FnCx {
            module,
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

impl FnCx<'_> {
    fn err(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    fn check(&mut self, stage: Stage) -> Result<(), Ice> {
        self.check_abi()?;
        let Some(defs) = self.check_definitions() else {
            return Ok(());
        };
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
        if self.counted(stage) && self.errors.is_empty() {
            if let (Some(recorded), Some(reachable)) = (self.recorded, &reachable) {
                self.check_pad_chains(recorded, reachable)?;
            }
            if self.simulate {
                self.check_ownership()?;
            }
        }
        Ok(())
    }

    /// Whether block `id` is a canonical pad.
    fn is_pad(&self, id: BlockId) -> bool {
        self.pads.get(id.index()).copied().unwrap_or(false)
    }

    /// The pad check of GAP(unwound-validation-split) (module
    /// documentation): every edge of a block the simulation reached, and
    /// the entry pad, against the recorded states. A pad of a generated
    /// body (D-744) that is not canonical was checked by the pass's own
    /// simulation, which followed that body's edges.
    fn check_pad_chains(&mut self, recorded: &Recorded, reachable: &[bool]) -> Result<(), Ice> {
        let f = self.f;
        if let Some(pad) = f.entry_unwind {
            let mut params = Vec::new();
            for &p in &f.params {
                if f.local(p)?.repr == Repr::Value {
                    params.push(p);
                }
            }
            params.sort_unstable();
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
    /// with repeats) and resumes.
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
        // The unwind pass drops the newest local first, so its chains come
        // out descending.
        dropped.reverse();
        if !dropped.is_sorted() {
            dropped.sort_unstable();
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
            Callee::Direct(def) => {
                let sig = self.module.callee(*def).map(|d| d.sig.clone());
                if sig.is_none() {
                    self.err(format!("direct callee {def:?} has no declaration"));
                }
                sig
            }
            Callee::Code(key) => {
                let sig = self
                    .module
                    .function(*key)
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
            Rvalue::CallForeign { sig, args, .. } => {
                if sig.params.len() != args.len() {
                    self.err("foreign call with the wrong number of arguments");
                }
                if sig.params.contains(&CType::Void) {
                    self.err("a foreign parameter of type void");
                }
                for (n, (arg, &ctype)) in args.iter().zip(&sig.params).enumerate() {
                    // The fiber's context of a `@std_context` kernel
                    // (D-692, D-745).
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
            Rvalue::CallRuntime { func, args } => self.check_runtime(*func, args),
            Rvalue::Alloc { layout, fields } => {
                self.check_alloc(*layout, fields);
                Some(Repr::Value)
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

/// Whether `value` is a call without a result: a foreign function
/// returning `void` or a runtime helper returning nothing.
fn is_void_call(value: &Rvalue) -> bool {
    match value {
        Rvalue::CallForeign { sig, .. } => sig.ret == CType::Void,
        Rvalue::CallRuntime { func, .. } => func.ret().is_none(),
        _ => false,
    }
}

/// Whether a foreign `int64_t` result lands in an `I64` local.
fn foreign_i64(value: &Rvalue, dst: Repr) -> bool {
    matches!(value, Rvalue::CallForeign { sig, .. } if sig.ret == CType::I64 && dst == Repr::I64)
}

/// The locals a statement reads, Dup and Drop included.
fn stmt_locals(stmt: &Stmt) -> Vec<Local> {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => {
            value.uses().into_iter().map(|(l, _)| l).collect()
        }
        Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
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
