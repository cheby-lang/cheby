//! Tests of maps and sets: unit tests of every kernel, property tests
//! against `BTreeMap`/`BTreeSet` (testing.md §3, plan §6) with `Int`,
//! `String` and tuple keys and forced collisions, the multi-version state
//! machine, seeds (plan §4.3.5), order-independent equality and hashing,
//! `upsert` through a trampoline, deep freeing, and the costs of
//! `docs/stdlib/map.md`.
//!
//! Tests run with poisoning on (every `cfg(test)` build), and each one
//! checks that its thread's accounting domain returns to where it started.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "test arithmetic on small, known values"
)]

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::kernels::*;
use super::node::{BITS, Bitmap, Collection, Collision, MAX_SHIFT, Root, drop_word, dup_word};
use super::ops;
use super::{collection_of, for_each_entry, rebuild_with_seed, reseed_in_place, seed_of, size};
use crate::abi::{self, Word, layout as ids};
use crate::alloc::{self, Domain, live};
use crate::ctx::ExecCtx;
use crate::entry::{
    self, Callbacks, ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome, run_entry,
};
use crate::generic;
use crate::object::{self, to_u64, write_field};
use crate::string::{new_string, to_rust_string};
use crate::values;

// ---------------------------------------------------------------------
// Keys, values and models

/// A model key: the three key shapes the tests use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Int(i64),
    Str(String),
    Pair(i64, String),
}

/// The uniform word of a model key, owned. Large integers are boxed.
fn key_word(key: &Key) -> Word {
    match key {
        Key::Int(n) => values::int_word(*n),
        Key::Str(text) => new_string(text),
        Key::Pair(n, text) => values::tuple(&[values::int_word(*n), new_string(text)]),
    }
}

/// The model key of a borrowed word.
fn key_of(word: Word) -> Key {
    // SAFETY: test words are live keys.
    unsafe {
        match values::layout_of(word) {
            None | Some(ids::BOXED_INT) => Key::Int(values::word_to_i64(word)),
            Some(ids::STRING) => Key::Str(to_rust_string(word).unwrap()),
            Some(ids::TUPLE) => {
                let parts = values::read_tuple(word);
                Key::Pair(
                    values::word_to_i64(parts[0]),
                    to_rust_string(parts[1]).unwrap(),
                )
            }
            Some(other) => panic!("unexpected key layout {other}"),
        }
    }
}

/// The value word of model value `v`: a heap `String` for multiples of 3
/// (so ownership shows in the accounting), a tagged `Int` otherwise.
fn elem(v: i64) -> Word {
    if v % 3 == 0 {
        new_string(&v.to_string())
    } else {
        abi::tag_int(v).unwrap()
    }
}

/// A tagged `Int`, for values passed borrowed that need no release.
fn int(v: i64) -> Word {
    abi::tag_int(v).unwrap()
}

/// The model value of a value word.
fn value(word: Word) -> i64 {
    if abi::is_tagged_int(word) {
        return abi::untag_int(word);
    }
    // SAFETY: values are live strings.
    unsafe { to_rust_string(word) }.unwrap().parse().unwrap()
}

/// Key strategies: small or wide integers, strings and tuples.
fn key_strategy() -> impl Strategy<Value = Key> {
    prop_oneof![
        (-300..300_i64).prop_map(Key::Int),
        any::<i64>().prop_map(Key::Int),
        "[a-c]{0,3}".prop_map(Key::Str),
        ((-5..5_i64), "[xy]{0,2}").prop_map(|(n, s)| Key::Pair(n, s)),
    ]
}

/// The `i`th key of a family, for bulk tests.
fn nth_key(family: usize, i: i64) -> Key {
    match family {
        0 => Key::Int(i.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64)),
        1 => Key::Str(format!("key-{i}")),
        _ => Key::Pair(i % 97, format!("p{}", i / 97)),
    }
}

