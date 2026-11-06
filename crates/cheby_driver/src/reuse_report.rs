//! `--reuse-report` on `cheby run` and `cheby test` (D-170, D-717,
//! §13.1): one line per reuse candidate of the package's own code, written
//! to standard error after the warnings and before anything runs.
//!
//! D-852, refining D-717 (and D-170 through it):
//!
//! - A line is `reuse: <file>:<line>:<col>: <text>`. The lines are sorted
//!   by file, line and column (then by text), so the report never depends
//!   on the order of units, functions or threads (D-264).
//! - The candidates are the allocations of a constructor with fields or of
//!   a tuple that [`cheby_mir::reuse::decisions`] judges, in the code units
//!   and tests units of the package's own modules, never std or a
//!   dependency. The location is the source site lowering recorded for the
//!   allocation ([`cheby_mir::Function::sites`]): the expression that
//!   builds the value. The text gives one of the five reasons of the
//!   decision (survey-perceus §4.2), and names the value it reuses or does
//!   not reuse, without the location of a later use:
//!
//!   ```text
//!   building `Shape` reuses `current` when it is unique
//!   building `Shape` does not reuse `current`: it is used again later or released elsewhere
//!   building `Node` does not reuse `t`: it is borrowed from the caller
//!   building `Policy` does not reuse `save_policy`: it is a constant
//!   building `Node` allocates: no value of the same size dies before it
//!   ```
//!
//! - The sixth kind of line is a kernel line: a call of a std kernel of
//!   [`cheby_mir::kernels_in_place`] whose first argument is a local at its
//!   last use (consumed by the call, not used after it, and held by no
//!   other value of the function that is still alive at the call,
//!   [`Flow::held_elsewhere`]) gets
//!
//!   ```text
//!   `set::insert` gets `held` at its last use: in place when it is unique at run time
//!   ```
//!
//!   The line is a claim about one function: it accounts only for the
//!   holders inside the function, never for a caller's or a callee's.
//!   A caller that keeps the value alive across the call, or a callee
//!   that stores a holder (a channel, for one), makes the kernel copy
//!   although the line is printed.
//!
//!   A call in tail position defines no local, so it has no site and gets
//!   no line.
//! - A value without a source name is `a temporary value`.
//! - Under `CHEBY_RC=naive` nothing is reused (D-746, D-808), and the
//!   report is the one line `reuse: reuse is off (CHEBY_RC=naive)`.
//! - In a `cheby test` build the borrow masks can differ from `cheby run`'s
//!   (D-779, D-813), so the report of the same function can differ too.
//!
//! The report only reads the MIR; the sites are recorded only when the
//! flag asks for them, and code generation never reads them, so the code
//! is the same with and without the flag.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use cheby_diag::{SourceMap, Span};
use cheby_iface::{
    CtorFields, DefId, Ice, Idx, InterfaceSet, Interner, ItemSig, ModuleId, Ty, TyInterner, TyKind,
    TypeDef, TypeDefKind, builtin,
};
use cheby_mir::ir::Rvalue;
use cheby_mir::reuse::{ReuseDecision, ReuseReason};
use cheby_mir::{
    BlockId, Callee, Const, Function, LayoutRef, Local, MirModule, Operand, RuntimeFn, Stmt,
    Terminator,
};

/// The report's only line under naive counting.
pub(crate) const NAIVE_LINE: &str = "reuse: reuse is off (CHEBY_RC=naive)\n";

/// One line before sorting: its place (file, line, column), then its text.
pub(crate) type Line = ((String, u32, u32), String);

/// The report of `units`, a counted build, for the units of the modules in
/// `local`: every line with its newline, sorted (module docs). `naive`
/// gives [`NAIVE_LINE`] alone. `types` reads the locals' types, which
/// tell a call result that cannot hold a kernel's argument.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR or a site outside the source map.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "commands report through `reuse_replay::report` (K4d), which caches lines per unit; tests compare with this"
    )
)]
pub(crate) fn report(
    units: &[MirModule],
    local: &BTreeSet<ModuleId>,
    sources: &SourceMap,
    names: &Interner,
    types: &Types<'_>,
    naive: bool,
) -> Result<String, Ice> {
    if naive {
        return Ok(NAIVE_LINE.to_owned());
    }
    let mut lines: Vec<Line> = Vec::new();
    for unit in units {
        if !unit.module().is_some_and(|m| local.contains(&m)) {
            continue;
        }
        for f in &unit.functions {
            if f.sites.is_empty() {
                continue;
            }
            function_lines(unit, f, sources, names, types, &mut lines)?;
        }
    }
    Ok(render(lines))
}

/// The report's text of `lines`, sorted, each with its newline (module
/// docs).
pub(crate) fn render(mut lines: Vec<Line>) -> String {
    lines.sort();
    let mut out = String::new();
    for ((path, line, col), text) in lines {
        // Writing to a `String` cannot fail.
        let _ = writeln!(out, "reuse: {path}:{line}:{col}: {text}");
    }
    out
}

