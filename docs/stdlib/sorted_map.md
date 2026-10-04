# `std::sorted_map`

A persistent map that keeps its entries ordered by key (D-331, D-337). Unlike `Map`, whose iteration order changes between runs (D-093), a sorted map always iterates in the order its contents fix, and it can answer range, minimum and maximum queries.

```cheby
pub type SortedMap<K, V>
```

`SortedMap` is opaque. Values are immutable, so every update returns a new map, and the old one stays usable and shares most of its nodes with the new one.

- **Order.** `keys`, `values`, `to_list`, `fold`, `each`, `show` and debug printing go from the smallest key to the largest, by the key type's `compare`.
- **Keys.** Keys must satisfy `Compare`, and every function has the bound `K: Compare`. Two keys are the same key when `compare` returns `Equal`, even if they are not `==`. Inserting such a key keeps the key already stored and replaces only the value (D-341). A `compare` that disagrees with `==` is a program bug (§8.7). The second example of [`insert`](#insert) shows what the map does with one.
- **Written in Cheby.** The map is a persistent balanced tree written in Cheby, with no runtime kernel (D-336). The kind of tree is not specified. Every comparison is an ordinary call to the key type's `compare` through the bound (§8.5), so a `compare` written in Cheby costs what any call costs, and a panic inside it unwinds through this module's functions like any other panic. The number and order of the calls to `compare` are not specified, but they are the same on every target (D-232, D-354). If a benchmark later moves the tree into a runtime kernel (D-336), the API and the costs below stay the same.
- **Equality, hashing and printing.** Maps with the same entries can have trees of different shapes, so the module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two sorted maps are `==` when they hold the same entries, however they were built, and equal maps hash equally (D-343). Keys and values are compared here with `==`, not with `compare`. Debug printing writes `sorted_map::from_list([…])` with the entries in key order, each one debug-printed (D-344).
- **Interfaces.** `SortedMap<K, V>` satisfies `Show` when `K` and `V` do (D-344). It does not satisfy `Compare`.
- **Costs.** For a map of n entries, a lookup or an update takes O(log n) time and calls `compare` O(log n) times. Iterating takes O(n) and never calls `compare`.

```cheby
test "sorted maps compare, hash and print by contents" {
  let built = sorted_map::from_list([("a", 1), ("b", 2)])
  let grown = sorted_map::insert(sorted_map::from_list([("b", 2)]), "a", 1)
  assert built == grown
  assert built != sorted_map::from_list([("a", 1), ("b", 3)])
  assert set::size(set::from_list([built, grown])) == 1
  assert "{built:?}" == "sorted_map::from_list([(\"a\", 1), (\"b\", 2)])"
}
```

Every function carries the bound `K: Compare`, including those that never compare keys, so the module has one rule (D-385).

## Building

### `new`

```cheby
pub fn new<K: Compare, V>() -> SortedMap<K, V>
```

Returns an empty map.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = sorted_map::new::<String, Int>()
  assert sorted_map::is_empty(empty)
  assert sorted_map::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K: Compare, V>(pairs: List<(K, V)>) -> SortedMap<K, V>
```

Returns a map with the given pairs, as if each pair were inserted in turn. When the list repeats a key, the last value wins (D-329). Among keys that compare `Equal` but are not `==`, the first one is kept with the last value (D-341).

- **Cost:** O(n log n), where n is the length of `pairs`. Required (D-300).

```cheby
test "from_list sorts by key and keeps the last duplicate" {
  let fruit = sorted_map::from_list([("pear", 1), ("apple", 2), ("pear", 3)])
  assert sorted_map::to_list(fruit) == [("apple", 2), ("pear", 3)]
}
```

## Size

### `size`

```cheby
pub fn size<K: Compare, V>(m: SortedMap<K, V>) -> Int
```

Returns the number of keys.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  assert sorted_map::size(sorted_map::from_list([("a", 1), ("b", 2), ("a", 3)])) == 2
  assert sorted_map::size(sorted_map::new::<String, Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<K: Compare, V>(m: SortedMap<K, V>) -> Bool
```

Tells whether the map has no keys.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty map" {
  assert sorted_map::is_empty(sorted_map::new::<String, Int>())
  assert !sorted_map::is_empty(sorted_map::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K: Compare, V>(m: SortedMap<K, V>, key: K) -> Option<V>
```

Returns the value stored under the key that compares `Equal` to `key`.

- **Fails:** `None` when no key compares `Equal` to `key`.
- **Cost:** O(log n), required (D-300).

```cheby
test "get looks a key up" {
  let ages = sorted_map::from_list([("ada", 36), ("alan", 41)])
  assert sorted_map::get(ages, "ada") == Some(36)
  assert sorted_map::get(ages, "grace") == None
}
```

### `has_key`

```cheby
pub fn has_key<K: Compare, V>(m: SortedMap<K, V>, key: K) -> Bool
```

Tells whether some key compares `Equal` to `key`.

- **Cost:** O(log n), required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let ages = sorted_map::from_list([("ada", 36)])
  assert sorted_map::has_key(ages, "ada")
  assert !sorted_map::has_key(ages, "alan")
}
```

### `first`

```cheby
pub fn first<K: Compare, V>(m: SortedMap<K, V>) -> Option<(K, V)>
```

Returns the entry with the smallest key.

- **Fails:** `None` when the map is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the smallest key" {
  let ages = sorted_map::from_list([("bob", 2), ("ada", 1), ("cy", 3)])
  assert sorted_map::first(ages) == Some(("ada", 1))
  assert sorted_map::first(sorted_map::new::<String, Int>()) == None
}
```

### `last`

```cheby
pub fn last<K: Compare, V>(m: SortedMap<K, V>) -> Option<(K, V)>
```

Returns the entry with the largest key.

- **Fails:** `None` when the map is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the largest key" {
  let ages = sorted_map::from_list([("bob", 2), ("ada", 1), ("cy", 3)])
  assert sorted_map::last(ages) == Some(("cy", 3))
  assert sorted_map::last(sorted_map::new::<String, Int>()) == None
}
```

### `range`

```cheby
pub fn range<K: Compare, V>(m: SortedMap<K, V>, start: K, end: K) -> List<(K, V)>
```

Returns the entries whose keys are at least `start` and less than `end`, in key order (D-319). `start` and `end` need not be keys of the map. The list is empty when `end` is not greater than `start`.

- **Cost:** O(log n + k), where k is the number of entries returned. Required (D-300).

```cheby
test "range includes start and excludes end" {
  let letters = sorted_map::from_list([(1, "a"), (3, "c"), (5, "e"), (7, "g")])
  assert sorted_map::range(letters, 3, 7) == [(3, "c"), (5, "e")]
  assert sorted_map::range(letters, 2, 4) == [(3, "c")]
  assert sorted_map::range(letters, 7, 3) == []
}
```

## Updating

### `insert`

```cheby
pub fn insert<K: Compare, V>(m: SortedMap<K, V>, key: K, value: V) -> SortedMap<K, V>
```

Returns the map with `value` stored under `key`. When a key that compares `Equal` to `key` is already present, the stored key stays and only its value is replaced (D-341).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert adds or replaces" {
  let start = sorted_map::from_list([("a", 1)])
  let added = sorted_map::insert(start, "b", 2)
  let replaced = sorted_map::insert(added, "a", 10)
  assert sorted_map::to_list(replaced) == [("a", 10), ("b", 2)]
  assert sorted_map::to_list(start) == [("a", 1)]
}
```

The second example uses a key type whose `compare` looks only at `id`, so two badges with the same `id` are the same key.

```cheby
import std::int
import std::sorted_map

