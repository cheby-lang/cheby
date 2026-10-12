//! The forward simulation of reference counts (D7, D-698), shared by the
//! validator and the unwind pass.
//!
//! On every path from the entry, each counted ([`Repr::Value`]) local holds
//! some references: a function or block parameter one, an owned result
//! one, a borrowed result (`field`, `load_const`) none until its `dup`.
//! Owned uses consume one each, borrowed uses need one that survives the
//! instruction, and every `return`, tail call and `resume` leaves nothing
//! held. Where control flow joins, every predecessor must bring the same
//! counts.
//!
//! The simulation follows the normal successors and the unwind edges
//! ([`crate::Function::successors`]) and the entry pad. Along the unwind
//! edge of an instruction it carries what an unwinding leaves held there
//! (D-698):
//!
//! - a Cheby callee owns its arguments on every path, so they are gone;
//! - a runtime helper with owned parameters ([`RuntimeFn::params`])
//!   consumes them even when it raises;
//! - a kernel borrows its arguments (D-187), an `alloc` or `box` consumes
//!   nothing until it succeeded, and the other instructions only read, so
//!   their operands stay held;
//! - the instruction's own result does not exist.
//!
//! The entry pad starts with the parameters, as `bb0` does.
//!
//! [`RuntimeFn::params`]: crate::RuntimeFn::params

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;

use cheby_iface::Ice;

use crate::ir::{BlockId, Function, Local, Rvalue, Stmt, Terminator, UseKind};
use crate::repr::Repr;

/// The references each counted local holds on one path.
///
/// `held` lists only the locals that hold at least one reference: a
/// local whose last reference is consumed leaves it, so the state, and
/// every copy of it taken at an unwind site, stays as large as what is
/// live rather than what the block has defined so far.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    /// References per local, never 0; a local that is absent holds none.
    /// A local defined by `field` or `load_const` holds none until its
    /// `dup`.
    pub(crate) held: BTreeMap<Local, i64>,
    /// Locals defined by `field` (with their base) or `load_const` in the
    /// current block, or flowing in still holding a reference.
    pub(crate) borrowed_from: BTreeMap<Local, Option<Local>>,
}

impl Counts {
    /// What the path owns: each local once per reference it holds, in
    /// ascending local order. The key of a cleanup pad (D-698).
    pub(crate) fn owned(&self) -> Vec<Local> {
        let mut out = Vec::new();
        for (&l, &c) in &self.held {
            for _ in 0..c.max(0) {
                out.push(l);
            }
        }
        out
    }

    /// The counts without the locals that hold nothing, as they flow into
    /// a successor. Linear in what is held, not in what was borrowed.
    fn normalized(&self) -> Self {
        let held = self.held.clone();
        let borrowed_from = held
            .keys()
            .filter_map(|l| self.borrowed_from.get(l).map(|&base| (*l, base)))
            .collect();
        Self {
            held,
            borrowed_from,
        }
    }

    /// The references `l` holds: 0 when it is absent.
    fn count(&self, l: Local) -> i64 {
        self.held.get(&l).copied().unwrap_or(0)
    }

    /// Sets the references `l` holds, keeping `held` free of zeros.
    fn set(&mut self, l: Local, count: i64) {
        if count == 0 {
            self.held.remove(&l);
        } else {
            self.held.insert(l, count);
        }
    }
}

/// The text of an owned multiset, for messages: `{_3, _4, _4}`.
pub(crate) fn set_text(owned: &[Local]) -> String {
    let mut out = String::from("{");
    for (n, l) in owned.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "_{}", l.as_u32());
    }
    out.push('}');
    out
}

/// What the simulation tells its user.
pub(crate) trait Observer {
    /// A broken ownership rule, with where it is broken.
    fn error(&mut self, message: String);

    /// The instruction at `at` of `block` (the terminator when
    /// `at == stmts.len()`) may unwind ([`crate::unwind::stmt_may_unwind`],
    /// [`crate::unwind::terminator_may_unwind`]) or has an unwind edge, and
    /// `state` is what an unwinding leaves held there, without the locals
    /// that hold nothing. Reported once per such instruction of a
    /// reachable block: always where it has an edge, and where it has none
    /// only when [`Observer::wants_unwinds`] says so.
    fn unwinds(&mut self, block: BlockId, at: usize, state: &Counts) {
        let _ = (block, at, state);
    }

    /// Whether [`Observer::unwinds`] should be called at instructions that
    /// may unwind but have no edge yet. Computing the unwind state costs
    /// a copy of the live counts, so an observer that ignores it says no;
    /// instructions with an edge compute it anyway, for the pad.
    fn wants_unwinds(&self) -> bool {
        false
    }
}

/// Runs the simulation over `f`, reporting to `observer`.
///
/// # Errors
///
/// An [`Ice`] for an unknown block or local.
pub(crate) fn simulate(f: &Function, observer: &mut impl Observer) -> Result<(), Ice> {
    Sim {
        f,
        observer,
        entry_state: BTreeMap::new(),
        from: BTreeMap::new(),
        work: Vec::new(),
    }
    .run()
}