/// The lines of one function with recorded sites, unsorted: also what K4d
/// caches beside a unit's code record (D-965, `reuse_replay.rs`).
pub(crate) fn function_lines(
    unit: &MirModule,
    f: &Function,
    sources: &SourceMap,
    names: &Interner,
    types: &Types<'_>,
    out: &mut Vec<Line>,
) -> Result<(), Ice> {
    // The first site of each local (a local is defined once).
    let mut sites: BTreeMap<Local, Span> = BTreeMap::new();
    for &(local, span) in &f.sites {
        sites.entry(local).or_insert(span);
    }
    let defs = definitions(f);
    let place = |span: Span| -> Result<(String, u32, u32), Ice> {
        let at = sources
            .location(span)
            .ok_or_else(|| Ice::new(format!("reuse report: no source file for {span:?}")))?;
        Ok((at.path, at.line, at.col))
    };
    for decision in cheby_mir::reuse::decisions(unit, f)? {
        let Some(&span) = sites.get(&decision.sink) else {
            continue;
        };
        let text = decision_text(unit, f, &defs, names, decision)?;
        out.push((place(span)?, text));
    }
    for (call, first) in kernel_calls(unit, f, types)? {
        let Some(&span) = sites.get(&call.result) else {
            continue;
        };
        let value = describe(unit, f, &defs, names, first)?;
        out.push((
            place(span)?,
            format!(
                "`{}` gets {value} at its last use: in place when it is unique at run time",
                call.name
            ),
        ));
    }
    Ok(())
}

/// The rvalue that defines each local assigned in `f`.
fn definitions(f: &Function) -> BTreeMap<Local, &Rvalue> {
    let mut defs = BTreeMap::new();
    for block in &f.blocks {
        for stmt in &block.stmts {
            if let Stmt::Assign { dst, value } = stmt {
                defs.entry(*dst).or_insert(value);
            }
        }
    }
    defs
}

/// The text of one allocation's decision.
fn decision_text(
    unit: &MirModule,
    f: &Function,
    defs: &BTreeMap<Local, &Rvalue>,
    names: &Interner,
    decision: ReuseDecision,
) -> Result<String, Ice> {
    let built = match defs.get(&decision.sink) {
        Some(Rvalue::Alloc { layout, .. } | Rvalue::AllocReuse { layout, .. }) => {
            built_value(unit, *layout)
        }
        _ => {
            return Err(Ice::new(
                "reuse report: a decision for a local no allocation defines",
            ));
        }
    };
    let source = |reason: &str| -> Result<String, Ice> {
        let local = decision
            .source
            .ok_or_else(|| Ice::new(format!("reuse report: {reason} without a candidate")))?;
        describe(unit, f, defs, names, local)
    };
    Ok(match decision.reason {
        ReuseReason::Reused => {
            format!(
                "building {built} reuses {} when it is unique",
                source("a reuse")?
            )
        }
        ReuseReason::UsedLater => format!(
            "building {built} does not reuse {}: it is used again later or released elsewhere",
            source("a later use")?
        ),
        ReuseReason::Borrowed => format!(
            "building {built} does not reuse {}: it is borrowed from the caller",
            source("a borrowed candidate")?
        ),
        ReuseReason::Constant => format!(
            "building {built} does not reuse {}: it is a constant",
            source("a constant candidate")?
        ),
        ReuseReason::NoCandidate => {
            format!("building {built} allocates: no value of the same size dies before it")
        }
    })
}

/// What an allocation of `layout` builds: `` `Shape` `` or `a tuple of 2`.
fn built_value(unit: &MirModule, layout: LayoutRef) -> String {
    match layout {
        LayoutRef::Tuple(n) => format!("a tuple of {n}"),
        _ => unit.layout(layout).map_or_else(
            || "a value".to_owned(),
            |desc| format!("`{}`", desc.ctor_name),
        ),
    }
}

/// How the report names `local`: `` `name` `` for a source name, the
/// constant's name for a constant's value, `a temporary value` otherwise.
fn describe(
    unit: &MirModule,
    f: &Function,
    defs: &BTreeMap<Local, &Rvalue>,
    names: &Interner,
    local: Local,
) -> Result<String, Ice> {
    if let Some(name) = f.local(local)?.name {
        return Ok(format!("`{}`", names.get(name)));
    }
    if let Some(Rvalue::LoadConst(def)) = defs.get(&local) {
        return Ok(format!("`{}`", unit.name(*def)?.item));
    }
    Ok("a temporary value".to_owned())
}

/// A call of an in-place std kernel: the local it defines and the kernel's
/// name as written in a program, `set::insert`.
struct KernelCall {
    result: Local,
    name: String,
}

