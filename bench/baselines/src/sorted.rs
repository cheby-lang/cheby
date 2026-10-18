//! The sorted family: `sorted_map`, `sorted_set` and `sorted_multimap`,
//! each on the `Rc` tree of `wbt` (`rc`) and on std's B-trees (`btree`).
//! Each function does what `bench/runtime/collections/<program>` does and
//! returns its checksum.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "the checksums stay below the modulus and the steps below the size, as in the Cheby \
              programs, so nothing overflows an i64"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::common::{KEY_SPACE, MODULUS, int, mix, next};
use crate::wbt::Tree;

/// `sorted_map` on the `Rc` tree.
pub(crate) fn sorted_map_rc(n: i64) -> i64 {
    let mut built = Tree::Tip;
    let mut seed = 42;
    for i in 0..n {
        built = built.insert(seed % KEY_SPACE, i);
        seed = next(seed);
    }
    let mut looked = 0;
    let mut seed = 42;
    for _ in 0..n {
        looked = (looked + built.get(&(seed % KEY_SPACE)).copied().unwrap_or(1)) % MODULUS;
        seed = next(seed);
    }
    let mut thinned = built;
    let mut seed = 42;
    for _ in 0..n / 2 {
        thinned = thinned.remove(&(seed % KEY_SPACE));
        seed = next(next(seed));
    }
    let evens = thinned.filter(&mut |_, value| value % 2 == 0);
    let summed = evens.fold(0, &mut |total, key, value| {
        (total + key * 7 + value) % MODULUS
    });
    let mut ranged = Vec::new();
    thinned.range(&250_000, &260_000, &mut ranged);
    mix(
        mix(mix(mix(0, looked), thinned.size()), summed),
        int(ranged.len()),
    )
}

/// `sorted_map` on std's `BTreeMap`.
pub(crate) fn sorted_map_btree(n: i64) -> i64 {
    let mut thinned = BTreeMap::new();
    let mut seed = 42;
    for i in 0..n {
        thinned.insert(seed % KEY_SPACE, i);
        seed = next(seed);
    }
    let mut looked = 0;
    let mut seed = 42;
    for _ in 0..n {
        looked = (looked + thinned.get(&(seed % KEY_SPACE)).copied().unwrap_or(1)) % MODULUS;
        seed = next(seed);
    }
    let mut seed = 42;
    for _ in 0..n / 2 {
        thinned.remove(&(seed % KEY_SPACE));
        seed = next(next(seed));
    }
    let evens: BTreeMap<i64, i64> = thinned
        .iter()
        .filter(|(_, value)| *value % 2 == 0)
        .map(|(key, value)| (*key, *value))
        .collect();
    let summed = evens
        .iter()
        .fold(0, |total, (key, value)| (total + key * 7 + value) % MODULUS);
    let ranged = thinned.range(250_000..260_000).count();
    mix(
        mix(mix(mix(0, looked), int(thinned.len())), summed),
        int(ranged),
    )
}

/// `sorted_set` on the `Rc` tree: a set is a tree with `()` values, and
/// union, intersection, difference and subset are written as
/// `std/sorted_set.cheby` writes them.
pub(crate) fn sorted_set_rc(n: i64) -> i64 {
    let build = |count: i64, mut seed: i64| {
        let mut s: Tree<i64, ()> = Tree::Tip;
        for _ in 0..count {
            s = s.insert(seed % KEY_SPACE, ());
            seed = next(seed);
        }
        s
    };
    let first = build(n, 42);
    let second = build(n, 7);
    let mut hits = 0;
    let mut seed = 99;
    for _ in 0..n {
        hits += i64::from(first.has_key(&(seed % KEY_SPACE)));
        seed = next(seed);
    }
    let mut thinned = first;
    let mut seed = 42;
    for _ in 0..n / 2 {
        thinned = thinned.remove(&(seed % KEY_SPACE));
        seed = next(next(seed));
    }
    let both = thinned.filter(&mut |item, ()| second.has_key(item));
    let only = thinned.filter(&mut |item, ()| !second.has_key(item));
    let subset = both.size() <= second.size()
        && both
            .fold(Vec::new(), &mut |mut found, item, ()| {
                found.push(*item);
                found
            })
            .iter()
            .all(|item| second.has_key(item));
    let joined = second.fold(thinned, &mut |joined, item, ()| joined.insert(*item, ()));
    let folded = joined.fold(0, &mut |total, item, ()| (total * 31 + item) % MODULUS);
    let sizes = mix(mix(mix(hits, both.size()), only.size()), i64::from(subset));
    mix(mix(sizes, joined.size()), folded)
}

/// `sorted_set` on std's `BTreeSet`.
pub(crate) fn sorted_set_btree(n: i64) -> i64 {
    let build = |count: i64, mut seed: i64| {
        let mut s = BTreeSet::new();
        for _ in 0..count {
            s.insert(seed % KEY_SPACE);
            seed = next(seed);
        }
        s
    };
    let mut thinned = build(n, 42);
    let second = build(n, 7);
    let mut hits = 0;
    let mut seed = 99;
    for _ in 0..n {
        hits += i64::from(thinned.contains(&(seed % KEY_SPACE)));
        seed = next(seed);
    }
    let mut seed = 42;
    for _ in 0..n / 2 {
        thinned.remove(&(seed % KEY_SPACE));
        seed = next(next(seed));
    }
    let both: BTreeSet<i64> = thinned.intersection(&second).copied().collect();
    let only: BTreeSet<i64> = thinned.difference(&second).copied().collect();
    let subset = both.is_subset(&second);
    let mut joined = thinned;
    joined.extend(second.iter().copied());
    let folded = joined
        .iter()
        .fold(0, |total, item| (total * 31 + item) % MODULUS);
    let sizes = mix(
        mix(mix(hits, int(both.len())), int(only.len())),
        i64::from(subset),
    );
    mix(mix(sizes, int(joined.len())), folded)
}

/// The number of keys of the multimap programs: about eight values each.
pub(crate) fn multimap_keys(n: i64) -> i64 {
    (n / 8).max(1)
}

/// Seven times the number of values and the first value, for `look`.
pub(crate) fn looked_values(values: &[i64]) -> i64 {
    int(values.len()) * 7 + values.first().copied().unwrap_or(0)
}

/// `sorted_multimap` on the `Rc` tree: each key holds an `Rc<Vec<_>>`
/// that an insert copies and extends, as a persistent list would be.
pub(crate) fn sorted_multimap_rc(n: i64) -> i64 {
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
    let pairs = m.fold(Vec::new(), &mut |mut pairs, key, values| {
        pairs.extend(values.iter().map(|value| (*key, *value)));
        pairs
    });
    let folded = pairs.iter().fold(0, |sum, (key, value)| {
        (sum * 31 + key * 7 + value) % MODULUS
    });
    mix(mix(mix(looked, m.size()), total), folded)
}

/// `sorted_multimap` on std's `BTreeMap<K, Vec<V>>`.
pub(crate) fn sorted_multimap_btree(n: i64) -> i64 {
    let keys = multimap_keys(n);
    let mut m: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
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
    let pairs: Vec<(i64, i64)> = m
        .iter()
        .flat_map(|(key, values)| values.iter().map(|value| (*key, *value)))
        .collect();
    let folded = pairs.iter().fold(0, |sum, (key, value)| {
        (sum * 31 + key * 7 + value) % MODULUS
    });
    mix(mix(mix(looked, int(m.len())), total), folded)
}
