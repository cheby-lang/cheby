//! Native runtime: memory, kernels, generic operations, panics, fibers.
//! Built as an rlib for the JIT and a staticlib for AOT.
//!
//! - [`abi`]: the contract with code generation (header encoding, tagged
//!   integers, layout ids, string layout, context offsets, exit statuses,
//!   panic kinds, symbol names). No statics, no dependencies.
//! - Memory: [`alloc`] (allocation, iterative free, accounting domains,
//!   poisoning), [`rc`] (`dup`/`drop`, uniqueness), [`object`] (headers and
//!   static objects), [`layout`] (the global layout table and
//!   runtime-defined kinds).
//! - Values: [`values`] (building and reading uniform words), [`string`]
//!   (`String` and `Bytes` objects), [`generic`] (structural equality,
//!   hashing and debug text).
//! - Running code: [`panic`](mod@panic), [`ctx`], [`entry`] (entries, the
//!   current entry, exit statuses), [`callback`] (kernels calling
//!   closures), [`consteval`] (constant evaluation's runtime side).
//! - Kernels, by std module: [`io`], and the private modules `num`, `list`,
//!   `text`, `bytes`, `env` and [`hamt`], each registering its symbols in
//!   [`symbols`] (plan §4.3.3).
//!
//! Every C-ABI entry point is `extern "C"`, so an internal Rust panic
//! aborts rather than unwinding through JIT frames.

pub mod abi;
pub mod alloc;
mod bytes;
pub mod callback;
#[doc(hidden)]
pub mod consteval;
pub mod ctx;
pub mod entry;
mod env;
pub mod generic;
#[doc(hidden)]
pub mod hamt;
mod helpers;
pub mod io;
pub mod layout;
#[doc(hidden)]
pub mod list;
mod num;
pub mod object;
pub mod panic;
pub mod rc;
pub mod string;
mod symbols;
mod text;
pub mod values;

pub use alloc::{LEAK_CHECK_ENABLED, live_objects};
pub use ctx::ExecCtx;
pub use entry::{
    Callbacks, ConstEvalLimits, EntryConfig, EntryFn, EntryMode, EntryOutcome, RunConfig,
    RuntimeError, run_entry, run_main,
};
pub use layout::{FieldInfo, FieldKind, LayoutError, LayoutInfo, LayoutKind};
pub use panic::PanicReport;
pub use symbols::{complete_modules, symbols};
