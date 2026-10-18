//! The ordered family: `ordered_map`, `ordered_set` and `ordered_multimap`.
//!
//! - `rc`: an index from each key to its place and value, and an order
//!   from each place to its key, both on the persistent `Rc` tree of
//!   `wbt`. `std/ordered_map.cheby` has the same two parts, with the
//!   runtime's `Map` as the index; here the index is a tree too, so every
//!   update copies its paths and nothing is updated in place.
//! - `hash-btree`: the same two parts, mutable: a `HashMap` index and a
//!   `BTreeMap` order.
//!
//! Each program function does what `bench/runtime/collections/<program>`
//! does and returns its checksum.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "the checksums stay below the modulus and the steps and places below the size, as in \
              the Cheby programs, so nothing overflows an i64"
)]

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::common::{FixedMap, KEY_SPACE, MODULUS, int, mix, next};
use crate::sorted::{looked_values, multimap_keys};
use crate::wbt::Tree;

/// The operations the ordered programs need, so each program is written
/// once for both variants.
trait Ordered<V>: Sized {
    fn empty() -> Self;
    fn size(&self) -> i64;
    fn get(&self, key: i64) -> Option<&V>;
    /// A present key keeps its place; a new key goes to the end.
    fn upsert(self, key: i64, update: &mut dyn FnMut(Option<&V>) -> V) -> Self;
    fn remove(self, key: i64) -> Self;
    /// The entries in insertion order.
    fn entries(&self) -> Vec<(i64, V)>;
    fn first(&self) -> Option<(i64, V)>;
    fn last(&self) -> Option<(i64, V)>;
    /// The entries for which `keep` returns true, in the same order.
    fn filter(&self, keep: &mut dyn FnMut(i64, &V) -> bool) -> Self;

    fn insert(self, key: i64, value: V) -> Self
    where
        V: Clone,
    {
        self.upsert(key, &mut |_| value.clone())
    }

    fn has_key(&self, key: i64) -> bool {
        self.get(key).is_some()
    }
}

/// The persistent variant.
struct RcOrdered<V> {
    index: Tree<i64, (i64, V)>,
    order: Tree<i64, i64>,
    next: i64,
}

impl<V: Clone> RcOrdered<V> {
    fn entry(&self, found: Option<(&i64, &i64)>) -> Option<(i64, V)> {
        let (_, key) = found?;
        let (_, value) = self.index.get(key)?;
        Some((*key, value.clone()))
    }
}

impl<V: Clone> Ordered<V> for RcOrdered<V> {
    fn empty() -> Self {
        RcOrdered {
            index: Tree::Tip,
            order: Tree::Tip,
            next: 0,
        }
    }

    fn size(&self) -> i64 {
        self.index.size()
    }

    fn get(&self, key: i64) -> Option<&V> {
        self.index.get(&key).map(|(_, value)| value)
    }

    fn upsert(self, key: i64, update: &mut dyn FnMut(Option<&V>) -> V) -> Self {
        if let Some((place, value)) = self.index.get(&key) {
            let place = *place;
            let changed = update(Some(value));
            RcOrdered {
                index: self.index.insert(key, (place, changed)),
                ..self
            }
        } else {
            let value = update(None);
            RcOrdered {
                index: self.index.insert(key, (self.next, value)),
                order: self.order.insert(self.next, key),
                next: self.next + 1,
            }
        }
    }

    fn remove(self, key: i64) -> Self {
        if let Some((place, _)) = self.index.get(&key) {
            RcOrdered {
                order: self.order.remove(place),
                index: self.index.remove(&key),
                next: self.next,
            }
        } else {
            self
        }
    }

    fn entries(&self) -> Vec<(i64, V)> {
        self.order.fold(Vec::new(), &mut |mut found, _, key| {
            if let Some((_, value)) = self.index.get(key) {
                found.push((*key, value.clone()));
            }
            found
        })
    }

    fn first(&self) -> Option<(i64, V)> {
        self.entry(self.order.first())
    }

    fn last(&self) -> Option<(i64, V)> {
        self.entry(self.order.last())
    }

