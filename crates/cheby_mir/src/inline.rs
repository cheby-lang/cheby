//! The release profile's inliner (design T5, T6): a callee's body copied
//! into its caller at a direct call, inside one unit.
//!
//! The inliner runs on lowered MIR ([`crate::Stage::Lowered`]), after
//! lowering and before borrowing inference and reference counting, in a
//! release build with the `inline` lever on ([`optimize`]). The MIR then
//! holds no counts, no reuse tokens and no unwind edges, so every later
//! pass sees the merged function as if lowering had written it, and
//! borrowing is inferred over the inlined bodies.
//!
//! # The splice ([`splice`])
//!
//! - **The call.** At a call in a statement the block is split there: its
//!   first part jumps into the copied body, and the rest of the block
//!   becomes a continuation block whose one parameter is the call's result
//!   local. A callee `return op` becomes a jump to the continuation with
//!   `op`, and a callee tail call becomes a call and that jump, so the stack
//!   depth is what it was: the callee's frame used to be replaced by its
//!   tail callee's. At a tail call, returns stay returns and tail calls stay
//!   tail calls (D-015).
//! - **Parameters.** Each one stands for its argument operand directly, so
//!   no move appears and counting sees the caller's values, which are then
//!   released at the last use in the copied body, as the callee released
//!   them. A constant argument that the body uses where only a local may
//!   stand (a field read's base, an indirect callee) gets a local first,
//!   `_n = use const`, as lowering writes one. A counted argument that the
//!   body never uses is moved into a local at the call, which counting
//!   releases there, where the callee's entry released it: a handle's drop
//!   function runs at the same point (D-205).
//! - **Strings, call locations and positions.** The body's strings join the
//!   caller's table, so a panic keeps its location text. A copied call keeps
//!   its own call location (D-856); one without, and a callee tail call
//!   that became a call, take the inlined call's. The call locations stay
//!   sorted by local, as code generation searches them. With
//!   [`CopiedPositions::Own`] the body's source positions join the caller's
//!   table, which is right when both are in one file (D-938); with
//!   [`CopiedPositions::AtCall`] every copied statement takes the call's
//!   position.
//! - **Sites.** Inlined statements record no site for `--reuse-report`
//!   (D-717), and the call's result is no call's result any more, so the
//!   report describes the caller's own expressions after inlining.
//! - **Everything else** (callee declarations, layouts, statics, closures,
//!   constants) is shared: a body of the same unit names the same defs,
//!   statics and functions. A body from another unit must be translated
//!   into the caller's unit before it is spliced.
//!
//! An inlined body that tail-calls the caller calls the caller itself, so
//! code generation's loop jump for a self tail call stays right wherever
//! the call came from. The selection never inlines a function in a cycle,
//! so here no body calls its caller.
//!
//! Backtraces lose the inlined frames. Panic messages and their locations
//! stay exact.
//!
//! # The selection ([`optimize`])
//!
//! - A callee is a top-level function of the unit ([`FnKey::Item`]) that is
//!   in no cycle of the unit's call graph (every loop in Cheby is one), is
//!   not `precounted`, names no continuation and no fiber context, makes no
//!   blocking foreign call and passes no counted parameter of its own to a
//!   foreign function (the shape of an `@external` wrapper, whose kernel
//!   must see the counts the wrapper gives it, D-525), and is not one of
//!   the waits that keep their caller's frame (D-861). A direct call of a
//!   std kernel moves its counted arguments into temporaries first, so a
//!   function that makes one is a callee like any other: the counting and
//!   the unwind pass, which run after inlining, give the merged function
//!   the counts and the edge the call needs (D-698).
//! - Its cost is its statements plus its blocks, after its own inlining
//!   and clean-up. A call is inlined when the callee costs at most
//!   [`InlineBudget::inline`], within a growth of the unit of
//!   [`InlineBudget::unit_growth_percent`]; a callee of at most
//!   [`InlineBudget::always`] is exempt from that growth. No function grows
//!   past [`InlineBudget::function`], D-780's bound for the register
//!   allocator.
//! - The functions are visited by the strongly connected components of the
//!   call graph, callees first, and each one's calls in block and statement
//!   order, greedily in one pass. A copied body is never searched again, so
//!   bodies are inlined flat. Every order comes from the unit's own order,
//!   so the result does not depend on threads (D-264).
//! - Every function is then cleaned up ([`crate::simplify`]).

use cheby_iface::{Ice, Idx};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{
    Abi, Block, BlockId, Callee, Const, FnKey, Function, Local, LocalDecl, MirModule, Operand,
    Rvalue, Stmt, StrId, Terminator,
};
use crate::positions::{Pos, PosRef};
use crate::repr::Repr;

/// Where a call to inline sits in its block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SiteAt {
    /// Statement `n` of the block, `dst = call f(args)`.
    Stmt(usize),
    /// The block's terminator, `tail_call f(args)`.
    Tail,
}

/// A direct call to inline: its block and its place there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallSite {
    pub block: BlockId,
    pub at: SiteAt,
}

/// Where the statements of a copied body take their source positions
/// from (D-938).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CopiedPositions {
    /// Their own, joined to the caller's table: the body is in the caller's
    /// file.
    Own,
    /// The inlined call's: the body is in another file, and a line table
    /// names one file (D-1172).
    AtCall,
}

/// What a splice added to the caller.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Spliced {
    /// The continuation block, which holds the rest of the call's block;
    /// `None` at a tail call.
    pub cont: Option<BlockId>,
    /// The copied body's blocks, in the callee's order: the first one is
    /// the callee's entry.
    pub body: Vec<BlockId>,
}

/// The size of `f`: its statements and its blocks, the unit of
/// `FunctionBuilder::size` and of D-780's bound.
#[must_use]
pub fn cost(f: &Function) -> usize {
    f.blocks
        .iter()
        .map(|b| b.stmts.len())
        .fold(f.blocks.len(), usize::saturating_add)
}

