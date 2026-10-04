# `std::ordered_multimap`

A persistent map from each key to a list of values, in the order the keys were first inserted (D-332, D-338). It wraps an [`OrderedMap`](ordered_map.md) whose values are lists, so it iterates in insertion order like an ordered map.

```cheby
pub type OrderedMultimap<K, V>
```

`OrderedMultimap` is opaque. Values are immutable, so every update returns a new multimap, and the old one stays usable.

- **Values.** Each key holds a `List<V>` in the order the values were inserted, with duplicates kept, so inserting `("a", 1)` twice gives `[1, 1]` (D-332). A key is present only while it has at least one value (D-364).
- **Order.** `keys`, `to_list`, `to_pairs`, `fold`, `each`, `show` and debug printing go from the key inserted first to the key inserted last. Inserting a value under a key that is already present appends it to that key's list, and the key keeps its position. Removing a key and inserting it again moves it to the end (D-340).
- **Keys.** Keys need no interface. They are compared with `==` and hashed with the built-in hash, as in `Map` (§3.13), so a key that contains a function or a handle panics when it is inserted or looked up (D-233).
- **Written in Cheby.** The module is written in Cheby over `std::ordered_map`, with no kernel of its own (D-336, D-338). Keys are hashed and compared by the runtime's built-in routines, and no `compare` of the key type is ever called.
- **Equality, hashing and printing.** The module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two ordered multimaps are `==` only when they have the same keys in the same order, and each key holds the same list of values in the same order. Their hash covers both orders (D-343). Debug printing writes `ordered_multimap::from_list([…])` with one debug-printed pair per value, in the order of `to_pairs` (D-344).
- **Interfaces.** `OrderedMultimap<K, V>` satisfies `Show` when `K` and `V` do (D-344). It does not satisfy `Compare`.
- **Costs.** For a multimap of n keys, a lookup takes O(log n), effectively constant as in `Map`, and an update takes O(log n). Iterating takes time in proportion to what it returns.

The map functions `values`, `upsert`, `map_values` and `merge` have no counterpart in tier 1. `get` and `to_pairs` reach the values, and a caller can rebuild a multimap with `fold`.

```cheby
test "ordered multimaps compare, hash and print by contents and order" {
  let ab = ordered_multimap::from_list([("a", 1), ("b", 3), ("a", 2)])
  let ba = ordered_multimap::from_list([("b", 3), ("a", 1), ("a", 2)])
  let grown = ordered_multimap::insert(ordered_multimap::from_list([("a", 1), ("b", 3)]), "a", 2)
  assert ab == grown
  assert ab != ba
  assert set::size(set::from_list([ab, grown, ba])) == 2
  assert "{ba:?}" == "ordered_multimap::from_list([(\"b\", 3), (\"a\", 1), (\"a\", 2)])"
}
```

## Building

### `new`

```cheby
pub fn new<K, V>() -> OrderedMultimap<K, V>
```

Returns an empty multimap.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = ordered_multimap::new::<String, Int>()
  assert ordered_multimap::is_empty(empty)
  assert ordered_multimap::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K, V>(pairs: List<(K, V)>) -> OrderedMultimap<K, V>
```

Returns a multimap with every pair, as if each were inserted in turn. No pair is dropped, so repeated pairs give repeated values, and each key takes the position of its first occurrence.

- **Cost:** O(n log n), where n is the length of `pairs`. Required (D-300).

```cheby
test "from_list keeps every pair" {
  let tags = ordered_multimap::from_list([("b", 1), ("a", 2), ("b", 1)])
  assert ordered_multimap::to_list(tags) == [("b", [1, 1]), ("a", [2])]
}
```

## Size

### `size`

```cheby
pub fn size<K, V>(m: OrderedMultimap<K, V>) -> Int
```

Returns the number of keys. [`count`](#count) returns the number of values.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  let tags = ordered_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert ordered_multimap::size(tags) == 2
}
```

`size` counts keys, as in every map-like module, and `count` counts values (D-363).