/// Runs `f` and checks that the domain's live count returns to its start.
fn balanced(f: impl FnOnce()) {
    let before = live();
    f();
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// Runs `f` with key hashes masked by `mask` (to force collisions).
fn with_mask<R>(mask: u64, f: impl FnOnce() -> R) -> R {
    let saved = crate::tls::set_hash_mask(mask);
    let result = f();
    crate::tls::set_hash_mask(saved);
    result
}

fn root(word: Word, coll: Collection) -> Root {
    // SAFETY: tests pass live maps and sets only.
    unsafe { Root::from_word(word, coll) }
}

/// What an `@external` wrapper does: owns the argument it passes, calls
/// the kernel with it borrowed, and drops it after the call.
fn as_wrapper(m: Word, call: impl FnOnce(Word) -> Word) -> Word {
    let result = call(m);
    drop_word(m);
    result
}

/// A map from model entries, built under `seed`, owned.
fn map_of(seed: u64, model: &BTreeMap<Key, i64>) -> Word {
    ops::build(
        Collection::Map,
        seed,
        model.iter().map(|(k, v)| (key_word(k), Some(elem(*v)))),
    )
    .word()
}

/// A set from model keys, built under `seed`, owned.
fn set_of(seed: u64, model: &BTreeSet<Key>) -> Word {
    ops::build(
        Collection::Set,
        seed,
        model.iter().map(|k| (key_word(k), None)),
    )
    .word()
}

// ---------------------------------------------------------------------
// Checking a trie against its model

/// The structural invariants of a root: counts, the CHAMP layout (each
/// entry in the slot its hash bits give, maps disjoint, nodes below the
/// top holding more than one entry), collision nodes only at the bottom
/// with two or more entries of one hash, and the stored seed's hashes.
fn check_trie(word: Word, coll: Collection) {
    let root = root(word, coll);
    let Some(top) = root.node() else {
        assert_eq!(root.count(), 0);
        assert_eq!(
            word,
            Root::empty(coll).word(),
            "the empty collection is static"
        );
        return;
    };
    assert!(root.count() > 0, "a heap root is never empty");
    let seed = root.seed();
    let stride = coll.stride();
    let mut total = 0;
    // (node, shift, the hash bits above this node, whether it is the top)
    let mut stack = vec![(top.0, 0_u32, 0_u64, true)];
    while let Some((node, shift, prefix, is_top)) = stack.pop() {
        if shift >= MAX_SHIFT {
            let leaf = Collision(node);
            assert!(
                leaf.len() >= 2,
                "a collision node holds two entries or more"
            );
            assert!(leaf.len() * stride <= leaf.cap());
            assert_eq!(leaf.hash(), prefix, "collision hash");
            for e in 0..leaf.len() {
                let key = leaf.get(e * stride);
                assert_eq!(super::compare::key_hash(key, seed), leaf.hash());
            }
            total += leaf.len();
            continue;
        }
        let here = Bitmap(node);
        let (data, nodes) = here.maps();
        assert_eq!(data & nodes, 0, "data and node maps overlap");
        assert!(here.used(coll) <= here.cap());
        if !is_top {
            assert!(
                here.node_len() > 0 || here.data_len() > 1,
                "a node below the top holds more than one entry"
            );
        }
        let mask = if shift == 0 { 0 } else { (1_u64 << shift) - 1 };
        let mut di = 0;
        let mut ni = 0;
        for bit_index in 0..32_u32 {
            let bit = 1_u32 << bit_index;
            let bits = prefix | (u64::from(bit_index).checked_shl(shift).unwrap_or(0));
            if data & bit != 0 {
                let key = here.get(di * stride);
                let hash = super::compare::key_hash(key, seed);
                assert_eq!(hash & mask, prefix & mask, "entry under the wrong prefix");
                assert_eq!(
                    (hash >> shift) & 31,
                    u64::from(bit_index),
                    "entry in the wrong bit"
                );
                di += 1;
                total += 1;
            }
            if nodes & bit != 0 {
                let child = here.child(coll, ni);
                stack.push((child, shift + BITS, bits, false));
                ni += 1;
            }
        }
    }
    assert_eq!(total, root.count(), "the root's count");
}

/// Checks a borrowed map against its model through the kernels.
fn check_map(m: Word, model: &BTreeMap<Key, i64>) {
    check_trie(m, Collection::Map);
    // SAFETY (every kernel call): live borrowed arguments.
    unsafe {
        assert_eq!(cheby_map_size(m), model.len() as i64);
        assert_eq!(cheby_map_is_empty(m), model.is_empty());
        for (k, v) in model {
            let key = key_word(k);
            let got = cheby_map_get(m, key);
            let inner = values::read_option(got).expect("a present key");
            assert_eq!(value(inner), *v, "get({k:?})");
            drop_word(got);
            assert!(cheby_map_has_key(m, key));
            drop_word(key);
        }
        let mut seen = Vec::new();
        for_each_entry(m, &mut |k, v| seen.push((key_of(k), value(v.unwrap()))));
        let mut sorted = seen.clone();
        sorted.sort();
        let expected: Vec<(Key, i64)> = model.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(sorted, expected, "entries");
        // keys, values and to_list follow for_each_entry's order.
        let keys = cheby_map_keys(m);
        let vals = cheby_map_values(m);
        let pairs = cheby_map_to_list(m);
        let keys_seen: Vec<Key> = crate::list::iter(keys).map(key_of).collect();
        let vals_seen: Vec<i64> = crate::list::iter(vals).map(value).collect();
        let pairs_seen: Vec<(Key, i64)> = crate::list::iter(pairs)
            .map(|pair| {
                let parts = values::read_tuple(pair);
                (key_of(parts[0]), value(parts[1]))
            })
            .collect();
        assert_eq!(
            keys_seen,
            seen.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
        );
        assert_eq!(vals_seen, seen.iter().map(|(_, v)| *v).collect::<Vec<_>>());
        assert_eq!(pairs_seen, seen);
        for list in [keys, vals, pairs] {
            drop_word(list);
        }
    }
}

/// Checks a borrowed set against its model through the kernels.
fn check_set(s: Word, model: &BTreeSet<Key>) {
    check_trie(s, Collection::Set);
    // SAFETY (every kernel call): live borrowed arguments.
    unsafe {
        assert_eq!(cheby_set_size(s), model.len() as i64);
        assert_eq!(cheby_set_is_empty(s), model.is_empty());
        for k in model {
            let key = key_word(k);
            assert!(cheby_set_contains(s, key), "contains({k:?})");
            drop_word(key);
        }
        let items = cheby_set_to_list(s);
        let mut seen: Vec<Key> = crate::list::iter(items).map(key_of).collect();
        drop_word(items);
        let mut walked = Vec::new();
        for_each_entry(s, &mut |k, v| {
            assert!(v.is_none());
            walked.push(key_of(k));
        });
        assert_eq!(seen, walked);
        seen.sort();
        assert_eq!(seen, model.iter().cloned().collect::<Vec<_>>());
    }
}

/// Checks that `key` is absent from a borrowed map.
fn check_absent(m: Word, key: &Key) {
    let word = key_word(key);
    // SAFETY: live borrowed arguments.
    unsafe {
        let got = cheby_map_get(m, word);
        assert_eq!(values::read_option(got), None, "{key:?} should be absent");
        assert!(!cheby_map_has_key(m, word));
    }
    drop_word(word);
}

// ---------------------------------------------------------------------
// Unit tests

#[test]
fn empty_collections_are_static_and_seedless() {
    balanced(|| {
        let m = cheby_map_new();
        let s = cheby_set_new();
        check_map(m, &BTreeMap::new());
        check_set(s, &BTreeSet::new());
        // SAFETY: live words.
        unsafe {
            assert_eq!(seed_of(m), None);
            assert_eq!(seed_of(s), None);
            assert_eq!(collection_of(m), Some(Collection::Map));
            assert_eq!(collection_of(s), Some(Collection::Set));
            assert_eq!(collection_of(abi::tag_int(3).unwrap()), None);
            assert_eq!(size(m), 0);
            // Lists of nothing are the static empty list.
            let empty = crate::list::from_iter([]);
            assert_eq!(cheby_map_from_list(empty), m);
            assert_eq!(cheby_set_from_list(empty), s);
            assert_eq!(cheby_map_keys(m), empty);
        }
        drop_word(m);
        drop_word(s);
    });
}

#[test]
fn every_map_kernel_on_small_maps() {
    balanced(|| {
        let ada = key_word(&Key::Str("ada".into()));
        let alan = key_word(&Key::Str("alan".into()));
        let grace = key_word(&Key::Str("grace".into()));
        // SAFETY (every kernel call): live borrowed arguments.
        unsafe {
            let pairs = crate::list::from_iter([
                values::tuple(&[new_string("pear"), elem(1)]),
                values::tuple(&[new_string("apple"), elem(2)]),
                values::tuple(&[new_string("pear"), elem(3)]),
            ]);
            let fruit = cheby_map_from_list(pairs);
            drop_word(pairs);
            let model: BTreeMap<Key, i64> =
                [(Key::Str("apple".into()), 2), (Key::Str("pear".into()), 3)].into();
            check_map(fruit, &model);
            drop_word(fruit);

            let m = cheby_map_new();
            let m = as_wrapper(m, |m| cheby_map_insert(m, ada, int(36)));
            let m = as_wrapper(m, |m| cheby_map_insert(m, alan, int(41)));
            let mut model: BTreeMap<Key, i64> =
                [(Key::Str("ada".into()), 36), (Key::Str("alan".into()), 41)].into();
            check_map(m, &model);
            check_absent(m, &Key::Str("grace".into()));
            // Replacing keeps the size.
            let m = as_wrapper(m, |m| cheby_map_insert(m, ada, int(37)));
            model.insert(Key::Str("ada".into()), 37);
            check_map(m, &model);
            // Removing an absent key returns the map itself.
            dup_word(m);
            let same = as_wrapper(m, |m| cheby_map_remove(m, grace));
            assert_eq!(same, m);
            drop_word(same);
            let m = as_wrapper(m, |m| cheby_map_remove(m, ada));
            model.remove(&Key::Str("ada".into()));
            check_map(m, &model);
            let m = as_wrapper(m, |m| cheby_map_remove(m, alan));
            assert_eq!(
                m,
                cheby_map_new(),
                "removing the last key gives the empty map"
            );
            check_map(m, &BTreeMap::new());
        }
        for word in [ada, alan, grace] {
            drop_word(word);
        }
    });
}

#[test]
fn every_set_kernel_on_small_sets() {
    balanced(|| {
        // SAFETY (every kernel call): live borrowed arguments.
        unsafe {
            let items = crate::list::from_iter([3, 1, 3, 2].map(|n| abi::tag_int(n).unwrap()));
            let digits = cheby_set_from_list(items);
            drop_word(items);
            let model: BTreeSet<Key> = [1, 2, 3].map(Key::Int).into();
            check_set(digits, &model);
            let one = abi::tag_int(1).unwrap();
            let five = abi::tag_int(5).unwrap();
            assert!(!cheby_set_contains(digits, five));
            // Inserting a present element and removing an absent one return
            // the set itself.
            dup_word(digits);
            let same = as_wrapper(digits, |s| cheby_set_insert(s, one));
            assert_eq!(same, digits);
            drop_word(same);
            dup_word(digits);
            let same = as_wrapper(digits, |s| cheby_set_remove(s, five));
            assert_eq!(same, digits);
            drop_word(same);
            let grown = as_wrapper(digits, |s| cheby_set_insert(s, five));
            let mut model = model;
            model.insert(Key::Int(5));
            check_set(grown, &model);
            let mut s = grown;
            for n in [1, 2, 3, 5] {
                s = as_wrapper(s, |s| cheby_set_remove(s, abi::tag_int(n).unwrap()));
            }
            assert_eq!(s, cheby_set_new());
        }
    });
}

#[test]
fn present_keys_keep_the_stored_key() {
    balanced(|| {
        // -0.0 == 0.0 (§3.3.2): the first key stays, the last value wins.
        let zero = values::f64_word(0.0);
        let negative = values::f64_word(-0.0);
        // SAFETY: live borrowed arguments.
        unsafe {
            let m = cheby_map_insert(cheby_map_new(), zero, int(1));
            let m = as_wrapper(m, |m| cheby_map_insert(m, negative, int(2)));
            let keys = cheby_map_keys(m);
            let stored = crate::list::iter(keys).next().unwrap();
            assert_eq!(values::word_to_f64(stored).to_bits(), 0.0_f64.to_bits());
            drop_word(keys);
            assert_eq!(cheby_map_size(m), 1);
            let got = cheby_map_get(m, negative);
            assert_eq!(values::read_option(got).map(value), Some(2));
            drop_word(got);
            drop_word(m);
        }
        drop_word(zero);
        drop_word(negative);
    });
}

#[test]
fn kernels_update_in_place_at_count_one_and_copy_when_shared() {
    balanced(|| {
        let model: BTreeMap<Key, i64> = (0..200).map(|i| (Key::Int(i), i)).collect();
        let m = map_of(7, &model);
        let k = abi::tag_int(5).unwrap();
        let fresh = abi::tag_int(1_000).unwrap();
        // SAFETY (every kernel call): live borrowed arguments.
        unsafe {
            // Unique: the same root comes back, and nothing new is
            // allocated for a replacement.
            let before = live();
            let m2 = as_wrapper(m, |m| cheby_map_insert(m, k, int(50)));
            assert_eq!(m2, m, "in place");
            assert_eq!(live(), before, "a replacement in place allocates nothing");
            let m3 = as_wrapper(m2, |m| cheby_map_insert(m, fresh, int(1)));
            assert_eq!(m3, m);
            let m4 = as_wrapper(m3, |m| cheby_map_remove(m, fresh));
            assert_eq!(m4, m);
            let mut expected = model.clone();
            expected.insert(Key::Int(5), 50);
            check_map(m4, &expected);
            // Shared: a new root, and the old version is untouched.
            dup_word(m4);
            let other = as_wrapper(m4, |m| cheby_map_insert(m, fresh, int(2)));
            assert_ne!(other, m4);
            check_map(m4, &expected);
            let mut other_model = expected.clone();
            other_model.insert(Key::Int(1_000), 2);
            check_map(other, &other_model);
            dup_word(m4);
            let removed = as_wrapper(m4, |m| cheby_map_remove(m, k));
            check_map(m4, &expected);
            expected.remove(&Key::Int(5));
            check_map(removed, &expected);
            for word in [m4, other, removed] {
                drop_word(word);
            }
        }
    });
}

#[test]
fn a_unique_root_over_shared_nodes_copies_only_the_path() {
    balanced(|| {
        let model: BTreeMap<Key, i64> = (0..5_000).map(|i| (Key::Int(i), i)).collect();
        let a = map_of(11, &model);
        // `b` is a second root sharing `a`'s top node.
        let b = {
            let ra = root(a, Collection::Map);
            let top = ra.node().unwrap();
            dup_word(top.0);
            Root::alloc(Collection::Map, ra.count(), ra.seed(), top).word()
        };
        let before = live();
        // SAFETY: live borrowed arguments.
        let b2 = unsafe { as_wrapper(b, |m| cheby_map_insert(m, int(3), int(9))) };
        assert_eq!(b2, b, "the unique root is reused");
        let copied = live() - before;
        assert!(copied <= 14, "copied {copied} nodes for one path");
        check_map(a, &model);
        let mut changed = model.clone();
        changed.insert(Key::Int(3), 9);
        check_map(b2, &changed);
        drop_word(a);
        drop_word(b2);
    });
}

// ---------------------------------------------------------------------
// Bulk tests: 100,000 entries per key shape

fn bulk(family: usize, n: i64, mask: u64) {
    with_mask(mask, || {
        balanced(|| {
            let mut model = BTreeMap::new();
            // Built in place, through the unique-root kernel path.
            let mut m = cheby_map_new();
            for i in 0..n {
                let k = nth_key(family, i);
                let key = key_word(&k);
                let v = elem(i);
                // SAFETY: live borrowed arguments.
                m = as_wrapper(m, |m| unsafe { cheby_map_insert(m, key, v) });
                drop_word(key);
                drop_word(v);
                model.insert(k, i);
            }
            check_map(m, &model);
            // from_list over to_list gives an equal map.
            // SAFETY: live borrowed arguments.
            unsafe {
                let pairs = cheby_map_to_list(m);
                let again = cheby_map_from_list(pairs);
                drop_word(pairs);
                assert_eq!(generic::equal(m, again), Ok(true));
                drop_word(again);
            }
            // Remove every other key, in place.
            for i in (0..n).step_by(2) {
                let k = nth_key(family, i);
                let key = key_word(&k);
                // SAFETY: live borrowed arguments.
                m = as_wrapper(m, |m| unsafe { cheby_map_remove(m, key) });
                drop_word(key);
                model.remove(&k);
            }
            check_map(m, &model);
            check_absent(m, &nth_key(family, 0));
            // A set of the same keys, through the kernel, from `map::keys`.
            let keys: BTreeSet<Key> = model.keys().cloned().collect();
            // SAFETY: live borrowed arguments.
            let s = unsafe {
                let listed = cheby_map_keys(m);
                as_wrapper(listed, |listed| cheby_set_from_list(listed))
            };
            check_set(s, &keys);
            drop_word(s);
            drop_word(m);
        });
    });
}

#[test]
fn a_hundred_thousand_int_keys() {
    bulk(0, 100_000, u64::MAX);
}

#[test]
fn a_hundred_thousand_string_keys() {
    bulk(1, 100_000, u64::MAX);
}

#[test]
fn a_hundred_thousand_tuple_keys() {
    bulk(2, 100_000, u64::MAX);
}

#[test]
fn colliding_hashes_share_collision_nodes() {
    // Sixteen distinct hashes for 3,000 keys: every key lives in a
    // collision node of ~190 entries under a chain of single-child nodes.
    bulk(0, 3_000, 0xF);
    bulk(1, 2_000, 0xF << 60);
    // Every key has the same hash.
    bulk(2, 300, 0);
}

// ---------------------------------------------------------------------
// Property tests against BTreeMap and BTreeSet

#[derive(Debug, Clone)]
enum Op {
    Insert(Key, i64),
    Remove(Key),
    Upsert(Key, i64),
    /// Insert through a shared reference: the old version must survive.
    SharedInsert(Key, i64),
    SharedRemove(Key),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (key_strategy(), -1_000..1_000_i64).prop_map(|(k, v)| Op::Insert(k, v)),
        3 => key_strategy().prop_map(Op::Remove),
        2 => (key_strategy(), -1_000..1_000_i64).prop_map(|(k, v)| Op::Upsert(k, v)),
        1 => (key_strategy(), -1_000..1_000_i64).prop_map(|(k, v)| Op::SharedInsert(k, v)),
        1 => key_strategy().prop_map(Op::SharedRemove),
    ]
}