/// The calls in `f`'s normally reachable blocks of a kernel of
/// [`cheby_mir::kernels_in_place`] whose first argument is a local at its
/// last use, with that local.
fn kernel_calls(
    unit: &MirModule,
    f: &Function,
    types: &Types<'_>,
) -> Result<Vec<(KernelCall, Local)>, Ice> {
    let mut out = Vec::new();
    let reached = normally_reachable(f);
    let flow = Flow::of(f, types, &reached);
    for (id, block) in f.blocks.iter_enumerated() {
        if !reached.get(id.index()).copied().unwrap_or(false) {
            continue;
        }
        for (at, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign {
                dst,
                value: Rvalue::Call { callee, args },
            } = stmt
            else {
                continue;
            };
            let Some(def) = callee.direct() else {
                continue;
            };
            let name = unit.name(def)?;
            if !cheby_mir::kernels_in_place::updates_in_place(&name.module, &name.item) {
                continue;
            }
            let Some(Operand::Local(first)) = args.first() else {
                continue;
            };
            let first = *first;
            // Borrowed (never for a `pub` std function, D-787), passed
            // twice, a constant's value, used after the call, or held by
            // another value at the call: not at its last use.
            let borrowed = matches!(callee, Callee::Borrowing(_, mask) if mask.is_borrowed(0));
            let twice = args.iter().skip(1).any(|a| a.local() == Some(first));
            if borrowed
                || twice
                || flow.constants.contains(&first)
                || used_after(f, id, at, first)?
                || flow.held_elsewhere(id, at, first)?
            {
                continue;
            }
            let module = name.module.strip_prefix("std::").unwrap_or(&name.module);
            out.push((
                KernelCall {
                    result: *dst,
                    name: format!("{module}::{}", name.item),
                },
                first,
            ));
        }
    }
    Ok(out)
}

/// The locals of `f` that hold a constant's value on every path, which is
/// static and never unique: a `load_const` or a constant operand, a move
/// of one, a block parameter every `goto` (`incoming`) passes one, and a
/// field read of an object whose field is one on every path (`("-", x)`
/// or `("", y)` from the arms of a `case`, then `.0`). A fixed point that
/// only adds, so a parameter of a cycle is never one (that is the safe
/// side: it keeps the line).
fn constants(f: &Function, incoming: &BTreeMap<Local, Vec<Option<Local>>>) -> BTreeSet<Local> {
    /// The fields that hold a constant: `None` for every field (an object
    /// that is itself a constant), else the indices.
    type ConstFields = Option<BTreeSet<u32>>;
    let constant = |fields: &BTreeMap<Local, ConstFields>, op: &Operand| match op {
        Operand::Const(_) => true,
        Operand::Local(l) => fields.get(l).is_some_and(Option::is_none),
    };
    // Every local with a constant field is a key; a constant itself maps
    // to `None`.
    let mut fields: BTreeMap<Local, ConstFields> = BTreeMap::new();
    // Only adds: a constant stays one, and a field once known stays known.
    let set = |fields: &mut BTreeMap<Local, ConstFields>, l: Local, v: ConstFields| {
        let new = match (fields.get(&l), v) {
            (Some(None), _) | (_, None) => None,
            (Some(Some(old)), Some(at)) => Some(old.union(&at).copied().collect()),
            (None, Some(at)) => Some(at),
        };
        if fields.get(&l) == Some(&new) {
            return false;
        }
        fields.insert(l, new);
        true
    };
    loop {
        let mut changed = false;
        for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
            let Stmt::Assign { dst, value } = stmt else {
                continue;
            };
            let new: Option<ConstFields> = match value {
                Rvalue::LoadConst(_) | Rvalue::Use(Operand::Const(_)) => Some(None),
                Rvalue::Use(Operand::Local(m)) => fields.get(m).cloned(),
                Rvalue::Field { base, field, .. } => fields
                    .get(base)
                    .filter(|v| v.as_ref().is_none_or(|at| at.contains(field)))
                    .map(|_| None),
                Rvalue::Alloc { fields: ops, .. } | Rvalue::AllocReuse { fields: ops, .. } => {
                    let at: BTreeSet<u32> = ops
                        .iter()
                        .enumerate()
                        .filter(|(_, op)| constant(&fields, op))
                        .filter_map(|(n, _)| u32::try_from(n).ok())
                        .collect();
                    (!at.is_empty()).then_some(Some(at))
                }
                _ => None,
            };
            if let Some(v) = new {
                changed |= set(&mut fields, *dst, v);
            }
        }
        for (&p, args) in incoming {
            if args.is_empty() {
                continue;
            }
            // What every incoming value has in common.
            let mut common: Option<ConstFields> = Some(None);
            for arg in args {
                let this = match arg {
                    None => None,
                    Some(a) => {
                        let Some(v) = fields.get(a) else {
                            common = None;
                            break;
                        };
                        v.clone()
                    }
                };
                common = common.map(|c| match (c, this) {
                    (None, t) => t,
                    (c, None) => c,
                    (Some(a), Some(b)) => Some(a.intersection(&b).copied().collect()),
                });
            }
            if let Some(v) = common.filter(|v| v.as_ref().is_none_or(|at| !at.is_empty())) {
                changed |= set(&mut fields, p, v);
            }
        }
        if !changed {
            break;
        }
    }
    fields
        .into_iter()
        .filter_map(|(l, v)| v.is_none().then_some(l))
        .collect()
}

