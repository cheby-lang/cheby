# `std::set`

The hashed set `Set<T>` (§3.8, D-039). `Set` is not in the prelude: `import std::set` brings in the module and `import std::set::{Set}` the type (§3.2). This module is where the type is declared (§8.2):

```cheby
pub type Set<T>
```

`Set` is opaque. It is a persistent hash array mapped trie, so every update returns a new set, and the old one stays usable and shares most of its nodes with the new one (D-039).

- **Elements.** Elements need no interface. They are compared with `==` and hashed with the built-in structural hash (§3.13).
- **Order.** `to_list`, `fold`, `each` and the callbacks of `filter` and `map` visit the elements in an order that depends on a per-process random seed and changes between runs (D-093). Programs that print or compare that order sort first, for example with [`to_sorted_list`](#to_sorted_list).
- **Kernels.** `new`, `from_list`, `size`, `is_empty`, `contains`, `insert`, `remove` and `to_list` are runtime kernels, and their costs are required (D-274, D-300). The other functions are written in Cheby over them, and their costs are informative. `insert` and `remove` update the set in place when it is unique (§9.3).
- **Equality, hashing and printing.** Two sets are `==` when they hold the same elements, in any order, and equal sets hash equally (D-093, §3.13). Debug printing writes `set::from_list([…])` with the elements sorted by their debug text, so it is the same on every run (D-344, D-382).
- **Interfaces.** `Set` satisfies neither `Show` nor `Compare`, because its order changes between runs (§8.8, D-344).
- **Costs.** For a set of n elements, a lookup or an update takes O(log n), effectively constant.

```cheby
test "sets compare by contents and print sorted" {
  let words = set::from_list(["pear", "apple", "pear"])
  assert words == set::from_list(["apple", "pear"])
  assert "{words:?}" == "set::from_list([\"apple\", \"pear\"])"
}
```

## Building

### `new`

```cheby
pub fn new<T>() -> Set<T>
```

Returns an empty set.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = set::new::<Int>()
  assert set::is_empty(empty)
  assert empty == set::from_list([])
}
```

### `from_list`

```cheby
pub fn from_list<T>(items: List<T>) -> Set<T>
```

Returns a set with the elements of `items`, each once.

- **Cost:** O(n log n), effectively O(n), where n is the length of `items`. Required (D-300).

```cheby
test "from_list drops repeats" {
  let digits = set::from_list([3, 1, 3, 2])
  assert set::size(digits) == 3
  assert set::to_sorted_list(digits) == [1, 2, 3]
}
```

## Size

### `size`

```cheby
pub fn size<T>(s: Set<T>) -> Int
```

Returns the number of elements (D-301).

- **Cost:** O(1), required (D-300).

```cheby
test "size counts elements" {
  assert set::size(set::from_list(["a", "b", "a"])) == 2
  assert set::size(set::new::<String>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<T>(s: Set<T>) -> Bool
```

Tells whether the set has no elements.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty set" {
  assert set::is_empty(set::new::<Int>())
  assert !set::is_empty(set::from_list([0]))
}
```

## Lookup

### `contains`

```cheby
pub fn contains<T>(s: Set<T>, item: T) -> Bool
```

Tells whether `item` is in the set.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "contains looks an element up" {
  let seen = set::from_list(["/", "/a"])
  assert set::contains(seen, "/a")
  assert !set::contains(seen, "/b")
}
```

## Updating

### `insert`

```cheby
pub fn insert<T>(s: Set<T>, item: T) -> Set<T>
```

Returns the set with `item` added. The set is returned unchanged when `item` is already in it.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "insert adds an element once" {
  let start = set::from_list([1])
  let grown = set::insert(set::insert(start, 2), 2)
  assert grown == set::from_list([1, 2])
  assert start == set::from_list([1])
  assert list::fold([3, 4, 3], start, set::insert) == set::from_list([1, 3, 4])
}
```

### `remove`

```cheby
pub fn remove<T>(s: Set<T>, item: T) -> Set<T>
```

Returns the set without `item`. The set is returned unchanged when `item` is not in it.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "remove drops an element and ignores a missing one" {
  let held = set::from_list([1, 2])
  assert set::remove(held, 1) == set::from_list([2])
  assert set::remove(held, 5) == held
}
```

## Iterating

### `to_list`

```cheby
pub fn to_list<T>(s: Set<T>) -> List<T>
```

Returns the elements, in the set's per-process order (D-093). `set::from_list(set::to_list(s))` is `s`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives every element once" {
  let digits = set::from_list([2, 1, 2])
  assert list::sort(set::to_list(digits)) == [1, 2]
  assert set::from_list(set::to_list(digits)) == digits
}
```

### `to_sorted_list`

```cheby
pub fn to_sorted_list<T: Compare>(s: Set<T>) -> List<T>
```

Returns the elements sorted by `compare`, so the result is the same on every run (D-093, D-344). Elements that compare `Equal` without being `==` come in an order that may change between runs, which only a `compare` that disagrees with `==` can cause (§8.7).

- **Cost:** O(n log n) calls to `compare`. Informative.

```cheby
test "to_sorted_list orders the elements" {
  assert set::to_sorted_list(set::from_list(["b", "c", "a"])) == ["a", "b", "c"]
  assert set::to_sorted_list(set::new::<Int>()) == []
}
```

### `fold`

```cheby
pub fn fold<T, A>(s: Set<T>, initial: A, f: fn(A, T) -> A) -> A
```

Combines the elements, starting from `initial` and calling `f` once per element, in the set's per-process order (D-093).

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "fold visits every element once" {
  let digits = set::from_list([1, 2, 3])
  assert set::fold(digits, 0, int::add) == 6
  let grown = set::fold(digits, digits, fn(found, n) { set::insert(found, n * 10) })
  assert grown == set::from_list([1, 2, 3, 10, 20, 30])
}
```

### `each`

```cheby
pub fn each<T>(s: Set<T>, f: fn(T))
```

Calls `f` once per element, in the set's per-process order (D-093), for its effects.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "each calls the function once per element" {
  set::each(set::from_list([1, 2, 3]), fn(n) { assert n > 0 })
}
```

## Transforming

### `filter`

```cheby
pub fn filter<T>(s: Set<T>, keep: fn(T) -> Bool) -> Set<T>
```

Returns the elements for which `keep` returns `True`. `keep` is called once per element.

- **Cost:** O(n), plus the calls to `keep`. Informative.

```cheby
test "filter keeps the matching elements" {
  let digits = set::from_list([1, 2, 3, 4])
  assert set::filter(digits, fn(n) { n % 2 == 0 }) == set::from_list([2, 4])
}
```

### `map`

```cheby
pub fn map<T, U>(s: Set<T>, f: fn(T) -> U) -> Set<U>
```

Returns the set of the results of `f`. `f` is called once per element, and results that are `==` become one element.

- **Cost:** O(n log n), effectively O(n), plus the calls to `f`. Informative.

```cheby
test "map can merge elements" {
  let digits = set::from_list([1, 2, 3, 4])
  assert set::map(digits, fn(n) { n / 2 }) == set::from_list([0, 1, 2])
}
```

## Combining

### `union`

```cheby
pub fn union<T>(s: Set<T>, other: Set<T>) -> Set<T>
```

Returns the elements that are in `s` or in `other`.

- **Cost:** O(k log (n + k)), effectively O(k), where k is the size of `other`. Informative.

```cheby
test "union joins two sets" {
  let low = set::from_list([1, 2, 3])
  let high = set::from_list([3, 4])
  assert set::union(low, high) == set::from_list([1, 2, 3, 4])
}
```

### `intersection`

```cheby
pub fn intersection<T>(s: Set<T>, other: Set<T>) -> Set<T>
```

Returns the elements of `s` that are also in `other`.

- **Cost:** O(n log k), effectively O(n), where k is the size of `other`. Informative.

```cheby
test "intersection keeps the shared elements" {
  let low = set::from_list([1, 2, 3])
  let high = set::from_list([2, 3, 4])
  assert set::intersection(low, high) == set::from_list([2, 3])
}
```

### `difference`

```cheby
pub fn difference<T>(s: Set<T>, other: Set<T>) -> Set<T>
```

Returns the elements of `s` that are not in `other`.

- **Cost:** O(n log k), effectively O(n), where k is the size of `other`. Informative.

```cheby
test "difference removes the elements of the second set" {
  let low = set::from_list([1, 2, 3])
  let high = set::from_list([2, 3, 4])
  assert set::difference(low, high) == set::from_list([1])
}
```

### `is_subset`

```cheby
pub fn is_subset<T>(s: Set<T>, other: Set<T>) -> Bool
```

Tells whether every element of `s` is in `other`. It stops at the first element that is not.

- **Cost:** O(n log k), effectively O(n), where k is the size of `other`. Informative.

```cheby
test "is_subset checks every element" {
  let small = set::from_list([1, 2])
  let large = set::from_list([1, 2, 3])
  assert set::is_subset(small, large)
  assert !set::is_subset(large, small)
  assert set::is_subset(set::new(), small)
}
```
