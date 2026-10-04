# `std::ordered_set`

A persistent set that keeps its elements in the order they were first inserted (D-337, D-339). It is [`std::ordered_map`](ordered_map.md) without values: the counterpart of `Set` that iterates in the order of its input rather than in an order that changes between runs (D-093).

```cheby
pub type OrderedSet<T>
```

`OrderedSet` is opaque. Values are immutable, so every update returns a new set, and the old one stays usable.

- **Order.** `to_list`, `fold`, `each`, `show` and debug printing go from the element inserted first to the element inserted last. Inserting an element that is already present keeps its position. Removing an element and inserting it again moves it to the end (D-340).
- **Elements.** Elements need no interface. They are compared with `==` and hashed with the built-in hash, as in `Set` (§3.13), so an element that contains a function or a handle panics when it is inserted or looked up (D-233).
- **Written in Cheby.** The module is written in Cheby, with no kernel of its own (D-336), using the same representation as `std::ordered_map`. Elements are hashed and compared by the runtime's built-in routines, and no `compare` of the element type is ever called.
- **Equality, hashing and printing.** The module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two ordered sets are `==` only when they hold the same elements in the same order, and their hash covers the order (D-343). Debug printing writes `ordered_set::from_list([…])` with the elements in order, each one debug-printed (D-344).
- **Interfaces.** `OrderedSet<T>` satisfies `Show` when `T` does (D-344). It does not satisfy `Compare`.
- **Costs.** For a set of n elements, a lookup takes O(log n), effectively constant as in `Set`, and an update, including `remove`, takes O(log n). Iterating takes O(n).

```cheby
test "ordered sets compare, hash and print by contents and order" {
  let abc = ordered_set::from_list(["a", "b", "c"])
  let cba = ordered_set::from_list(["c", "b", "a"])
  let grown = ordered_set::insert(ordered_set::from_list(["a", "b"]), "c")
  assert abc == grown
  assert abc != cba
  assert set::size(set::from_list([abc, grown, cba])) == 2
  assert "{cba:?}" == "ordered_set::from_list([\"c\", \"b\", \"a\"])"
}
```

## Building

### `new`

```cheby
pub fn new<T>() -> OrderedSet<T>
```

Returns an empty set.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = ordered_set::new::<Int>()
  assert ordered_set::is_empty(empty)
  assert ordered_set::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<T>(items: List<T>) -> OrderedSet<T>
```

Returns a set of the given items, as if each were inserted in turn, so a repeated item keeps the position of its first occurrence (D-340).

- **Cost:** O(n log n), where n is the length of `items`. Required (D-300).

```cheby
test "from_list drops duplicates and keeps first positions" {
  assert ordered_set::to_list(ordered_set::from_list(["pear", "apple", "pear"])) == ["pear", "apple"]
}
```

## Size

### `size`

```cheby
pub fn size<T>(s: OrderedSet<T>) -> Int
```

Returns the number of elements.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts distinct elements" {
  assert ordered_set::size(ordered_set::from_list([1, 2, 1])) == 2
  assert ordered_set::size(ordered_set::new::<Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<T>(s: OrderedSet<T>) -> Bool
```

Tells whether the set has no elements.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty set" {
  assert ordered_set::is_empty(ordered_set::new::<Int>())
  assert !ordered_set::is_empty(ordered_set::from_list([1]))
}
```

## Lookup

### `contains`

```cheby
pub fn contains<T>(s: OrderedSet<T>, item: T) -> Bool
```

Tells whether `item` is in the set.

- **Cost:** O(log n), effectively constant. Required (D-300).

```cheby
test "contains looks an element up" {
  let primes = ordered_set::from_list([5, 2, 3])
  assert ordered_set::contains(primes, 3)
  assert !ordered_set::contains(primes, 4)
}
```

### `first`

```cheby
pub fn first<T>(s: OrderedSet<T>) -> Option<T>
```

Returns the element that comes first in the set's order, the oldest element still present.

- **Fails:** `None` when the set is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the oldest element" {
  let seen = ordered_set::from_list([5, 2, 8])
  assert ordered_set::first(seen) == Some(5)
  assert ordered_set::first(ordered_set::remove(seen, 5)) == Some(2)
  assert ordered_set::first(ordered_set::new::<Int>()) == None
}
```

### `last`

```cheby
pub fn last<T>(s: OrderedSet<T>) -> Option<T>
```

Returns the element that comes last in the set's order, the one inserted most recently.

- **Fails:** `None` when the set is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the newest element" {
  assert ordered_set::last(ordered_set::from_list([5, 2, 8])) == Some(8)
  assert ordered_set::last(ordered_set::new::<Int>()) == None
}
```

## Updating

### `insert`

```cheby
pub fn insert<T>(s: OrderedSet<T>, item: T) -> OrderedSet<T>
```

Returns the set with `item` added at the end. When `item` is already present, the set is returned unchanged, so the element keeps its position (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert adds at the end and keeps an existing position" {
  let start = ordered_set::from_list([3, 1])
  let added = ordered_set::insert(start, 2)
  assert ordered_set::to_list(added) == [3, 1, 2]
  assert ordered_set::insert(added, 3) == added
  assert ordered_set::to_list(start) == [3, 1]
}
```

### `remove`

```cheby
pub fn remove<T>(s: OrderedSet<T>, item: T) -> OrderedSet<T>
```

Returns the set without `item`. The other elements keep their order. The set is returned unchanged when `item` is not in it. A removed element that is inserted again goes at the end (D-340).

- **Cost:** O(log n), required (D-300).

