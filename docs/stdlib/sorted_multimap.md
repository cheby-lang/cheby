# `std::sorted_multimap`

A persistent map from each key to a list of values, ordered by key (D-332, D-338). It wraps a [`SortedMap`](sorted_map.md) whose values are lists, so it iterates in key order like a sorted map.

```cheby
pub type SortedMultimap<K, V>
```

`SortedMultimap` is opaque. Values are immutable, so every update returns a new multimap, and the old one stays usable.

- **Values.** Each key holds a `List<V>` in the order the values were inserted, with duplicates kept, so inserting `("a", 1)` twice gives `[1, 1]` (D-332). A key is present only while it has at least one value (D-364).
- **Order.** `keys`, `to_list`, `to_pairs`, `fold`, `each`, `show` and debug printing go from the smallest key to the largest, by the key type's `compare`. The values of one key stay in insertion order.
- **Keys.** Keys must satisfy `Compare`, and every function has the bound `K: Compare` (D-385). Two keys are the same key when `compare` returns `Equal`, even if they are not `==`. A value inserted under such a key joins the list of the key already stored, which stays (D-341). A `compare` that disagrees with `==` is a program bug (§8.7). The second example of [`insert`](#insert) shows what the multimap does with one.
- **Written in Cheby.** The module is written in Cheby over `std::sorted_map`, with no runtime kernel (D-336, D-338). Every comparison is an ordinary call to the key type's `compare` through the bound (§8.5), so a panic inside it unwinds through this module's functions like any other panic. The number and order of the calls to `compare` are not specified, but they are the same on every target (D-232, D-354).
- **Equality, hashing and printing.** The module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two sorted multimaps are `==` when they have the same keys, and each key holds the same list of values in the same order, and equal multimaps hash equally (D-343). Debug printing writes `sorted_multimap::from_list([…])` with one debug-printed pair per value, in the order of `to_pairs`, so the text rebuilds the same multimap (D-344).
- **Interfaces.** `SortedMultimap<K, V>` satisfies `Show` when `K` and `V` do (D-344). It does not satisfy `Compare`.
- **Costs.** For a multimap of n keys, a lookup or an update takes O(log n) time and calls `compare` O(log n) times. Iterating takes time in proportion to what it returns and never calls `compare`.

The map functions `values`, `upsert`, `map_values` and `merge` have no counterpart in tier 1. `get` and `to_pairs` reach the values, and a caller can rebuild a multimap with `fold`.

```cheby
test "sorted multimaps compare, hash and print by contents" {
  let built = sorted_multimap::from_list([("b", 3), ("a", 1), ("a", 2)])
  let grown = sorted_multimap::insert(sorted_multimap::from_list([("a", 1), ("b", 3)]), "a", 2)
  assert built == grown
  assert built != sorted_multimap::from_list([("a", 2), ("a", 1), ("b", 3)])
  assert set::size(set::from_list([built, grown])) == 1
  assert "{built:?}" == "sorted_multimap::from_list([(\"a\", 1), (\"a\", 2), (\"b\", 3)])"
}
```

## Building

### `new`

```cheby
pub fn new<K: Compare, V>() -> SortedMultimap<K, V>
```

Returns an empty multimap.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = sorted_multimap::new::<String, Int>()
  assert sorted_multimap::is_empty(empty)
  assert sorted_multimap::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K: Compare, V>(pairs: List<(K, V)>) -> SortedMultimap<K, V>
```

Returns a multimap with every pair, as if each were inserted in turn. No pair is dropped, so repeated pairs give repeated values.

- **Cost:** O(n log n), where n is the length of `pairs`. Required (D-300).

```cheby
test "from_list keeps every pair" {
  let tags = sorted_multimap::from_list([("b", 1), ("a", 2), ("b", 1)])
  assert sorted_multimap::to_list(tags) == [("a", [2]), ("b", [1, 1])]
}
```

## Size

### `size`

```cheby
pub fn size<K: Compare, V>(m: SortedMultimap<K, V>) -> Int
```

Returns the number of keys. [`count`](#count) returns the number of values.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  let tags = sorted_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::size(tags) == 2
}
```

`size` counts keys, as in every map-like module, and `count` counts values (D-363).

### `count`

```cheby
pub fn count<K: Compare, V>(m: SortedMultimap<K, V>) -> Int
```

Returns the total number of values, over every key.

- **Cost:** O(1), required (D-300). The multimap keeps the total next to its map.

```cheby
test "count counts values" {
  let tags = sorted_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::count(tags) == 3
}
```

### `is_empty`

```cheby
pub fn is_empty<K: Compare, V>(m: SortedMultimap<K, V>) -> Bool
```

Tells whether the multimap has no keys, and so no values.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty multimap" {
  assert sorted_multimap::is_empty(sorted_multimap::new::<String, Int>())
  assert !sorted_multimap::is_empty(sorted_multimap::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K: Compare, V>(m: SortedMultimap<K, V>, key: K) -> List<V>
```

Returns the values stored under the key that compares `Equal` to `key`, in insertion order, or `[]` when there is no such key.

- **Cost:** O(log n), required (D-300).

```cheby
test "get returns every value of a key" {
  let tags = sorted_multimap::from_list([("a", 1), ("b", 3), ("a", 2)])
  assert sorted_multimap::get(tags, "a") == [1, 2]
  assert sorted_multimap::get(tags, "z") == []
}
```

`get` returns a list rather than an `Option`, because no key is kept without values (D-364).

### `has_key`

```cheby
pub fn has_key<K: Compare, V>(m: SortedMultimap<K, V>, key: K) -> Bool
```

Tells whether some key compares `Equal` to `key`.

- **Cost:** O(log n), required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let tags = sorted_multimap::from_list([("a", 1)])
  assert sorted_multimap::has_key(tags, "a")
  assert !sorted_multimap::has_key(tags, "b")
}
```

### `first`

```cheby
pub fn first<K: Compare, V>(m: SortedMultimap<K, V>) -> Option<(K, List<V>)>
```

Returns the smallest key with its values.

- **Fails:** `None` when the multimap is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the smallest key" {
  let tags = sorted_multimap::from_list([("b", 3), ("a", 1), ("a", 2)])
  assert sorted_multimap::first(tags) == Some(("a", [1, 2]))
  assert sorted_multimap::first(sorted_multimap::new::<String, Int>()) == None
}
```

### `last`

```cheby
pub fn last<K: Compare, V>(m: SortedMultimap<K, V>) -> Option<(K, List<V>)>
```

Returns the largest key with its values.

- **Fails:** `None` when the multimap is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the largest key" {
  let tags = sorted_multimap::from_list([("b", 3), ("a", 1), ("b", 4)])
  assert sorted_multimap::last(tags) == Some(("b", [3, 4]))
  assert sorted_multimap::last(sorted_multimap::new::<String, Int>()) == None
}
```

### `range`

```cheby
pub fn range<K: Compare, V>(m: SortedMultimap<K, V>, start: K, end: K) -> List<(K, List<V>)>
```

Returns the keys that are at least `start` and less than `end`, each with its values, in key order (D-319). The list is empty when `end` is not greater than `start`.

- **Cost:** O(log n + k), where k is the number of keys returned. Required (D-300).

```cheby
test "range includes start and excludes end" {
  let marks = sorted_multimap::from_list([(1, "a"), (3, "c"), (3, "C"), (5, "e")])
  assert sorted_multimap::range(marks, 2, 5) == [(3, ["c", "C"])]
  assert sorted_multimap::range(marks, 5, 2) == []
}
```

## Updating

### `insert`

```cheby
pub fn insert<K: Compare, V>(m: SortedMultimap<K, V>, key: K, value: V) -> SortedMultimap<K, V>
```

Returns the multimap with `value` appended to the values of `key`. When a key that compares `Equal` to `key` is already present, the stored key stays (D-341).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert appends a value" {
  let start = sorted_multimap::from_list([("a", 1)])
  let again = sorted_multimap::insert(sorted_multimap::insert(start, "a", 1), "b", 2)
  assert sorted_multimap::to_list(again) == [("a", [1, 1]), ("b", [2])]
  assert sorted_multimap::get(start, "a") == [1]
}
```

The second example uses a key type whose `compare` looks only at `id`, so two badges with the same `id` are the same key.

```cheby
import std::int
import std::sorted_multimap

