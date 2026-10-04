# `std::ordered_map`

A persistent map that keeps its entries in the order their keys were first inserted (D-331, D-337). Unlike `Map`, whose iteration order changes between runs (D-093), an ordered map iterates in the order of its input, so a program that reads entries and writes them back keeps their order.

```cheby
pub type OrderedMap<K, V>
```

`OrderedMap` is opaque. Values are immutable, so every update returns a new map, and the old one stays usable.

- **Order.** `keys`, `values`, `to_list`, `fold`, `each`, `show` and debug printing go from the key inserted first to the key inserted last. Inserting a key that is already present keeps its position and replaces its value. Removing a key and inserting it again moves it to the end (D-340).
- **Keys.** Keys need no interface. They are compared with `==` and hashed with the built-in hash, as in `Map` (§3.13), so a key that contains a function or a handle panics when it is inserted or looked up (D-233).
- **Written in Cheby.** The module is written in Cheby over `std::map` and `std::sorted_map`, with no kernel of its own (D-336). The representation is not specified. One that meets the costs below is a `Map` from each key to an insertion number and its value, plus a sorted map from insertion number to key. Keys are hashed and compared by the runtime's built-in routines, and no `compare` of the key type is ever called.
- **Equality, hashing and printing.** Maps with the same entries in the same order can hold different insertion numbers, so the module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two ordered maps are `==` only when they hold the same entries in the same order, and their hash covers the order (D-343). Debug printing writes `ordered_map::from_list([…])` with the entries in order, each one debug-printed (D-344).
- **Interfaces.** `OrderedMap<K, V>` satisfies `Show` when `K` and `V` do (D-344). It does not satisfy `Compare`.
- **Costs.** For a map of n entries, a lookup takes O(log n), effectively constant as in `Map`, and an update, including `remove`, takes O(log n). Iterating takes O(n).

```cheby
test "ordered maps compare, hash and print by contents and order" {
  let ab = ordered_map::from_list([("a", 1), ("b", 2)])
  let ba = ordered_map::from_list([("b", 2), ("a", 1)])
  let grown = ordered_map::insert(ordered_map::from_list([("a", 1)]), "b", 2)
  assert ab == grown
  assert ab != ba
  assert set::size(set::from_list([ab, grown, ba])) == 2
  assert "{ba:?}" == "ordered_map::from_list([(\"b\", 2), (\"a\", 1)])"
}
```

## Building

### `new`

```cheby
pub fn new<K, V>() -> OrderedMap<K, V>
```

Returns an empty map.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = ordered_map::new::<String, Int>()
  assert ordered_map::is_empty(empty)
  assert ordered_map::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<K, V>(pairs: List<(K, V)>) -> OrderedMap<K, V>
```

Returns a map with the given pairs, as if each pair were inserted in turn. When the list repeats a key, the last value wins (D-329) and the key keeps the position of its first occurrence (D-340).

- **Cost:** O(n log n), where n is the length of `pairs`. Required (D-300).

```cheby
test "from_list keeps the first position and the last value" {
  let fruit = ordered_map::from_list([("pear", 1), ("apple", 2), ("pear", 3)])
  assert ordered_map::to_list(fruit) == [("pear", 3), ("apple", 2)]
}
```

## Size

### `size`

```cheby
pub fn size<K, V>(m: OrderedMap<K, V>) -> Int
```

Returns the number of keys.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts keys" {
  assert ordered_map::size(ordered_map::from_list([("a", 1), ("b", 2), ("a", 3)])) == 2
  assert ordered_map::size(ordered_map::new::<String, Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<K, V>(m: OrderedMap<K, V>) -> Bool
```

Tells whether the map has no keys.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty map" {
  assert ordered_map::is_empty(ordered_map::new::<String, Int>())
  assert !ordered_map::is_empty(ordered_map::from_list([("a", 1)]))
}
```

## Lookup

### `get`

```cheby
pub fn get<K, V>(m: OrderedMap<K, V>, key: K) -> Option<V>
```

Returns the value stored under `key`.

- **Fails:** `None` when `key` is not in the map.
- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "get looks a key up" {
  let ages = ordered_map::from_list([("ada", 36), ("alan", 41)])
  assert ordered_map::get(ages, "alan") == Some(41)
  assert ordered_map::get(ages, "grace") == None
}
```

### `has_key`

