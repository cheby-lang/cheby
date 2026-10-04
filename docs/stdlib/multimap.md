# `std::multimap`

A persistent hashed map from each key to a list of values (D-330, D-332, D-338). It wraps a [`Map`](map.md) whose values are lists, so its keys come in the same per-process random order as `Map`'s (D-093).

```cheby
pub type Multimap<K, V>
```

`Multimap` is opaque. Values are immutable, so every update returns a new multimap, and the old one stays usable.

- **Values.** Each key holds a `List<V>` in the order the values were inserted, with duplicates kept, so inserting `("a", 1)` twice gives `[1, 1]` (D-332). A key is present only while it has at least one value (D-364).
- **Keys.** Keys need no interface. They are compared with `==` and hashed with the built-in structural hash, as in `Map` (§3.13).
- **Order.** `keys`, `to_list`, `to_pairs`, `fold`, `each` and the callback of `filter` visit the keys in an order that depends on a per-process random seed and changes between runs (D-093). The values of one key stay in insertion order. Programs that print or compare the order of keys sort first, for example with [`to_sorted_list`](#to_sorted_list).
- **Written in Cheby.** The module is written in Cheby over `std::map`, with no kernel of its own (D-338). The representation is not specified. One that meets the costs below is a `Map<K, List<V>>` plus the total number of values. The costs of the functions that build, read and update follow from those of `Map` and `List`, and are required (D-300). The costs of the functions that take a callback are informative, as in `std::map`.
- **Equality, hashing and printing.** The module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two multimaps are `==` when they have the same keys, in any order, and each key holds the same list of values in the same order, and equal multimaps hash equally. Debug printing writes `multimap::from_list([…])` with one debug-printed pair per value, the pairs of each key in insertion order and the keys sorted by their debug text, so the text is the same on every run and rebuilds the same multimap (D-344, D-382).
- **Interfaces.** `Multimap` satisfies neither `Show` nor `Compare`, because its order changes between runs (§8.8, D-344).
- **Costs.** For a multimap of n keys, a lookup takes O(log n), effectively constant, and an insert O(log n + log m), where m is the number of values of the key. Iterating takes time in proportion to what it returns.

The map functions `values`, `upsert`, `map_values` and `merge` have no counterpart in tier 1, as in `std::sorted_multimap`. `get` and `to_pairs` reach the values, and a caller can rebuild a multimap with `fold`.

```cheby
test "multimaps compare by contents and print sorted" {
  let built = multimap::from_list([("b", 3), ("a", 1), ("a", 2)])
  let grown = multimap::insert(multimap::from_list([("b", 3), ("a", 1)]), "a", 2)
  assert built == grown
  assert built != multimap::from_list([("a", 2), ("a", 1), ("b", 3)])
  assert set::size(set::from_list([built, grown])) == 1
  assert "{built:?}" == "multimap::from_list([(\"a\", 1), (\"a\", 2), (\"b\", 3)])"
}
```

## Building

### `new`

```cheby
pub fn new<K, V>() -> Multimap<K, V>
```

Returns an empty multimap.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = multimap::new::<String, Int>()
  assert multimap::is_empty(empty)
  assert multimap::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K, V>(pairs: List<(K, V)>) -> Multimap<K, V>
```

Returns a multimap with every pair, as if each were inserted in turn. No pair is dropped, so repeated pairs give repeated values.

- **Cost:** O(t log t), where t is the length of `pairs`. Required (D-300).

```cheby
test "from_list keeps every pair" {
  let tags = multimap::from_list([("b", 1), ("a", 2), ("b", 1)])
  assert multimap::to_sorted_list(tags) == [("a", [2]), ("b", [1, 1])]
}
```

## Size

### `size`

```cheby
pub fn size<K, V>(m: Multimap<K, V>) -> Int
```

Returns the number of keys (D-301). [`count`](#count) returns the number of values.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  let tags = multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert multimap::size(tags) == 2
}
```

`size` counts keys, as in every map-like module, and `count` counts values (D-363).

### `count`

```cheby
pub fn count<K, V>(m: Multimap<K, V>) -> Int
```

Returns the total number of values, over every key.

- **Cost:** O(1), required (D-300). The multimap keeps the total next to its map.

```cheby
test "count counts values" {
  let tags = multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert multimap::count(tags) == 3
}
```

### `is_empty`

```cheby
pub fn is_empty<K, V>(m: Multimap<K, V>) -> Bool
```

Tells whether the multimap has no keys, and so no values.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty multimap" {
  assert multimap::is_empty(multimap::new::<String, Int>())
  assert !multimap::is_empty(multimap::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K, V>(m: Multimap<K, V>, key: K) -> List<V>
```

Returns the values stored under `key`, in insertion order, or `[]` when `key` is not in the multimap.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "get returns every value of a key" {
  let tags = multimap::from_list([("a", 1), ("b", 3), ("a", 2)])
  assert multimap::get(tags, "a") == [1, 2]
  assert multimap::get(tags, "z") == []
}
```

`get` returns a list rather than an `Option`, because no key is kept without values (D-364).

### `has_key`

```cheby
pub fn has_key<K, V>(m: Multimap<K, V>, key: K) -> Bool
```

Tells whether `key` is in the multimap.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let tags = multimap::from_list([("a", 1)])
  assert multimap::has_key(tags, "a")
  assert !multimap::has_key(tags, "b")
}
```

## Updating

### `insert`

```cheby
pub fn insert<K, V>(m: Multimap<K, V>, key: K, value: V) -> Multimap<K, V>
```

Returns the multimap with `value` appended to the values of `key`. A key that was not present gets the list `[value]`.

- **Cost:** O(log n + log m), where m is the number of values of `key`. Required (D-300).

```cheby
test "insert appends a value" {
  let start = multimap::from_list([("a", 1)])
  let again = multimap::insert(multimap::insert(start, "a", 1), "b", 2)
  assert multimap::to_sorted_list(again) == [("a", [1, 1]), ("b", [2])]
  assert multimap::get(start, "a") == [1]
}
```

### `remove`

```cheby
pub fn remove<K, V>(m: Multimap<K, V>, key: K) -> Multimap<K, V>
```

Returns the multimap without `key` and without all of its values. The multimap is returned unchanged when `key` is not in it.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "remove drops a key with all its values" {
  let tags = multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert multimap::remove(tags, "a") == multimap::from_list([("b", 3)])
  assert multimap::remove(tags, "z") == tags
}
```

Removing a key's last value removes the key, so every key holds at least one value (D-364).

## Iterating

### `keys`

```cheby
pub fn keys<K, V>(m: Multimap<K, V>) -> List<K>
```

Returns the keys, each once, in the multimap's per-process order (D-093).

- **Cost:** O(n), required (D-300).

```cheby
test "keys lists every key once" {
  let tags = multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert list::sort(multimap::keys(tags)) == ["a", "b"]
}
```

### `to_list`

```cheby
pub fn to_list<K, V>(m: Multimap<K, V>) -> List<(K, List<V>)>
```

Returns each key with its values, in the multimap's per-process order of keys (D-093).

- **Cost:** O(n), required (D-300).

```cheby
test "to_list groups the values by key" {
  let tags = multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert list::sort(multimap::to_list(tags)) == [("a", [2]), ("b", [1, 3])]
}
```

### `to_sorted_list`

```cheby
pub fn to_sorted_list<K: Compare, V>(m: Multimap<K, V>) -> List<(K, List<V>)>
```

Returns each key with its values, sorted by key, so the result is the same on every run (D-093, D-344). The values of each key stay in insertion order. Keys that compare `Equal` without being `==` come in an order that may change between runs, which only a `compare` that disagrees with `==` can cause (§8.7).

- **Cost:** O(n log n) calls to `compare`. Informative.

```cheby
test "to_sorted_list orders by key" {
  let tags = multimap::from_list([("b", 3), ("a", 2), ("b", 1)])
  assert multimap::to_sorted_list(tags) == [("a", [2]), ("b", [3, 1])]
}
```

### `to_pairs`

```cheby
pub fn to_pairs<K, V>(m: Multimap<K, V>) -> List<(K, V)>
```

Returns one pair per value: the keys in the multimap's per-process order (D-093), and the values of each key in insertion order. `multimap::from_list(multimap::to_pairs(m))` is `m`.

- **Cost:** O(n + t), where t is the number of values. Required (D-300).

```cheby
test "to_pairs gives one pair per value" {
  let tags = multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert list::sort(multimap::to_pairs(tags)) == [("a", 2), ("b", 1), ("b", 3)]
  assert multimap::from_list(multimap::to_pairs(tags)) == tags
}
```

### `fold`

```cheby
pub fn fold<K, V, A>(m: Multimap<K, V>, initial: A, f: fn(A, K, List<V>) -> A) -> A
```

Combines the keys, starting from `initial` and calling `f` once per key with all of its values, in the multimap's per-process order (D-093).

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "fold visits each key once" {
  let tags = multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert multimap::fold(tags, 0, fn(total, _, values) { total + list::length(values) }) == 3
  assert multimap::fold(tags, 0, fn(seen, _, _) { seen + 1 }) == 2
}
```

### `each`

```cheby
pub fn each<K, V>(m: Multimap<K, V>, f: fn(K, List<V>))
```

Calls `f` once per key, with all of its values, in the multimap's per-process order (D-093), for its effects.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "each sees every key with its values" {
  let tags = multimap::from_list([("a", 1), ("b", 2), ("b", 3)])
  multimap::each(tags, fn(_, values) { assert !list::is_empty(values) })
}
```

## Transforming

### `filter`

```cheby
pub fn filter<K, V>(m: Multimap<K, V>, keep: fn(K, List<V>) -> Bool) -> Multimap<K, V>
```

Returns the keys, with all their values, for which `keep` returns `True`. `keep` is called once per key.

- **Cost:** O(n), plus the calls to `keep`. Informative.

```cheby
test "filter keeps whole keys" {
  let tags = multimap::from_list([("a", 1), ("b", 2), ("b", 3)])
  let repeated = multimap::filter(tags, fn(_, values) { list::length(values) > 1 })
  assert repeated == multimap::from_list([("b", 2), ("b", 3)])
}
```
