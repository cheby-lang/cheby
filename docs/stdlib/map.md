# `std::map`

The hashed map `Map<K, V>` (§3.8, D-039). `Map` is not in the prelude: `import std::map` brings in the module and `import std::map::{Map}` the type (§3.2). This module is where the type is declared (§8.2):

```cheby
pub type Map<K, V>
```

`Map` is opaque. It is a persistent hash array mapped trie, so every update returns a new map, and the old one stays usable and shares most of its nodes with the new one (D-039).

- **Keys.** Keys need no interface. They are compared with `==` and hashed with the built-in structural hash (§3.13).
- **Order.** `keys`, `values`, `to_list`, `fold`, `each` and the callbacks of `filter` and `map_values` visit the entries in an order that depends on a per-process random seed and changes between runs (D-093). Programs that print or compare that order sort first, for example with [`to_sorted_list`](#to_sorted_list).
- **Kernels.** `new`, `from_list`, `size`, `is_empty`, `get`, `has_key`, `insert`, `remove`, `upsert`, `keys`, `values` and `to_list` are runtime kernels, and their costs are required (D-274, D-300). The other functions are written in Cheby over them, and their costs are informative. `insert`, `remove` and `upsert` update the map in place when it is unique (§9.3).
- **Equality, hashing and printing.** Two maps are `==` when they hold the same entries, in any order, and equal maps hash equally (D-093, §3.13). Debug printing writes `map::from_list([…])` with the entries sorted by the debug text of each `(key, value)` pair, so it is the same on every run (D-344).
- **Interfaces.** `Map` satisfies neither `Show` nor `Compare`, because its order changes between runs (§8.8, D-344).
- **Costs.** For a map of n entries, a lookup or an update takes O(log n), effectively constant.

```cheby
test "maps compare by contents and print sorted" {
  let numbers = map::from_list([(10, "ten"), (9, "nine")])
  assert numbers == map::from_list([(9, "nine"), (10, "ten")])
  assert set::size(set::from_list([numbers, map::insert(map::new(), 9, "nine")])) == 2
  assert "{numbers:?}" == "map::from_list([(10, \"ten\"), (9, \"nine\")])"
}
```

Debug printing writes `map::from_list([…])`, with the entries sorted by their whole debug text, so `10` comes before `9` (D-344, D-382).

## Building

### `new`

```cheby
pub fn new<K, V>() -> Map<K, V>
```

Returns an empty map.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = map::new::<String, Int>()
  assert map::is_empty(empty)
  assert map::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K, V>(pairs: List<(K, V)>) -> Map<K, V>
```

Returns a map with the given pairs, as if each pair were inserted in turn. When the list repeats a key, the last value wins (D-329).

- **Cost:** O(n log n), effectively O(n), where n is the length of `pairs`. Required (D-300).

```cheby
test "from_list keeps the last duplicate" {
  let fruit = map::from_list([("pear", 1), ("apple", 2), ("pear", 3)])
  assert fruit == map::from_list([("apple", 2), ("pear", 3)])
  assert map::get(fruit, "pear") == Some(3)
}
```

## Size

### `size`

```cheby
pub fn size<K, V>(m: Map<K, V>) -> Int
```

Returns the number of keys (D-301).

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  assert map::size(map::from_list([("a", 1), ("b", 2), ("a", 3)])) == 2
  assert map::size(map::new::<String, Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<K, V>(m: Map<K, V>) -> Bool
```

Tells whether the map has no keys.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty map" {
  assert map::is_empty(map::new::<String, Int>())
  assert !map::is_empty(map::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K, V>(m: Map<K, V>, key: K) -> Option<V>
```

Returns the value stored under `key`.

- **Fails:** `None` when `key` is not in the map.
- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "get looks a key up" {
  let ages = map::from_list([("ada", 36), ("alan", 41)])
  assert map::get(ages, "ada") == Some(36)
  assert map::get(ages, "grace") == None
}
```

### `has_key`

```cheby
pub fn has_key<K, V>(m: Map<K, V>, key: K) -> Bool
```

Tells whether `key` is in the map.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let ages = map::from_list([("ada", 36)])
  assert map::has_key(ages, "ada")
  assert !map::has_key(ages, "alan")
}
```

## Updating

### `insert`

```cheby
pub fn insert<K, V>(m: Map<K, V>, key: K, value: V) -> Map<K, V>
```

Returns the map with `value` stored under `key`, replacing any value already there.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "insert adds or replaces" {
  let start = map::from_list([("a", 1)])
  let added = map::insert(start, "b", 2)
  let replaced = map::insert(added, "a", 10)
  assert replaced == map::from_list([("a", 10), ("b", 2)])
  assert start == map::from_list([("a", 1)])
}
```

### `remove`

```cheby
pub fn remove<K, V>(m: Map<K, V>, key: K) -> Map<K, V>
```

Returns the map without `key`. The map is returned unchanged when `key` is not in it.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "remove drops a key and ignores a missing one" {
  let pair = map::from_list([("a", 1), ("b", 2)])
  assert map::remove(pair, "a") == map::from_list([("b", 2)])
  assert map::remove(pair, "z") == pair
}
```

### `upsert`

```cheby
pub fn upsert<K, V>(m: Map<K, V>, key: K, update: fn(Option<V>) -> V) -> Map<K, V>
```

Calls `update` once, with `Some` of the value stored under `key` or `None` when there is none, and stores its result under `key`.

- **Cost:** O(log n), effectively constant, plus the call to `update`. Required (D-300).

```cheby
test "upsert updates or inserts through one function" {
  fn bump(previous: Option<Int>) -> Int {
    case previous {
      Some(n) => n + 1
      None => 1
    }
  }
  let counts = list::fold(["b", "a", "b"], map::new(), fn(found, word) { map::upsert(found, word, bump) })
  assert counts == map::from_list([("a", 1), ("b", 2)])
}
```

### `merge`

```cheby
pub fn merge<K, V>(m: Map<K, V>, other: Map<K, V>) -> Map<K, V>
```

Returns the entries of both maps, as if each entry of `other` were inserted into `m`. On a shared key the value from `other` wins.

- **Cost:** O(k log (n + k)), effectively O(k), where k is the size of `other`. Informative.

```cheby
test "merge prefers the second map on shared keys" {
  let base = map::from_list([("a", 1), ("b", 2)])
  let changes = map::from_list([("b", 20), ("c", 30)])
  assert map::merge(base, changes) == map::from_list([("a", 1), ("b", 20), ("c", 30)])
}
```

## Iterating

### `keys`

```cheby
pub fn keys<K, V>(m: Map<K, V>) -> List<K>
```

Returns the keys, in the map's per-process order (D-093).

- **Cost:** O(n), required (D-300).

```cheby
test "keys lists every key once" {
  let scores = map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert list::sort(map::keys(scores)) == ["a", "b", "c"]
}
```

### `values`

```cheby
pub fn values<K, V>(m: Map<K, V>) -> List<V>
```

Returns the values, in the same order as `keys` returns the keys of the same map.

- **Cost:** O(n), required (D-300).

```cheby
test "values lists every value" {
  let scores = map::from_list([("b", 2), ("c", 2), ("a", 1)])
  assert list::sort(map::values(scores)) == [1, 2, 2]
  assert list::zip(map::keys(scores), map::values(scores)) == map::to_list(scores)
}
```

### `to_list`

```cheby
pub fn to_list<K, V>(m: Map<K, V>) -> List<(K, V)>
```

Returns the entries as pairs, in the same order as `keys` returns the keys of the same map (D-093). `map::from_list(map::to_list(m))` is `m`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives every entry" {
  let scores = map::from_list([("b", 2), ("a", 1)])
  assert list::sort(map::to_list(scores)) == [("a", 1), ("b", 2)]
  assert map::from_list(map::to_list(scores)) == scores
}
```

### `to_sorted_list`

```cheby
pub fn to_sorted_list<K: Compare, V>(m: Map<K, V>) -> List<(K, V)>
```

Returns the entries as pairs, sorted by key, so the result is the same on every run (D-093, D-344). Keys that compare `Equal` without being `==` come in an order that may change between runs, which only a `compare` that disagrees with `==` can cause (§8.7).

- **Cost:** O(n log n) calls to `compare`. Informative.

```cheby
test "to_sorted_list orders by key" {
  let scores = map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert map::to_sorted_list(scores) == [("a", 1), ("b", 2), ("c", 3)]
  assert map::to_sorted_list(map::new::<Int, Int>()) == []
}
```

### `fold`

```cheby
pub fn fold<K, V, A>(m: Map<K, V>, initial: A, f: fn(A, K, V) -> A) -> A
```

Combines the entries, starting from `initial` and calling `f` once per entry, in the map's per-process order (D-093).

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "fold visits every entry once" {
  let scores = map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert map::fold(scores, 0, fn(total, _, value) { total + value }) == 6
  assert map::fold(scores, 0, fn(seen, _, _) { seen + 1 }) == 3
}
```

### `each`

```cheby
pub fn each<K, V>(m: Map<K, V>, f: fn(K, V))
```

Calls `f` once per entry, in the map's per-process order (D-093), for its effects.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "each calls the function once per entry" {
  let lengths = map::from_list([("a", 1), ("bb", 2)])
  map::each(lengths, fn(key, value) { assert string::length(key) == value })
}
```

## Transforming

### `map_values`

```cheby
pub fn map_values<K, V, W>(m: Map<K, V>, f: fn(K, V) -> W) -> Map<K, W>
```

Returns a map with the same keys, each value replaced by `f` of its key and value. `f` is called once per entry.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "map_values keeps the keys" {
  let counts = map::from_list([("a", 1), ("b", 2)])
  let labels = map::map_values(counts, fn(key, value) { "{key}={value}" })
  assert labels == map::from_list([("a", "a=1"), ("b", "b=2")])
}
```

### `filter`

```cheby
pub fn filter<K, V>(m: Map<K, V>, keep: fn(K, V) -> Bool) -> Map<K, V>
```

Returns the entries for which `keep` returns `True`. `keep` is called once per entry.

- **Cost:** O(n), plus the calls to `keep`. Informative.

```cheby
test "filter keeps the matching entries" {
  let counts = map::from_list([("a", 1), ("b", 2), ("c", 3)])
  let odd = map::filter(counts, fn(_, value) { value % 2 == 1 })
  assert odd == map::from_list([("a", 1), ("c", 3)])
}
```
