//! The `std::map` and `std::set` kernels (D-274, D-484): arguments
//! borrowed, results owned (D-187, D-455); `Int` is `i64` and `Bool` is
//! `bool`.
//!
//! `insert`, `remove` and `upsert` update the collection in place when its
//! count is exactly one: the `@external` wrapper owns that reference and
//! drops it after the call, so the kernel changes the root, adds a
//! reference and returns it (plan §6). Costs follow `docs/stdlib/map.md`
//! and `set.md` (D-300): `new`, `size` and `is_empty` O(1); lookups and
//! updates O(log n), at most 14 nodes deep; `from_list`, `keys`, `values`
//! and `to_list` O(n).

use super::node::{Collection, Root, dup_word};
use super::ops::{self, Entries};
use crate::abi::Word;
use crate::callback;
use crate::entry;
use crate::list;
use crate::values;

/// The map behind a borrowed word from generated code.
///
/// # Safety
///
/// `word` is a live map.
unsafe fn map(word: Word) -> Root {
    // SAFETY: forwarded from the caller.
    unsafe { Root::from_word(word, Collection::Map) }
}

/// The set behind a borrowed word.
///
/// # Safety
///
/// `word` is a live set.
unsafe fn set(word: Word) -> Root {
    // SAFETY: forwarded from the caller.
    unsafe { Root::from_word(word, Collection::Set) }
}

/// A size as an `Int`.
fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A list of `f` of each entry of a borrowed root, in node order.
fn listed(root: Root, coll: Collection, f: impl FnMut((Word, Option<Word>)) -> Word) -> Word {
    list::from_iter(Entries::new(root, coll).map(f))
}

/// Builds from a borrowed list of elements under the current seed.
///
/// # Safety
///
/// `items` is a live list whose elements `entry` accepts.
unsafe fn from_items(
    items: Word,
    coll: Collection,
    entry: impl Fn(Word) -> (Word, Option<Word>),
) -> Word {
    // SAFETY: forwarded from the caller; the iterator keeps the list alive.
    let elements = unsafe { list::iter(items) };
    ops::build(coll, entry::current_seed(), elements.map(entry)).word()
}

// ---------------------------------------------------------------------
// `std::map`

/// `map::new`. O(1).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_map_new() -> u64 {
    Root::empty(Collection::Map).word()
}

/// `map::from_list`: the last value wins for a repeated key (D-329), and
/// the first key stays. O(n) amortized, in place.
///
/// # Safety
///
/// `pairs` is a live list of pairs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_from_list(pairs: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    unsafe {
        from_items(pairs, Collection::Map, |pair| {
            // SAFETY: the elements are live pairs.
            let parts = values::read_tuple(pair);
            let [key, value] = *parts.as_slice() else {
                crate::panic::fatal(format_args!("map::from_list: not a pair"))
            };
            dup_word(key);
            dup_word(value);
            (key, Some(value))
        })
    }
}

/// `map::size`. O(1).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_size(m: u64) -> i64 {
    // SAFETY: forwarded from the caller.
    int(unsafe { map(m) }.count())
}

/// `map::is_empty`. O(1).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_is_empty(m: u64) -> bool {
    // SAFETY: forwarded from the caller.
    unsafe { map(m) }.count() == 0
}

/// `map::get`. O(log n).
///
/// # Safety
///
/// `m` is a live map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_get(m: u64, key: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    let found = ops::find(unsafe { map(m) }, Collection::Map, key);
    values::option(found.map(|found| {
        let value = found.value();
        dup_word(value);
        value
    }))
}

/// `map::has_key`. O(log n).
///
/// # Safety
///
/// `m` is a live map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_has_key(m: u64, key: u64) -> bool {
    // SAFETY: forwarded from the caller.
    ops::find(unsafe { map(m) }, Collection::Map, key).is_some()
}

/// `map::insert`. O(log n); in place at count one.
///
/// # Safety
///
/// `m` is a live map; `key` and `value` are live uniform words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_insert(m: u64, key: u64, value: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    ops::insert(unsafe { map(m) }, Collection::Map, key, Some(value)).word()
}

/// `map::remove`: unchanged when the key is absent. O(log n); in place at
/// count one.
///
/// # Safety
///
/// `m` is a live map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_remove(m: u64, key: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    ops::remove(unsafe { map(m) }, Collection::Map, key).word()
}