    fn filter(&self, keep: &mut dyn FnMut(i64, &V) -> bool) -> Self {
        // As `ordered_map::filter`: a fold over the entries that removes
        // each one `keep` rejects from a copy of the map.
        let start = RcOrdered {
            index: self.index.clone(),
            order: self.order.clone(),
            next: self.next,
        };
        self.order
            .fold(start, &mut |kept, _, key| match self.index.get(key) {
                Some((_, value)) if !keep(*key, value) => kept.remove(*key),
                _ => kept,
            })
    }
}

/// The mutable variant.
struct HashOrdered<V> {
    index: FixedMap<i64, (i64, V)>,
    order: BTreeMap<i64, i64>,
    next: i64,
}

impl<V: Clone> HashOrdered<V> {
    fn entry(&self, found: Option<(&i64, &i64)>) -> Option<(i64, V)> {
        let (_, key) = found?;
        let (_, value) = self.index.get(key)?;
        Some((*key, value.clone()))
    }
}

impl<V: Clone> Ordered<V> for HashOrdered<V> {
    fn empty() -> Self {
        HashOrdered {
            index: FixedMap::default(),
            order: BTreeMap::new(),
            next: 0,
        }
    }

    fn size(&self) -> i64 {
        int(self.index.len())
    }

    fn get(&self, key: i64) -> Option<&V> {
        self.index.get(&key).map(|(_, value)| value)
    }

    fn upsert(mut self, key: i64, update: &mut dyn FnMut(Option<&V>) -> V) -> Self {
        if let Some((_, value)) = self.index.get_mut(&key) {
            let changed = update(Some(&*value));
            *value = changed;
        } else {
            let value = update(None);
            self.index.insert(key, (self.next, value));
            self.order.insert(self.next, key);
            self.next += 1;
        }
        self
    }

    fn remove(mut self, key: i64) -> Self {
        if let Some((place, _)) = self.index.remove(&key) {
            self.order.remove(&place);
        }
        self
    }

    fn entries(&self) -> Vec<(i64, V)> {
        self.order
            .values()
            .filter_map(|key| self.index.get(key).map(|(_, value)| (*key, value.clone())))
            .collect()
    }

    fn first(&self) -> Option<(i64, V)> {
        self.entry(self.order.first_key_value())
    }

    fn last(&self) -> Option<(i64, V)> {
        self.entry(self.order.last_key_value())
    }

    fn filter(&self, keep: &mut dyn FnMut(i64, &V) -> bool) -> Self {
        let mut kept = HashOrdered::empty();
        for (key, value) in self.entries() {
            if keep(key, &value) {
                kept = kept.insert(key, value);
            }
        }
        kept
    }
}

/// `ordered_map` on either variant.
fn ordered_map<M: Ordered<i64>>(n: i64) -> i64 {
    let mut m = M::empty();
    let mut seed = 42;
    for i in 0..n {
        m = m.insert(seed % KEY_SPACE, i);
        seed = next(seed);
    }
    let mut seed = 42;
    for count in (1..=n / 4).rev() {
        m = m.insert(seed % KEY_SPACE, n + count);
        seed = next(seed);
    }
    let mut seed = 42;
    for count in (1..=n / 4).rev() {
        let key = seed % KEY_SPACE;
        m = m.remove(key).insert(key, count);
        seed = next(next(seed));
    }
    let mut looked = 0;
    let mut seed = 7;
    for _ in 0..n {
        looked = (looked + m.get(seed % KEY_SPACE).copied().unwrap_or(1)) % MODULUS;
        seed = next(seed);
    }
    let folded = m.entries().iter().fold(0, |total, (key, value)| {
        (total * 31 + key * 7 + value) % MODULUS
    });
    let entry =
        |found: Option<(i64, i64)>| found.map_or(0, |(key, value)| (key * 7 + value) % MODULUS);
    let ends = mix(entry(m.first()), entry(m.last()));
    mix(mix(mix(looked, m.size()), ends), folded)
}

