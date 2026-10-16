//! MIR, lowering from typed bodies, and the RC passes: naive in M1,
//! Perceus in M2 (D-273). See `docs/plan/m1-core.md` phase D.
//!
//! - [`ir`]: the data model (D1); [`builder`] builds functions.
//! - [`repr`] and [`abi`]: value representations and the one source of
//!   machine signatures (D-453, D-454).
//! - [`layout`]: constructor layouts from interfaces (D-246, D-252).
//! - [`reach`]: the items a run needs, from its entry (D-510).
//! - [`lower`]: THIR to MIR (D2), with tail-position marking (§5.15.2),
//!   external wrappers and entry thunks. Every THIR construct lowers, so
//!   lowering reports no diagnostics (D-554); its only error is an
//!   internal one.
//! - [`rc`]: naive dup/drop placement by the last-use rule (D7, D-205).
//! - [`unwind`]: unwind edges to shared cleanup pads, after counting
//!   (D-698), with the ownership simulation it shares with the validator;
//!   [`never_raise`]: the std kernels whose calls get no edge (D-767).
//! - [`reuse`]: pairs drops with allocations of the same size so a
//!   unique object's block is reused (D-716), after counting.
//! - [`validate`] and [`dump`]: the validator and the stable text form.
//! - [`stats`]: static counts for reports (D-759).

pub mod abi;
pub mod borrow;
#[cfg(test)]
mod borrow_tests;
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
pub mod never_raise;
pub mod options;
mod ownership;
pub mod rc;
pub mod reach;
pub mod repr;
pub mod reuse;
#[cfg(test)]
mod reuse_tests;
pub mod stats;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
pub mod unwind;
#[cfg(test)]
mod unwind_ir_tests;
#[cfg(test)]
mod unwind_tests;
pub mod validate;
#[cfg(test)]
mod validate_tests;

pub use abi::{AbiSig, abi_sig, entry_sig, fixed_sig, is_uniform_sig, trampoline_sig, uniform_sig};
pub use builder::FunctionBuilder;
pub use defs::Defs;
pub use dump::{dump, dump_function};
pub use ir::LayoutRef;
pub use ir::{
    Abi, ArithOp, BitOp, Block, BlockId, BuiltinDesc, BuiltinLayout, Callee, CalleeDecl, CmpOp,
    Const, DefName, EntryKind, FnKey, Function, Local, LocalDecl, MirModule, Operand, PanicKind,
    RuntimeFn, Rvalue, StaticId, StaticKind, StaticObject, Stmt, StrId, TagType, Terminator,
    UnitKind, UseKind,
};
pub use layout::{FieldDesc, LayoutDesc, ctor_layout, type_key, type_layouts};
pub use lower::{
    EntryRequest, LowerCx, lower_entries, lower_fn, lower_items, lower_module, lower_tests,
};
pub use options::{MirOptions, RcMode, check_lowered, count_and_check_with, count_lowered_with};
pub use reach::{Program, Reach, Root, reachable};
pub use repr::{CType, ForeignSig, Repr, ctype_of, foreign_sig, repr_of};
pub use validate::{Stage, validate};

/// Validates a freshly lowered unit, inserts reference counts and the
/// unwind edges (D-698), and validates again with the ownership and
/// unwinding checks: the pipeline every unit goes through before codegen.
/// [`count_and_check_with`] with the default options. A unit counted on
/// its own is never inferred, so it borrows only what
/// [`borrow::apply`] wrote into it (D-715, D-779).
///
/// # Errors
///
/// An [`Ice`](cheby_iface::Ice) listing the validator's findings.
pub fn count_and_check(module: &mut MirModule) -> Result<(), cheby_iface::Ice> {
    count_and_check_with(module, &MirOptions::default())
}
