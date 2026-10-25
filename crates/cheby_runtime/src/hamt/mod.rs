//! Hash array mapped tries for `std::map` and `std::set` (F4, D-039,
//! D-093, D-538): persistent CHAMP tries over uniform keys and values,
//! keys hashed with SipHash-1-3 through `generic::hash_with_seed` and
//! compared with `generic::equal` (§3.13, D-281).
//!
//! - [`node`]: the objects (layout ids 32-37), their `CustomKind`s and the
//!   static empty map and set.
//! - [`ops`]: lookup, insertion, removal, `upsert`'s single lookup,
//!   iteration and building, in place along the path where nodes are
//!   unique (§9.3).
//! - [`compare`]: key hashing and equality, and the order-independent
//!   equality and hashing of whole maps and sets.
//! - [`kernels`]: the `std::map` and `std::set` kernels (D-484).
//!
//! **Seeds** (plan §4.3.5, D-538). Every non-empty root records the seed
//! it was built with. `from_list` and an insertion into an empty
//! collection take the current entry's seed ([`entry::current_seed`]);
//! every other operation uses the root's own, so a map built under one
//! seed is read and extended correctly under any other (constants, plan
//! §7.5). The empty map and set are statics with no seed.
//!
//! **For other runtime modules and hosts**: [`for_each_entry`] (W3-B's
//! sorted printing, W4-C's range walk), [`rebuild_with_seed`],
//! [`reseed_in_place`] (constant evaluation's rebuild, D-196, D-539),
//! [`seed_of`], [`collection_of`] and [`size`].
//!
//! [`entry::current_seed`]: crate::entry::current_seed

mod compare;
pub(crate) mod kernels;
mod node;
mod ops;

#[cfg(test)]
mod tests;

pub use kernels::{
    cheby_map_from_list, cheby_map_get, cheby_map_has_key, cheby_map_insert, cheby_map_is_empty,
    cheby_map_keys, cheby_map_new, cheby_map_remove, cheby_map_size, cheby_map_to_list,
    cheby_map_upsert, cheby_map_values, cheby_set_contains, cheby_set_from_list, cheby_set_insert,
    cheby_set_is_empty, cheby_set_new, cheby_set_remove, cheby_set_size, cheby_set_to_list,
};
pub use node::{Collection, EMPTY_MAP, EMPTY_SET, StaticRoot};

use crate::abi::Word;
use crate::panic::fatal;
use crate::unwind::guard::DropOnUnwind;
use node::{Root, drop_word};
use ops::Entries;

/// Whether a borrowed word is a map or a set root, and which: checks the
/// layout id only.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
#[must_use]
pub unsafe fn collection_of(word: Word) -> Option<Collection> {
    // SAFETY: forwarded from the caller.
    unsafe { Root::probe(word) }.map(|(_, coll)| coll)
}

/// The root of a borrowed map or set, or `fatal`.
///
/// # Safety
///
/// `map` is a live map or set.
unsafe fn any_root(map: Word) -> (Root, Collection) {
    // SAFETY: forwarded from the caller.
    unsafe { Root::probe(map) }
        .unwrap_or_else(|| fatal(format_args!("expected a map or a set, found {map:#x}")))
}

/// Calls `f` for every entry of a borrowed map or set, in node order (the
/// order of `map::keys`), with the key and the value borrowed (`None` for
/// a set). Walks with an explicit stack.
///
/// # Safety
///
/// `map` is a live map or set, unchanged during the call.
pub unsafe fn for_each_entry(map: Word, f: &mut dyn FnMut(Word, Option<Word>)) {
    // SAFETY: forwarded from the caller.
    let (root, coll) = unsafe { any_root(map) };
    for (key, value) in Entries::new(root, coll) {
        f(key, value);
    }
}

/// The number of entries of a borrowed map or set. O(1).
///
/// # Safety
///
/// `map` is a live map or set.
#[must_use]
pub unsafe fn size(map: Word) -> usize {
    // SAFETY: forwarded from the caller.
    unsafe { any_root(map) }.0.count()
}

