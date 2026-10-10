//! Structural equality, hashing and debug printing (§3.13, D-252, F5):
//! one runtime routine each, driven by the layout table.
//!
//! Each routine walks the value with an explicit worklist on the heap,
//! never Rust recursion over the data, so a million-deep value cannot
//! overflow the Rust stack (plan §0, deep data). Runtime-defined kinds
//! ([`crate::layout::CustomKind`]) take part through their functions,
//! which schedule their children on the same worklists.
//!
//! Every kind of value takes part:
//!
//! - tagged and boxed numbers, with reflexive float equality (§3.3.2);
//! - `String` and `Bytes`, printed with the literal escapes (D-531, D-557)
//!   and as `bytes::from_list([0x66])` (D-388);
//! - tuples, and builtin and user constructors by their layouts, hashing
//!   their type key and tag, never a layout id (D-538, D-559);
//! - lists, through the list root's kind ([`crate::list`]);
//! - maps and sets: equality and hashing through their kinds
//!   ([`crate::hamt`], order-independent, D-538), and debug text sorted by
//!   each entry's text (D-344, D-382);
//! - types with `@std_hooks`, through their hook closures (D-342, D-482);
//! - closures and values of external types: `<fn>` and `<Db>`, and
//!   [`Reached::Function`] for `==` and hashing (D-233);
//! - `dyn` values and type descriptors, compared by structure (D-530).

mod debug;
mod equal;
mod hash;
mod hooks;
mod raw;

use std::hash::{BuildHasher, Hasher as _};
use std::sync::OnceLock;

pub use debug::{Printer, SortItem, debug, debug_object, debug_string};
pub use equal::{Cx, equal};
pub(crate) use hash::mark;
pub use hash::{Hasher, hash, hash_with_seed};

/// What made equality or hashing stop without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    /// A function value or a handle: `==` and hashing panic with
    /// `EqualityOnFunction` (D-233).
    Function,
}

/// The per-process hash seed of D-093 and D-538, drawn once from the
/// operating system's randomness through
/// `std::collections::hash_map::RandomState` (process-hash-seed, D-538).
#[must_use]
pub fn process_seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(0x6368_6562_7973_6565);
        hasher.finish()
    })
}

#[cfg(test)]
pub(crate) mod tests;
