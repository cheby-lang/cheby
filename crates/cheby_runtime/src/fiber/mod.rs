//! The machinery below the scheduler: stack slots carved from shared
//! regions (D-677, D-678) and the context switch between stacks (D-676).
//!
//! - [`stack`]: the process-global [`StackPool`] of power-of-two stack
//!   slots, with debug guard pages and the release of deep pages on
//!   reuse.
//! - [`switch`]: the `naked_asm!` switch, [`prepare`](switch::prepare)
//!   for a fresh stack, and [`SUPPORTED`](switch::SUPPORTED), which is
//!   false on Windows until M6 (D-714).
//!
//! - [`sanitizer`]: what the address and thread sanitizers are told about
//!   fiber switches and reused memory, under `--cfg cheby_sanitize` only
//!   (D-721).
//!
//! Nothing here knows the fiber control block: a fresh stack starts with
//! an opaque argument, and the scheduler places the block at the top of
//! the slot with [`StackSlot::reserve_top`].

pub(crate) mod sanitizer;
pub mod stack;
pub mod switch;

pub use stack::{ClassCounts, StackError, StackPool, StackSlot};
pub use switch::Saved;