/// Where the simulation is, for messages. Formatted only when a message
/// is reported.
#[derive(Clone, Copy, Debug)]
enum Site {
    /// The function's entry, into `bb0`.
    Entry,
    /// The function-entry check, into the entry pad.
    EntryCheck,
    /// Statement `n` of a block.
    Stmt(BlockId, usize),
    /// A block's terminator.
    Term(BlockId),
    /// A `resume` terminator.
    Resume(BlockId),
    /// The unwind edge of statement `n` (the terminator when `n` is the
    /// number of statements).
    Edge(BlockId, usize),
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Entry => f.write_str("the function's entry"),
            Self::EntryCheck => f.write_str("the entry check"),
            Self::Stmt(b, n) => write!(f, "bb{} statement {n}", b.as_u32()),
            Self::Term(b) => write!(f, "bb{} terminator", b.as_u32()),
            Self::Resume(b) => write!(f, "bb{} terminator (resume)", b.as_u32()),
            Self::Edge(b, n) => write!(f, "the unwind edge of bb{} statement {n}", b.as_u32()),
        }
    }
}

struct Sim<'a, O> {
    f: &'a Function,
    observer: &'a mut O,
    /// The counts each block starts with, from its first predecessor.
    entry_state: BTreeMap<BlockId, Counts>,
    /// Where each block's first predecessor is, for messages.
    from: BTreeMap<BlockId, Site>,
    work: Vec<BlockId>,
}