/// By block index, whether the block is reached from the entry by normal
/// edges, not only through unwind pads.
fn normally_reachable(f: &Function) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![BlockId::from_u32(0)];
    while let Some(b) = work.pop() {
        let Some(block) = f.blocks.get(b) else {
            continue;
        };
        match seen.get_mut(b.index()) {
            Some(s) if !*s => *s = true,
            _ => continue,
        }
        work.extend(block.term.successors());
    }
    seen
}

/// Whether `x` is read or released after statement `at` of block `id`: in
/// the rest of the block, its terminator, or any block reached from it by
/// normal edges.
fn used_after(f: &Function, id: BlockId, at: usize, x: Local) -> Result<bool, Ice> {
    let block = f.block(id)?;
    let rest = block.stmts.iter().skip(at.saturating_add(1));
    if rest.clone().any(|s| mentions(s, x)) || term_mentions(&block.term, x) {
        return Ok(true);
    }
    let mut seen: BTreeSet<BlockId> = BTreeSet::new();
    let mut work: Vec<BlockId> = block.term.successors();
    while let Some(b) = work.pop() {
        if !seen.insert(b) {
            continue;
        }
        let next = f.block(b)?;
        if next.stmts.iter().any(|s| mentions(s, x)) || term_mentions(&next.term, x) {
            return Ok(true);
        }
        work.extend(next.term.successors());
    }
    Ok(false)
}

/// How a local of a function refers to the object that a kernel call's
/// first argument holds at the call ([`Flow::held_elsewhere`]).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Hold {
    /// The object itself, under another name or another reference.
    Same,
    /// The object, or a value that holds it, in one of these fields
    /// (declaration order).
    Fields(BTreeSet<u32>),
    /// The object somewhere inside, at any depth.
    Any,
}

impl Hold {
    /// The object in field `field`.
    fn field(field: u32) -> Self {
        Self::Fields(BTreeSet::from([field]))
    }

    /// What a local that refers to the object as `self` on one path and as
    /// `other` on another holds.
    fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Same, Self::Same) => Self::Same,
            (Self::Fields(a), Self::Fields(b)) => Self::Fields(a.union(b).copied().collect()),
            _ => Self::Any,
        }
    }

    /// Whether field `field` of a value holding the object as `self` may
    /// hold it: never for the object itself, whose fields it contains.
    fn reaches(&self, field: u32) -> bool {
        match self {
            Self::Same => false,
            Self::Fields(fields) => fields.contains(&field),
            Self::Any => true,
        }
    }
}

/// Joins `hold` into what `holds` says of `local`: whether that changed.
fn join_into(holds: &mut BTreeMap<Local, Hold>, local: Local, hold: Hold) -> bool {
    let new = match holds.get(&local) {
        Some(old) => old.join(&hold),
        None => hold,
    };
    if holds.get(&local) == Some(&new) {
        return false;
    }
    holds.insert(local, new);
    true
}

/// What [`Flow::held_elsewhere`] needs of one function: the normal
/// predecessors of each reachable block, the values each block parameter
/// receives, and the locals that hold a constant's value.
struct Flow<'f> {
    f: &'f Function,
    types: &'f Types<'f>,
    preds: BTreeMap<BlockId, Vec<BlockId>>,
    /// The block of each block parameter.
    param_block: BTreeMap<Local, BlockId>,
    /// For each block parameter, what every normally reachable `goto` to
    /// its block passes it: a local, or `None` for a constant.
    incoming: BTreeMap<Local, Vec<Option<Local>>>,
    /// The locals that hold a constant's value on every path: static,
    /// never unique ([`constants`]).
    constants: BTreeSet<Local>,
}

/// One kernel call that [`Flow::held_elsewhere`] judges.
struct KernelAt {
    /// The call is statement `at` of block `id`.
    id: BlockId,
    at: usize,
    /// The kernel's first argument.
    x: Local,
    /// The locals the call reads besides `x` (its other arguments, and an
    /// indirect callee).
    operands: BTreeSet<Local>,
    /// The blocks that can run before the call ([`Flow::reaching`]).
    before: BTreeSet<BlockId>,
    /// The Cheby type of `x`, when the MIR knows it.
    ty: Option<Ty>,
}

