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

use super::node::{Collection, Root, drop_word, dup_word};
use super::ops::{self, Entries, Found};
use crate::abi::Word;
use crate::callback;
use crate::ctx::ExecCtx;
use crate::entry;
use crate::list;
use crate::unwind::guard::{DropOnUnwind, guarded};
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
    guarded(None, || unsafe {
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
    })
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
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let found = ops::find(unsafe { map(m) }, Collection::Map, key);
        let value = found.map(|found| {
            let value = found.value();
            dup_word(value);
            value
        });
        // The reference is released if the `Some` cannot be allocated.
        let held = DropOnUnwind::new([value.unwrap_or(0)]);
        let result = values::option(value);
        held.disarm();
        result
    })
}

/// `MapFind` (`map::get` whose `Option` generated code takes apart at
/// once): the map and the key borrowed; the address of the value stored
/// under `key`, or 0 when the key is absent. Generated code reads the
/// value there with [`cheby_rt_map_value_at`], or inline, while it holds
/// the map. The key is hashed and compared as by [`cheby_map_get`].
/// O(log n).
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block, `m` is a live
/// map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_find(ctx: *mut ExecCtx, m: u64, key: u64) -> u64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        ops::find(unsafe { map(m) }, Collection::Map, key).map_or(0, Found::value_address)
    })
}

/// `MapValueAt`: the value word at `address`, dup'd, so the result is
/// owned. Never unwinds.
///
/// # Safety
///
/// `address` is a nonzero result of [`cheby_rt_map_find`] of a map the
/// caller still holds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_value_at(address: u64) -> u64 {
    // SAFETY: forwarded from the caller: the slot of a live node.
    let value = unsafe { crate::object::read_field(crate::object::word_to_ptr(address), 0) };
    dup_word(value);
    value
}

/// `MapUpsert` (`map::upsert` at a direct call, whose callback generated
/// code calls): `m` owned, `key` borrowed. Returns a root of the caller's
/// own, unique or empty, with the path to the key's entry writable, and
/// writes to `out` the address of the entry's value slot, that address
/// plus 1 for a key that was absent and is now in with a placeholder
/// value, or 1 for an empty map, whose key is not hashed
/// (`ops::upsert_open`). The key is hashed and compared as by
/// [`cheby_map_upsert`]. `m` is consumed even when it raises. O(log n).
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block, `m` is a live map
/// owned by the caller and given up, `key` a live uniform word, and `out`
/// a writable word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_upsert(
    ctx: *mut ExecCtx,
    m: u64,
    key: u64,
    out: *mut u64,
) -> u64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (root, at) = ops::upsert_open(unsafe { map(m) }, key);
        // SAFETY: forwarded from the caller.
        unsafe { out.write(at) };
        root.word()
    })
}

/// `MapSlotTake`: the value word at `address`, moved out: the slot holds
/// the placeholder `HOLE` afterwards, a tagged word that a release of the
/// map skips. Generated code does this inline, where the release clean-up
/// takes the `Option` of [`cheby_rt_map_upsert_found`] apart. Never
/// unwinds.
///
/// # Safety
///
/// `address` is the address of a present key's value slot that
/// [`cheby_rt_map_upsert`] gave, of a map the caller holds uniquely.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_slot_take(address: u64) -> u64 {
    let slot = crate::object::word_to_ptr(address);
    // SAFETY: forwarded from the caller: the value slot of a live node
    // that only the caller's map reaches.
    unsafe {
        let value = crate::object::read_field(slot, 0);
        crate::object::write_field(slot, 0, super::node::HOLE);
        value
    }
}

/// `MapUpsertFound`: the `Option` that the callback of a direct
/// `map::upsert` gets, for an address that [`cheby_rt_map_upsert`] gave:
/// for a present key, `Some` of the stored value, moved out of its slot,
/// which holds the placeholder `HOLE` afterwards (a tagged word that a
/// release of the map skips); otherwise the static `None`. The value keeps
/// the count it had in the map, as the kernel's `Some` does. When the
/// `Some` cannot be allocated, the value is released and the slot keeps
/// the placeholder.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block, and `address` a
/// result of [`cheby_rt_map_upsert`] whose map the caller holds uniquely
/// and has not taken the value of yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_upsert_found(ctx: *mut ExecCtx, address: u64) -> u64 {
    if address & 1 != 0 {
        return values::none();
    }
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let old = unsafe { cheby_rt_map_slot_take(address) };
        let held = DropOnUnwind::new([old]);
        // SAFETY: `ctx` is null or the running fiber's control block.
        let some = unsafe { values::some_for(ctx, old) };
        held.disarm();
        some
    })
}

/// `MapUpsertStore`: the last step of a direct `map::upsert`: stores the
/// owned `value` in the slot at `address`, its lowest bit cleared, over
/// the placeholder there, and returns `m`. For an empty map, whose
/// address is 1, it is `map::insert`'s work instead, which hashes `key` as
/// the kernel does after its callback: a map of one entry, the key with a
/// reference added. `m` and `value` are owned and consumed even when it
/// raises. Generated code stores into a slot inline.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block; `m` is the map
/// and `address` a result of [`cheby_rt_map_upsert`] whose slot holds the
/// placeholder, `key` the key it was given and `value` a live uniform
/// word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_map_upsert_store(
    ctx: *mut ExecCtx,
    m: u64,
    address: u64,
    key: u64,
    value: u64,
) -> u64 {
    if address != ops::UPSERT_EMPTY {
        let slot = crate::object::word_to_ptr(address & !1);
        // SAFETY: forwarded from the caller: the value slot of a live node
        // that only the caller's map reaches.
        unsafe { crate::object::write_field(slot, 0, value) };
        return m;
    }
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([m, value]);
        // SAFETY: forwarded from the caller: `m` is the empty map.
        let inserted = ops::insert(unsafe { map(m) }, Collection::Map, key, Some(value));
        owned.disarm();
        drop_word(value);
        drop_word(m);
        inserted.word()
    })
}

