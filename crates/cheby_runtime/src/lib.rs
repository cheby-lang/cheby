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
//!   hashing and debug text), [`desc`] (type descriptors and the builtin
//!   nominal descriptors).
//! - Running code: [`panic`](mod@panic), [`ctx`] (the fiber control
//!   block and interrupt reasons, D-685, D-688), [`entry`] (entries, the
//!   current entry, exit statuses), [`sched`] (the M:N scheduler: one
//!   [`Runtime`] per host run, its workers, run queues and monitor,
//!   D-689, D-690), [`callback`] (kernels calling
//!   closures), [`consteval`] (constant evaluation's runtime side),
//!   [`stats`] (counters of allocations and in-place updates), the private
//!   `tls` (every thread-local of the crate, D-686), `unwind` (unwinding
//!   by the return flag, D-733) and the private `fiber_ops`
//!   (scopes, fibers and channels).
//! - Kernels, by std module: [`io`], and the private modules `num`, `list`,
//!   `text`, `bytes`, `env`, `random` (D-656), `runtime_info` (`std::runtime`,
//!   D-657) and [`hamt`], each registering its symbols in [`symbols`] (plan
//!   §4.3.3).
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
pub mod desc;
pub mod entry;
mod env;
pub mod fiber;
pub(crate) mod fiber_ops;
pub mod generic;
#[doc(hidden)]
pub mod hamt;
pub(crate) mod handles;
mod helpers;
pub mod io;
pub mod layout;
#[doc(hidden)]
pub mod list;
mod num;
pub mod object;
pub mod panic;
mod random;
pub mod rc;
mod runtime_info;
pub mod sched;
pub mod stats;
pub mod string;
mod symbols;
mod text;
mod tls;
#[doc(hidden)]
pub mod unwind;
pub mod values;

pub use alloc::{LEAK_CHECK_ENABLED, live_objects};
pub use ctx::ExecCtx;
pub use entry::{
    Callbacks, ConstEvalLimits, EntryConfig, EntryFn, EntryMode, EntryOutcome, RunConfig,
    RuntimeError, run_entry, run_main,
};
pub use layout::{FieldInfo, FieldKind, LayoutError, LayoutInfo, LayoutKind};
pub use panic::PanicReport;
pub use sched::{CodeToken, DetachedGroup, Runtime, RuntimeConfig, TeardownReport};
pub use symbols::{complete_modules, symbols};