/// Copies `callee`'s body into `host` at the direct call `site`, as the
/// module docs say. `callee` must name the defs, statics and functions of
/// `host`'s unit. It is lowered MIR without counts or unwind edges, of
/// the Cheby convention, and takes as many parameters as the call passes.
///
/// # Errors
///
/// An [`Ice`] if `site` is no direct call, or if either function is not
/// lowered MIR of that shape.
#[expect(
    clippy::too_many_lines,
    reason = "the steps of the module docs' splice, in their order"
)]
pub fn splice(
    host: &mut Function,
    site: CallSite,
    callee: &Function,
    positions: CopiedPositions,
) -> Result<Spliced, Ice> {
    check_body(callee)?;
    if host.abi != Abi::Cheby || host.precounted {
        return Err(Ice::new(
            "inline: only a counted Cheby function is inlined into",
        ));
    }
    let block = host.block(site.block)?;
    if !block.unwind.is_empty() {
        return Err(Ice::new("inline: the call's block has unwind edges"));
    }
    let (dst, args, call_pos) = match site.at {
        SiteAt::Stmt(i) => match block.stmts.get(i) {
            Some(Stmt::Assign {
                dst,
                value:
                    Rvalue::Call {
                        callee: Callee::Direct(_),
                        args,
                    },
            }) => (Some(*dst), args.clone(), block.pos_at(i)),
            _ => return Err(Ice::new("inline: the site is no direct call")),
        },
        SiteAt::Tail => {
            let Terminator::TailCall {
                callee: Callee::Direct(_),
                args,
            } = &block.term
            else {
                return Err(Ice::new("inline: the site is no direct tail call"));
            };
            (None, args.clone(), block.term_pos())
        }
    };
    if args.len() != callee.params.len() {
        return Err(Ice::new(format!(
            "inline: a call with {} arguments of a function of {}",
            args.len(),
            callee.params.len()
        )));
    }
    let host_has_pos = block.has_positions();
    let first = host.blocks.len();
    let base = if dst.is_some() {
        first
            .checked_add(1)
            .ok_or_else(|| Ice::new("inline: too many blocks"))?
    } else {
        first
    };
    let host_loc = dst.and_then(|d| call_loc(host, d));
    let mut cx = Copier::new(host, callee, &args, base, positions, call_pos, host_has_pos)?;
    let mut blocks = Vec::with_capacity(callee.blocks.len());
    let mut locs: Vec<(Local, StrId)> = Vec::new();
    for b in &callee.blocks {
        blocks.push(cx.block(host, b, dst.is_some().then_some(first), host_loc, &mut locs)?);
    }
    let materialized = std::mem::take(&mut cx.materialized_stmts);
    let entry = block_id(base)?;
    // The call's block: its statements up to the call, the constants'
    // locals, and a jump into the body.
    let b = host
        .blocks
        .get_mut(site.block)
        .ok_or_else(|| Ice::new("inline: the call's block"))?;
    let mut cont = None;
    match site.at {
        SiteAt::Stmt(i) => {
            let after = i
                .checked_add(1)
                .ok_or_else(|| Ice::new("inline: statement index"))?;
            let rest = b.stmts.split_off(after);
            b.stmts.pop();
            let rest_pos = if host_has_pos {
                let rest_pos = b.pos.split_off(after);
                b.pos.truncate(i);
                rest_pos
            } else {
                Vec::new()
            };
            let term = std::mem::replace(
                &mut b.term,
                Terminator::Goto {
                    target: entry,
                    args: Vec::new(),
                },
            );
            push_materialized(b, materialized, call_pos, host_has_pos);
            if host_has_pos {
                b.pos.push(call_pos);
            }
            let params = dst.into_iter().collect();
            cont = Some(Block {
                params,
                stmts: rest,
                term,
                unwind: Vec::new(),
                pos: rest_pos,
            });
        }
        SiteAt::Tail => {
            let term_pos = b.term_pos();
            if host_has_pos {
                b.pos.pop();
            }
            push_materialized(b, materialized, call_pos, host_has_pos);
            if host_has_pos {
                b.pos.push(term_pos);
            }
            b.term = Terminator::Goto {
                target: entry,
                args: Vec::new(),
            };
        }
    }
    let mut out = Spliced {
        cont: None,
        body: Vec::with_capacity(blocks.len()),
    };
    if let Some(c) = cont {
        out.cont = Some(host.blocks.push(c)?);
    }
    for b in blocks {
        out.body.push(host.blocks.push(b)?);
    }
    if let Some(d) = dst {
        host.call_locs.retain(|&(l, _)| l != d);
        host.sites.retain(|&(l, _)| l != d);
    }
    host.call_locs.extend(locs);
    host.call_locs.sort_by_key(|&(l, _)| l);
    Ok(out)
}

/// The constants' locals of a splice, appended to the call's block before
/// its terminator, at the call's position.
fn push_materialized(b: &mut Block, stmts: Vec<Stmt>, at: PosRef, has_pos: bool) {
    for stmt in stmts {
        b.stmts.push(stmt);
        if has_pos {
            b.pos.push(at);
        }
    }
}

/// The location recorded for the call that defines `local`.
fn call_loc(f: &Function, local: Local) -> Option<StrId> {
    f.call_locs
        .binary_search_by_key(&local, |&(l, _)| l)
        .ok()
        .and_then(|at| f.call_locs.get(at))
        .map(|&(_, loc)| loc)
}

fn block_id(n: usize) -> Result<BlockId, Ice> {
    BlockId::from_usize(n).ok_or_else(|| Ice::new("inline: too many blocks"))
}