/// The update `upsert` applies in the tests: adds `v` to the old value,
/// or stores `v`. Takes and returns owned words.
fn bump(previous: Option<Word>, v: i64) -> Word {
    match previous {
        Some(old) => {
            let total = value(old) + v;
            drop_word(old);
            elem(total)
        }
        None => elem(v),
    }
}

/// Applies one operation to an owned map and its model; returns the new
/// map, owned.
fn apply_map(m: Word, model: &mut BTreeMap<Key, i64>, op: &Op) -> Word {
    let (k, shared) = match op {
        Op::Insert(k, _) | Op::Remove(k) | Op::Upsert(k, _) => (k, false),
        Op::SharedInsert(k, _) | Op::SharedRemove(k) => (k, true),
    };
    let key = key_word(k);
    let before = model.clone();
    if shared {
        dup_word(m);
    }
    let result = match *op {
        Op::Insert(_, v) | Op::SharedInsert(_, v) => {
            model.insert(k.clone(), v);
            let v = elem(v);
            // SAFETY: live borrowed arguments.
            let result = as_wrapper(m, |m| unsafe { cheby_map_insert(m, key, v) });
            drop_word(v);
            result
        }
        Op::Remove(_) | Op::SharedRemove(_) => {
            model.remove(k);
            // SAFETY: live borrowed arguments.
            as_wrapper(m, |m| unsafe { cheby_map_remove(m, key) })
        }
        Op::Upsert(_, v) => {
            let next = model.get(k).map_or(v, |old| old + v);
            model.insert(k.clone(), next);
            let calls = Cell::new(0);
            let result = as_wrapper(m, |m| {
                ops::upsert(root(m, Collection::Map), key, |previous| {
                    calls.set(calls.get() + 1);
                    bump(previous, v)
                })
                .word()
            });
            assert_eq!(calls.get(), 1, "upsert calls its update once");
            result
        }
    };
    if shared {
        check_map(m, &before);
        drop_word(m);
    }
    drop_word(key);
    result
}