impl<'f> Flow<'f> {
    fn of(f: &'f Function, types: &'f Types<'f>, reached: &[bool]) -> Self {
        let mut preds: BTreeMap<BlockId, Vec<BlockId>> = BTreeMap::new();
        let mut param_block: BTreeMap<Local, BlockId> = BTreeMap::new();
        let mut incoming: BTreeMap<Local, Vec<Option<Local>>> = BTreeMap::new();
        for (b, block) in f.blocks.iter_enumerated() {
            for &p in &block.params {
                param_block.insert(p, b);
                incoming.entry(p).or_default();
            }
        }
        for (b, block) in f.blocks.iter_enumerated() {
            if !reached.get(b.index()).copied().unwrap_or(false) {
                continue;
            }
            for s in block.term.successors() {
                preds.entry(s).or_default().push(b);
            }
            if let Terminator::Goto { target, args } = &block.term
                && let Some(to) = f.blocks.get(*target)
            {
                for (p, arg) in to.params.iter().zip(args) {
                    incoming.entry(*p).or_default().push(arg.local());
                }
            }
        }
        let constants = constants(f, &incoming);
        Flow {
            f,
            types,
            preds,
            param_block,
            incoming,
            constants,
        }
    }

    /// Whether another value of this function is sure to hold `x`'s object
    /// when statement `at` of block `id` consumes `x`, so that the kernel
    /// there never finds it unique.
    ///
    /// D-852: survey-perceus §4.2 (UQ-21 (a)) wants a
    /// kernel line's argument to be "a local consumed by the call with no
    /// `dup` before it". Read literally, that also drops the line of a
    /// value lent to a call first (`set::contains(held, x)` before
    /// `set::insert(held, y)` in 013, a `dup` the call consumes, and the set
    /// is unique again at the insert). The report reads the rule as: no
    /// other value still alive at the call holds the argument's object.
    ///
    /// The holders come from a may-hold analysis of the statements that
    /// can run before the call, to a fixed point ([`Flow::holders`]):
    /// - the argument under another name: a move (`let t = s` after a
    ///   `dup`, a second reference), or a block parameter every `goto` to
    ///   whose block passes the object;
    /// - the base of a field projection that gives the object (D-205's
    ///   rule 4: the projection's `dup` leaves the base's reference in
    ///   place), and another projection of the same field; a list
    ///   pattern's element (`list_get` at a constant index, which works as
    ///   a field's) and a view of a list (`list_slice`) count as
    ///   projections of the list, and so does a fused step (`list_uncons`
    ///   and its `list_uncons_head`, D-909), read as that slice and get;
    /// - a value built from the object or a holder: an owned field of an
    ///   `alloc`, `alloc_reuse` or `init_field` (records, tuples, variants,
    ///   closures' captures), and a list (`list_push`, `list_append`,
    ///   `list_get`, `list_slice`);
    /// - the result of a call that takes the object or a holder, owned or
    ///   borrowed, unless the result's type cannot contain the argument's
    ///   type (`set::contains` returns a `Bool`);
    /// - a block parameter every `goto` to whose block passes a holder:
    ///   a `case` whose arms all hold the object. One arm that holds
    ///   nothing keeps the line, since the object can be unique there.
    ///
    /// A holder is alive at the call when it is used after the call, is
    /// another operand of the call itself, is a borrowed parameter (the
    /// caller holds it) or holds a constant's value on every path
    /// ([`constants`], which also drops the line of an argument that is
    /// one, as `""` or `"-"` from every arm of a `case`). When the argument
    /// itself comes from a block parameter, it is held when the value each
    /// `goto` passes is (a constant is never unique). The analysis sees only
    /// this function: a caller's holder, or one a callee keeps (a channel),
    /// is not seen, and the line, which says "when it is unique at run
    /// time", stays.
    fn held_elsewhere(&self, id: BlockId, at: usize, x: Local) -> Result<bool, Ice> {
        let operands = match self.f.block(id)?.stmts.get(at) {
            Some(Stmt::Assign { value, .. }) => value
                .uses()
                .into_iter()
                .map(|(l, _)| l)
                .filter(|&l| l != x)
                .collect(),
            _ => BTreeSet::new(),
        };
        let call = KernelAt {
            id,
            at,
            x,
            operands,
            before: self.reaching(id),
            ty: self.f.local(x)?.ty,
        };
        let mut open: BTreeSet<Local> = BTreeSet::new();
        let mut done: BTreeMap<Local, bool> = BTreeMap::new();
        self.held(&call, x, &mut open, &mut done)
    }

    /// Whether a value alive at `call` other than its argument holds the
    /// object `seed` refers to, on every path. `open` holds the seeds in
    /// progress (a cycle answers no), `done` the answers so far.
    fn held(
        &self,
        call: &KernelAt,
        seed: Local,
        open: &mut BTreeSet<Local>,
        done: &mut BTreeMap<Local, bool>,
    ) -> Result<bool, Ice> {
        if let Some(&answer) = done.get(&seed) {
            return Ok(answer);
        }
        if !open.insert(seed) {
            return Ok(false);
        }
        let holds = self.holders(call, seed)?;
        let mut answer = false;
        for &l in holds.keys() {
            if l != call.x && self.alive(call, l)? {
                answer = true;
                break;
            }
        }
        // The object under a block parameter's name: held when what every
        // `goto` passes is held.
        if !answer {
            for (p, hold) in &holds {
                let Some(incoming) = self.incoming.get(p) else {
                    continue;
                };
                if *hold != Hold::Same || incoming.is_empty() || !self.param_before(call, *p) {
                    continue;
                }
                let mut every = true;
                for arg in incoming {
                    let sure = match arg {
                        None => true,
                        Some(a) => self.held(call, *a, open, done)?,
                    };
                    if !sure {
                        every = false;
                        break;
                    }
                }
                if every {
                    answer = true;
                    break;
                }
            }
        }
        open.remove(&seed);
        done.insert(seed, answer);
        Ok(answer)
    }