### `count`

```cheby
pub fn count<K, V>(m: OrderedMultimap<K, V>) -> Int
```

Returns the total number of values, over every key.

- **Cost:** O(1), required (D-300). The multimap keeps the total next to its map.

```cheby
test "count counts values" {
  let tags = ordered_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert ordered_multimap::count(tags) == 3
}
```

### `is_empty`

```cheby
pub fn is_empty<K, V>(m: OrderedMultimap<K, V>) -> Bool
```

Tells whether the multimap has no keys, and so no values.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty multimap" {
  assert ordered_multimap::is_empty(ordered_multimap::new::<String, Int>())
  assert !ordered_multimap::is_empty(ordered_multimap::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K, V>(m: OrderedMultimap<K, V>, key: K) -> List<V>
```

Returns the values stored under `key`, in insertion order, or `[]` when `key` is not present.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "get returns every value of a key" {
  let tags = ordered_multimap::from_list([("a", 1), ("b", 3), ("a", 2)])
  assert ordered_multimap::get(tags, "a") == [1, 2]
  assert ordered_multimap::get(tags, "z") == []
}
```

`get` returns a list rather than an `Option`, because no key is kept without values (D-364).

### `has_key`

```cheby
pub fn has_key<K, V>(m: OrderedMultimap<K, V>, key: K) -> Bool
```

Tells whether `key` is present.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let tags = ordered_multimap::from_list([("a", 1)])
  assert ordered_multimap::has_key(tags, "a")
  assert !ordered_multimap::has_key(tags, "b")
}
```

### `first`

```cheby
pub fn first<K, V>(m: OrderedMultimap<K, V>) -> Option<(K, List<V>)>
```

Returns the oldest key still present, with its values.

- **Fails:** `None` when the multimap is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the oldest key" {
  let tags = ordered_multimap::from_list([("b", 3), ("a", 1), ("b", 4)])
  assert ordered_multimap::first(tags) == Some(("b", [3, 4]))
  assert ordered_multimap::first(ordered_multimap::new::<String, Int>()) == None
}
```

### `last`

```cheby
pub fn last<K, V>(m: OrderedMultimap<K, V>) -> Option<(K, List<V>)>
```

Returns the newest key, the one first inserted most recently, with its values.

- **Fails:** `None` when the multimap is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the newest key" {
  let tags = ordered_multimap::from_list([("b", 3), ("a", 1), ("b", 4)])
  assert ordered_multimap::last(tags) == Some(("a", [1]))
  assert ordered_multimap::last(ordered_multimap::new::<String, Int>()) == None
}
```

## Updating

### `insert`

```cheby
pub fn insert<K, V>(m: OrderedMultimap<K, V>, key: K, value: V) -> OrderedMultimap<K, V>
```

Returns the multimap with `value` appended to the values of `key`. A new key goes at the end, and a key already present keeps its position (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert appends a value and keeps the key's position" {
  let start = ordered_multimap::from_list([("b", 1), ("a", 2)])
  let again = ordered_multimap::insert(ordered_multimap::insert(start, "b", 1), "c", 3)
  assert ordered_multimap::to_list(again) == [("b", [1, 1]), ("a", [2]), ("c", [3])]
  assert ordered_multimap::get(start, "b") == [1]
}
```

### `remove`

```cheby
pub fn remove<K, V>(m: OrderedMultimap<K, V>, key: K) -> OrderedMultimap<K, V>
```