fn apply_set(s: Word, model: &mut BTreeSet<Key>, op: &Op) -> Word {
    let shared = matches!(op, Op::SharedInsert(..) | Op::SharedRemove(_));
    let before = model.clone();
    if shared {
        dup_word(s);
    }
    let result = match op {
        Op::Insert(k, _) | Op::Upsert(k, _) | Op::SharedInsert(k, _) => {
            model.insert(k.clone());
            let key = key_word(k);
            // SAFETY: live borrowed arguments.
            let result = as_wrapper(s, |s| unsafe { cheby_set_insert(s, key) });
            drop_word(key);
            result
        }
        Op::Remove(k) | Op::SharedRemove(k) => {
            model.remove(k);
            let key = key_word(k);
            // SAFETY: live borrowed arguments.
            let result = as_wrapper(s, |s| unsafe { cheby_set_remove(s, key) });
            drop_word(key);
            result
        }
    };
    if shared {
        check_set(s, &before);
        drop_word(s);
    }
    result
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Random operations on a map and a set, each checked against its model
    /// after every step, with real hashes or forced collisions.
    #[test]
    fn operations_match_btree_models(
        start in prop::collection::btree_map(key_strategy(), -50..50_i64, 0..400),
        ops in prop::collection::vec(op(), 1..80),
        mask in prop_oneof![Just(u64::MAX), Just(0x7_u64), Just(0x3_u64 << 61), Just(0)],
    ) {
        with_mask(mask, || balanced(|| {
            let mut model = start.clone();
            let mut set_model: BTreeSet<Key> = start.keys().cloned().collect();
            let mut m = map_of(3, &model);
            let mut s = set_of(4, &set_model);
            check_map(m, &model);
            check_set(s, &set_model);
            for op in &ops {
                m = apply_map(m, &mut model, op);
                check_map(m, &model);
                s = apply_set(s, &mut set_model, op);
                check_set(s, &set_model);
            }
            drop_word(m);
            drop_word(s);
        }));
    }
}

// ---------------------------------------------------------------------
// The multi-version state machine (plan §6)