    /// Whether `l`'s reference is alive at `call`: used after it, read by
    /// the call itself, a borrowed parameter, or a constant's value.
    fn alive(&self, call: &KernelAt, l: Local) -> Result<bool, Ice> {
        let borrowed = self
            .f
            .params
            .iter()
            .position(|&p| p == l)
            .is_some_and(|n| self.f.borrow.is_borrowed(n));
        Ok(call.operands.contains(&l)
            || borrowed
            || self.constants.contains(&l)
            || used_after(self.f, call.id, call.at, l)?)
    }

    /// Whether block parameter `p` is defined before `call`: its block can
    /// run before the call, or is the call's.
    fn param_before(&self, call: &KernelAt, p: Local) -> bool {
        self.param_block
            .get(&p)
            .is_some_and(|b| *b == call.id || call.before.contains(b))
    }

    /// The locals that refer to the object `seed` refers to, or hold it,
    /// when `call` runs, with how: a fixed point over the statements that
    /// can run before the call and the block parameters
    /// ([`Flow::held_elsewhere`] lists the rules). `seed` itself is
    /// [`Hold::Same`].
    fn holders(&self, call: &KernelAt, seed: Local) -> Result<BTreeMap<Local, Hold>, Ice> {
        let mut holds: BTreeMap<Local, Hold> = BTreeMap::from([(seed, Hold::Same)]);
        let stmts: Vec<&Stmt> = stmts_before(self.f, &call.before, call.id, call.at)
            .filter(|&(b, i, _)| (b, i) != (call.id, call.at))
            .map(|(_, _, stmt)| stmt)
            .collect();
        // A list pattern step read in one call (D-909) reads, for these
        // rules, as the element read and the spread it replaced: `rest =
        // list_uncons(l, at, s, e)` as `list_slice(l, s, e)` and the
        // `list_uncons_head()` right after it as `list_get(l, at)`.
        let stmts: Vec<Stmt> = stmts
            .iter()
            .enumerate()
            .map(|(k, stmt)| unfused(stmt, k.checked_sub(1).and_then(|p| stmts.get(p).copied())))
            .collect();
        loop {
            let mut changed = false;
            for stmt in &stmts {
                changed |= self.step(call, stmt, &mut holds)?;
            }
            for (&p, incoming) in &self.incoming {
                if incoming.is_empty() || !self.param_before(call, p) {
                    continue;
                }
                let mut joined: Option<Hold> = None;
                for arg in incoming {
                    let Some(h) = arg.and_then(|a| holds.get(&a)) else {
                        joined = None;
                        break;
                    };
                    joined = Some(joined.map_or_else(|| h.clone(), |j| j.join(h)));
                }
                if let Some(h) = joined {
                    changed |= join_into(&mut holds, p, h);
                }
            }
            if !changed {
                break;
            }
        }
        Ok(holds)
    }