/// A key ordered by `id` alone. Two badges with the same `id` and
/// different labels compare `Equal` without being `==`.
type Badge { id: Int, label: String }

fn compare(a: Badge, b: Badge) -> Order {
  int::compare(a.id, b.id)
}

test "values of keys that compare Equal share one list" {
  let stored = Badge { id: 1, label: "stored" }
  let later = Badge { id: 1, label: "later" }
  let badges = sorted_multimap::insert(sorted_multimap::from_list([(stored, "a")]), later, "b")
  assert sorted_multimap::to_list(badges) == [(stored, ["a", "b"])]
  assert sorted_multimap::get(badges, Badge { id: 1, label: "other" }) == ["a", "b"]
}
```

### `remove`

```cheby
pub fn remove<K: Compare, V>(m: SortedMultimap<K, V>, key: K) -> SortedMultimap<K, V>
```

Returns the multimap without the key that compares `Equal` to `key` and without all of its values. The multimap is returned unchanged when there is no such key.

- **Cost:** O(log n), required (D-300).

```cheby
test "remove drops a key with all its values" {
  let tags = sorted_multimap::from_list([("a", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::remove(tags, "a") == sorted_multimap::from_list([("b", 3)])
  assert sorted_multimap::remove(tags, "z") == tags
}
```

Removing a key's last value removes the key, so every key holds at least one value (D-364).

## Iterating

### `keys`

```cheby
pub fn keys<K: Compare, V>(m: SortedMultimap<K, V>) -> List<K>
```

Returns the keys in order, each once.

- **Cost:** O(n), required (D-300).

```cheby
test "keys come in key order" {
  let tags = sorted_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::keys(tags) == ["a", "b"]
}
```

### `to_list`

```cheby
pub fn to_list<K: Compare, V>(m: SortedMultimap<K, V>) -> List<(K, List<V>)>
```

Returns each key with its values, in key order.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list groups the values by key" {
  let tags = sorted_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::to_list(tags) == [("a", [2]), ("b", [1, 3])]
}
```

### `to_pairs`

```cheby
pub fn to_pairs<K: Compare, V>(m: SortedMultimap<K, V>) -> List<(K, V)>
```

Returns one pair per value, in key order and, within a key, in insertion order. `sorted_multimap::from_list(sorted_multimap::to_pairs(m))` is `m`.

- **Cost:** O(n + t), where t is the number of values. Required (D-300).

```cheby
test "to_pairs gives one pair per value" {
  let tags = sorted_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  assert sorted_multimap::to_pairs(tags) == [("a", 2), ("b", 1), ("b", 3)]
}
```

### `fold`

```cheby
pub fn fold<K: Compare, V, A>(m: SortedMultimap<K, V>, initial: A, f: fn(A, K, List<V>) -> A) -> A
```

Combines the keys in order, starting from `initial` and calling `f` once per key with all of its values.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits each key once" {
  let tags = sorted_multimap::from_list([("b", 1), ("a", 2), ("b", 3)])
  let text = sorted_multimap::fold(tags, "", fn(acc, key, values) {
    let n = list::length(values)
    acc + "{key}{n}"
  })
  assert text == "a1b2"
}
```

### `each`

```cheby
pub fn each<K: Compare, V>(m: SortedMultimap<K, V>, f: fn(K, List<V>))
```

Calls `f` once per key, in key order, with all of its values, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each sees every key with its values" {
  let tags = sorted_multimap::from_list([("a", 1), ("b", 2), ("b", 3)])
  sorted_multimap::each(tags, fn(_, values) { assert !list::is_empty(values) })
}
```

## Transforming

### `filter`

```cheby
pub fn filter<K: Compare, V>(m: SortedMultimap<K, V>, keep: fn(K, List<V>) -> Bool) -> SortedMultimap<K, V>
```

Returns the keys, with all their values, for which `keep` returns `True`. `keep` is called once per key, in key order, and `compare` is not called.

- **Cost:** O(n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps whole keys" {
  let tags = sorted_multimap::from_list([("a", 1), ("b", 2), ("b", 3)])
  let repeated = sorted_multimap::filter(tags, fn(_, values) { list::length(values) > 1 })
  assert sorted_multimap::to_list(repeated) == [("b", [2, 3])]
}
```

## Text

### `show`

```cheby
pub fn show<K: Compare + Show, V: Show>(m: SortedMultimap<K, V>) -> String
```

Makes `SortedMultimap<K, V>` satisfy `Show` when `K` and `V` do (§8.8, D-064, D-344). The text follows D-355: `sorted_multimap::from_list([`, one pair per value in the order of `to_pairs` written with `show`, and `])`.

- **Cost:** O(n + t), where t is the number of values, plus the `show` of every key and value. Informative.

```cheby
test "a sorted multimap shows one pair per value" {
  let tags = sorted_multimap::from_list([("b", 3), ("a", 1), ("a", 2)])
  assert sorted_multimap::show(tags) == "sorted_multimap::from_list([(a, 1), (a, 2), (b, 3)])"
  assert "{tags}" == "sorted_multimap::from_list([(a, 1), (a, 2), (b, 3)])"
}
```