#[derive(Debug, Clone)]
enum Step {
    Share(usize),
    Drop(usize),
    Insert(usize, Key, i64),
    Remove(usize, Key),
    Upsert(usize, Key, i64),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => any::<usize>().prop_map(Step::Share),
        2 => any::<usize>().prop_map(Step::Drop),
        5 => (any::<usize>(), key_strategy(), -1_000..1_000_i64)
            .prop_map(|(i, k, v)| Step::Insert(i, k, v)),
        3 => (any::<usize>(), key_strategy()).prop_map(|(i, k)| Step::Remove(i, k)),
        3 => (any::<usize>(), key_strategy(), -1_000..1_000_i64)
            .prop_map(|(i, k, v)| Step::Upsert(i, k, v)),
    ]
}

/// The most versions kept live at once.
const VERSIONS: usize = 6;

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// N live versions under random sharing, dropping, insertion, removal
    /// and upsert, each checked against its own model after every step; the
    /// test's own accounting domain is back to zero at the end.
    #[test]
    fn versions_stay_independent(
        start in prop::collection::btree_map(key_strategy(), -50..50_i64, 0..300),
        steps in prop::collection::vec(step(), 1..60),
        mask in prop_oneof![Just(u64::MAX), Just(0x3_u64)],
    ) {
        let domain = Domain::new();
        alloc::with_domain(std::sync::Arc::clone(&domain), || with_mask(mask, || {
            let mut versions: Vec<(Word, BTreeMap<Key, i64>)> =
                vec![(map_of(9, &start), start.clone())];
            for step in &steps {
                let n = versions.len();
                match step {
                    Step::Share(i) if n < VERSIONS => {
                        let (m, model) = versions[i % n].clone();
                        dup_word(m);
                        versions.push((m, model));
                    }
                    Step::Drop(i) if n > 1 => {
                        let (m, _) = versions.swap_remove(i % n);
                        drop_word(m);
                    }
                    Step::Insert(i, k, v) => {
                        let (m, model) = &mut versions[i % n];
                        *m = apply_map(*m, model, &Op::Insert(k.clone(), *v));
                    }
                    Step::Remove(i, k) => {
                        let (m, model) = &mut versions[i % n];
                        *m = apply_map(*m, model, &Op::Remove(k.clone()));
                    }
                    Step::Upsert(i, k, v) => {
                        let (m, model) = &mut versions[i % n];
                        *m = apply_map(*m, model, &Op::Upsert(k.clone(), *v));
                    }
                    _ => {}
                }
                for (m, model) in &versions {
                    check_map(*m, model);
                }
            }
            for (m, _) in versions {
                drop_word(m);
            }
        }));
        prop_assert_eq!(domain.live_objects(), 0);
    }
}

// ---------------------------------------------------------------------
// Deep data

#[test]
fn freeing_a_million_nested_maps_uses_no_stack() {
    let length = if cfg!(miri) { 1_000 } else { 1_000_000 };
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let domain = Domain::new();
            alloc::with_domain(std::sync::Arc::clone(&domain), || {
                // Each map's only value is the previous map.
                let mut inner = cheby_map_new();
                for _ in 0..length {
                    inner = ops::build(
                        Collection::Map,
                        1,
                        [(abi::tag_int(0).unwrap(), Some(inner))],
                    )
                    .word();
                }
                assert_eq!(domain.live_objects(), 2 * length);
                drop_word(inner);
            });
            assert_eq!(domain.live_objects(), 0);
        })
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn walking_a_large_map_uses_no_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            balanced(|| {
                let m = ops::build(
                    Collection::Map,
                    2,
                    (0..200_000)
                        .map(|i| (abi::tag_int(i).unwrap(), Some(abi::tag_int(i).unwrap()))),
                );
                let mut total = 0_i64;
                // SAFETY: a live map.
                unsafe { for_each_entry(m.word(), &mut |k, _| total += abi::untag_int(k)) };
                assert_eq!(total, (0..200_000).sum::<i64>());
                drop_word(m.word());
            });
        })
        .unwrap();
    handle.join().unwrap();
}

/// A chain of `length` maps, each the only value of the next, under
/// `seed`, with the innermost value `last`; owned.
fn nested(length: usize, seed: u64, last: i64) -> Word {
    let mut inner = int(last);
    for _ in 0..length {
        inner = ops::build(Collection::Map, seed, [(int(0), Some(inner))]).word();
    }
    inner
}

#[test]
fn hashing_deeply_nested_collections_uses_no_stack() {
    // A map's entries are hashed as an unordered group on the caller's
    // stack, and a key's equality puts the key pair on the caller's
    // worklist, so neither values nor keys that hold collections nest Rust
    // frames (D-586).
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            balanced(|| {
                let length = if cfg!(miri) { 100 } else { 100_000 };
                let first = nested(length, 1, 5);
                let same = nested(length, 2, 5);
                let other = nested(length, 3, 6);
                // SAFETY: live maps.
                unsafe {
                    let hash = generic::hash_with_seed(first, 9);
                    assert!(hash.is_ok());
                    assert_eq!(hash, generic::hash_with_seed(same, 9));
                    assert_ne!(hash, generic::hash_with_seed(other, 9));
                }
                // Sets nested through their elements: each level hashes
                // the one below when it is built, so the depth is smaller.
                let depth = if cfg!(miri) { 20 } else { 3_000 };
                let sets = |seed: u64, last: i64| {
                    let mut inner = int(last);
                    for _ in 0..depth {
                        inner = ops::build(Collection::Set, seed, [(inner, None)]).word();
                    }
                    inner
                };
                let (bag, bag_again, bag_other) = (sets(4, 1), sets(5, 1), sets(6, 2));
                // SAFETY: live sets.
                unsafe {
                    assert_eq!(generic::equal(bag, bag_again), Ok(true));
                    assert_eq!(generic::equal(bag, bag_other), Ok(false));
                    assert_eq!(
                        generic::hash_with_seed(bag, 9),
                        generic::hash_with_seed(bag_again, 9)
                    );
                }
                for word in [first, same, other, bag, bag_again, bag_other] {
                    drop_word(word);
                }
            });
        })
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn equality_of_deeply_nested_maps_uses_no_stack() {
    // Values are compared on the generic worklist (`Cx::push`).
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            balanced(|| {
                let length = if cfg!(miri) { 100 } else { 100_000 };
                let a = nested(length, 1, 5);
                let b = nested(length, 2, 5);
                let c = nested(length, 3, 6);
                // SAFETY: live maps.
                unsafe {
                    assert_eq!(generic::equal(a, b), Ok(true));
                    assert_eq!(generic::equal(a, c), Ok(false));
                }
                for word in [a, b, c] {
                    drop_word(word);
                }
            });
        })
        .unwrap();
    handle.join().unwrap();
}

// ---------------------------------------------------------------------
// Seeds (plan §4.3.5, D-538)