/// `ordered_set` on either variant: a map with `()` values whose insert
/// leaves a present element alone.
fn ordered_set<M: Ordered<()>>(n: i64) -> i64 {
    let universe = (n / 2).max(1);
    let mut s = M::empty();
    let mut seed = 42;
    for _ in 0..n {
        let item = seed % universe;
        if !s.has_key(item) {
            s = s.insert(item, ());
        }
        seed = next(seed);
    }
    let mut seed = 7;
    for _ in 0..n / 4 {
        s = s.remove(seed % universe);
        seed = next(seed);
    }
    let mut hits = 0;
    let mut seed = 99;
    for _ in 0..n {
        hits += i64::from(s.has_key(seed % universe));
        seed = next(seed);
    }
    let evens = s.filter(&mut |item, ()| item % 2 == 0);
    let folded = evens
        .entries()
        .iter()
        .fold(0, |total, (item, ())| (total * 31 + item) % MODULUS);
    let element = |found: Option<(i64, ())>| found.map_or(0, |(item, ())| item + 1);
    let ends = mix(element(s.first()), element(s.last()));
    let sizes = mix(mix(hits, s.size()), evens.size());
    mix(mix(sizes, ends), folded)
}

/// One more value under a key, for `ordered_multimap`'s inserts: the
/// key's list copied and extended, as a persistent list would be.
fn pushed(found: Option<&Rc<Vec<i64>>>, value: i64) -> Rc<Vec<i64>> {
    match found {
        Some(values) => {
            let mut grown = Vec::with_capacity(values.len() + 1);
            grown.extend_from_slice(values);
            grown.push(value);
            Rc::new(grown)
        }
        None => Rc::new(vec![value]),
    }
}

/// `ordered_multimap` on either variant.
fn ordered_multimap<M: Ordered<Rc<Vec<i64>>>>(n: i64, push: fn(M, i64, i64) -> M) -> i64 {
    let keys = multimap_keys(n);
    let mut m = M::empty();
    let mut total = 0;
    let mut seed = 42;
    for i in 0..n {
        m = push(m, seed % keys, i);
        total += 1;
        seed = next(seed);
    }
    let mut looked = 0;
    let mut seed = 7;
    for _ in 0..n / 2 {
        let found = m.get(seed % keys).map_or(0, |values| looked_values(values));
        looked = (looked + found) % MODULUS;
        seed = next(seed);
    }
    let mut seed = 99;
    for _ in 0..n / 16 {
        let key = seed % keys;
        if let Some(values) = m.get(key) {
            total -= int(values.len());
            m = m.remove(key);
        }
        seed = next(seed);
    }
    let mut seed = 99;
    for count in (1..=n / 16).rev() {
        m = push(m, seed % keys, n + count);
        total += 1;
        seed = next(seed);
    }
    let folded = m.entries().iter().fold(0, |sum, (key, values)| {
        values
            .iter()
            .fold(sum, |sum, value| (sum * 31 + key * 7 + value) % MODULUS)
    });
    let entry = |found: Option<(i64, Rc<Vec<i64>>)>| {
        found.map_or(0, |(key, values)| (key * 7 + int(values.len())) % MODULUS)
    };
    let ends = mix(entry(m.first()), entry(m.last()));
    let counts = mix(mix(looked, m.size()), total);
    mix(mix(counts, ends), folded)
}

pub(crate) fn ordered_map_rc(n: i64) -> i64 {
    ordered_map::<RcOrdered<i64>>(n)
}

pub(crate) fn ordered_map_hash_btree(n: i64) -> i64 {
    ordered_map::<HashOrdered<i64>>(n)
}

pub(crate) fn ordered_set_rc(n: i64) -> i64 {
    ordered_set::<RcOrdered<()>>(n)
}

pub(crate) fn ordered_set_hash_btree(n: i64) -> i64 {
    ordered_set::<HashOrdered<()>>(n)
}

pub(crate) fn ordered_multimap_rc(n: i64) -> i64 {
    ordered_multimap::<RcOrdered<Rc<Vec<i64>>>>(n, |m, key, value| {
        m.upsert(key, &mut |found| pushed(found, value))
    })
}

/// The mutable variant appends in place: its lists are never shared.
pub(crate) fn ordered_multimap_hash_btree(n: i64) -> i64 {
    ordered_multimap::<HashOrdered<Rc<Vec<i64>>>>(n, |mut m, key, value| {
        if let Some((_, values)) = m.index.get_mut(&key) {
            Rc::make_mut(values).push(value);
            m
        } else {
            m.insert(key, Rc::new(vec![value]))
        }
    })
}