Returns the multimap without `key` and without all of its values. The other keys keep their order. The multimap is returned unchanged when `key` is not present. A removed key that is inserted again goes at the end, with only the new value (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "remove then insert moves a key to the end" {
  let start = ordered_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  let removed = ordered_multimap::remove(start, "a")
  let back = ordered_multimap::insert(removed, "a", 9)
  assert ordered_multimap::to_list(removed) == [("b", [3])]
  assert ordered_multimap::to_list(back) == [("b", [3]), ("a", [9])]
  assert ordered_multimap::remove(start, "z") == start
}
```

Removing a key's last value removes the key, so every key holds at least one value (D-364).

## Iterating

### `keys`

```cheby
pub fn keys<K, V>(m: OrderedMultimap<K, V>) -> List<K>
```

Returns the keys in the multimap's order, each once.

- **Cost:** O(n), required (D-300).

```cheby
test "keys come in insertion order" {
  let tags = ordered_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert ordered_multimap::keys(tags) == ["b", "a"]
}
```

### `to_list`

```cheby
pub fn to_list<K, V>(m: OrderedMultimap<K, V>) -> List<(K, List<V>)>
```

Returns each key with its values, in the multimap's order.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list groups the values by key" {
  let tags = ordered_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert ordered_multimap::to_list(tags) == [("b", [1, 3]), ("a", [2])]
}
```

### `to_pairs`

```cheby
pub fn to_pairs<K, V>(m: OrderedMultimap<K, V>) -> List<(K, V)>
```

Returns one pair per value, in the order of the keys and, within a key, in insertion order. The pairs are grouped by key, so they need not come in the order they were inserted. `ordered_multimap::from_list(ordered_multimap::to_pairs(m))` is `m`.

- **Cost:** O(n + t), where t is the number of values. Required (D-300).

```cheby
test "to_pairs groups the pairs by key" {
  let tags = ordered_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert ordered_multimap::to_pairs(tags) == [("b", 1), ("b", 3), ("a", 2)]
}
```

### `fold`

```cheby
pub fn fold<K, V, A>(m: OrderedMultimap<K, V>, initial: A, f: fn(A, K, List<V>) -> A) -> A
```

Combines the keys in the multimap's order, starting from `initial` and calling `f` once per key with all of its values.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits each key once in order" {
  let tags = ordered_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  let text = ordered_multimap::fold(tags, "", fn(acc, key, values) {
    let n = list::length(values)
    acc + "{key}{n}"
  })
  assert text == "b2a1"
}
```

### `each`

```cheby
pub fn each<K, V>(m: OrderedMultimap<K, V>, f: fn(K, List<V>))
```

Calls `f` once per key, in the multimap's order, with all of its values, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each sees every key with its values" {
  let tags = ordered_multimap::from_list([("a", 1), ("b", 2), ("b", 3)])
  ordered_multimap::each(tags, fn(_, values) { assert !list::is_empty(values) })
}
```

## Transforming

### `filter`

```cheby
pub fn filter<K, V>(m: OrderedMultimap<K, V>, keep: fn(K, List<V>) -> Bool) -> OrderedMultimap<K, V>
```

Returns the keys, with all their values, for which `keep` returns `True`, in the same order. `keep` is called once per key, in the multimap's order.

- **Cost:** O(n log n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps whole keys in order" {
  let tags = ordered_multimap::from_list([("c", 1), ("c", 2), ("a", 3), ("b", 4), ("b", 5)])
  let repeated = ordered_multimap::filter(tags, fn(_, values) { list::length(values) > 1 })
  assert ordered_multimap::keys(repeated) == ["c", "b"]
}
```

## Text

### `show`

```cheby
pub fn show<K: Show, V: Show>(m: OrderedMultimap<K, V>) -> String
```

Makes `OrderedMultimap<K, V>` satisfy `Show` when `K` and `V` do (§8.8, D-064, D-344). The text follows D-355: `ordered_multimap::from_list([`, one pair per value in the order of `to_pairs` written with `show`, and `])`.

- **Cost:** O(n + t), where t is the number of values, plus the `show` of every key and value. Informative.

```cheby
test "an ordered multimap shows one pair per value" {
  let tags = ordered_multimap::from_list([("b", 3), ("a", 1), ("b", 4)])
  assert ordered_multimap::show(tags) == "ordered_multimap::from_list([(b, 3), (b, 4), (a, 1)])"
  assert "{tags}" == "ordered_multimap::from_list([(b, 3), (b, 4), (a, 1)])"
}
```