/// A key ordered by `id` alone. Two badges with the same `id` and
/// different labels compare `Equal` without being `==`.
type Badge { id: Int, label: String }

fn compare(a: Badge, b: Badge) -> Order {
  int::compare(a.id, b.id)
}

test "keys that compare Equal are the same key" {
  let stored = Badge { id: 1, label: "stored" }
  let later = Badge { id: 1, label: "later" }
  let badges = sorted_map::insert(sorted_map::insert(sorted_map::new(), stored, "a"), later, "b")
  assert sorted_map::size(badges) == 1
  assert sorted_map::to_list(badges) == [(stored, "b")]
  assert sorted_map::get(badges, Badge { id: 1, label: "other" }) == Some("b")
}
```

### `remove`

```cheby
pub fn remove<K: Compare, V>(m: SortedMap<K, V>, key: K) -> SortedMap<K, V>
```

Returns the map without the key that compares `Equal` to `key`. The map is returned unchanged when there is no such key.

- **Cost:** O(log n), required (D-300).

```cheby
test "remove drops a key and ignores a missing one" {
  let pair = sorted_map::from_list([("a", 1), ("b", 2)])
  assert sorted_map::remove(pair, "a") == sorted_map::from_list([("b", 2)])
  assert sorted_map::remove(pair, "z") == pair
}
```

### `upsert`

```cheby
pub fn upsert<K: Compare, V>(m: SortedMap<K, V>, key: K, update: fn(Option<V>) -> V) -> SortedMap<K, V>
```

Calls `update` once, with `Some` of the value stored under `key` or `None` when there is none, and stores its result under `key`. A key already present stays, as with `insert` (D-341).

- **Cost:** O(log n), plus the call to `update`. Required (D-300).

```cheby
test "upsert updates or inserts through one function" {
  fn bump(previous: Option<Int>) -> Int {
    case previous {
      Some(n) => n + 1
      None => 1
    }
  }
  let once = sorted_map::upsert(sorted_map::from_list([("a", 1)]), "a", bump)
  let twice = sorted_map::upsert(once, "b", bump)
  assert sorted_map::to_list(twice) == [("a", 2), ("b", 1)]
}
```

### `merge`

```cheby
pub fn merge<K: Compare, V>(m: SortedMap<K, V>, other: SortedMap<K, V>) -> SortedMap<K, V>
```

Returns the entries of both maps, as if each entry of `other` were inserted into `m`. On a shared key the value from `other` wins, and the key stored in `m` stays (D-341).

- **Cost:** O(k log (n + k)), where k is the size of `other`. Required (D-300).

```cheby
test "merge prefers the second map on shared keys" {
  let base = sorted_map::from_list([("a", 1), ("b", 2)])
  let changes = sorted_map::from_list([("b", 20), ("c", 30)])
  assert sorted_map::to_list(sorted_map::merge(base, changes)) == [("a", 1), ("b", 20), ("c", 30)]
}
```

## Iterating

### `keys`

```cheby
pub fn keys<K: Compare, V>(m: SortedMap<K, V>) -> List<K>
```

Returns the keys in order.

- **Cost:** O(n), required (D-300).

```cheby
test "keys come in key order" {
  let scores = sorted_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert sorted_map::keys(scores) == ["a", "b", "c"]
}
```

### `values`

```cheby
pub fn values<K: Compare, V>(m: SortedMap<K, V>) -> List<V>
```

Returns the values in the order of their keys.

- **Cost:** O(n), required (D-300).

```cheby
test "values follow the order of their keys" {
  let scores = sorted_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert sorted_map::values(scores) == [1, 2, 3]
}
```

### `to_list`

```cheby
pub fn to_list<K: Compare, V>(m: SortedMap<K, V>) -> List<(K, V)>
```

Returns the entries as pairs, in key order. `sorted_map::from_list(sorted_map::to_list(m))` is `m`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives the entries in key order" {
  let scores = sorted_map::from_list([("b", 2), ("a", 1)])
  assert sorted_map::to_list(scores) == [("a", 1), ("b", 2)]
}
```