impl<O: Observer> Sim<'_, O> {
    fn counted(&self, l: Local) -> Result<bool, Ice> {
        Ok(self.f.local(l)?.repr == Repr::Value)
    }

    fn run(mut self) -> Result<(), Ice> {
        let f = self.f;
        let mut start = Counts::default();
        for &p in &f.params {
            if self.counted(p)? {
                start.held.insert(p, 1);
            }
        }
        let bb0 = BlockId::from_u32(0);
        if let Some(pad) = f.entry_unwind {
            self.flow(pad, start.clone(), Site::EntryCheck);
        }
        self.flow(bb0, start, Site::Entry);
        let mut visited = BTreeSet::new();
        while let Some(id) = self.work.pop() {
            if visited.insert(id) {
                self.block(id)?;
            }
        }
        Ok(())
    }

    fn block(&mut self, id: BlockId) -> Result<(), Ice> {
        let f = self.f;
        let block = f.block(id)?;
        let mut st = self.entry_state.get(&id).cloned().unwrap_or_default();
        for &p in &block.params {
            if self.counted(p)? {
                st.held.insert(p, 1);
            }
        }
        let mut edges = block.unwind.iter().peekable();
        for (n, stmt) in block.stmts.iter().enumerate() {
            let edge = match edges.peek() {
                Some(&&(i, pad)) if usize::try_from(i).ok() == Some(n) => {
                    edges.next();
                    Some(pad)
                }
                _ => None,
            };
            if edge.is_some()
                || (self.observer.wants_unwinds() && crate::unwind::stmt_may_unwind(stmt))
            {
                let unwound = unwind_state(f, &st, stmt)?;
                self.observer.unwinds(id, n, &unwound);
                if let Some(pad) = edge {
                    self.flow(pad, unwound, Site::Edge(id, n));
                }
            }
            let at = Site::Stmt(id, n);
            match stmt {
                Stmt::Assign { dst, value } => {
                    self.consume(&mut st, &value.uses(), at)?;
                    if self.counted(*dst)? {
                        if value.result_is_borrowed() {
                            st.held.remove(dst);
                            let base = match value {
                                Rvalue::Field { base, .. } => Some(*base),
                                _ => None,
                            };
                            st.borrowed_from.insert(*dst, base);
                        } else {
                            st.held.insert(*dst, 1);
                        }
                    }
                }
                Stmt::Eval(value) => self.consume(&mut st, &value.uses(), at)?,
                Stmt::Dup(l) => self.dup(&mut st, *l, at),
                Stmt::Drop(l) => self.consume(&mut st, &[(*l, UseKind::Owned)], at)?,
                Stmt::ClearUnwind => {}
            }
        }
        let len = block.stmts.len();
        let here = Site::Term(id);
        let term_edge = block.unwind_target(len);
        if term_edge.is_some()
            || (self.observer.wants_unwinds() && crate::unwind::terminator_may_unwind(&block.term))
        {
            // A panic only reads its message.
            let unwound = st.normalized();
            self.observer.unwinds(id, len, &unwound);
            if let Some(pad) = term_edge {
                self.flow(pad, unwound, Site::Edge(id, len));
            }
        }
        match &block.term {
            Terminator::Return(_) | Terminator::TailCall { .. } => {
                self.consume(&mut st, &block.term.uses(), here)?;
                self.nothing_held(&st, here);
            }
            Terminator::Resume => self.nothing_held(&st, Site::Resume(id)),
            Terminator::Goto { target, .. } => {
                self.consume(&mut st, &block.term.uses(), here)?;
                self.flow(*target, st, here);
            }
            Terminator::Branch { .. } | Terminator::Switch { .. } => {
                self.consume(&mut st, &block.term.uses(), here)?;
                for succ in block.term.successors() {
                    self.flow(succ, st.clone(), here);
                }
            }
            // Without an edge, what is live leaks (no unwinding yet).
            Terminator::Panic { .. } | Terminator::Unreachable => {}
        }
        Ok(())
    }

    /// `return`, a tail call and `resume` leave nothing held.
    fn nothing_held(&mut self, st: &Counts, here: Site) {
        for (l, c) in &st.held {
            if *c != 0 {
                self.observer.error(format!(
                    "{here}: _{} still holds {c} reference(s)",
                    l.as_u32()
                ));
            }
        }
    }

    /// `dup l`: a `field` or `load_const` result holds nothing until its
    /// dup, which needs the base still alive.
    fn dup(&mut self, st: &mut Counts, l: Local, at: Site) {
        let c = st.count(l);
        let ok = match st.borrowed_from.get(&l) {
            _ if c >= 1 => true,
            Some(Some(base)) => st.count(*base) >= 1,
            Some(None) => true,
            None => false,
        };
        if ok {
            st.set(l, c.saturating_add(1));
        } else {
            self.observer.error(format!(
                "{at}: dup of _{} which holds no reference",
                l.as_u32()
            ));
        }
    }

    /// Passes `st` to `target`, which must agree with what its earlier
    /// predecessors brought.
    fn flow(&mut self, target: BlockId, mut st: Counts, from: Site) {
        let held = &st.held;
        st.borrowed_from.retain(|l, _| held.contains_key(l));
        match self.entry_state.get(&target) {
            Some(existing) if *existing != st => {
                let first = self.from.get(&target).copied().unwrap_or(from);
                self.observer.error(format!(
                    "reference counts differ between the predecessors of bb{}: {first} brings {}, {from} brings {}",
                    target.as_u32(),
                    set_text(&existing.owned()),
                    set_text(&st.owned()),
                ));
            }
            Some(_) => {}
            None => {
                self.entry_state.insert(target, st);
                self.from.insert(target, from);
                self.work.push(target);
            }
        }
    }

    /// Applies the uses of one instruction: borrowed uses need a reference
    /// that survives the instruction, owned uses consume one each.
    fn consume(&mut self, st: &mut Counts, uses: &[(Local, UseKind)], at: Site) -> Result<(), Ice> {
        let mut per: BTreeMap<Local, (i64, bool)> = BTreeMap::new();
        for &(l, kind) in uses {
            if !self.counted(l)? {
                continue;
            }
            let e = per.entry(l).or_insert((0, false));
            match kind {
                UseKind::Owned => e.0 = e.0.saturating_add(1),
                UseKind::Borrowed => e.1 = true,
            }
        }
        for (l, (owned, borrowed)) in per {
            let have = st.count(l);
            let need = owned.saturating_add(i64::from(borrowed));
            if have >= need && need > 0 {
                st.set(l, have.saturating_sub(owned));
            } else {
                self.observer.error(format!(
                    "{at}: _{} is used without a reference (holds {have}, needs {need})",
                    l.as_u32()
                ));
            }
        }
        Ok(())
    }
}

/// What an unwinding out of `stmt` leaves held, from the counts `st`
/// before it (module docs), without the locals that hold nothing.
/// Shortfalls are the normal path's errors, so here a count only goes
/// down to 0.
fn unwind_state(f: &Function, st: &Counts, stmt: &Stmt) -> Result<Counts, Ice> {
    let consumed = match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => consumed_on_unwind(value),
        Stmt::Dup(_) | Stmt::Drop(_) | Stmt::ClearUnwind => Vec::new(),
    };
    let mut out = st.normalized();
    for (l, kind) in consumed {
        if kind == UseKind::Owned
            && f.local(l)?.repr == Repr::Value
            && let Some(c) = out.held.get_mut(&l)
        {
            *c = c.saturating_sub(1).max(0);
            if *c == 0 {
                out.held.remove(&l);
                out.borrowed_from.remove(&l);
            }
        }
    }
    Ok(out)
}

/// The uses of `value` that are consumed even when it unwinds: every
/// owned argument of a Cheby call (the callee owns them on every path) and
/// the owned arguments of a runtime helper, which releases them before it
/// raises (D-698). Nothing else consumes before it succeeded.
fn consumed_on_unwind(value: &Rvalue) -> Vec<(Local, UseKind)> {
    match value {
        Rvalue::Call { .. } | Rvalue::CallRuntime { .. } => value.uses(),
        _ => Vec::new(),
    }
}
