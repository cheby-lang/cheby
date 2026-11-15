//! A fiber's context bindings (D-1571, D-1572): a persistent map, in this
//! module's tries, from a key's identity to the value bound to it.
//!
//! The trie key is the key's identity word, the Cheby tuple
//! `(U64, String, String)` of its 64-bit hash, its constant's path and its
//! type's text. Its trie hash is element 0, read back from the stored word,
//! never `SipHash`, so the root's seed is unused. Keys are compared with the
//! stock `generic::equal`, which compares the hash first and then both
//! texts, D-1572's order, so a collision of two hashes is harmless.
//!
//! A map word here is a root of `Collection::Map`, or 0 for the empty
//! context, which a fiber's field holds; Cheby is handed the static empty
//! map instead (D-733). A bind never updates a root in place (I-D).

use super::node::{self, Collection, Root};
use super::ops;
use crate::abi::Word;
use crate::object::{read_field, word_to_ptr};
use crate::values;

/// The byte offset of a tuple's first element.
const FIRST_ELEMENT: usize = 8;

/// The hash a key's identity carries: its element 0. Allocates nothing,
/// and never raises.
///
/// # Safety
///
/// `identity` is a live identity tuple, borrowed.
pub(crate) unsafe fn identity_hash(identity: Word) -> u64 {
    // SAFETY: a live tuple of three elements, the first at offset 8; it is
    // a `U64`'s uniform word.
    unsafe { values::word_to_u64(read_field(word_to_ptr(identity), FIRST_ELEMENT)) }
}

/// The value bound to `identity` in `map`, borrowed, or `None`. It
/// allocates no object, but comparing `identity` with a stored key goes
/// through `generic::equal`, whose worklists are scratch memory, so it
/// may raise `out of memory` (D-642); it raises before it takes any
/// reference, so nothing is left to release. A kernel over it must not be
/// listed in `NEVER_RAISE_KERNELS` while it compares through
/// `generic::equal`.
///
/// # Safety
///
/// `map` is 0 or a live bindings map, and `identity` a live identity, both
/// borrowed.
pub(crate) unsafe fn lookup(map: Word, identity: Word) -> Option<Word> {
    if map == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    let root = unsafe { Root::from_word(map, Collection::Map) };
    // SAFETY: as above.
    let hash = unsafe { identity_hash(identity) };
    ops::find_hashed(root.node()?, Collection::Map, identity, hash).map(ops::Found::value)
}

/// `map` with `identity` bound to `value`, owned: a new root, which
/// replaces an older binding of the same identity, while `map` keeps what
/// it held (D-1571, I-D). `identity` and `value` are borrowed and gain a
/// reference each in the new map. Raises `out of memory` (D-642) with
/// nothing to release.
///
/// # Safety
///
/// As for [`lookup`]; `value` is a live value, borrowed.
pub(crate) unsafe fn bind(map: Word, identity: Word, value: Word) -> Word {
    let root = if map == 0 {
        Root::empty(Collection::Map)
    } else {
        // SAFETY: forwarded from the caller.
        unsafe { Root::from_word(map, Collection::Map) }
    };
    // SAFETY: forwarded from the caller.
    let hash = unsafe { identity_hash(identity) };
    let rehash = |stored: Word| {
        // SAFETY: a stored key of a bindings map is a live identity.
        unsafe { identity_hash(stored) }
    };
    ops::insert_hashed(root, Collection::Map, identity, hash, Some(value), &rehash).word()
}

/// The static empty map: the word Cheby holds for the empty context.
pub(crate) fn empty() -> Word {
    node::empty(Collection::Map)
}