### `fold`

```cheby
pub fn fold<K: Compare, V, A>(m: SortedMap<K, V>, initial: A, f: fn(A, K, V) -> A) -> A
```

Combines the entries in key order, starting from `initial` and calling `f` once per entry.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits the keys in order" {
  let scores = sorted_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert sorted_map::fold(scores, "", fn(text, key, _) { text + key }) == "abc"
  assert sorted_map::fold(scores, 0, fn(total, _, value) { total + value }) == 6
}
```

### `each`

```cheby
pub fn each<K: Compare, V>(m: SortedMap<K, V>, f: fn(K, V))
```

Calls `f` once per entry, in key order, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each calls the function once per entry" {
  let lengths = sorted_map::from_list([("a", 1), ("bb", 2)])
  sorted_map::each(lengths, fn(key, value) { assert string::length(key) == value })
}
```

## Transforming

### `map_values`

```cheby
pub fn map_values<K: Compare, V, W>(m: SortedMap<K, V>, f: fn(K, V) -> W) -> SortedMap<K, W>
```

Returns a map with the same keys, each value replaced by `f` of its key and value. `f` is called once per entry, in key order, and `compare` is not called.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "map_values keeps the keys" {
  let counts = sorted_map::from_list([("a", 1), ("b", 2)])
  let labels = sorted_map::map_values(counts, fn(key, value) { "{key}={value}" })
  assert sorted_map::to_list(labels) == [("a", "a=1"), ("b", "b=2")]
}
```

### `filter`

```cheby
pub fn filter<K: Compare, V>(m: SortedMap<K, V>, keep: fn(K, V) -> Bool) -> SortedMap<K, V>
```

Returns the entries for which `keep` returns `True`. `keep` is called once per entry, in key order, and `compare` is not called, because the kept entries are already in order.

- **Cost:** O(n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps the matching entries" {
  let counts = sorted_map::from_list([("a", 1), ("b", 2), ("c", 3)])
  let odd = sorted_map::filter(counts, fn(_, value) { value % 2 == 1 })
  assert sorted_map::keys(odd) == ["a", "c"]
}
```

## Text

### `show`

```cheby
pub fn show<K: Compare + Show, V: Show>(m: SortedMap<K, V>) -> String
```

Makes `SortedMap<K, V>` satisfy `Show` when `K` and `V` do (§8.8, D-064, D-344). The text follows D-355: `sorted_map::from_list([`, the entries in key order as pairs written with `show`, and `])`.

- **Cost:** O(n), plus the `show` of every key and value. Informative.

```cheby
test "a sorted map shows its entries in key order" {
  let scores = sorted_map::from_list([("b", 2), ("a", 1)])
  assert sorted_map::show(scores) == "sorted_map::from_list([(a, 1), (b, 2)])"
  assert "{scores}" == "sorted_map::from_list([(a, 1), (b, 2)])"
}
```
