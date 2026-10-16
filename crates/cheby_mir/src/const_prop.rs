//! Static constants left uncounted (D-454, D-539, D-850): a
//! local defined as a static constant (a string literal, a nullary
//! constructor's static object, a function value's static closure, the
//! empty list, a static of the unit, a descriptor, `Nil`, `Err(Nil)`)
//! becomes the constant itself in the operand positions that may hold one,
//! before reference counting, so counting never `dup`s or drops it: a
//! static object's count is 0, and `dup` and `drop` skip it at run time,
//! but each still costs a test of the count.
//!
//! The pass runs after lowering (and after the borrowing inference that a
//! build runs over every unit) and before [`crate::rc::insert`], on Perceus
//! counting only and under [`crate::MirOptions::fuse`]
//! ([`crate::MirOptions::fuses`]), so naive counting stays the oracle
//! (D-718, D-808). It never edits lowering.
//!
//! The positions that keep the local: a base (`field`, `read_layout`,
//! `read_tag`), a tuple being filled (`init_field`'s object), an indirect
//! callee, and a `use` that copies the local into another, so what reads
//! a constant's fields or tag, and which locals are constants, stays as
//! lowering made it: reuse proves layouts by those reads and never reuses
//! a constant (D-839), and its decisions are the same
//! with and without this pass. A panic's message keeps its local too (the
//! path is cold). A definition no position reads any more is removed; one
//! nothing read in the first place stays, as lowering made it. Raw
//! operands (arithmetic, comparisons, `box`, `unbox`) never hold a value
//! constant. A `precounted` body (D-693) is left alone.

use cheby_iface::Ice;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{Const, Function, Local, MirModule, Operand, Rvalue, Stmt, Terminator};
use crate::repr::Repr;

/// Propagates the static constant locals of every function of `module`
/// (module documentation).
///
/// # Errors
///
/// An [`Ice`] if a function already has reference counts, or is
/// malformed.
pub fn run_module(module: &mut MirModule) -> Result<(), Ice> {
    for f in &mut module.functions {
        run(f)?;
    }
    Ok(())
}

/// Propagates the static constant locals of `f`, a lowered function
/// (module documentation).
///
/// # Errors
///
/// An [`Ice`] if `f` already has reference counts, or is malformed.
pub fn run(f: &mut Function) -> Result<(), Ice> {
    if f.precounted {
        return Ok(());
    }
    let mut constants: FxHashMap<Local, Const> = FxHashMap::default();
    for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
        match stmt {
            Stmt::Assign {
                dst,
                value: Rvalue::Use(Operand::Const(c)),
            } if is_static(c) && f.local(*dst)?.repr == Repr::Value => {
                constants.insert(*dst, c.clone());
            }
            Stmt::Dup(_) | Stmt::Drop(_) | Stmt::DropKeep { .. } => {
                return Err(Ice::new("constant propagation after reference counting"));
            }
            _ => {}
        }
    }
    if constants.is_empty() {
        return Ok(());
    }
    // A constant nothing reads stays as lowering made it (counting drops
    // it after its definition, as naive counting does).
    let read_before = reads(f);
    constants.retain(|l, _| read_before.contains(l));
    for block in &mut f.blocks {
        for stmt in &mut block.stmts {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                rewrite_rvalue(value, &constants);
            }
        }
        rewrite_terminator(&mut block.term, &constants);
    }
    // The constants something still reads keep their definition.
    let read = reads(f);
    for block in &mut f.blocks {
        block.stmts.retain(|s| match s {
            Stmt::Assign { dst, .. } => !constants.contains_key(dst) || read.contains(dst),
            _ => true,
        });
    }
    Ok(())
}

/// The locals some statement or terminator of `f` reads.
fn reads(f: &Function) -> FxHashSet<Local> {
    let mut read: FxHashSet<Local> = FxHashSet::default();
    for block in &f.blocks {
        for stmt in &block.stmts {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                read.extend(value.uses().into_iter().map(|(l, _)| l));
            }
        }
        read.extend(block.term.uses().into_iter().map(|(l, _)| l));
    }
    read
}

/// Whether `c` is a static value constant this pass propagates: a static
/// object's address, never a code word or the context.
const fn is_static(c: &Const) -> bool {
    matches!(
        c,
        Const::Str(_)
            | Const::Nil
            | Const::StaticCtor(_)
            | Const::FnValue(_)
            | Const::EmptyList
            | Const::Static(_)
            | Const::BuiltinDesc(_)
            | Const::ErrNil
    )
}

/// Replaces each constant local in `ops` by its constant.
fn rewrite(ops: &mut [Operand], constants: &FxHashMap<Local, Const>) {
    for op in ops {
        if let Operand::Local(l) = op
            && let Some(c) = constants.get(l)
        {
            *op = Operand::Const(c.clone());
        }
    }
}

/// The operand positions of `value` that may hold a value constant
/// (module documentation).
fn rewrite_rvalue(value: &mut Rvalue, constants: &FxHashMap<Local, Const>) {
    match value {
        Rvalue::Call { args, .. }
        | Rvalue::CallForeign { args, .. }
        | Rvalue::CallRuntime { args, .. } => rewrite(args, constants),
        Rvalue::Alloc { fields, .. } | Rvalue::AllocReuse { fields, .. } => {
            rewrite(fields, constants);
        }
        Rvalue::InitField { value, .. } => rewrite(std::slice::from_mut(value), constants),
        // Kept: a copy into another local, bases, raw operands.
        Rvalue::Use(_)
        | Rvalue::AllocPlaceholder { .. }
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
        | Rvalue::DropReuse { .. } => {}
    }
}

/// The operand positions of `term` that may hold a value constant.
fn rewrite_terminator(term: &mut Terminator, constants: &FxHashMap<Local, Const>) {
    match term {
        Terminator::Return(op) => rewrite(std::slice::from_mut(op), constants),
        Terminator::Goto { args, .. } | Terminator::TailCall { args, .. } => {
            rewrite(args, constants);
        }
        // A panic's message keeps its local: the path is cold.
        Terminator::Panic { .. }
        | Terminator::Branch { .. }
        | Terminator::Switch { .. }
        | Terminator::Unreachable
        | Terminator::Resume => {}
    }
}