/// Checks that `f` is a body the splice can copy: lowered MIR of the
/// Cheby convention with a uniform result, no counts and no unwinding.
fn check_body(f: &Function) -> Result<(), Ice> {
    if f.abi != Abi::Cheby || f.ret != Repr::Value || f.precounted || f.entry_unwind.is_some() {
        return Err(Ice::new(format!(
            "inline: {:?} is not a lowered Cheby body",
            f.key
        )));
    }
    for b in &f.blocks {
        if !b.unwind.is_empty() || matches!(b.term, Terminator::Resume) {
            return Err(Ice::new(format!("inline: {:?} unwinds", f.key)));
        }
        for s in &b.stmts {
            let lowered = match s {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                    !matches!(value, Rvalue::DropReuse { .. } | Rvalue::AllocReuse { .. })
                }
                Stmt::Dup(_) | Stmt::Drop(_) | Stmt::DropKeep { .. } | Stmt::ClearUnwind => false,
            };
            if !lowered {
                return Err(Ice::new(format!("inline: {:?} is counted", f.key)));
            }
        }
    }
    Ok(())
}

/// The locals `f` uses where only a local may stand: a field read's or
/// layout read's base, a tuple being filled, an indirect callee, and what
/// counts and reuse name.
pub(crate) fn local_only_uses(f: &Function) -> FxHashSet<Local> {
    let mut out = FxHashSet::default();
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                    rvalue_local_only(value, &mut out);
                }
                Stmt::Dup(l) | Stmt::Drop(l) => {
                    out.insert(*l);
                }
                Stmt::DropKeep { local, keep, .. } => {
                    out.insert(*local);
                    out.extend(keep.iter().map(|&(_, p)| p));
                }
                Stmt::ClearUnwind => {}
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Indirect(l),
            ..
        } = &b.term
        {
            out.insert(*l);
        }
    }
    out
}

/// The locals `value` uses where only a local may stand.
fn rvalue_local_only(value: &Rvalue, out: &mut FxHashSet<Local>) {
    match value {
        Rvalue::Field { base, .. } | Rvalue::ReadLayout { base } => {
            out.insert(*base);
        }
        Rvalue::InitField { obj, .. } => {
            out.insert(*obj);
        }
        Rvalue::DropReuse { local, keep, .. } => {
            out.insert(*local);
            out.extend(keep.iter().map(|&(_, p)| p));
        }
        Rvalue::AllocReuse { token, .. } => {
            out.insert(*token);
        }
        Rvalue::Call {
            callee: Callee::Indirect(l),
            ..
        } => {
            out.insert(*l);
        }
        Rvalue::Use(_)
        | Rvalue::Call { .. }
        | Rvalue::CallForeign { .. }
        | Rvalue::CallRuntime { .. }
        | Rvalue::Alloc { .. }
        | Rvalue::AllocPlaceholder { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::Box { .. }
        | Rvalue::Unbox { .. }
        | Rvalue::Checked { .. }
        | Rvalue::Compare { .. }
        | Rvalue::FloatArith { .. }
        | Rvalue::Bitwise { .. }
        | Rvalue::FloatNeg { .. }
        | Rvalue::Not(_)
        | Rvalue::ReadTag { .. } => {}
    }
}

/// The renaming of one splice: the callee's locals, strings, blocks and
/// positions in the host.
struct Copier<'c> {
    callee: &'c Function,
    /// Each callee local's operand in the host, by index: the argument for
    /// a parameter, a new local otherwise.
    operands: Vec<Option<Operand>>,
    /// The local of a parameter whose argument is a constant, where only a
    /// local may stand, by index.
    materialized: Vec<Option<Local>>,
    /// The statements that define those locals, and the moves of the
    /// counted arguments the body never uses, for the call's block.
    materialized_stmts: Vec<Stmt>,
    /// Each callee string's id in the host's table, by index; none for a
    /// string the body does not use, such as its entry's location.
    strings: Vec<Option<StrId>>,
    /// Each callee call location's id in the host's table, by index.
    loc_strings: Vec<Option<StrId>>,
    /// The host block of the callee's first block.
    base: usize,
    positions: CopiedPositions,
    /// The host's positions, for interning.
    pos_ids: FxHashMap<Pos, PosRef>,
    call_pos: PosRef,
    /// Whether the call's block keeps positions.
    host_has_pos: bool,
}

impl<'c> Copier<'c> {
    fn new(
        host: &mut Function,
        callee: &'c Function,
        args: &[Operand],
        base: usize,
        positions: CopiedPositions,
        call_pos: PosRef,
        host_has_pos: bool,
    ) -> Result<Self, Ice> {
        let n = callee.locals.len();
        let mut operands: Vec<Option<Operand>> = vec![None; n];
        let mut materialized: Vec<Option<Local>> = vec![None; n];
        let mut materialized_stmts = Vec::new();
        let local_only = local_only_uses(callee);
        let uses = crate::simplify::use_counts(callee);
        for (&p, arg) in callee.params.iter().zip(args) {
            let slot = operands
                .get_mut(p.index())
                .ok_or_else(|| Ice::new("inline: a parameter outside the locals"))?;
            *slot = Some(arg.clone());
            // A counted argument the body never uses was released at the
            // callee's entry: a move at the call keeps that release there,
            // so a handle's drop function runs where it did (D-205).
            if let Operand::Local(x) = arg
                && host.local(*x)?.repr == Repr::Value
                && uses.get(p.index()).is_none_or(|&n| n == 0)
            {
                let keep = host.locals.push(LocalDecl {
                    repr: Repr::Value,
                    name: None,
                    ty: None,
                })?;
                materialized_stmts.push(Stmt::Assign {
                    dst: keep,
                    value: Rvalue::Use(arg.clone()),
                });
            }
            if let Operand::Const(c) = arg
                && local_only.contains(&p)
            {
                let decl = callee.local(p)?.clone();
                let m = host.locals.push(decl)?;
                materialized_stmts.push(Stmt::Assign {
                    dst: m,
                    value: Rvalue::Use(Operand::Const(c.clone())),
                });
                if let Some(slot) = materialized.get_mut(p.index()) {
                    *slot = Some(m);
                }
            }
        }
        for (l, decl) in callee.locals.iter_enumerated() {
            let Some(slot) = operands.get_mut(l.index()) else {
                continue;
            };
            if slot.is_none() && !callee.params.contains(&l) {
                *slot = Some(Operand::Local(host.locals.push(decl.clone())?));
            }
        }
        let strings = intern_used(&mut host.strings, &callee.strings, &used_strings(callee))?;
        let every = vec![true; callee.call_loc_strings.len()];
        let loc_strings =
            intern_used(&mut host.call_loc_strings, &callee.call_loc_strings, &every)?;
        let pos_ids = host
            .positions
            .table
            .iter()
            .enumerate()
            .filter_map(|(i, &pos)| {
                let bits = u32::try_from(i.checked_add(1)?).ok()?;
                Some((pos, PosRef::from_bits(bits)))
            })
            .collect();
        Ok(Self {
            callee,
            operands,
            materialized,
            materialized_stmts,
            strings,
            loc_strings,
            base,
            positions,
            pos_ids,
            call_pos,
            host_has_pos,
        })
    }

