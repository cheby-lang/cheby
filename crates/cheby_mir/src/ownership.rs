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
//! Borrowed locals (D-715, [`crate::Function::borrowed_locals`]): a
//! borrowed parameter, and a `field` result of a borrowed local (a
//! borrowed projection, D-807), hold no reference of
//! their own and stay alive for the whole call: the caller's reference
//! keeps them. They start at 0, a borrowed use of one needs no reference,
//! a `dup` of one is always allowed, and an owned use needs the
//! references its `dup`s gave. So a borrowed parameter consumed without a
//! `dup`, dropped, or released by a pad is an error, and a borrowed
//! position of a tail call fed an owned local leaves that local held at
//! the jump. Borrowed locals live in a per-function table, outside the
//! counts copied at every unwind site, so they add nothing to D-735's
//! hot path.
//!
//! [`RuntimeFn::params`]: crate::RuntimeFn::params

use std::fmt;
use std::fmt::Write as _;

use cheby_iface::{Ice, Idx};

use crate::ir::{BlockId, Function, Local, Rvalue, Stmt, Terminator, UseKind};
use crate::liveness::is_borrowed_local;
use crate::repr::Repr;

/// The references each counted local holds on one path.
///
/// `held` lists only the locals that hold at least one reference: a
/// local whose last reference is consumed leaves it, so the state, and
/// every copy of it taken at an unwind site, stays as large as what is
/// live rather than what the block has defined so far. Both lists are
/// sorted by local without duplicates (D-735: a sorted vector is cheaper
/// to copy at every unwind site and to compare at every join than an
/// ordered map, and iterates in the same order, D-264).
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    /// References per local, never 0; a local that is absent holds none.
    /// A local defined by `field` or `load_const` holds none until its
    /// `dup`.
    held: Vec<(Local, i64)>,
    /// Locals defined by `field` (with their base) or `load_const` in the
    /// current block, or flowing in still holding a reference.
    borrowed_from: Vec<(Local, Option<Local>)>,
}

impl Counts {
    /// Appends what the path owns to `out`: each local once per reference
    /// it holds, in ascending local order. The key of a cleanup pad
    /// (D-698).
    pub(crate) fn extend_owned(&self, out: &mut Vec<Local>) {
        for &(l, c) in &self.held {
            for _ in 0..c.max(0) {
                out.push(l);
            }
        }
    }

