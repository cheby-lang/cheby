//! Native runtime: memory, kernels, generic operations, panics, fibers.
//! Built as an rlib for the JIT and a staticlib for AOT.
//!
//! M1 so far (F1, F7 and the part of F2 that example 001 needs):
//!
//! - [`abi`]: the contract with code generation (header encoding, tagged
//!   integers, layout ids, string layout, context offsets, exit statuses,
//!   panic kinds, symbol names). No statics, no dependencies.
//! - Memory: [`alloc`] (allocation, iterative free, debug leak counter),
//!   [`rc`] (`dup`/`drop`), [`object`] (headers and static objects),
//!   [`layout`] (the global layout table).
//! - [`string`]: `String` objects.
//! - [`io`]: the print kernels and standard output.
//! - [`panic`](mod@panic), [`ctx`], [`entry`]: panics, the execution context, and
//!   running entries with the exit statuses of §10.10.
//! - [`symbols`]: the address of every C-ABI symbol, for the JIT.
//!
//! Every C-ABI entry point is `extern "C"`, so an internal Rust panic
//! aborts rather than unwinding through JIT frames.

pub mod abi;
pub mod alloc;
pub mod ctx;
pub mod entry;
pub mod io;
pub mod layout;
pub mod object;
pub mod panic;
pub mod rc;
pub mod string;
mod symbols;

pub use alloc::{LEAK_CHECK_ENABLED, live_objects};
pub use ctx::ExecCtx;
pub use entry::{
    ConstEvalLimits, EntryConfig, EntryFn, EntryMode, EntryOutcome, RunConfig, RuntimeError,
    run_entry, run_main,
};
pub use layout::{FieldInfo, FieldKind, LayoutError, LayoutInfo, LayoutKind};
pub use panic::PanicReport;
pub use symbols::symbols;