    /// The host block of callee block `b`.
    fn target(&self, b: BlockId) -> Result<BlockId, Ice> {
        block_id(
            self.base
                .checked_add(b.index())
                .ok_or_else(|| Ice::new("inline: too many blocks"))?,
        )
    }

    fn operand(&self, op: &Operand) -> Result<Operand, Ice> {
        match op {
            Operand::Local(l) => self
                .operands
                .get(l.index())
                .cloned()
                .flatten()
                .ok_or_else(|| Ice::new(format!("inline: unknown local _{}", l.as_u32()))),
            Operand::Const(c) => Ok(Operand::Const(self.constant(c)?)),
        }
    }

    fn operands(&self, ops: &[Operand]) -> Result<Vec<Operand>, Ice> {
        ops.iter().map(|op| self.operand(op)).collect()
    }

    fn constant(&self, c: &Const) -> Result<Const, Ice> {
        Ok(match c {
            Const::Str(id) => Const::Str(self.string(*id)?),
            Const::Int(_)
            | Const::Float(_)
            | Const::Bool(_)
            | Const::Sized { .. }
            | Const::Nil
            | Const::StaticCtor(_)
            | Const::FnValue(_)
            | Const::Code(_)
            | Const::EmptyList
            | Const::Static(_)
            | Const::BuiltinDesc(_)
            | Const::Ctx
            | Const::ErrNil => c.clone(),
        })
    }

    fn string(&self, id: StrId) -> Result<StrId, Ice> {
        self.strings
            .get(id.index())
            .copied()
            .flatten()
            .ok_or_else(|| Ice::new(format!("inline: unknown string str#{}", id.as_u32())))
    }

    /// The host local of callee local `l` where only a local may stand.
    fn local(&self, l: Local) -> Result<Local, Ice> {
        if let Some(&Some(m)) = self.materialized.get(l.index()) {
            return Ok(m);
        }
        match self.operands.get(l.index()) {
            Some(Some(Operand::Local(x))) => Ok(*x),
            Some(Some(Operand::Const(_))) => Err(Ice::new(
                "inline: a constant argument where only a local may stand",
            )),
            Some(None) | None => Err(Ice::new(format!("inline: unknown local _{}", l.as_u32()))),
        }
    }

    fn callee_of(&self, callee: &Callee) -> Result<Callee, Ice> {
        Ok(match callee {
            Callee::Indirect(l) => Callee::Indirect(self.local(*l)?),
            Callee::Direct(_) | Callee::Borrowing(..) | Callee::Code(_) => callee.clone(),
        })
    }