    /// One statement's rules of [`Flow::holders`]: whether it changed
    /// `holds`.
    fn step(
        &self,
        call: &KernelAt,
        stmt: &Stmt,
        holds: &mut BTreeMap<Local, Hold>,
    ) -> Result<bool, Ice> {
        let held = |holds: &BTreeMap<Local, Hold>, op: &Operand| {
            op.local().is_some_and(|l| holds.contains_key(&l))
        };
        let (dst, value) = match stmt {
            Stmt::Assign { dst, value } => (*dst, value),
            Stmt::Eval(Rvalue::InitField {
                obj, field, value, ..
            }) => {
                return Ok(held(holds, value) && join_into(holds, *obj, Hold::field(*field)));
            }
            _ => return Ok(false),
        };
        Ok(match value {
            // A move: one object under two names, both ways.
            Rvalue::Use(Operand::Local(m)) => {
                let mut changed = false;
                if let Some(h) = holds.get(m).cloned() {
                    changed |= join_into(holds, dst, h);
                }
                if let Some(h) = holds.get(&dst).cloned() {
                    changed |= join_into(holds, *m, h);
                }
                changed
            }
            Rvalue::Field { base, field, .. } => {
                let mut changed = false;
                if holds.get(base).is_some_and(|h| h.reaches(*field)) {
                    changed |= join_into(holds, dst, Hold::Any);
                }
                if holds.contains_key(&dst) {
                    changed |= join_into(holds, *base, Hold::field(*field));
                }
                changed
            }
            Rvalue::Alloc { layout, fields } | Rvalue::AllocReuse { layout, fields, .. } => {
                let at: BTreeSet<u32> = fields
                    .iter()
                    .enumerate()
                    .filter(|(_, op)| held(holds, op))
                    .filter_map(|(n, _)| u32::try_from(n).ok())
                    .collect();
                if at.is_empty() {
                    false
                } else if matches!(layout, LayoutRef::Closure(_)) {
                    join_into(holds, dst, Hold::Any)
                } else {
                    join_into(holds, dst, Hold::Fields(at))
                }
            }
            Rvalue::CallRuntime { func, args } => {
                let list = args.first().and_then(Operand::local);
                // A list pattern's element at a constant index, which
                // works as a field's index.
                let index = match (func, args.get(1)) {
                    (RuntimeFn::ListGet, Some(Operand::Const(Const::Int(i)))) => {
                        u32::try_from(*i).ok()
                    }
                    _ => None,
                };
                match func {
                    RuntimeFn::ListPush | RuntimeFn::ListAppend => {
                        args.iter().any(|op| held(holds, op)) && join_into(holds, dst, Hold::Any)
                    }
                    RuntimeFn::ListGet | RuntimeFn::ListSlice | RuntimeFn::ListSliceOwned => {
                        let get = matches!(func, RuntimeFn::ListGet);
                        // An element of a list that holds the object there,
                        // or a view of one, which shares its root.
                        let reaches =
                            list.and_then(|l| holds.get(&l))
                                .is_some_and(|h| match (get, index) {
                                    (true, Some(i)) => h.reaches(i),
                                    (true, None) => *h != Hold::Same,
                                    (false, _) => true,
                                });
                        let mut changed = reaches && join_into(holds, dst, Hold::Any);
                        // The list holds its element, and a view's root, as
                        // a projection's base does.
                        if holds.contains_key(&dst)
                            && let Some(l) = list
                        {
                            let where_ = match (get, index) {
                                (true, Some(i)) => Hold::field(i),
                                _ => Hold::Any,
                            };
                            changed |= join_into(holds, l, where_);
                        }
                        changed
                    }
                    _ => false,
                }
            }
            Rvalue::Call { .. } | Rvalue::CallForeign { .. } => {
                value.uses().iter().any(|(l, _)| holds.contains_key(l))
                    && self.may_hold(call, dst)?
                    && join_into(holds, dst, Hold::Any)
            }
            _ => false,
        })
    }

    /// Whether `dst`, a call's result, can hold a value of the kernel
    /// argument's type: yes unless both types are known and the result's
    /// cannot contain the argument's ([`Types::may_contain`]).
    fn may_hold(&self, call: &KernelAt, dst: Local) -> Result<bool, Ice> {
        let (Some(target), Some(ty)) = (call.ty, self.f.local(dst)?.ty) else {
            return Ok(true);
        };
        Ok(self.types.may_contain(ty, target))
    }

    /// The reachable blocks from which `to` is reached by normal edges, in
    /// one step or more (`to` itself only through a cycle).
    fn reaching(&self, to: BlockId) -> BTreeSet<BlockId> {
        let mut seen: BTreeSet<BlockId> = BTreeSet::new();
        let mut work: Vec<BlockId> = self.preds.get(&to).cloned().unwrap_or_default();
        while let Some(b) = work.pop() {
            if seen.insert(b)
                && let Some(more) = self.preds.get(&b)
            {
                work.extend(more.iter().copied());
            }
        }
        seen
    }
}

/// The statements that can run before statement `at` of block `id`, each
/// with its block and index: those of the blocks in `before`
/// ([`Flow::reaching`]), then the ones before `at` in `id` itself unless a
/// cycle already put all of `id` in.
fn stmts_before<'f>(
    f: &'f Function,
    before: &'f BTreeSet<BlockId>,
    id: BlockId,
    at: usize,
) -> impl Iterator<Item = (BlockId, usize, &'f Stmt)> {
    f.blocks.iter_enumerated().flat_map(move |(b, block)| {
        let upto = if before.contains(&b) {
            block.stmts.len()
        } else if b == id {
            at
        } else {
            0
        };
        block
            .stmts
            .iter()
            .take(upto)
            .enumerate()
            .map(move |(i, stmt)| (b, i, stmt))
    })
}

/// `stmt` with a fused list pattern step undone ([`Flow::holders`]):
/// `prev` is the statement before it, if any.
fn unfused(stmt: &Stmt, prev: Option<&Stmt>) -> Stmt {
    let call = |s: &Stmt| match s {
        Stmt::Assign {
            dst,
            value: Rvalue::CallRuntime { func, args },
        } => Some((*dst, *func, args.clone())),
        _ => None,
    };
    match call(stmt) {
        Some((dst, RuntimeFn::ListUncons | RuntimeFn::ListUnconsOwned, args)) => {
            let mut it = args.into_iter();
            let (list, _at) = (it.next(), it.next());
            Stmt::Assign {
                dst,
                value: Rvalue::CallRuntime {
                    func: RuntimeFn::ListSlice,
                    args: list.into_iter().chain(it).collect(),
                },
            }
        }
        Some((dst, RuntimeFn::ListUnconsHead, _)) => match prev.and_then(call) {
            Some((_, RuntimeFn::ListUncons | RuntimeFn::ListUnconsOwned, args)) => Stmt::Assign {
                dst,
                value: Rvalue::CallRuntime {
                    func: RuntimeFn::ListGet,
                    args: args.into_iter().take(2).collect(),
                },
            },
            _ => stmt.clone(),
        },
        _ => stmt.clone(),
    }
}