/// `map::upsert`: one lookup, then `update` called once through
/// `callback::call` (D-484, D-526) with `Some` of the stored value (moved
/// out when the path is this map's alone) or `None`, and its result
/// stored in place at count one (§9.3).
///
/// # Safety
///
/// `m` is a live map, `key` a live uniform word and `update` a live
/// closure of one parameter; called on an entry's thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_upsert(m: u64, key: u64, update: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    let root = unsafe { map(m) };
    ops::upsert(root, key, |previous| {
        // SAFETY: on the caller's entry thread; the closure is borrowed
        // and its argument given away.
        unsafe { callback::call(update, &[values::option(previous)]) }
    })
    .word()
}

/// `map::keys`, in node order. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_keys(m: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    listed(unsafe { map(m) }, Collection::Map, |(key, _)| {
        dup_word(key);
        key
    })
}

/// `map::values`, in the order of `keys`. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_values(m: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    listed(unsafe { map(m) }, Collection::Map, |(_, value)| {
        let value = value.unwrap_or(crate::object::nil());
        dup_word(value);
        value
    })
}

/// `map::to_list`: `(key, value)` pairs in the order of `keys`. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_to_list(m: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    listed(unsafe { map(m) }, Collection::Map, |(key, value)| {
        let value = value.unwrap_or(crate::object::nil());
        dup_word(key);
        dup_word(value);
        values::tuple(&[key, value])
    })
}

// ---------------------------------------------------------------------
// `std::set`

/// `set::new`. O(1).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_set_new() -> u64 {
    Root::empty(Collection::Set).word()
}

/// `set::from_list`: each element once, the first copy kept. O(n)
/// amortized, in place.
///
/// # Safety
///
/// `items` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_from_list(items: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    unsafe {
        from_items(items, Collection::Set, |item| {
            dup_word(item);
            (item, None)
        })
    }
}

/// `set::size`. O(1).
///
/// # Safety
///
/// `s` is a live set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_size(s: u64) -> i64 {
    // SAFETY: forwarded from the caller.
    int(unsafe { set(s) }.count())
}

/// `set::is_empty`. O(1).
///
/// # Safety
///
/// `s` is a live set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_is_empty(s: u64) -> bool {
    // SAFETY: forwarded from the caller.
    unsafe { set(s) }.count() == 0
}

/// `set::contains`. O(log n).
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_contains(s: u64, item: u64) -> bool {
    // SAFETY: forwarded from the caller.
    ops::find(unsafe { set(s) }, Collection::Set, item).is_some()
}

/// `set::insert`: unchanged when present. O(log n); in place at count one.
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_insert(s: u64, item: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    ops::insert(unsafe { set(s) }, Collection::Set, item, None).word()
}

/// `set::remove`: unchanged when absent. O(log n); in place at count one.
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_remove(s: u64, item: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    ops::remove(unsafe { set(s) }, Collection::Set, item).word()
}

/// `set::to_list`, in node order. O(n).
///
/// # Safety
///
/// `s` is a live set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_to_list(s: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    listed(unsafe { set(s) }, Collection::Set, |(item, _)| {
        dup_word(item);
        item
    })
}

/// The kernels, with their std symbols.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn all() -> [(&'static str, *const u8); 20] {
    [
        ("cheby_map_new", cheby_map_new as *const u8),
        ("cheby_map_from_list", cheby_map_from_list as *const u8),
        ("cheby_map_size", cheby_map_size as *const u8),
        ("cheby_map_is_empty", cheby_map_is_empty as *const u8),
        ("cheby_map_get", cheby_map_get as *const u8),
        ("cheby_map_has_key", cheby_map_has_key as *const u8),
        ("cheby_map_insert", cheby_map_insert as *const u8),
        ("cheby_map_remove", cheby_map_remove as *const u8),
        ("cheby_map_upsert", cheby_map_upsert as *const u8),
        ("cheby_map_keys", cheby_map_keys as *const u8),
        ("cheby_map_values", cheby_map_values as *const u8),
        ("cheby_map_to_list", cheby_map_to_list as *const u8),
        ("cheby_set_new", cheby_set_new as *const u8),
        ("cheby_set_from_list", cheby_set_from_list as *const u8),
        ("cheby_set_size", cheby_set_size as *const u8),
        ("cheby_set_is_empty", cheby_set_is_empty as *const u8),
        ("cheby_set_contains", cheby_set_contains as *const u8),
        ("cheby_set_insert", cheby_set_insert as *const u8),
        ("cheby_set_remove", cheby_set_remove as *const u8),
        ("cheby_set_to_list", cheby_set_to_list as *const u8),
    ]
}