    fn keep(&self, keep: &[(u32, Local)]) -> Result<Vec<(u32, Local)>, Ice> {
        keep.iter().map(|&(i, p)| Ok((i, self.local(p)?))).collect()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "its match names every variant (D-1560)"
    )]
    fn rvalue(&self, value: &Rvalue) -> Result<Rvalue, Ice> {
        Ok(match value {
            Rvalue::Use(op) => Rvalue::Use(self.operand(op)?),
            Rvalue::Call { callee, args } => Rvalue::Call {
                callee: self.callee_of(callee)?,
                args: self.operands(args)?,
            },
            Rvalue::CallForeign {
                symbol,
                sig,
                args,
                blocking,
            } => Rvalue::CallForeign {
                symbol: symbol.clone(),
                sig: sig.clone(),
                args: self.operands(args)?,
                blocking: *blocking,
            },
            Rvalue::CallRuntime { func, args } => Rvalue::CallRuntime {
                func: *func,
                args: self.operands(args)?,
            },
            Rvalue::Alloc { layout, fields } => Rvalue::Alloc {
                layout: *layout,
                fields: self.operands(fields)?,
            },
            Rvalue::AllocPlaceholder { layout } => Rvalue::AllocPlaceholder { layout: *layout },
            Rvalue::InitField {
                obj,
                layout,
                field,
                value,
            } => Rvalue::InitField {
                obj: self.local(*obj)?,
                layout: *layout,
                field: *field,
                value: self.operand(value)?,
            },
            Rvalue::Field {
                base,
                layout,
                field,
            } => Rvalue::Field {
                base: self.local(*base)?,
                layout: *layout,
                field: *field,
            },
            Rvalue::ReadLayout { base } => Rvalue::ReadLayout {
                base: self.local(*base)?,
            },
            Rvalue::LoadConst(def) => Rvalue::LoadConst(*def),
            Rvalue::Box { value, from } => Rvalue::Box {
                value: self.operand(value)?,
                from: *from,
            },
            Rvalue::Unbox { value, to } => Rvalue::Unbox {
                value: self.operand(value)?,
                to: *to,
            },
            Rvalue::Checked {
                op,
                lhs,
                rhs,
                repr,
                loc,
            } => Rvalue::Checked {
                op: *op,
                lhs: self.operand(lhs)?,
                rhs: self.operand(rhs)?,
                repr: *repr,
                loc: self.string(*loc)?,
            },
            Rvalue::Compare { op, lhs, rhs, repr } => Rvalue::Compare {
                op: *op,
                lhs: self.operand(lhs)?,
                rhs: self.operand(rhs)?,
                repr: *repr,
            },
            Rvalue::FloatArith { op, lhs, rhs, repr } => Rvalue::FloatArith {
                op: *op,
                lhs: self.operand(lhs)?,
                rhs: self.operand(rhs)?,
                repr: *repr,
            },
            Rvalue::Bitwise { op, lhs, rhs, repr } => Rvalue::Bitwise {
                op: *op,
                lhs: self.operand(lhs)?,
                rhs: self.operand(rhs)?,
                repr: *repr,
            },
            Rvalue::FloatNeg { value, repr } => Rvalue::FloatNeg {
                value: self.operand(value)?,
                repr: *repr,
            },
            Rvalue::Not(op) => Rvalue::Not(self.operand(op)?),
            Rvalue::ReadTag { base, ty } => Rvalue::ReadTag {
                base: self.operand(base)?,
                ty: *ty,
            },
            Rvalue::DropReuse {
                local,
                layout,
                keep,
            } => Rvalue::DropReuse {
                local: self.local(*local)?,
                layout: *layout,
                keep: self.keep(keep)?,
            },
            Rvalue::AllocReuse {
                token,
                layout,
                fields,
            } => Rvalue::AllocReuse {
                token: self.local(*token)?,
                layout: *layout,
                fields: self.operands(fields)?,
            },
        })
    }

    fn stmt(&self, s: &Stmt) -> Result<Stmt, Ice> {
        Ok(match s {
            Stmt::Assign { dst, value } => Stmt::Assign {
                dst: self.local(*dst)?,
                value: self.rvalue(value)?,
            },
            Stmt::Eval(value) => Stmt::Eval(self.rvalue(value)?),
            Stmt::Dup(l) => Stmt::Dup(self.local(*l)?),
            Stmt::Drop(l) => Stmt::Drop(self.local(*l)?),
            Stmt::DropKeep {
                local,
                layout,
                keep,
            } => Stmt::DropKeep {
                local: self.local(*local)?,
                layout: *layout,
                keep: self.keep(keep)?,
            },
            Stmt::ClearUnwind => Stmt::ClearUnwind,
        })
    }

    /// The host position of callee position `r`.
    fn pos(&mut self, host: &mut Function, r: PosRef) -> Result<PosRef, Ice> {
        match self.positions {
            CopiedPositions::AtCall => Ok(self.call_pos),
            CopiedPositions::Own => {
                let Some(pos) = self.callee.positions.get(r) else {
                    return Ok(PosRef::NONE);
                };
                if let Some(&id) = self.pos_ids.get(&pos) {
                    return Ok(id);
                }
                let bits = u32::try_from(host.positions.table.len())
                    .ok()
                    .and_then(|n| n.checked_add(1))
                    .ok_or_else(|| Ice::new("too many source positions in one function"))?;
                host.positions.table.push(pos);
                let id = PosRef::from_bits(bits);
                self.pos_ids.insert(pos, id);
                Ok(id)
            }
        }
    }

    /// The positions of a copied block: the callee block's own, or the
    /// call's for each statement and the terminator when the call's block
    /// keeps positions.
    fn block_pos(&mut self, host: &mut Function, b: &Block) -> Result<Vec<PosRef>, Ice> {
        match self.positions {
            CopiedPositions::Own => b.pos.iter().map(|&r| self.pos(host, r)).collect(),
            CopiedPositions::AtCall if self.host_has_pos => {
                Ok(vec![self.call_pos; b.stmts.len().saturating_add(1)])
            }
            CopiedPositions::AtCall => Ok(Vec::new()),
        }
    }

    /// The copy of callee block `b`. With `cont` (a call in a statement),
    /// returns jump there and tail calls become calls that jump there, each
    /// such call taking `host_loc` as its location; the call locations of
    /// the copied calls are added to `locs`.
    fn block(
        &mut self,
        host: &mut Function,
        b: &Block,
        cont: Option<usize>,
        host_loc: Option<StrId>,
        locs: &mut Vec<(Local, StrId)>,
    ) -> Result<Block, Ice> {
        let params = b
            .params
            .iter()
            .map(|&p| self.local(p))
            .collect::<Result<Vec<_>, Ice>>()?;
        let mut stmts = Vec::with_capacity(b.stmts.len().saturating_add(1));
        for s in &b.stmts {
            let copied = self.stmt(s)?;
            if let (
                Stmt::Assign {
                    dst,
                    value: Rvalue::Call { .. },
                },
                Stmt::Assign { dst: new, .. },
            ) = (s, &copied)
            {
                let own = call_loc(self.callee, *dst)
                    .and_then(|loc| self.loc_strings.get(loc.index()).copied().flatten());
                if let Some(loc) = own.or(host_loc) {
                    locs.push((*new, loc));
                }
            }
            stmts.push(copied);
        }
        let mut pos = self.block_pos(host, b)?;
        let cont = cont.map(block_id).transpose()?;
        let term = match (&b.term, cont) {
            (Terminator::Return(op), Some(cont)) => Terminator::Goto {
                target: cont,
                args: vec![self.operand(op)?],
            },
            (Terminator::TailCall { callee, args }, Some(cont)) => {
                let result = host.locals.push(LocalDecl {
                    repr: Repr::Value,
                    name: None,
                    ty: None,
                })?;
                stmts.push(Stmt::Assign {
                    dst: result,
                    value: Rvalue::Call {
                        callee: self.callee_of(callee)?,
                        args: self.operands(args)?,
                    },
                });
                if let Some(&term_pos) = pos.last() {
                    pos.push(term_pos);
                }
                if let Some(loc) = host_loc {
                    locs.push((result, loc));
                }
                Terminator::Goto {
                    target: cont,
                    args: vec![Operand::Local(result)],
                }
            }
            (Terminator::Return(op), None) => Terminator::Return(self.operand(op)?),
            (Terminator::TailCall { callee, args }, None) => Terminator::TailCall {
                callee: self.callee_of(callee)?,
                args: self.operands(args)?,
            },
            (Terminator::Goto { target, args }, _) => Terminator::Goto {
                target: self.target(*target)?,
                args: self.operands(args)?,
            },
            (Terminator::Branch { cond, then_, else_ }, _) => Terminator::Branch {
                cond: self.operand(cond)?,
                then_: self.target(*then_)?,
                else_: self.target(*else_)?,
            },
            (Terminator::Switch { on, cases, default }, _) => Terminator::Switch {
                on: self.operand(on)?,
                cases: cases
                    .iter()
                    .map(|&(v, t)| Ok((v, self.target(t)?)))
                    .collect::<Result<Vec<_>, Ice>>()?,
                default: default.map(|t| self.target(t)).transpose()?,
            },
            (Terminator::Panic { kind, message, loc }, _) => Terminator::Panic {
                kind: *kind,
                message: message.as_ref().map(|m| self.operand(m)).transpose()?,
                loc: self.string(*loc)?,
            },
            (Terminator::Unreachable, _) => Terminator::Unreachable,
            (Terminator::Resume, _) => return Err(Ice::new("inline: a body that resumes")),
        };
        Ok(Block {
            params,
            stmts,
            term,
            unwind: Vec::new(),
            pos,
        })
    }
}