/// `map::has_key`. O(log n).
///
/// # Safety
///
/// `m` is a live map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_has_key(m: u64, key: u64) -> bool {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::find(unsafe { map(m) }, Collection::Map, key).is_some()
    })
}

/// `map::insert`. O(log n); in place at count one.
///
/// # Safety
///
/// `m` is a live map; `key` and `value` are live uniform words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_insert(m: u64, key: u64, value: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::insert(unsafe { map(m) }, Collection::Map, key, Some(value)).word()
    })
}

/// `map::remove`: unchanged when the key is absent. O(log n); in place at
/// count one.
///
/// # Safety
///
/// `m` is a live map and `key` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_remove(m: u64, key: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::remove(unsafe { map(m) }, Collection::Map, key).word()
    })
}

/// `map::upsert`: one lookup, then `update` called once through
/// `callback::call` (D-484, D-526) with `Some` of the stored value (moved
/// out when the path is this map's alone) or `None`, and its result
/// stored in place at count one (§9.3). When `update` panics, a drop
/// guard removes the key's entry, whose value
/// was moved out, before the map is released (D-697, `ops::upsert`).
///
/// # Safety
///
/// `m` is a live map, `key` a live uniform word and `update` a live
/// closure of one parameter; called on an entry's thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_upsert(m: u64, key: u64, update: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let root = unsafe { map(m) };
        // One lookup of the running fiber serves the `Some` and the call:
        // the kernel runs on one fiber from its start to its end.
        let ctx = entry::current_ctx();
        ops::upsert(root, key, |previous| {
            // The moved-out value is released if its `Some` cannot be
            // allocated.
            let held = DropOnUnwind::new([previous.unwrap_or(0)]);
            let argument = match previous {
                // SAFETY: `ctx` is the fiber running on this thread, or none.
                Some(value) => unsafe {
                    values::some_for(ctx.unwrap_or(std::ptr::null_mut()), value)
                },
                None => values::none(),
            };
            held.disarm();
            // SAFETY: on the caller's entry thread, whose control block is
            // `ctx`; the closure is borrowed and its argument given away.
            unsafe { callback::call_in(ctx, update, &[argument]) }
        })
        .word()
    })
}

/// `map::keys`, in node order. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_keys(m: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        listed(unsafe { map(m) }, Collection::Map, |(key, _)| {
            dup_word(key);
            key
        })
    })
}

/// `map::values`, in the order of `keys`. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_values(m: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        listed(unsafe { map(m) }, Collection::Map, |(_, value)| {
            let value = value.unwrap_or_else(crate::object::nil);
            dup_word(value);
            value
        })
    })
}

/// `map::to_list`: `(key, value)` pairs in the order of `keys`. O(n).
///
/// # Safety
///
/// `m` is a live map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_map_to_list(m: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        listed(unsafe { map(m) }, Collection::Map, |(key, value)| {
            let value = value.unwrap_or_else(crate::object::nil);
            dup_word(key);
            dup_word(value);
            let held = DropOnUnwind::new([key, value]);
            let pair = values::tuple(&[key, value]);
            held.disarm();
            pair
        })
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
    guarded(None, || unsafe {
        from_items(items, Collection::Set, |item| {
            dup_word(item);
            (item, None)
        })
    })
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
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::find(unsafe { set(s) }, Collection::Set, item).is_some()
    })
}

/// `set::sys_get`, the private kernel behind `set::intersection`: the
/// stored element `==` to `item`, which may print differently from it,
/// as `-0.0` does from `0.0` (D-588). O(log n).
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_sys_get(s: u64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let found = ops::find(unsafe { set(s) }, Collection::Set, item);
        let stored = found.map(|found| {
            let stored = found.key();
            dup_word(stored);
            stored
        });
        // The reference is released if the `Some` cannot be allocated.
        let held = DropOnUnwind::new([stored.unwrap_or(0)]);
        let result = values::option(stored);
        held.disarm();
        result
    })
}

/// `set::insert`: unchanged when present. O(log n); in place at count one.
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_insert(s: u64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::insert(unsafe { set(s) }, Collection::Set, item, None).word()
    })
}

/// `set::remove`: unchanged when absent. O(log n); in place at count one.
///
/// # Safety
///
/// `s` is a live set and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_remove(s: u64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        ops::remove(unsafe { set(s) }, Collection::Set, item).word()
    })
}

/// `set::to_list`, in node order. O(n).
///
/// # Safety
///
/// `s` is a live set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_set_to_list(s: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        listed(unsafe { set(s) }, Collection::Set, |(item, _)| {
            dup_word(item);
            item
        })
    })
}

/// The kernels, with their std symbols.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn all() -> [(&'static str, *const u8); 21] {
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
        ("cheby_set_sys_get", cheby_set_sys_get as *const u8),
        ("cheby_set_insert", cheby_set_insert as *const u8),
        ("cheby_set_remove", cheby_set_remove as *const u8),
        ("cheby_set_to_list", cheby_set_to_list as *const u8),
    ]
}
