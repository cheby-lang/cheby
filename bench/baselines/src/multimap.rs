//! `multimap`: on the persistent `Rc` tree of `wbt` (`rc`; the Cheby
//! module's index is the runtime's `Map`, here it is a tree), and on a
//! `HashMap<K, Vec<V>>` (`hash-vec`). Each function does what
//! `bench/runtime/collections/multimap` does and returns its checksum.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "the checksums stay below the modulus and the steps below the size, as in the Cheby \
              program, so nothing overflows an i64"
)]

use std::rc::Rc;

use crate::common::{FixedMap, MODULUS, int, mix, next};
use crate::sorted::{looked_values, multimap_keys};
use crate::wbt::Tree;

/// The checksum of the keys in order, each with its values.
fn fold_sorted<'a>(entries: impl Iterator<Item = (i64, &'a [i64])>) -> i64 {
    entries.fold(0, |total, (key, values)| {
        let head = (total * 31 + key * 7 + int(values.len())) % MODULUS;
        values
            .iter()
            .fold(head, |sum, value| (sum * 31 + value) % MODULUS)
    })
}

pub(crate) fn multimap_rc(n: i64) -> i64 {
    let keys = multimap_keys(n);
    let mut m: Tree<i64, Rc<Vec<i64>>> = Tree::Tip;
    let mut total = 0;
    let mut seed = 42;
    for i in 0..n {
        m = m.upsert(seed % keys, &mut |found| match found {
            Some(values) => {
                let mut grown = Vec::with_capacity(values.len() + 1);
                grown.extend_from_slice(values);
                grown.push(i);
                Rc::new(grown)
            }
            None => Rc::new(vec![i]),
        });
        total += 1;
        seed = next(seed);
    }
    let mut looked = 0;
    let mut seed = 7;
    for _ in 0..n / 2 {
        let found = m
            .get(&(seed % keys))
            .map_or(0, |values| looked_values(values));
        looked = (looked + found) % MODULUS;
        seed = next(seed);
    }
    let mut seed = 99;
    for _ in 0..n / 16 {
        let key = seed % keys;
        if let Some(values) = m.get(&key) {
            total -= int(values.len());
            m = m.remove(&key);
        }
        seed = next(seed);
    }
    let sorted = m.fold(Vec::new(), &mut |mut found, key, values| {
        found.push((*key, Rc::clone(values)));
        found
    });
    let folded = fold_sorted(sorted.iter().map(|(key, values)| (*key, values.as_slice())));
    mix(mix(mix(looked, m.size()), total), folded)
}

pub(crate) fn multimap_hash_vec(n: i64) -> i64 {
    let keys = multimap_keys(n);
    let mut m: FixedMap<i64, Vec<i64>> = FixedMap::default();
    let mut total = 0;
    let mut seed = 42;
    for i in 0..n {
        m.entry(seed % keys).or_default().push(i);
        total += 1;
        seed = next(seed);
    }
    let mut looked = 0;
    let mut seed = 7;
    for _ in 0..n / 2 {
        let found = m
            .get(&(seed % keys))
            .map_or(0, |values| looked_values(values));
        looked = (looked + found) % MODULUS;
        seed = next(seed);
    }
    let mut seed = 99;
    for _ in 0..n / 16 {
        if let Some(values) = m.remove(&(seed % keys)) {
            total -= int(values.len());
        }
        seed = next(seed);
    }
    let mut sorted: Vec<(&i64, &Vec<i64>)> = m.iter().collect();
    sorted.sort_unstable_by_key(|(key, _)| **key);
    let folded = fold_sorted(
        sorted
            .iter()
            .map(|(key, values)| (**key, values.as_slice())),
    );
    mix(mix(mix(looked, int(m.len())), total), folded)
}