/// A map (every value `Some`) or a set (every value `None`) built under
/// `seed` from owned entries, in order, a repeated key keeping the first
/// key and the last value as `map::from_list` does (D-329): installing a
/// constant's image builds its maps and sets this way (D-979, D-196),
/// with no list of pairs and no second trie. Runs no Cheby code unless a
/// key's hashing reaches `@std_hooks`. O(n).
pub(crate) fn from_entries(
    coll: Collection,
    seed: u64,
    entries: impl IntoIterator<Item = (Word, Option<Word>)>,
) -> Word {
    node::register();
    ops::build(coll, seed, entries).word()
}

/// Consumes a map or set and returns an equal one built under `seed`
/// (D-196, plan §7.6): itself when it is empty or already has that seed,
/// otherwise a new trie of the same entries, whose keys and values gain a
/// reference each before the old one is released. Runs no Cheby code
/// unless a key's hashing reaches `@std_hooks`. O(n).
///
/// # Safety
///
/// `map` is a live map or set, owned by the caller and given up.
#[must_use]
pub unsafe fn rebuild_with_seed(map: Word, seed: u64) -> Word {
    // SAFETY: forwarded from the caller.
    let (root, coll) = unsafe { any_root(map) };
    if root.count() == 0 || root.seed() == seed {
        return map;
    }
    // A key's hash hook may raise while the new trie is built: `build`
    // releases what it holds, and the old map is still given up (D-697).
    let held = DropOnUnwind::new([map]);
    let entries = Entries::new(root, coll).map(|(key, value)| {
        node::dup_word(key);
        if let Some(value) = value {
            node::dup_word(value);
        }
        (key, value)
    });
    let rebuilt = ops::build(coll, seed, entries);
    held.disarm();
    drop_word(map);
    rebuilt.word()
}

/// Rebuilds a borrowed map or set under `seed` in place (D-196, D-539):
/// a new trie of the same entries, each gaining a reference, replaces the
/// root's trie, and the old trie is released, so every reference to the
/// root, shared or not, now sees the rebuilt collection. Nothing changes
/// when it is empty, static or already has that seed. Runs no Cheby code
/// unless a key's hashing reaches `@std_hooks`. O(n).
///
/// # Safety
///
/// `map` is a live map or set that no other thread uses and that nobody
/// iterates during the call.
pub unsafe fn reseed_in_place(map: Word, seed: u64) {
    // SAFETY: forwarded from the caller.
    let (root, coll) = unsafe { any_root(map) };
    let Some(old_node) = root.node() else {
        return;
    };
    if root.count() == 0 || root.seed() == seed {
        return;
    }
    let entries = Entries::new(root, coll).map(|(key, value)| {
        node::dup_word(key);
        if let Some(value) = value {
            node::dup_word(value);
        }
        (key, value)
    });
    let rebuilt = ops::build(coll, seed, entries);
    let (Some(new_node), true) = (rebuilt.node(), rebuilt.count() == root.count()) else {
        fatal(format_args!(
            "rebuilding a {coll:?} of {} entries gave {}",
            root.count(),
            rebuilt.count()
        ));
    };
    let old_seed = root.seed();
    root.put_node(new_node);
    root.set_seed(seed);
    // The fresh root, unique, now owns the old trie, which goes with it.
    rebuilt.put_node(old_node);
    rebuilt.set_seed(old_seed);
    drop_word(rebuilt.word());
}

/// The seed a borrowed map or set was built with; `None` when it is empty.
///
/// # Safety
///
/// `map` is a live map or set.
#[must_use]
pub unsafe fn seed_of(map: Word) -> Option<u64> {
    // SAFETY: forwarded from the caller.
    let (root, _) = unsafe { any_root(map) };
    (root.count() > 0).then(|| root.seed())
}

/// Appends every `std::map` and `std::set` kernel symbol and its address to
/// `out`, and registers the map and set layouts (32-37), so code that only
/// passes the empty map around finds them: `symbols()` calls this whenever
/// a JIT is built.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    node::register();
    out.extend(kernels::all());
}

/// Registers the map and set layouts (32-37) once per process, as
/// [`kernels`] does: an executable's start calls it through
/// `image::install`, since it never builds the symbol table (D-919).
pub(crate) fn register_kinds() {
    node::register();
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["map", "set"];