    /// The counts without the locals that hold nothing, as they flow into
    /// a successor. Linear in what is held and borrowed.
    fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.retain_held_borrows();
        out
    }

    /// Forgets the borrow bases of locals that hold nothing.
    fn retain_held_borrows(&mut self) {
        let held = &self.held;
        self.borrowed_from
            .retain(|(l, _)| held.binary_search_by_key(l, |&(h, _)| h).is_ok());
    }

    /// Forgets the borrow bases of locals that hold nothing, except the
    /// `field` results whose base still keeps them alive
    /// ([`Counts::borrow_alive`]): fusion sinks a projection's `dup` into a
    /// successor that only its block enters (D-850), so the
    /// projection flows there holding nothing.
    fn retain_alive_borrows(&mut self) {
        let keep: Vec<bool> = self
            .borrowed_from
            .iter()
            .map(|&(l, base)| self.count(l) >= 1 || (base.is_some() && self.borrow_alive(l)))
            .collect();
        let mut keep = keep.into_iter();
        self.borrowed_from.retain(|_| keep.next().unwrap_or(false));
    }

    /// The references `l` holds: 0 when it is absent.
    fn count(&self, l: Local) -> i64 {
        self.held
            .binary_search_by_key(&l, |&(h, _)| h)
            .ok()
            .and_then(|i| self.held.get(i))
            .map_or(0, |&(_, c)| c)
    }

    /// Sets the references `l` holds, keeping `held` free of zeros.
    fn set(&mut self, l: Local, count: i64) {
        match self.held.binary_search_by_key(&l, |&(h, _)| h) {
            Ok(i) if count == 0 => {
                self.held.remove(i);
            }
            Ok(i) => {
                if let Some(entry) = self.held.get_mut(i) {
                    entry.1 = count;
                }
            }
            Err(_) if count == 0 => {}
            Err(i) => self.held.insert(i, (l, count)),
        }
    }

    /// Whether `l` is a borrowed result whose base still holds a
    /// reference: a `load_const` result always, a `field` result while its
    /// base holds one, or while its base is itself such a result (a
    /// projection of a projection whose `dup` fusion sank,
    /// D-850). The chain is walked without recursion, at most
    /// once per entry.
    fn borrow_alive(&self, l: Local) -> bool {
        let mut at = l;
        for _ in 0..=self.borrowed_from.len() {
            let entry = self
                .borrowed_from
                .binary_search_by_key(&at, |&(b, _)| b)
                .ok()
                .and_then(|i| self.borrowed_from.get(i));
            match entry {
                Some(&(_, Some(base))) if self.count(base) >= 1 => return true,
                Some(&(_, Some(base))) => at = base,
                Some(&(_, None)) => return true,
                None => return false,
            }
        }
        false
    }

    /// Records that `l` is borrowed from `base`.
    fn set_borrowed(&mut self, l: Local, base: Option<Local>) {
        match self.borrowed_from.binary_search_by_key(&l, |&(b, _)| b) {
            Ok(i) => {
                if let Some(entry) = self.borrowed_from.get_mut(i) {
                    entry.1 = base;
                }
            }
            Err(i) => self.borrowed_from.insert(i, (l, base)),
        }
    }

    /// Forgets that `l` was borrowed.
    fn unset_borrowed(&mut self, l: Local) {
        if let Ok(i) = self.borrowed_from.binary_search_by_key(&l, |&(b, _)| b) {
            self.borrowed_from.remove(i);
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
    let blocks = f.blocks.len();
    Sim {
        f,
        observer,
        entry_state: vec![None; blocks],
        from: vec![None; blocks],
        work: Vec::new(),
        borrowed: f.borrowed_locals()?,
        single_pred: single_predecessor(f)?,
    }
    .run()
}

/// By block index, whether the block has exactly one predecessor: one
/// block whose normal successors or unwind edges lead to it (the function
/// entry counts as the predecessor of `bb0` and of the entry pad).
fn single_predecessor(f: &Function) -> Result<Vec<bool>, Ice> {
    let n = f.blocks.len();
    // By block index: the first predecessor seen, and whether another
    // one was.
    let mut first: Vec<Option<usize>> = vec![None; n];
    let mut more = vec![false; n];
    let mut see = |to: BlockId, from: usize| {
        let t = to.index();
        match first.get(t).copied() {
            Some(None) => {
                if let Some(slot) = first.get_mut(t) {
                    *slot = Some(from);
                }
            }
            Some(Some(p)) if p != from => {
                if let Some(slot) = more.get_mut(t) {
                    *slot = true;
                }
            }
            _ => {}
        }
    };
    // The function entry, as a block index no block has.
    see(BlockId::from_u32(0), n);
    if let Some(pad) = f.entry_unwind {
        see(pad, n);
    }
    for (id, _) in f.blocks.iter_enumerated() {
        for succ in f.successors(id)? {
            see(succ, id.index());
        }
    }
    Ok(first
        .iter()
        .zip(&more)
        .map(|(p, m)| p.is_some() && !m)
        .collect())
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
    /// The counts each block starts with, from its first predecessor, by
    /// block index.
    entry_state: Vec<Option<Counts>>,
    /// Where each block's first predecessor is, for messages, by block
    /// index.
    from: Vec<Option<Site>>,
    work: Vec<BlockId>,
    /// By local index, the borrowed locals (module documentation); empty
    /// when the function borrows nothing.
    borrowed: Vec<bool>,
    /// By block index, whether the block has one predecessor
    /// ([`single_predecessor`]): only there may a projection flow in
    /// holding nothing.
    single_pred: Vec<bool>,
}

impl<O: Observer> Sim<'_, O> {
    fn counted(&self, l: Local) -> Result<bool, Ice> {
        Ok(self.f.local(l)?.repr == Repr::Value)
    }

    /// Whether `l` is a borrowed local (D-715).
    fn is_borrowed(&self, l: Local) -> bool {
        is_borrowed_local(&self.borrowed, l)
    }

    fn run(mut self) -> Result<(), Ice> {
        let f = self.f;
        let mut start = Counts::default();
        // A borrowed parameter holds nothing (D-715).
        for p in f.owned_params()? {
            start.set(p, 1);
        }
        let bb0 = BlockId::from_u32(0);
        if let Some(pad) = f.entry_unwind {
            self.flow(pad, start.clone(), Site::EntryCheck);
        }
        self.flow(bb0, start, Site::Entry);
        let mut visited = vec![false; f.blocks.len()];
        while let Some(id) = self.work.pop() {
            match visited.get_mut(id.index()) {
                Some(true) => {}
                Some(seen) => {
                    *seen = true;
                    self.block(id)?;
                }
                // An unknown block: `block` reports it.
                None => self.block(id)?,
            }
        }
        Ok(())
    }

    fn block(&mut self, id: BlockId) -> Result<(), Ice> {
        let f = self.f;
        let block = f.block(id)?;
        let mut st = self
            .entry_state
            .get(id.index())
            .and_then(Clone::clone)
            .unwrap_or_default();
        for &p in &block.params {
            if self.counted(p)? {
                st.set(p, 1);
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
                    // A keep list is its `dup`s before the release (D-850).
                    for &(_, p) in value.kept() {
                        self.dup(&mut st, p, at);
                    }
                    self.consume(&mut st, &value.uses(), at)?;
                    if self.counted(*dst)? {
                        if self.is_borrowed(*dst) {
                            // A borrowed projection: alive for the whole
                            // call, holding nothing.
                            st.set(*dst, 0);
                        } else if value.result_is_borrowed() {
                            st.set(*dst, 0);
                            let base = match value {
                                Rvalue::Field { base, .. } => Some(*base),
                                _ => None,
                            };
                            st.set_borrowed(*dst, base);
                        } else {
                            st.set(*dst, 1);
                        }
                    }
                }
                Stmt::Eval(value) => self.consume(&mut st, &value.uses(), at)?,
                Stmt::Dup(l) => self.dup(&mut st, *l, at),
                Stmt::Drop(l) => self.consume(&mut st, &[(*l, UseKind::Owned)], at)?,
                // Its `dup`s, then the drop (D-850).
                Stmt::DropKeep { local, keep, .. } => {
                    for &(_, p) in keep {
                        self.dup(&mut st, p, at);
                    }
                    self.consume(&mut st, &[(*local, UseKind::Owned)], at)?;
                }
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
        if c >= 1 || self.is_borrowed(l) || st.borrow_alive(l) {
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
        // Into a block with one predecessor, a projection whose `dup`
        // fusion sank keeps its base (D-850); where paths join,
        // only what holds a reference flows, so every predecessor brings
        // the same state.
        if self
            .single_pred
            .get(target.index())
            .copied()
            .unwrap_or(false)
        {
            st.retain_alive_borrows();
        } else {
            st.retain_held_borrows();
        }
        let Some(slot) = self.entry_state.get_mut(target.index()) else {
            // An unknown block: simulating it reports it.
            self.work.push(target);
            return;
        };
        match slot {
            Some(existing) if *existing != st => {
                let first = self
                    .from
                    .get(target.index())
                    .copied()
                    .flatten()
                    .unwrap_or(from);
                let (mut was, mut now) = (Vec::new(), Vec::new());
                existing.extend_owned(&mut was);
                st.extend_owned(&mut now);
                self.observer.error(format!(
                    "reference counts differ between the predecessors of bb{}: {first} brings {}, {from} brings {}",
                    target.as_u32(),
                    set_text(&was),
                    set_text(&now),
                ));
            }
            Some(_) => {}
            None => {
                *slot = Some(st);
                if let Some(site) = self.from.get_mut(target.index()) {
                    *site = Some(from);
                }
                self.work.push(target);
            }
        }
    }

    /// Applies the uses of one instruction: borrowed uses need a reference
    /// that survives the instruction (a borrowed local's caller holds it,
    /// D-715), owned uses consume one each.
    fn consume(&mut self, st: &mut Counts, uses: &[(Local, UseKind)], at: Site) -> Result<(), Ice> {
        // Per local: owned uses and whether it is borrowed, in ascending
        // local order (an instruction has few operands).
        let mut per: Vec<(Local, i64, bool)> = Vec::with_capacity(uses.len());
        for &(l, kind) in uses {
            if !self.counted(l)? {
                continue;
            }
            let i = per.iter().position(|e| e.0 == l).unwrap_or_else(|| {
                per.push((l, 0, false));
                per.len().saturating_sub(1)
            });
            if let Some(e) = per.get_mut(i) {
                match kind {
                    UseKind::Owned => e.1 = e.1.saturating_add(1),
                    UseKind::Borrowed => e.2 = true,
                }
            }
        }
        per.sort_unstable_by_key(|e| e.0);
        // Borrowed uses of `field` results that hold nothing, whose base
        // keeps them alive: checked after the instruction's owned uses, so
        // an instruction that consumes the base cannot also read them.
        let mut projections = Vec::new();
        for (l, owned, borrowed) in per {
            let have = st.count(l);
            let kept = self.is_borrowed(l);
            let need = owned.saturating_add(i64::from(borrowed && !kept));
            if have >= need && (need > 0 || kept) {
                st.set(l, have.saturating_sub(owned));
            } else if have == 0 && owned == 0 && borrowed {
                projections.push(l);
            } else {
                self.observer.error(format!(
                    "{at}: _{} is used without a reference (holds {have}, needs {need})",
                    l.as_u32()
                ));
            }
        }
        // D-850: fusion sinks a projection's `dup` past its
        // borrowed uses, which its base's reference keeps valid.
        for l in projections {
            if !st.borrow_alive(l) {
                self.observer.error(format!(
                    "{at}: _{} is used without a reference (holds 0, needs 1)",
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
        Stmt::Dup(_) | Stmt::Drop(_) | Stmt::DropKeep { .. } | Stmt::ClearUnwind => Vec::new(),
    };
    let mut out = st.normalized();
    for (l, kind) in consumed {
        if kind == UseKind::Owned && f.local(l)?.repr == Repr::Value {
            let c = out.count(l);
            if c > 0 {
                let left = c.saturating_sub(1);
                out.set(l, left);
                if left == 0 {
                    out.unset_borrowed(l);
                }
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
