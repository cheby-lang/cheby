//! MIR, lowering from typed bodies, and the RC passes: naive in M1,
//! Perceus in M2 (D-273). See `docs/plan/m1-core.md` phase D.
//!
//! - [`ir`]: the data model (D1); [`builder`] builds functions.
//! - [`repr`] and [`abi`]: value representations and the one source of
//!   machine signatures (D-453, D-454).
//! - [`layout`]: constructor layouts from interfaces (D-246, D-252).
//! - [`reach`]: the items a run needs, from its entry
//!   (GAP(backend-reachability)).
//! - [`lower`]: THIR to MIR (D2), with tail-position marking (§5.15.2),
//!   external wrappers and entry thunks. What it cannot lower yet is
//!   E0038 at the construct (plan §8), never an internal error.
//! - [`rc`]: naive dup/drop placement by the last-use rule (D7, D-205).
//! - [`validate`] and [`dump`]: the validator and the stable text form.

pub mod abi;
pub mod builder;
mod defs;
pub mod dump;
#[cfg(test)]
mod golden_tests;
pub mod ir;
pub mod layout;
mod liveness;
pub mod lower;
#[cfg(test)]
mod lower_tests;
pub mod rc;
pub mod reach;
pub mod repr;
#[cfg(test)]
mod tests;
pub mod validate;

pub use abi::{AbiSig, abi_sig, entry_sig, fixed_sig, trampoline_sig, uniform_sig};
pub use builder::FunctionBuilder;
pub use defs::Defs;
pub use dump::{dump, dump_function};
pub use ir::LayoutRef;
pub use ir::{
    Abi, ArithOp, Block, BlockId, BuiltinLayout, Callee, CalleeDecl, CmpOp, Const, DefName,
    EntryKind, FnKey, Function, Local, LocalDecl, MirModule, Operand, PanicKind, RuntimeFn, Rvalue,
    Stmt, StrId, Terminator, UnitKind, UseKind,
};
pub use layout::{FieldDesc, LayoutDesc, ctor_layout};
pub use lower::{
    EntryRequest, LowerCx, LowerError, Unsupported, lower_entries, lower_fn, lower_items,
    lower_module, lower_tests,
};
pub use reach::{Program, Reach, Root, reachable};
pub use repr::{CType, ForeignSig, Repr, ctype_of, foreign_sig, repr_of};
pub use validate::{Stage, validate};

/// Validates a freshly lowered unit, inserts reference counts, and
/// validates again with the ownership check: the pipeline every unit goes
/// through before codegen.
///
/// # Errors
///
/// An [`Ice`](cheby_iface::Ice) listing the validator's findings.
pub fn count_and_check(module: &mut MirModule) -> Result<(), cheby_iface::Ice> {
    let report = |stage: &str, errors: Vec<String>| {
        cheby_iface::Ice::new(format!("invalid MIR {stage}:\n  {}", errors.join("\n  ")))
    };
    validate(module, Stage::Lowered).map_err(|e| report("after lowering", e))?;
    rc::insert_module(module)?;
    validate(module, Stage::Counted).map_err(|e| report("after reference counting", e))
}