#[test]
fn a_map_built_under_one_seed_is_read_extended_and_compared_under_another() {
    balanced(|| {
        let model: BTreeMap<Key, i64> = (0..2_000)
            .map(|i| (nth_key(i as usize % 3, i), i))
            .collect();
        let a = map_of(0xA, &model);
        // SAFETY: live words.
        unsafe {
            assert_eq!(seed_of(a), Some(0xA));
            // Operations on a non-empty map keep its seed, whatever the
            // current one (the process seed here).
            assert_ne!(entry::current_seed(), 0xA);
            check_map(a, &model);
            let k = key_word(&Key::Str("new".into()));
            dup_word(a);
            let extended = as_wrapper(a, |m| cheby_map_insert(m, k, int(1)));
            drop_word(k);
            assert_eq!(seed_of(extended), Some(0xA));
            let mut bigger = model.clone();
            bigger.insert(Key::Str("new".into()), 1);
            check_map(extended, &bigger);
            // The same entries under seed B: a different trie, equal and
            // with equal hashes under one caller key.
            let b = map_of(0xB, &model);
            assert_eq!(seed_of(b), Some(0xB));
            assert_eq!(generic::equal(a, b), Ok(true));
            assert_eq!(generic::equal(b, a), Ok(true));
            assert_eq!(generic::equal(a, extended), Ok(false));
            for key in [0, 1, 0xDEAD_BEEF] {
                assert_eq!(
                    generic::hash_with_seed(a, key),
                    generic::hash_with_seed(b, key)
                );
            }
            assert_ne!(
                generic::hash_with_seed(a, 5),
                generic::hash_with_seed(extended, 5)
            );
            // Rebuilding: an equal map with the new seed; the same seed or
            // an empty map come back as they are.
            dup_word(a);
            let rebuilt = rebuild_with_seed(a, 0xC);
            assert_ne!(rebuilt, a);
            assert_eq!(seed_of(rebuilt), Some(0xC));
            assert_eq!(generic::equal(rebuilt, a), Ok(true));
            check_map(rebuilt, &model);
            dup_word(rebuilt);
            assert_eq!(rebuild_with_seed(rebuilt, 0xC), rebuilt);
            drop_word(rebuilt);
            let empty = cheby_set_new();
            assert_eq!(rebuild_with_seed(empty, 9), empty);
            for word in [a, b, extended, rebuilt] {
                drop_word(word);
            }
        }
    });
}

/// D-196, D-539: rebuilding in place keeps the root's address, so every
/// reference sees the new seed and the same entries, and the old trie is
/// released (nothing leaks, nothing is freed twice).
#[test]
fn reseeding_in_place_keeps_the_root_and_its_entries() {
    balanced(|| {
        let model: BTreeMap<Key, i64> = (0..200).map(|i| (nth_key(i as usize % 3, i), i)).collect();
        let m = map_of(0xA, &model);
        // A second reference to the same root, and a map sharing its trie.
        dup_word(m);
        let shared = m;
        // SAFETY: live words.
        unsafe {
            let extended = cheby_map_insert(m, key_word(&Key::Int(-1)), int(1));
            reseed_in_place(m, 0xB);
            assert_eq!(seed_of(shared), Some(0xB));
            check_trie(m, Collection::Map);
            check_map(m, &model);
            // The map that shared the old trie is unchanged.
            assert_eq!(seed_of(extended), Some(0xA));
            assert_eq!(size(extended), 201);
            check_trie(extended, Collection::Map);
            // The same seed, an empty or a static collection: nothing.
            reseed_in_place(m, 0xB);
            assert_eq!(seed_of(m), Some(0xB));
            let empty = cheby_map_new();
            reseed_in_place(empty, 0xC);
            assert_eq!(seed_of(empty), None);
            for word in [m, shared, extended] {
                drop_word(word);
            }
        }
        let set = set_of(1, &(0..50).map(Key::Int).collect());
        // SAFETY: a live set.
        unsafe {
            reseed_in_place(set, 2);
            assert_eq!(seed_of(set), Some(2));
            check_set(set, &(0..50).map(Key::Int).collect());
        }
        drop_word(set);
    });
}

#[test]
fn sets_of_sets_are_looked_up_across_seeds() {
    balanced(|| {
        // Inner sets under different seeds; the outer set finds each by
        // value whatever seed the probe was built with.
        let inner =
            |seed: u64, items: &[i64]| set_of(seed, &items.iter().map(|&n| Key::Int(n)).collect());
        let outer = ops::build(
            Collection::Set,
            0x51,
            [inner(1, &[1, 2]), inner(2, &[3]), inner(3, &[])].map(|s| (s, None)),
        )
        .word();
        // SAFETY: live words.
        unsafe {
            for (seed, items) in [(7, &[2, 1][..]), (8, &[3][..]), (9, &[][..])] {
                let probe = inner(seed, items);
                assert!(cheby_set_contains(outer, probe), "{items:?}");
                drop_word(probe);
            }
            let probe = inner(10, &[1]);
            assert!(!cheby_set_contains(outer, probe));
            drop_word(probe);
            assert_eq!(size(outer), 3);
            // The outer set rebuilt under another seed is equal and still
            // finds its elements.
            dup_word(outer);
            let moved = rebuild_with_seed(outer, 0x99);
            assert_eq!(generic::equal(moved, outer), Ok(true));
            let probe = inner(11, &[1, 2]);
            assert!(cheby_set_contains(moved, probe));
            drop_word(probe);
            drop_word(moved);
        }
        drop_word(outer);
    });
}

#[test]
fn equality_and_hashing_ignore_order_and_compare_values() {
    balanced(|| {
        let forward: BTreeMap<Key, i64> = (0..500).map(|i| (Key::Int(i), i)).collect();
        // Built in two insertion orders under one seed, with collisions.
        with_mask(0xF, || {
            let a = ops::build(
                Collection::Map,
                1,
                forward.iter().map(|(k, v)| (key_word(k), Some(elem(*v)))),
            )
            .word();
            let b = ops::build(
                Collection::Map,
                1,
                forward
                    .iter()
                    .rev()
                    .map(|(k, v)| (key_word(k), Some(elem(*v)))),
            )
            .word();
            let mut changed = forward.clone();
            changed.insert(Key::Int(7), -7);
            let c = map_of(1, &changed);
            // SAFETY: live words.
            unsafe {
                assert_eq!(generic::equal(a, b), Ok(true));
                assert_eq!(generic::hash_with_seed(a, 3), generic::hash_with_seed(b, 3));
                assert_eq!(generic::equal(a, c), Ok(false), "a value differs");
                assert_ne!(generic::hash_with_seed(a, 3), generic::hash_with_seed(c, 3));
                let empty = cheby_map_new();
                assert_eq!(generic::equal(empty, cheby_map_new()), Ok(true));
                assert_eq!(generic::equal(a, empty), Ok(false));
                // Sets: the same keys.
                let ka: BTreeSet<Key> = forward.keys().cloned().collect();
                let s1 = set_of(4, &ka);
                let s2 = set_of(5, &ka);
                assert_eq!(generic::equal(s1, s2), Ok(true));
                assert_eq!(
                    generic::hash_with_seed(s1, 8),
                    generic::hash_with_seed(s2, 8)
                );
                for word in [a, b, c, s1, s2] {
                    drop_word(word);
                }
            }
        });
    });
}