```cheby
pub fn has_key<K, V>(m: OrderedMap<K, V>, key: K) -> Bool
```

Tells whether `key` is in the map.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "has_key tells whether a key is present" {
  let ages = ordered_map::from_list([("ada", 36)])
  assert ordered_map::has_key(ages, "ada")
  assert !ordered_map::has_key(ages, "alan")
}
```

### `first`

```cheby
pub fn first<K, V>(m: OrderedMap<K, V>) -> Option<(K, V)>
```

Returns the entry that comes first in the map's order, the oldest key still present.

- **Fails:** `None` when the map is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the oldest key" {
  let queue = ordered_map::from_list([("bob", 2), ("ada", 1), ("cy", 3)])
  assert ordered_map::first(queue) == Some(("bob", 2))
  assert ordered_map::first(ordered_map::remove(queue, "bob")) == Some(("ada", 1))
  assert ordered_map::first(ordered_map::new::<String, Int>()) == None
}
```

### `last`

```cheby
pub fn last<K, V>(m: OrderedMap<K, V>) -> Option<(K, V)>
```

Returns the entry that comes last in the map's order, the key inserted most recently.

- **Fails:** `None` when the map is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the newest key" {
  let queue = ordered_map::from_list([("bob", 2), ("ada", 1), ("cy", 3)])
  assert ordered_map::last(queue) == Some(("cy", 3))
  assert ordered_map::last(ordered_map::new::<String, Int>()) == None
}
```

## Updating

### `insert`

```cheby
pub fn insert<K, V>(m: OrderedMap<K, V>, key: K, value: V) -> OrderedMap<K, V>
```

Returns the map with `value` stored under `key`. A new key goes at the end. A key already present keeps its position, and only its value is replaced (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert adds at the end and replaces in place" {
  let start = ordered_map::from_list([("b", 1), ("a", 2)])
  let added = ordered_map::insert(start, "c", 3)
  let replaced = ordered_map::insert(added, "b", 10)
  assert ordered_map::to_list(added) == [("b", 1), ("a", 2), ("c", 3)]
  assert ordered_map::to_list(replaced) == [("b", 10), ("a", 2), ("c", 3)]
  assert ordered_map::to_list(start) == [("b", 1), ("a", 2)]
}
```

### `remove`

```cheby
pub fn remove<K, V>(m: OrderedMap<K, V>, key: K) -> OrderedMap<K, V>
```

