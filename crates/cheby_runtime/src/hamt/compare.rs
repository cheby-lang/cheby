//! Keys, and maps and sets as values: hashing a key with a collection's
//! seed, comparing keys, and the order-independent equality and hashing of
//! whole maps and sets (plan §4.3.5, D-093, D-538).
//!
//! Equality never compares hashes across seeds: two collections are equal
//! when they have the same size and every entry of one is found in the
//! other, looked up with the other's own seed, with equal values. Hashing
//! one as a value hashes each entry with the caller's key
//! ([`Hasher::seed`]) and adds the results, so equal collections hash
//! equally whatever seeds they were built with.
//!
//! Neither nests a walk per entry (plan §0, deep data;
//! D-586):
//!
//! - hashing schedules the entries as an unordered group on the caller's
//!   own stack ([`Hasher::push_unordered`]);
//! - equality finds, from a key's hash alone, the one entry of the other
//!   collection the key can equal ([`candidate`]), and puts the key pair
//!   and the value pair on the caller's worklist ([`Cx::push`]). Only a
//!   full 64-bit hash collision, which a random seed makes vanishingly
//!   rare, compares keys in a nested walk.
//!
//! So values that nest maps and sets inside each other, through keys or
//! values, to any depth, compare and hash without deepening the Rust stack.

use super::node::{Collection, Root};
use super::ops::{Candidate, Entries, candidate, find_hashed};
use crate::abi::{PanicKind, Word};
use crate::generic::{self, Cx, Hasher};
use crate::panic::panic_current;

/// The first byte of a hashed map or set, after the generic routine's own
/// marks (1-11).
const MARK_MAP: u8 = 16;
const MARK_SET: u8 = 17;

#[cfg(test)]
thread_local! {
    /// Tests force collisions by masking key hashes.
    pub(super) static HASH_MASK: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// The hash of a borrowed key under a collection's seed. A key that holds
/// a function panics with `EqualityOnFunction` (D-233).
pub(super) fn key_hash(key: Word, seed: u64) -> u64 {
    // SAFETY: keys are live words borrowed for the call.
    match unsafe { generic::hash_with_seed(key, seed) } {
        Ok(hash) => masked(hash),
        Err(generic::Reached::Function) => panic_current(PanicKind::EqualityOnFunction, None),
    }
}

#[cfg(test)]
fn masked(hash: u64) -> u64 {
    hash & HASH_MASK.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn masked(hash: u64) -> u64 {
    hash
}

/// Whether two borrowed keys are equal. Keys in a collection were hashed
/// when they went in, so neither holds a function.
pub(super) fn keys_equal(a: Word, b: Word) -> bool {
    // SAFETY: keys are live words borrowed for the call.
    match unsafe { generic::equal(a, b) } {
        Ok(same) => same,
        Err(generic::Reached::Function) => panic_current(PanicKind::EqualityOnFunction, None),
    }
}

/// `CustomKind::equal` of map and set roots.
///
/// # Safety
///
/// `a` and `b` are live map or set roots.
pub(super) unsafe fn equal(a: Word, b: Word, cx: &mut Cx) -> bool {
    // SAFETY: forwarded from the caller.
    let (Some((ra, ca)), Some((rb, cb))) = (unsafe { Root::probe(a) }, unsafe { Root::probe(b) })
    else {
        return false;
    };
    if ca != cb || ra.count() != rb.count() {
        return false;
    }
    let Some(top) = rb.node() else {
        // Both are empty.
        return true;
    };
    for (key, value) in Entries::new(ra, ca) {
        let hash = key_hash(key, rb.seed());
        let found = match candidate(top, cb, hash) {
            Candidate::Absent => return false,
            Candidate::One(found) => {
                // The key is in `b` exactly when it equals this entry's.
                cx.push(key, found.key());
                found
            }
            Candidate::Collision => match find_hashed(top, cb, key, hash) {
                Some(found) => found,
                None => return false,
            },
        };
        if let Some(value) = value {
            cx.push(value, found.value());
        }
    }
    true
}

/// `CustomKind::hash` of map and set roots.
///
/// # Safety
///
/// `word` is a live map or set root.
pub(super) unsafe fn hash(word: Word, h: &mut Hasher) {
    // SAFETY: forwarded from the caller.
    let Some((root, coll)) = (unsafe { Root::probe(word) }) else {
        return;
    };
    h.write_u8(match coll {
        Collection::Map => MARK_MAP,
        Collection::Set => MARK_SET,
    });
    h.write_u64(crate::object::to_u64(root.count()));
    // Entries are borrowed from the root, which the caller keeps alive
    // for the whole walk.
    h.push_unordered(Entries::new(root, coll));
}