/// A closure object for the tests: header, a dummy code word, then the
/// captures (owned).
fn closure(captures: &[Word]) -> Word {
    let count = captures.len() + 1;
    let obj = alloc::alloc_object(
        to_u64((count + 1) * 8),
        abi::header(1, u8::try_from(count).unwrap(), ids::CLOSURE),
    );
    // SAFETY: initializing a fresh closure.
    unsafe {
        write_field(obj, 8, 0x1000 | 1);
        for (i, capture) in captures.iter().enumerate() {
            write_field(obj, (i + 2) * 8, *capture);
        }
    }
    object::ptr_to_word(obj)
}

#[test]
fn a_function_inside_a_map_stops_equality_and_hashing() {
    balanced(|| {
        let f = closure(&[]);
        dup_word(f);
        let a = ops::build(Collection::Map, 1, [(abi::tag_int(1).unwrap(), Some(f))]).word();
        let b = ops::build(Collection::Map, 2, [(abi::tag_int(1).unwrap(), Some(f))]).word();
        // SAFETY: live words.
        unsafe {
            assert_eq!(generic::equal(a, b), Err(generic::Reached::Function));
            assert_eq!(
                generic::hash_with_seed(a, 1),
                Err(generic::Reached::Function)
            );
        }
        drop_word(a);
        drop_word(b);
    });
}

// ---------------------------------------------------------------------
// Entries: seeds of new maps, upsert through a trampoline

/// Bits of the result of the entry tests: a set bit is a failed check.
fn fail(mask: &mut u64, bit: u32, ok: bool) {
    if !ok {
        *mask |= 1 << bit;
    }
}

/// An entry under constant seed 0xA: `from_list` and an insertion into an
/// empty map take 0xA; under `with_seed(0xB)`, the 0xA map is read,
/// extended (keeping 0xA) and compared with a 0xB map of the same
/// entries.
unsafe extern "C" fn seeds_entry(_: *mut ExecCtx) -> u64 {
    let mut failed = 0;
    let pairs = crate::list::from_iter(
        (0..300).map(|i| values::tuple(&[values::int_word(i), values::int_word(i * 2)])),
    );
    // SAFETY (every kernel call): live borrowed arguments.
    unsafe {
        let a = cheby_map_from_list(pairs);
        fail(&mut failed, 0, seed_of(a) == Some(0xA));
        let one = cheby_map_insert(cheby_map_new(), values::int_word(1), values::int_word(1));
        fail(&mut failed, 1, seed_of(one) == Some(0xA));
        let set = cheby_set_insert(cheby_set_new(), values::int_word(1));
        fail(&mut failed, 2, seed_of(set) == Some(0xA));
        entry::with_seed(0xB, || {
            let b = cheby_map_from_list(pairs);
            fail(&mut failed, 3, seed_of(b) == Some(0xB));
            let got = cheby_map_get(a, values::int_word(150));
            fail(
                &mut failed,
                4,
                values::read_option(got).map(|w| values::word_to_i64(w)) == Some(300),
            );
            drop_word(got);
            dup_word(a);
            let extended = cheby_map_insert(a, values::int_word(1_000), values::int_word(0));
            drop_word(a);
            fail(&mut failed, 5, seed_of(extended) == Some(0xA));
            fail(
                &mut failed,
                6,
                cheby_map_has_key(extended, values::int_word(1_000)),
            );
            fail(&mut failed, 7, generic::equal(a, b) == Ok(true));
            fail(&mut failed, 8, generic::hash(a) == generic::hash(b));
            fail(&mut failed, 9, generic::equal(extended, b) == Ok(false));
            drop_word(b);
            drop_word(extended);
        });
        for word in [a, one, set, pairs] {
            drop_word(word);
        }
    }
    failed
}

#[test]
fn new_maps_take_the_current_entrys_seed() {
    let config = EntryConfig {
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
            seed: 0xA,
            ..ConstEvalLimits::default()
        }))
    };
    // SAFETY: a test thunk with the `EntryFn` ABI.
    let outcome = unsafe { run_entry(seeds_entry, &config) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(0), "failed checks (bits)");
    assert_eq!(config.domain.live_objects(), 0);
}