/// Which strings of `f`'s table its body uses: constants, checked
/// operations' and panics' locations. Not its entry's location, which an
/// inlined body has no use for.
fn used_strings(f: &Function) -> Vec<bool> {
    let mut ids: Vec<StrId> = Vec::new();
    let operand = |ids: &mut Vec<StrId>, op: &Operand| {
        if let Operand::Const(Const::Str(id)) = op {
            ids.push(*id);
        }
    };
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = s {
                for op in value.operands() {
                    operand(&mut ids, &op);
                }
                if let Rvalue::Checked { loc, .. } = value {
                    ids.push(*loc);
                }
            }
        }
        match &b.term {
            Terminator::Return(op)
            | Terminator::Branch { cond: op, .. }
            | Terminator::Switch { on: op, .. } => operand(&mut ids, op),
            Terminator::TailCall { args, .. } | Terminator::Goto { args, .. } => {
                for op in args {
                    operand(&mut ids, op);
                }
            }
            Terminator::Panic { message, loc, .. } => {
                if let Some(m) = message {
                    operand(&mut ids, m);
                }
                ids.push(*loc);
            }
            Terminator::Unreachable | Terminator::Resume => {}
        }
    }
    let mut used = vec![false; f.strings.len()];
    for id in ids {
        if let Some(u) = used.get_mut(id.index()) {
            *u = true;
        }
    }
    used
}

/// The id in `table` of each string of `from` that `used` marks, in
/// order, added to `table` when it holds no equal string; none for the
/// others.
fn intern_used(
    table: &mut cheby_iface::IndexVec<StrId, Box<str>>,
    from: &cheby_iface::IndexVec<StrId, Box<str>>,
    used: &[bool],
) -> Result<Vec<Option<StrId>>, Ice> {
    if from.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: FxHashMap<Box<str>, StrId> = table
        .iter_enumerated()
        .map(|(id, s)| (s.clone(), id))
        .collect();
    let mut out = Vec::with_capacity(from.len());
    for (n, s) in from.iter().enumerate() {
        if used.get(n) != Some(&true) {
            out.push(None);
            continue;
        }
        let id = if let Some(&id) = ids.get(s) {
            id
        } else {
            let id = table.push(s.clone())?;
            ids.insert(s.clone(), id);
            id
        };
        out.push(Some(id));
    }
    Ok(out)
}

/// The budgets of the selection (module docs). Compiler constants, so the
/// build ID covers them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InlineBudget {
    /// The largest callee inlined within the unit's growth.
    pub inline: usize,
    /// The largest callee inlined whatever the unit's growth.
    pub always: usize,
    /// The size no function grows past by inlining.
    pub function: usize,
    /// How much inlining may grow a unit, in percent of its size before.
    pub unit_growth_percent: usize,
}

impl InlineBudget {
    /// The release profile's budgets.
    pub const DEFAULT: Self = Self {
        inline: 30,
        always: 8,
        function: crate::lower::SPLIT_SIZE,
        unit_growth_percent: 50,
    };
}

/// Inlines the unit's calls of its own functions and cleans up every
/// function (module docs), with [`InlineBudget::DEFAULT`]: the release
/// profile's `inline` lever. The unit stays lowered MIR; the caller
/// validates it again ([`crate::check_lowered`]).
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn optimize(unit: &mut MirModule) -> Result<(), Ice> {
    optimize_with(unit, &InlineBudget::DEFAULT)
}

/// [`optimize`] with the budgets `budget`.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn optimize_with(unit: &mut MirModule, budget: &InlineBudget) -> Result<(), Ice> {
    let index = unit.function_index();
    let graph = call_graph(unit, &index);
    let (order, in_cycle) = components(&graph);
    let tags = crate::simplify::Tags::of(unit);
    let callable: Vec<bool> = unit
        .functions
        .iter()
        .zip(&in_cycle)
        .map(|(f, &cycle)| !cycle && is_callee(unit, f))
        .collect();
    let before = unit
        .functions
        .iter()
        .map(cost)
        .fold(0, usize::saturating_add);
    let mut budget_left = before
        .saturating_mul(budget.unit_growth_percent)
        .checked_div(100)
        .unwrap_or(0);
    for host in order {
        inline_into(unit, host, &index, &callable, budget, &mut budget_left)?;
        if let Some(f) = unit.functions.get_mut(host) {
            crate::simplify::run(f, &tags)?;
        }
    }
    Ok(())
}

