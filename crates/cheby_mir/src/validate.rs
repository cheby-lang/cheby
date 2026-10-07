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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use cheby_iface::Ice;

use crate::abi::{AbiSig, fixed_sig};
use crate::ir::{
    Abi, BlockId, Callee, Const, FnKey, Function, LayoutRef, Local, MirModule, Operand, Rvalue,
    Stmt, Terminator, UnitKind, UseKind,
};
use crate::repr::{CType, Repr};

/// Which invariants hold at this point of the pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// After lowering: no `dup` or `drop` yet.
    Lowered,
    /// After [`crate::rc::insert`]: reference counts balance.
    Counted,
}

/// Checks `module`. Returns every problem found, each prefixed with the
/// function it is in.
///
/// # Errors
///
/// The list of problems, if any.
pub fn validate(module: &MirModule, stage: Stage) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check_module(module, &mut errors);
    for f in &module.functions {
        let mut cx = FnCx {
            module,
            f,
            errors: Vec::new(),
        };
        if let Err(ice) = cx.check(stage) {
            cx.errors.push(format!("internal: {ice}"));
        }
        let name = key_label(f.key);
        errors.extend(cx.errors.into_iter().map(|e| format!("{name}: {e}")));
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

struct FnCx<'a> {
    module: &'a MirModule,
    f: &'a Function,
    errors: Vec<String>,
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
        self.check_dominance(&defs)?;
        self.check_reprs(stage)?;
        if stage == Stage::Counted && self.errors.is_empty() {
            self.check_ownership()?;
        }
        Ok(())
    }

    fn check_abi(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let entries = self.module.unit == UnitKind::Entries;
        match (f.key, f.abi) {
            (FnKey::Entry(_), Abi::C) if !entries => {
                self.err("entry thunks belong in an entries unit");
            }
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::C) => {}
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::Cheby) => {
                self.err("entry thunks and trampolines use the C ABI");
            }
            (_, Abi::C) => self.err("only entry thunks and trampolines use the C ABI"),
            (_, Abi::Cheby) if entries => {
                self.err("an entries unit holds only entry thunks and trampolines");
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
        Ok(())
    }

    /// Every local defined exactly once; returns the definition sites.
    fn check_definitions(&mut self) -> Option<BTreeMap<Local, DefSite>> {
        let f = self.f;
        let mut sites: BTreeMap<Local, DefSite> = BTreeMap::new();
        let mut ok = true;
        let entry = BlockId::from_u32(0);
        let mut define = |cx: &mut Self, l: Local, site: DefSite| {
            if cx.f.locals.get(l).is_none() {
                cx.err(format!("unknown local _{}", l.as_u32()));
                ok = false;
            } else if sites.insert(l, site).is_some() {
                cx.err(format!("_{} is defined more than once", l.as_u32()));
                ok = false;
            }
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

    fn check_dominance(&mut self, defs: &BTreeMap<Local, DefSite>) -> Result<(), Ice> {
        let f = self.f;
        let doms = dominators(f)?;
        for (id, block) in f.blocks.iter_enumerated() {
            // Unreachable blocks have no dominators; their uses are not
            // checked.
            let Some(block_doms) = doms.get(&id) else {
                continue;
            };
            let check = |cx: &mut Self, l: Local, at: Option<usize>| {
                let Some(&(def_block, def_pos)) = defs.get(&l) else {
                    cx.err(format!("_{} is used but never defined", l.as_u32()));
                    return;
                };
                let ok = if def_block == id {
                    match (def_pos, at) {
                        (Some(d), Some(u)) => d < u,
                        (None, _) | (Some(_), None) => true,
                    }
                } else {
                    block_doms.contains(&def_block)
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
                    check(self, l, Some(n));
                }
            }
            for (l, _) in block.term.uses() {
                check(self, l, None);
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
                for (arg, &ctype) in args.iter().zip(&sig.params) {
                    self.check_const(arg, false);
                    if let Some(r) = self.repr(arg)
                        && !ctype.accepts(r)
                    {
                        self.err(format!("foreign argument {r} passed as {ctype}"));
                    }
                }
                sig.ret.repr()
            }
            Rvalue::CallRuntime { func, .. } => match *func {},
            Rvalue::Alloc { layout, fields } => {
                self.check_alloc(*layout, fields);
                Some(Repr::Value)
            }
            Rvalue::Field { base, .. } => {
                self.expect_repr(&Operand::Local(*base), Repr::Value, "field base");
                // The field's representation comes from the layout, which
                // codegen resolves; the destination's is trusted here.
                None
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
                if *repr == Repr::Value || *repr == Repr::Bool {
                    self.err(format!("checked {} on {repr}", op.name()));
                }
                self.expect_repr(lhs, *repr, "checked operand");
                self.expect_repr(rhs, *repr, "checked operand");
                Some(*repr)
            }
            Rvalue::Compare { lhs, rhs, repr, .. } => {
                if *repr == Repr::Value {
                    self.err("compare on uniform values");
                }
                self.expect_repr(lhs, *repr, "compare operand");
                self.expect_repr(rhs, *repr, "compare operand");
                Some(Repr::Bool)
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
                if !matches!(code, Operand::Const(Const::Code(_))) {
                    self.err("a closure's first field is its code word");
                }
                if usize::try_from(n).ok() != Some(captures.len()) {
                    self.err("closure capture count differs from its layout");
                }
                for c in captures {
                    self.expect_repr(c, Repr::Value, "closure capture");
                }
            }
            LayoutRef::Tuple(n) => {
                if usize::try_from(n).ok() != Some(fields.len()) {
                    self.err("tuple arity differs from its layout");
                }
                for c in fields {
                    self.expect_repr(c, Repr::Value, "tuple field");
                }
            }
            LayoutRef::Ctor(_) | LayoutRef::Builtin(_) => {
                for c in fields {
                    self.check_const(c, false);
                    self.repr(c);
                }
            }
        }
    }

    fn check_reprs(&mut self, stage: Stage) -> Result<(), Ice> {
        let f = self.f;
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign { dst, value } => {
                        if let Rvalue::CallForeign { sig, .. } = value
                            && sig.ret == CType::Void
                        {
                            self.err("a void foreign call cannot define a local");
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
                        let void_call = matches!(
                            value,
                            Rvalue::CallForeign { sig, .. } if sig.ret == CType::Void
                        );
                        if !void_call {
                            self.err("only a void foreign call is evaluated for effect");
                        }
                        self.check_rvalue(value);
                    }
                    Stmt::Dup(l) | Stmt::Drop(l) => {
                        if stage == Stage::Lowered {
                            self.err("dup or drop before reference-count insertion");
                        }
                        if f.local(*l)?.repr != Repr::Value {
                            self.err(format!("dup or drop of uncounted _{}", l.as_u32()));
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
            Terminator::Unreachable => {}
        }
        Ok(())
    }

    /// Reference counts balance on every path (D7).
    fn check_ownership(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let counted = |l: Local| f.local(l).map(|d| d.repr == Repr::Value);
        let mut entry_state: BTreeMap<BlockId, Counts> = BTreeMap::new();
        let mut start = Counts::default();
        for &p in &f.params {
            if counted(p)? {
                start.held.insert(p, 1);
            }
        }
        let bb0 = BlockId::from_u32(0);
        entry_state.insert(bb0, start);
        let mut work = vec![bb0];
        let mut visited = BTreeSet::new();
        while let Some(id) = work.pop() {
            if !visited.insert(id) {
                continue;
            }
            let block = f.block(id)?;
            let mut st = entry_state.get(&id).cloned().unwrap_or_default();
            for &p in &block.params {
                if counted(p)? {
                    st.held.insert(p, 1);
                }
            }
            let at = |n: usize| format!("bb{} statement {n}", id.as_u32());
            for (n, stmt) in block.stmts.iter().enumerate() {
                match stmt {
                    Stmt::Assign { dst, value } => {
                        self.consume(&mut st, &value.uses(), &at(n))?;
                        if counted(*dst)? {
                            if value.result_is_borrowed() {
                                st.held.insert(*dst, 0);
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
                    Stmt::Eval(value) => self.consume(&mut st, &value.uses(), &at(n))?,
                    Stmt::Dup(l) => {
                        // A `field` or `load_const` result holds nothing
                        // until its dup, which needs the base still alive.
                        let ok = match (st.held.get(l).copied(), st.borrowed_from.get(l)) {
                            (Some(c), _) if c >= 1 => true,
                            (Some(0), Some(Some(base))) => {
                                st.held.get(base).is_some_and(|&c| c >= 1)
                            }
                            (Some(0), Some(None)) => true,
                            _ => false,
                        };
                        match st.held.get_mut(l) {
                            Some(c) if ok => *c = c.saturating_add(1),
                            _ => self.err(format!(
                                "{}: dup of _{} which holds no reference",
                                at(n),
                                l.as_u32()
                            )),
                        }
                    }
                    Stmt::Drop(l) => self.consume(&mut st, &[(*l, UseKind::Owned)], &at(n))?,
                }
            }
            let here = format!("bb{} terminator", id.as_u32());
            match &block.term {
                Terminator::Return(_) | Terminator::TailCall { .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    for (l, c) in &st.held {
                        if *c != 0 {
                            self.err(format!(
                                "{here}: _{} still holds {c} reference(s)",
                                l.as_u32()
                            ));
                        }
                    }
                }
                Terminator::Goto { target, .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    self.flow(&mut entry_state, &mut work, *target, &st);
                }
                Terminator::Branch { .. } | Terminator::Switch { .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    for succ in block.term.successors() {
                        self.flow(&mut entry_state, &mut work, succ, &st);
                    }
                }
                Terminator::Panic { .. } | Terminator::Unreachable => {}
            }
        }
        Ok(())
    }

    fn flow(
        &mut self,
        entry_state: &mut BTreeMap<BlockId, Counts>,
        work: &mut Vec<BlockId>,
        target: BlockId,
        st: &Counts,
    ) {
        let mut normalized = st.clone();
        normalized.held.retain(|_, c| *c != 0);
        normalized
            .borrowed_from
            .retain(|l, _| normalized.held.contains_key(l));
        match entry_state.get(&target) {
            Some(existing) if *existing != normalized => self.err(format!(
                "reference counts differ between the predecessors of bb{}",
                target.as_u32()
            )),
            Some(_) => {}
            None => {
                entry_state.insert(target, normalized);
                work.push(target);
            }
        }
    }

    /// Applies the uses of one instruction: borrowed uses need a reference
    /// that survives the instruction, owned uses consume one each.
    fn consume(&mut self, st: &mut Counts, uses: &[(Local, UseKind)], at: &str) -> Result<(), Ice> {
        let mut per: BTreeMap<Local, (i64, bool)> = BTreeMap::new();
        for &(l, kind) in uses {
            if self.f.local(l)?.repr != Repr::Value {
                continue;
            }
            let e = per.entry(l).or_insert((0, false));
            match kind {
                UseKind::Owned => e.0 = e.0.saturating_add(1),
                UseKind::Borrowed => e.1 = true,
            }
        }
        for (l, (owned, borrowed)) in per {
            let have = st.held.get(&l).copied();
            let need = owned.saturating_add(i64::from(borrowed));
            match have {
                Some(c) if c >= need && need > 0 => {
                    st.held.insert(l, c.saturating_sub(owned));
                }
                _ => {
                    let mut msg = format!("{at}: _{} is used without a reference", l.as_u32());
                    if let Some(c) = have {
                        let _ = write!(msg, " (holds {c}, needs {need})");
                    }
                    self.err(msg);
                }
            }
        }
        Ok(())
    }
}

/// The references each counted local holds on one path.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Counts {
    held: BTreeMap<Local, i64>,
    /// Locals defined by `field` (with their base) or `load_const`.
    borrowed_from: BTreeMap<Local, Option<Local>>,
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
    }
}

/// The dominators of every block reachable from `bb0`, itself included.
fn dominators(f: &Function) -> Result<BTreeMap<BlockId, BTreeSet<BlockId>>, Ice> {
    let entry = BlockId::from_u32(0);
    let mut reachable = BTreeSet::new();
    let mut stack = vec![entry];
    let mut preds: BTreeMap<BlockId, BTreeSet<BlockId>> = BTreeMap::new();
    while let Some(b) = stack.pop() {
        if !reachable.insert(b) {
            continue;
        }
        for s in f.block(b)?.term.successors() {
            preds.entry(s).or_default().insert(b);
            stack.push(s);
        }
    }
    let all: BTreeSet<BlockId> = reachable.clone();
    let mut doms: BTreeMap<BlockId, BTreeSet<BlockId>> = reachable
        .iter()
        .map(|&b| {
            if b == entry {
                (b, BTreeSet::from([entry]))
            } else {
                (b, all.clone())
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &reachable {
            if b == entry {
                continue;
            }
            let mut new: Option<BTreeSet<BlockId>> = None;
            for p in preds.get(&b).into_iter().flatten() {
                let Some(pd) = doms.get(p) else { continue };
                new = Some(match new {
                    None => pd.clone(),
                    Some(acc) => acc.intersection(pd).copied().collect(),
                });
            }
            let mut new = new.unwrap_or_default();
            new.insert(b);
            if doms.get(&b) != Some(&new) {
                doms.insert(b, new);
                changed = true;
            }
        }
    }
    Ok(doms)
}