thread_local! {
    /// How often the test trampoline ran on this (entry) thread.
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

/// The trampoline of one-argument closures for the upsert tests: the
/// closure's first capture is the tagged step. `Some(n)` of an `Int`
/// gives `n + step`; `Some(list)` pushes `step` onto the list (in place
/// when the list is unique); `None` gives `step`.
unsafe extern "C" fn tramp1(_: *mut ExecCtx, closure: u64, arg: u64) -> u64 {
    CALLS.with(|calls| calls.set(calls.get() + 1));
    // SAFETY: closures built by `closure` with one capture; the argument
    // is an owned `Option`.
    unsafe {
        let step = object::read_field(object::word_to_ptr(closure), 16);
        match values::read_option(arg) {
            None => {
                drop_word(arg);
                step
            }
            Some(inner) => {
                dup_word(inner);
                drop_word(arg);
                if abi::is_tagged_int(inner) {
                    values::int_word(abi::untag_int(inner) + abi::untag_int(step))
                } else {
                    as_wrapper(inner, |xs| crate::list::cheby_list_push(xs, step))
                }
            }
        }
    }
}

/// An entry exercising `cheby_map_upsert`: counting words, a call per
/// upsert, in place on a unique map, the stored value moved (so a unique
/// list inside is pushed in place), and a shared map left alone.
unsafe extern "C" fn upsert_entry(_: *mut ExecCtx) -> u64 {
    let mut failed = 0;
    let bump = closure(&[abi::tag_int(1).unwrap()]);
    // SAFETY (every kernel call): live borrowed arguments.
    unsafe {
        let mut counts = cheby_map_new();
        for word in ["b", "a", "b"] {
            let key = new_string(word);
            counts = as_wrapper(counts, |m| cheby_map_upsert(m, key, bump));
            drop_word(key);
        }
        fail(&mut failed, 0, CALLS.with(Cell::get) == 3);
        let pairs = crate::list::from_iter([
            values::tuple(&[new_string("a"), values::int_word(1)]),
            values::tuple(&[new_string("b"), values::int_word(2)]),
        ]);
        let expected = as_wrapper(pairs, |pairs| cheby_map_from_list(pairs));
        fail(&mut failed, 1, generic::equal(counts, expected) == Ok(true));
        drop_word(expected);
        // In place: the same root comes back for a unique map.
        let key = new_string("a");
        let same = as_wrapper(counts, |m| cheby_map_upsert(m, key, bump));
        fail(&mut failed, 2, same == counts);
        // Shared: a new map; the old one keeps its value.
        dup_word(same);
        let other = as_wrapper(same, |m| cheby_map_upsert(m, key, bump));
        fail(&mut failed, 3, other != same);
        let old = cheby_map_get(same, key);
        let new = cheby_map_get(other, key);
        fail(
            &mut failed,
            4,
            values::read_option(old).map(|w| values::word_to_i64(w)) == Some(2),
        );
        fail(
            &mut failed,
            5,
            values::read_option(new).map(|w| values::word_to_i64(w)) == Some(3),
        );
        for word in [old, new, other, same, key] {
            drop_word(word);
        }
        // Map<String, List<Int>>: the list under a key is moved to the
        // closure, so a unique list grows in place.
        let push = closure(&[abi::tag_int(7).unwrap()]);
        let key = new_string("xs");
        let list = crate::list::from_iter([abi::tag_int(1).unwrap()]);
        let lists = cheby_map_insert(cheby_map_new(), key, list);
        drop_word(list);
        let lists = as_wrapper(lists, |m| cheby_map_upsert(m, key, push));
        let got = cheby_map_get(lists, key);
        let stored = values::read_option(got).unwrap_or(0);
        fail(&mut failed, 6, stored == list);
        fail(&mut failed, 7, crate::list::length(stored) == 2);
        for word in [got, lists, key, push] {
            drop_word(word);
        }
    }
    drop_word(bump);
    failed
}

#[test]
fn upsert_calls_its_closure_through_the_entrys_trampoline() {
    let config = EntryConfig {
        domain: Domain::new(),
        callbacks: Callbacks {
            t1: Some(tramp1),
            ..Callbacks::default()
        },
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: a test thunk with the `EntryFn` ABI and a trampoline with its
    // declared ABI.
    let outcome = unsafe { run_entry(upsert_entry, &config) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(0), "failed checks (bits)");
    assert_eq!(config.domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// Registration and costs

#[test]
fn kernels_are_registered_with_their_std_symbols() {
    let mut out = Vec::new();
    super::kernels(&mut out);
    let names: BTreeSet<&str> = out.iter().map(|(name, _)| *name).collect();
    assert_eq!(names.len(), 20);
    for module in ["map", "set"] {
        let source = std::fs::read_to_string(format!(
            "{}/../../std/{module}.cheby",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("@external(native, \"") {
                let symbol = rest.split('"').next().unwrap();
                assert!(names.contains(symbol), "{symbol} is not registered");
            }
        }
    }
    assert_eq!(super::COMPLETE, &["map", "set"]);
}

#[test]
fn cost_lookups_allocate_nothing_and_updates_copy_one_path() {
    balanced(|| {
        let n = 100_000;
        let model: BTreeMap<Key, i64> = (0..n).map(|i| (Key::Int(i), i)).collect();
        let m = map_of(21, &model);
        // Unique building allocates O(n) objects: at most one node per
        // entry plus the root.
        assert!(live() <= n * 2 + 1);
        let before = live();
        // SAFETY: live borrowed arguments.
        unsafe {
            for i in (0..n).step_by(97) {
                assert!(cheby_map_has_key(m, abi::tag_int(i).unwrap()));
            }
            assert_eq!(live(), before, "lookups allocate nothing");
            // A shared insertion copies the root and one path (at most 14
            // nodes deep: 13 bitmap levels and a collision node).
            dup_word(m);
            let other = as_wrapper(m, |m| cheby_map_insert(m, int(n), int(0)));
            let copied = live() - before;
            assert!(copied <= 16, "a shared insert allocated {copied} objects");
            drop_word(other);
            // The trie stays shallow: an entry sits at depth log32(n), about
            // 3.3 for 100,000 on average; the deepest pair of keys shares
            // about log2(n^2) bits, so 7 or 8 levels.
            let (mut depth, mut weighted) = (0, 0);
            let mut stack = vec![(root(m, Collection::Map).node().unwrap().0, 1)];
            while let Some((node, d)) = stack.pop() {
                depth = depth.max(d);
                let here = Bitmap(node);
                weighted += here.data_len() * d;
                for ni in 0..here.node_len() {
                    stack.push((here.child(Collection::Map, ni), d + 1));
                }
            }
            let average = weighted as f64 / n as f64;
            assert!(average < 4.5, "average depth {average}");
            assert!(depth <= 10, "depth {depth}");
        }
        drop_word(m);
    });
}

/// Measured costs for the report: `cargo test -p cheby_runtime --release
/// -- --ignored --nocapture hamt::tests::measure`.
#[test]
#[ignore = "a measurement, not a check"]
fn measure() {
    use std::time::Instant;
    for n in [1_000_i64, 100_000, 1_000_000] {
        let per = |start: Instant, count: i64| start.elapsed().as_nanos() as f64 / count as f64;
        let t = Instant::now();
        let mut m = cheby_map_new();
        for i in 0..n {
            // SAFETY: live borrowed arguments.
            m = as_wrapper(m, |m| unsafe {
                cheby_map_insert(m, abi::tag_int(i).unwrap(), abi::tag_int(i).unwrap())
            });
        }
        let insert_unique = per(t, n);
        let t = Instant::now();
        for i in 0..n {
            // SAFETY: as above.
            assert!(unsafe { cheby_map_has_key(m, abi::tag_int(i).unwrap()) });
        }
        let lookup = per(t, n);
        let rounds = n.min(100_000);
        let t = Instant::now();
        for i in 0..rounds {
            dup_word(m);
            // SAFETY: as above.
            let other = as_wrapper(m, |m| unsafe {
                cheby_map_insert(m, abi::tag_int(i).unwrap(), abi::tag_int(0).unwrap())
            });
            drop_word(other);
        }
        let insert_shared = per(t, rounds);
        let t = Instant::now();
        let keys: Vec<Word> = (0..n).map(|i| new_string(&format!("k{i}"))).collect();
        let s = ops::build(Collection::Set, 3, keys.iter().map(|&k| (k, None)));
        let strings = per(t, n);
        let t = Instant::now();
        // SAFETY: a live map.
        unsafe { for_each_entry(m, &mut |_, _| {}) };
        let iterate = per(t, n);
        println!(
            "n={n}: insert unique {insert_unique:.0} ns, lookup {lookup:.0} ns, insert shared {insert_shared:.0} ns, build string set {strings:.0} ns, iterate {iterate:.1} ns"
        );
        drop_word(s.word());
        drop_word(m);
    }
}