/// Whether `f`, a function of `unit`, may be inlined at its direct calls,
/// cycles aside (module docs).
fn is_callee(unit: &MirModule, f: &Function) -> bool {
    let FnKey::Item(def) = f.key else {
        return false;
    };
    if check_body(f).is_err()
        || unit
            .name(def)
            .is_ok_and(|n| crate::waits::keeps_frame(&n.module, &n.item))
    {
        return false;
    }
    let counted_param = |op: &Operand| {
        op.local().is_some_and(|l| {
            f.params.contains(&l) && f.local(l).is_ok_and(|d| d.repr == Repr::Value)
        })
    };
    let names_continuation = |c: &Const| matches!(c, Const::Code(FnKey::Continuation { .. }));
    let operand_bad = |op: &Operand| match op {
        Operand::Const(c) => names_continuation(c) || *c == Const::Ctx,
        Operand::Local(_) => false,
    };
    let callee_bad = |c: &Callee| matches!(c, Callee::Code(FnKey::Continuation { .. }));
    for b in &f.blocks {
        for s in &b.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = s else {
                continue;
            };
            if value.operands().iter().any(operand_bad) {
                return false;
            }
            match value {
                Rvalue::CallForeign { args, blocking, .. } => {
                    if *blocking || args.iter().any(counted_param) {
                        return false;
                    }
                }
                Rvalue::Call { callee, .. } if callee_bad(callee) => return false,
                Rvalue::Use(_)
                | Rvalue::Call { .. }
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
                | Rvalue::Compare { .. }
                | Rvalue::FloatArith { .. }
                | Rvalue::Bitwise { .. }
                | Rvalue::FloatNeg { .. }
                | Rvalue::Not(_)
                | Rvalue::ReadTag { .. }
                | Rvalue::DropReuse { .. }
                | Rvalue::AllocReuse { .. } => {}
            }
        }
        let term_ops: Vec<Operand> = match &b.term {
            Terminator::TailCall { callee, args } => {
                if callee_bad(callee) {
                    return false;
                }
                args.clone()
            }
            Terminator::Return(op) => vec![op.clone()],
            Terminator::Goto { args, .. } => args.clone(),
            Terminator::Branch { cond, .. } => vec![cond.clone()],
            Terminator::Switch { on, .. } => vec![on.clone()],
            Terminator::Panic { message, .. } => message.iter().cloned().collect(),
            Terminator::Unreachable | Terminator::Resume => Vec::new(),
        };
        if term_ops.iter().any(operand_bad) {
            return false;
        }
    }
    true
}

/// The functions each function of `unit` calls or names by key (a
/// closure's code word, [`Const::Code`]), as indexes into
/// [`MirModule::functions`], sorted and without duplicates. A closure
/// counts as a callee of the function that builds it, so the functions it
/// calls come before that function in the order of [`components`].
fn call_graph(unit: &MirModule, index: &FxHashMap<FnKey, usize>) -> Vec<Vec<usize>> {
    let target = |callee: &Callee| -> Option<usize> {
        let key = match callee {
            Callee::Direct(def) | Callee::Borrowing(def, _) => FnKey::Item(*def),
            Callee::Code(key) => *key,
            Callee::Indirect(_) => return None,
        };
        index.get(&key).copied()
    };
    unit.functions
        .iter()
        .map(|f| {
            let mut out: Vec<usize> = Vec::new();
            for b in &f.blocks {
                for s in &b.stmts {
                    let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = s else {
                        continue;
                    };
                    if let Rvalue::Call { callee, .. } = value {
                        out.extend(target(callee));
                    }
                    for op in value.operands() {
                        if let Operand::Const(Const::Code(key)) = op {
                            out.extend(index.get(&key).copied());
                        }
                    }
                }
                if let Terminator::TailCall { callee, .. } = &b.term {
                    out.extend(target(callee));
                }
            }
            out.sort_unstable();
            out.dedup();
            out
        })
        .collect()
}