/// What the report reads of the build's types: the type table and the
/// interfaces, which hold every constructor's field types (opaque types'
/// included, as their layouts need them).
pub(crate) struct Types<'a> {
    pub(crate) table: &'a TyInterner,
    pub(crate) interfaces: &'a InterfaceSet,
}

/// How many types [`Types::may_contain`] looks into before it answers yes:
/// a type that keeps growing (`Deep(Nested<List<T>>)`) ends there.
const TYPE_BUDGET: usize = 256;

impl Types<'_> {
    /// Whether a value of type `ty` can contain a value of type `target`
    /// (`target` itself included). No only when every type inside is
    /// known: the scalars hold nothing, `List`, `Map`, `Set` and tuples
    /// hold their arguments, and a type with constructors (`Option`,
    /// `Result`, a user type, an opaque one) holds its constructors'
    /// fields, its arguments put in. A type parameter, a function (its
    /// captures), `dyn`, an `@external` type and a type whose interface
    /// is not at hand may hold anything.
    fn may_contain(&self, ty: Ty, target: Ty) -> bool {
        let mut seen: rustc_hash::FxHashSet<Ty> = rustc_hash::FxHashSet::default();
        let mut work = vec![ty];
        while let Some(t) = work.pop() {
            if t == target {
                return true;
            }
            if !seen.insert(t) {
                continue;
            }
            if seen.len() > TYPE_BUDGET {
                return true;
            }
            match self.table.kind(t) {
                TyKind::Never => {}
                TyKind::Error | TyKind::Param(_) | TyKind::Fn { .. } | TyKind::Dyn { .. } => {
                    return true;
                }
                TyKind::Tuple(items) => work.extend(items.iter().copied()),
                TyKind::Named { def, args } => {
                    if SCALARS.contains(def) {
                        continue;
                    }
                    if CONTAINERS.contains(def) {
                        work.extend(args.iter().copied());
                        continue;
                    }
                    let Some(fields) = self.ctor_fields(*def) else {
                        return true;
                    };
                    work.extend(
                        fields
                            .into_iter()
                            .map(|field| cheby_types::infer::substitute(self.table, field, args)),
                    );
                }
            }
        }
        false
    }

    /// The field types of every constructor of the type `def`, in the
    /// type's own parameters; `None` for a type without constructors or
    /// whose interface is not at hand.
    fn ctor_fields(&self, def: DefId) -> Option<Vec<Ty>> {
        let iface = self.interfaces.get(def.module)?;
        let ItemSig::Type(TypeDef {
            kind: TypeDefKind::Adt { ctors },
            ..
        }) = &iface.item(def.index)?.sig
        else {
            return None;
        };
        let mut out = Vec::new();
        for &ctor in ctors {
            let ItemSig::Ctor(sig) = &iface.item(ctor)?.sig else {
                return None;
            };
            match &sig.fields {
                CtorFields::Unit => {}
                CtorFields::Positional(tys) => out.extend(tys.iter().copied()),
                CtorFields::Named(fields) => out.extend(fields.iter().map(|&(_, ty)| ty)),
            }
        }
        Some(out)
    }
}

/// The types that hold no other value ([`Types::may_contain`]).
const SCALARS: [DefId; 17] = [
    builtin::INT,
    builtin::FLOAT,
    builtin::STRING,
    builtin::BOOL,
    builtin::NIL_TY,
    builtin::ORDER,
    builtin::NEVER,
    builtin::I8,
    builtin::I16,
    builtin::I32,
    builtin::I64,
    builtin::U8,
    builtin::U16,
    builtin::U32,
    builtin::U64,
    builtin::F32,
    builtin::BYTES,
];

/// The builtin types without constructors that hold exactly their
/// arguments ([`Types::may_contain`]).
const CONTAINERS: [DefId; 3] = [builtin::LIST, builtin::MAP, builtin::SET];

/// Whether `stmt` reads, consumes or releases `x`.
fn mentions(stmt: &Stmt, x: Local) -> bool {
    if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
        return value.uses().iter().any(|&(l, _)| l == x);
    }
    if let Stmt::Dup(l) | Stmt::Drop(l) = stmt {
        return *l == x;
    }
    if let Stmt::DropKeep { local, keep, .. } = stmt {
        return *local == x || keep.iter().any(|&(_, l)| l == x);
    }
    false
}

/// Whether the terminator reads or passes on `x`.
fn term_mentions(term: &cheby_mir::Terminator, x: Local) -> bool {
    term.uses().iter().any(|&(l, _)| l == x)
}
