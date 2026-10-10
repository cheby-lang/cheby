//! Structural equality, hashing and debug printing (§3.13, D-252, F5):
//! one runtime routine each, driven by the layout table.
//!
//! Each routine walks the value with an explicit worklist on the heap,
//! never Rust recursion over the data, so a million-deep value cannot
//! overflow the Rust stack (plan §0, deep data). Runtime-defined kinds
//! ([`crate::layout::CustomKind`]) take part through their functions,
//! which schedule their children on the same worklists.
//!
//! What wave 1 covers: tagged and boxed numbers, strings, `Bytes`, tuples,
//! closures (`<fn>`, and `EqualityOnFunction` for `==` and hashing, D-233),
//! builtin and user constructors by layout, `dyn` values in debug text, and
//! runtime-defined kinds. Extension points, marked in the code: collections
//! with order-independent equality and sorted debug text (W3-B, W3-C),
//! `@std_hooks` (W3-B), `dyn` equality and hashing (W4-A), external types
//! (W3-B).

mod debug;
mod equal;
mod hash;
mod raw;

use std::hash::{BuildHasher, Hasher as _};
use std::sync::OnceLock;

pub use debug::{Printer, debug, debug_string};
pub use equal::{Cx, equal};
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