/// The functions of `graph` in the order of its strongly connected
/// components, callees first (Tarjan's order), each component's members
/// by index; and whether each function is in a cycle (a component of more
/// than one, or a call of itself).
fn components(graph: &[Vec<usize>]) -> (Vec<usize>, Vec<bool>) {
    const UNSEEN: usize = usize::MAX;
    let n = graph.len();
    let mut number = vec![UNSEEN; n];
    let mut low = vec![0_usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0_usize;
    let mut order = Vec::with_capacity(n);
    let mut in_cycle = vec![false; n];
    for root in 0..n {
        if number.get(root) != Some(&UNSEEN) {
            continue;
        }
        // Each frame: a node and the position of its next successor.
        let mut frames: Vec<(usize, usize)> = vec![(root, 0)];
        visit(
            root,
            &mut number,
            &mut low,
            &mut on_stack,
            &mut stack,
            &mut next,
        );
        while let Some(&mut (node, ref mut at)) = frames.last_mut() {
            let succs = graph.get(node).map_or(&[][..], Vec::as_slice);
            if let Some(&succ) = succs.get(*at) {
                *at = at.saturating_add(1);
                if number.get(succ) == Some(&UNSEEN) {
                    visit(
                        succ,
                        &mut number,
                        &mut low,
                        &mut on_stack,
                        &mut stack,
                        &mut next,
                    );
                    frames.push((succ, 0));
                } else if on_stack.get(succ) == Some(&true) {
                    let n_succ = number.get(succ).copied().unwrap_or(UNSEEN);
                    if let Some(l) = low.get_mut(node) {
                        *l = (*l).min(n_succ);
                    }
                }
                continue;
            }
            frames.pop();
            let node_low = low.get(node).copied().unwrap_or(0);
            if let Some(&(parent, _)) = frames.last()
                && let Some(l) = low.get_mut(parent)
            {
                *l = (*l).min(node_low);
            }
            if number.get(node) == Some(&node_low) {
                let mut members = Vec::new();
                while let Some(m) = stack.pop() {
                    if let Some(s) = on_stack.get_mut(m) {
                        *s = false;
                    }
                    members.push(m);
                    if m == node {
                        break;
                    }
                }
                members.sort_unstable();
                let cycle = members.len() > 1
                    || graph
                        .get(node)
                        .is_some_and(|succs| succs.binary_search(&node).is_ok());
                for &m in &members {
                    if let Some(c) = in_cycle.get_mut(m) {
                        *c = cycle;
                    }
                }
                order.extend(members);
            }
        }
    }
    (order, in_cycle)
}

/// Tarjan's first visit of `node`.
fn visit(
    node: usize,
    number: &mut [usize],
    low: &mut [usize],
    on_stack: &mut [bool],
    stack: &mut Vec<usize>,
    next: &mut usize,
) {
    if let Some(n) = number.get_mut(node) {
        *n = *next;
    }
    if let Some(l) = low.get_mut(node) {
        *l = *next;
    }
    *next = next.saturating_add(1);
    if let Some(s) = on_stack.get_mut(node) {
        *s = true;
    }
    stack.push(node);
}

/// The next direct call of block `b` of `f` from statement `from` on, of a
/// function `callable` names: its place and the callee's index.
fn next_site(
    f: &Function,
    b: BlockId,
    from: usize,
    index: &FxHashMap<FnKey, usize>,
    callable: &[bool],
) -> Option<(SiteAt, usize)> {
    let block = f.blocks.get(b)?;
    let pick = |callee: &Callee| -> Option<usize> {
        let Callee::Direct(def) = callee else {
            return None;
        };
        let at = *index.get(&FnKey::Item(*def))?;
        (callable.get(at) == Some(&true) && f.key != FnKey::Item(*def)).then_some(at)
    };
    for (i, s) in block.stmts.iter().enumerate().skip(from) {
        if let Stmt::Assign {
            value: Rvalue::Call { callee, .. },
            ..
        } = s
            && let Some(at) = pick(callee)
        {
            return Some((SiteAt::Stmt(i), at));
        }
    }
    if let Terminator::TailCall { callee, .. } = &block.term {
        return pick(callee).map(|at| (SiteAt::Tail, at));
    }
    None
}

/// How much splicing `callee` at a call grows the host (in [`cost`]'s
/// unit): the callee's statements and blocks, and a local for each
/// constant argument that needs one; at a call in a statement also a call
/// for each tail call, and a continuation block for the call it removes.
fn growth(callee: &Function, at: SiteAt, args: &[Operand]) -> usize {
    let tail_calls = callee
        .blocks
        .iter()
        .filter(|b| matches!(b.term, Terminator::TailCall { .. }))
        .count();
    let local_only = local_only_uses(callee);
    let uses = crate::simplify::use_counts(callee);
    // Constants that need a local, and arguments the body never uses,
    // which get a move (an upper bound: a raw argument gets none).
    let locals = callee
        .params
        .iter()
        .zip(args)
        .filter(|&(p, a)| match a {
            Operand::Const(_) => local_only.contains(p),
            Operand::Local(_) => uses.get(p.index()).is_none_or(|&n| n == 0),
        })
        .count();
    let body = cost(callee).saturating_add(locals);
    match at {
        SiteAt::Stmt(_) => body.saturating_add(tail_calls),
        SiteAt::Tail => body,
    }
}

/// The arguments of the call at `at` in block `b` of `f`.
fn site_args(f: &Function, b: BlockId, at: SiteAt) -> Vec<Operand> {
    let Some(block) = f.blocks.get(b) else {
        return Vec::new();
    };
    match at {
        SiteAt::Stmt(i) => match block.stmts.get(i) {
            Some(Stmt::Assign {
                value: Rvalue::Call { args, .. },
                ..
            }) => args.clone(),
            _ => Vec::new(),
        },
        SiteAt::Tail => {
            if let Terminator::TailCall { args, .. } = &block.term {
                args.clone()
            } else {
                Vec::new()
            }
        }
    }
}

/// Inlines the calls of function `host` of `unit` that the budgets allow
/// (module docs), searching only its own blocks and the continuations.
fn inline_into(
    unit: &mut MirModule,
    host: usize,
    index: &FxHashMap<FnKey, usize>,
    callable: &[bool],
    budget: &InlineBudget,
    unit_left: &mut usize,
) -> Result<(), Ice> {
    let Some(f) = unit.functions.get(host) else {
        return Ok(());
    };
    if f.abi != Abi::Cheby || f.precounted || f.blocks.iter().any(|b| !b.unwind.is_empty()) {
        return Ok(());
    }
    let mut scan = vec![true; f.blocks.len()];
    let mut b = 0_usize;
    while scan.get(b).is_some() {
        if scan.get(b) != Some(&true) {
            b = b.saturating_add(1);
            continue;
        }
        let id = block_id(b)?;
        let mut from = 0_usize;
        loop {
            let Some(f) = unit.functions.get(host) else {
                return Ok(());
            };
            let Some((at, callee)) = next_site(f, id, from, index, callable) else {
                break;
            };
            let Some(body) = unit.functions.get(callee) else {
                break;
            };
            let size = cost(body);
            let grows = growth(body, at, &site_args(f, id, at));
            let fits = cost(f).saturating_add(grows) <= budget.function;
            let tiny = size <= budget.always;
            let accepted = fits && (tiny || (size <= budget.inline && grows <= *unit_left));
            if !accepted {
                match at {
                    SiteAt::Stmt(i) => {
                        from = i.saturating_add(1);
                        continue;
                    }
                    SiteAt::Tail => break,
                }
            }
            let body = body.clone();
            let f = unit
                .functions
                .get_mut(host)
                .ok_or_else(|| Ice::new("inline: the host"))?;
            let spliced = splice(f, CallSite { block: id, at }, &body, CopiedPositions::Own)?;
            if !tiny {
                *unit_left = unit_left.saturating_sub(grows);
            }
            if spliced.cont.is_some() {
                scan.push(true);
            }
            scan.extend(std::iter::repeat_n(false, spliced.body.len()));
            break;
        }
        b = b.saturating_add(1);
    }
    Ok(())
}