Returns the map without `key`. The other keys keep their order. The map is returned unchanged when `key` is not in it. A removed key that is inserted again goes at the end (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "remove then insert moves a key to the end" {
  let start = ordered_map::from_list([("a", 1), ("b", 2), ("c", 3)])
  let removed = ordered_map::remove(start, "a")
  let back = ordered_map::insert(removed, "a", 1)
  assert ordered_map::keys(removed) == ["b", "c"]
  assert ordered_map::keys(back) == ["b", "c", "a"]
  assert back != start
  assert ordered_map::remove(start, "z") == start
}
```

### `upsert`

```cheby
pub fn upsert<K, V>(m: OrderedMap<K, V>, key: K, update: fn(Option<V>) -> V) -> OrderedMap<K, V>
```

Calls `update` once, with `Some` of the value stored under `key` or `None` when there is none, and stores its result under `key`. A key already present keeps its position, and a new key goes at the end (D-340).

- **Cost:** O(log n), plus the call to `update`. Required (D-300).

```cheby
test "upsert keeps the position of an existing key" {
  fn bump(previous: Option<Int>) -> Int {
    case previous {
      Some(n) => n + 1
      None => 1
    }
  }
  let words = ["to", "be", "or", "not", "to", "be"]
  let counts = list::fold(words, ordered_map::new(), fn(acc, word) { ordered_map::upsert(acc, word, bump) })
  assert ordered_map::to_list(counts) == [("to", 2), ("be", 2), ("or", 1), ("not", 1)]
}
```

### `merge`

```cheby
pub fn merge<K, V>(m: OrderedMap<K, V>, other: OrderedMap<K, V>) -> OrderedMap<K, V>
```

Returns the entries of both maps, as if each entry of `other` were inserted into `m` in `other`'s order. On a shared key the value from `other` wins and the key keeps its position in `m`. Keys only in `other` follow, in `other`'s order (D-340).

- **Cost:** O(k log (n + k)), where k is the size of `other`. Required (D-300).

```cheby
test "merge appends new keys and replaces shared ones in place" {
  let base = ordered_map::from_list([("b", 1), ("a", 2)])
  let changes = ordered_map::from_list([("c", 30), ("b", 10)])
  assert ordered_map::to_list(ordered_map::merge(base, changes)) == [("b", 10), ("a", 2), ("c", 30)]
}
```

## Iterating

### `keys`

```cheby
pub fn keys<K, V>(m: OrderedMap<K, V>) -> List<K>
```

Returns the keys in the map's order.

- **Cost:** O(n), required (D-300).

```cheby
test "keys come in insertion order" {
  let scores = ordered_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert ordered_map::keys(scores) == ["b", "c", "a"]
}
```

### `values`

```cheby
pub fn values<K, V>(m: OrderedMap<K, V>) -> List<V>
```

Returns the values in the order of their keys.

- **Cost:** O(n), required (D-300).

```cheby
test "values follow the order of their keys" {
  let scores = ordered_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert ordered_map::values(scores) == [2, 3, 1]
}
```

### `to_list`

```cheby
pub fn to_list<K, V>(m: OrderedMap<K, V>) -> List<(K, V)>
```

Returns the entries as pairs, in the map's order. `ordered_map::from_list(ordered_map::to_list(m))` is `m`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives the entries in insertion order" {
  let scores = ordered_map::from_list([("b", 2), ("a", 1)])
  assert ordered_map::to_list(scores) == [("b", 2), ("a", 1)]
}
```

### `fold`

```cheby
pub fn fold<K, V, A>(m: OrderedMap<K, V>, initial: A, f: fn(A, K, V) -> A) -> A
```

Combines the entries in the map's order, starting from `initial` and calling `f` once per entry.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits the keys in insertion order" {
  let scores = ordered_map::from_list([("b", 2), ("c", 3), ("a", 1)])
  assert ordered_map::fold(scores, "", fn(text, key, _) { text + key }) == "bca"
}
```

### `each`

```cheby
pub fn each<K, V>(m: OrderedMap<K, V>, f: fn(K, V))
```

Calls `f` once per entry, in the map's order, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each calls the function once per entry" {
  let lengths = ordered_map::from_list([("bb", 2), ("a", 1)])
  ordered_map::each(lengths, fn(key, value) { assert string::length(key) == value })
}
```

## Transforming

### `map_values`

```cheby
pub fn map_values<K, V, W>(m: OrderedMap<K, V>, f: fn(K, V) -> W) -> OrderedMap<K, W>
```

Returns a map with the same keys in the same order, each value replaced by `f` of its key and value. `f` is called once per entry, in the map's order.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "map_values keeps the keys and their order" {
  let counts = ordered_map::from_list([("b", 2), ("a", 1)])
  let labels = ordered_map::map_values(counts, fn(key, value) { "{key}={value}" })
  assert ordered_map::to_list(labels) == [("b", "b=2"), ("a", "a=1")]
}
```

### `filter`

```cheby
pub fn filter<K, V>(m: OrderedMap<K, V>, keep: fn(K, V) -> Bool) -> OrderedMap<K, V>
```

Returns the entries for which `keep` returns `True`, in the same order. `keep` is called once per entry, in the map's order.

- **Cost:** O(n log n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps the matching entries in order" {
  let counts = ordered_map::from_list([("c", 3), ("b", 2), ("a", 1)])
  let odd = ordered_map::filter(counts, fn(_, value) { value % 2 == 1 })
  assert ordered_map::keys(odd) == ["c", "a"]
}
```

## Text

### `show`

```cheby
pub fn show<K: Show, V: Show>(m: OrderedMap<K, V>) -> String
```

Makes `OrderedMap<K, V>` satisfy `Show` when `K` and `V` do (§8.8, D-064, D-344). The text follows D-355: `ordered_map::from_list([`, the entries in the map's order as pairs written with `show`, and `])`.

- **Cost:** O(n), plus the `show` of every key and value. Informative.

```cheby
test "an ordered map shows its entries in order" {
  let scores = ordered_map::from_list([("b", 2), ("a", 1)])
  assert ordered_map::show(scores) == "ordered_map::from_list([(b, 2), (a, 1)])"
  assert "{scores}" == "ordered_map::from_list([(b, 2), (a, 1)])"
}
```