```cheby
test "remove then insert moves an element to the end" {
  let start = ordered_set::from_list([1, 2, 3])
  let removed = ordered_set::remove(start, 1)
  let back = ordered_set::insert(removed, 1)
  assert ordered_set::to_list(removed) == [2, 3]
  assert ordered_set::to_list(back) == [2, 3, 1]
  assert back != start
  assert ordered_set::remove(start, 9) == start
}
```

## Iterating

### `to_list`

```cheby
pub fn to_list<T>(s: OrderedSet<T>) -> List<T>
```

Returns the elements in the set's order. `ordered_set::from_list(ordered_set::to_list(s))` is `s`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives the elements in insertion order" {
  assert ordered_set::to_list(ordered_set::from_list(["b", "c", "a"])) == ["b", "c", "a"]
}
```

### `fold`

```cheby
pub fn fold<T, A>(s: OrderedSet<T>, initial: A, f: fn(A, T) -> A) -> A
```

Combines the elements in the set's order, starting from `initial` and calling `f` once per element.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits the elements in insertion order" {
  let letters = ordered_set::from_list(["b", "c", "a"])
  assert ordered_set::fold(letters, "", fn(text, letter) { text + letter }) == "bca"
}
```

### `each`

```cheby
pub fn each<T>(s: OrderedSet<T>, f: fn(T))
```

Calls `f` once per element, in the set's order, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each calls the function once per element" {
  ordered_set::each(ordered_set::from_list([6, 2, 4]), fn(n) { assert n % 2 == 0 })
}
```

## Transforming

### `filter`

```cheby
pub fn filter<T>(s: OrderedSet<T>, keep: fn(T) -> Bool) -> OrderedSet<T>
```

Returns the elements for which `keep` returns `True`, in the same order. `keep` is called once per element, in the set's order.

- **Cost:** O(n log n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps the matching elements in order" {
  let digits = ordered_set::from_list([4, 1, 2, 3])
  assert ordered_set::to_list(ordered_set::filter(digits, fn(n) { n % 2 == 0 })) == [4, 2]
}
```

### `map`

```cheby
pub fn map<T, U>(s: OrderedSet<T>, f: fn(T) -> U) -> OrderedSet<U>
```

Returns the set of the results of `f`, in the order of the elements they came from. `f` is called once per element, in the set's order, and a result that is already present keeps the position where it first appeared.

- **Cost:** O(n log n), plus the calls to `f`. Required (D-300).

```cheby
test "map can merge elements" {
  let digits = ordered_set::from_list([4, 1, 2, 3])
  assert ordered_set::to_list(ordered_set::map(digits, fn(n) { n / 2 })) == [2, 0, 1]
}
```

## Combining

### `union`

```cheby
pub fn union<T>(s: OrderedSet<T>, other: OrderedSet<T>) -> OrderedSet<T>
```

Returns the elements of `s` in their order, followed by the elements of `other` that are not in `s`, in `other`'s order.

- **Cost:** O(k log (n + k)), where k is the size of `other`. Required (D-300).

```cheby
test "union appends the new elements of the second set" {
  let first_set = ordered_set::from_list([3, 1])
  let second_set = ordered_set::from_list([2, 1, 4])
  assert ordered_set::to_list(ordered_set::union(first_set, second_set)) == [3, 1, 2, 4]
}
```

### `intersection`

```cheby
pub fn intersection<T>(s: OrderedSet<T>, other: OrderedSet<T>) -> OrderedSet<T>
```

Returns the elements of `s` that are also in `other`, in `s`'s order.

- **Cost:** O(n log n), required (D-300).

```cheby
test "intersection keeps the order of the first set" {
  let first_set = ordered_set::from_list([3, 2, 1])
  let second_set = ordered_set::from_list([1, 2, 4])
  assert ordered_set::to_list(ordered_set::intersection(first_set, second_set)) == [2, 1]
}
```

### `difference`

```cheby
pub fn difference<T>(s: OrderedSet<T>, other: OrderedSet<T>) -> OrderedSet<T>
```

Returns the elements of `s` that are not in `other`, in `s`'s order.

- **Cost:** O(n log n), required (D-300).

```cheby
test "difference removes the elements of the second set" {
  let first_set = ordered_set::from_list([3, 2, 1])
  let second_set = ordered_set::from_list([2, 4])
  assert ordered_set::to_list(ordered_set::difference(first_set, second_set)) == [3, 1]
}
```

### `is_subset`

```cheby
pub fn is_subset<T>(s: OrderedSet<T>, other: OrderedSet<T>) -> Bool
```

Tells whether every element of `s` is in `other`. Order is ignored.

- **Cost:** O(n log k), effectively O(n), where k is the size of `other`. Required (D-300).

```cheby
test "is_subset ignores order" {
  let small = ordered_set::from_list([2, 1])
  let large = ordered_set::from_list([1, 2, 3])
  assert ordered_set::is_subset(small, large)
  assert !ordered_set::is_subset(large, small)
  assert ordered_set::is_subset(ordered_set::new(), small)
}
```

## Text

### `show`

```cheby
pub fn show<T: Show>(s: OrderedSet<T>) -> String
```

Makes `OrderedSet<T>` satisfy `Show` when `T` does (§8.8, D-064, D-344). The text follows D-355: `ordered_set::from_list([`, the elements in order written with `show`, and `])`.

- **Cost:** O(n), plus the `show` of every element. Informative.

```cheby
test "an ordered set shows its elements in order" {
  let words = ordered_set::from_list(["b", "a"])
  assert ordered_set::show(words) == "ordered_set::from_list([b, a])"
  assert "{words}" == "ordered_set::from_list([b, a])"
}
```
