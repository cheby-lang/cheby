# `std::sorted_set`

A persistent set that keeps its elements in order (D-333, D-337). It is the tree of [`std::sorted_map`](sorted_map.md) without values: the deterministic counterpart of `Set`, whose iteration order changes between runs (D-093).

```cheby
pub type SortedSet<T>
```

`SortedSet` is opaque. Values are immutable, so every update returns a new set, and the old one stays usable.

- **Order.** `to_list`, `fold`, `try_fold`, `each`, `any`, `all`, `find`, `show` and debug printing go from the smallest element to the largest, by the element type's `compare`, and `at` counts positions in that order.
- **Elements.** Elements must satisfy `Compare`, and every public function has the bound `T: Compare` (D-385). The hooks of equality, hashing and debug printing take none and never call `compare` (D-845). Two elements are the same element when `compare` returns `Equal`, even if they are not `==`. Inserting such an element keeps the one already stored (D-341). A `compare` that disagrees with `==` is a program bug (§8.7). The second example of [`insert`](#insert) shows what the set does with one.
- **Written in Cheby.** The set is a persistent balanced tree written in Cheby, with no runtime kernel (D-336). Every comparison is an ordinary call to the element type's `compare` through the bound (§8.5), so a panic inside it unwinds through this module's functions like any other panic. The number and order of the calls to `compare` are not specified, but they are the same on every target (D-232, D-354). Every node of the tree keeps the size of its subtree, which `size` and `at` need for their required costs. If the tree later moves into a runtime kernel (D-336), the API and the costs below stay the same, so that tree keeps subtree sizes too (D-P04).
- **Equality, hashing and printing.** Sets with the same elements can have trees of different shapes, so the module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two sorted sets are `==` when they hold the same elements, compared with `==`, and equal sets hash equally (D-343). Debug printing writes `sorted_set::from_list([…])` with the elements in order, each one debug-printed (D-344).
- **Interfaces.** `SortedSet<T>` satisfies `Show` when `T` does (D-344). It does not satisfy `Compare`.
- **Costs.** For a set of n elements, a lookup or an update takes O(log n) time and calls `compare` O(log n) times. Iterating takes O(n) and never calls `compare`, and a walk that stops after k elements, such as `find`, takes O(log n + k).

```cheby
test "sorted sets compare, hash and print by contents" {
  let built = sorted_set::from_list([3, 1, 2])
  let grown = sorted_set::insert(sorted_set::from_list([2, 3]), 1)
  assert built == grown
  assert built != sorted_set::from_list([1, 2])
  assert set::size(set::from_list([built, grown])) == 1
  assert "{built:?}" == "sorted_set::from_list([1, 2, 3])"
}
```

## Building

### `new`

```cheby
pub fn new<T: Compare>() -> SortedSet<T>
```

Returns an empty set.

- **Cost:** O(1), required (D-300).

```cheby
test "new is empty" {
  let empty = sorted_set::new::<Int>()
  assert sorted_set::is_empty(empty)
  assert sorted_set::to_list(empty) == []
}
```

### `from_list`

```cheby
pub fn from_list<T: Compare>(items: List<T>) -> SortedSet<T>
```

Returns a set of the given items, as if each were inserted in turn, so of several items that compare `Equal` the first is kept (D-341).

- **Cost:** O(n log n), where n is the length of `items`. Required (D-300).

```cheby
test "from_list sorts and drops duplicates" {
  assert sorted_set::to_list(sorted_set::from_list(["pear", "apple", "pear"])) == ["apple", "pear"]
}
```

## Size

### `size`

```cheby
pub fn size<T: Compare>(s: SortedSet<T>) -> Int
```

Returns the number of elements.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts distinct elements" {
  assert sorted_set::size(sorted_set::from_list([1, 2, 1])) == 2
  assert sorted_set::size(sorted_set::new::<Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<T: Compare>(s: SortedSet<T>) -> Bool
```

Tells whether the set has no elements.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty set" {
  assert sorted_set::is_empty(sorted_set::new::<Int>())
  assert !sorted_set::is_empty(sorted_set::from_list([1]))
}
```

## Lookup

### `contains`

```cheby
pub fn contains<T: Compare>(s: SortedSet<T>, item: T) -> Bool
```

Tells whether some element compares `Equal` to `item`.

- **Cost:** O(log n), required (D-300).

```cheby
test "contains looks an element up" {
  let primes = sorted_set::from_list([2, 3, 5])
  assert sorted_set::contains(primes, 3)
  assert !sorted_set::contains(primes, 4)
}
```

### `first`

```cheby
pub fn first<T: Compare>(s: SortedSet<T>) -> Option<T>
```

Returns the smallest element.

- **Fails:** `None` when the set is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first is the smallest element" {
  assert sorted_set::first(sorted_set::from_list([5, 2, 8])) == Some(2)
  assert sorted_set::first(sorted_set::new::<Int>()) == None
}
```

### `last`

```cheby
pub fn last<T: Compare>(s: SortedSet<T>) -> Option<T>
```

Returns the largest element.

- **Fails:** `None` when the set is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last is the largest element" {
  assert sorted_set::last(sorted_set::from_list([5, 2, 8])) == Some(8)
  assert sorted_set::last(sorted_set::new::<Int>()) == None
}
```

### `range`

```cheby
pub fn range<T: Compare>(s: SortedSet<T>, start: T, end: T) -> List<T>
```

Returns the elements that are at least `start` and less than `end`, in order (D-319). `start` and `end` need not be elements of the set. The list is empty when `end` is not greater than `start`.

- **Cost:** O(log n + k), where k is the number of elements returned. Required (D-300).

```cheby
test "range includes start and excludes end" {
  let odd = sorted_set::from_list([1, 3, 5, 7])
  assert sorted_set::range(odd, 3, 7) == [3, 5]
  assert sorted_set::range(odd, 2, 4) == [3]
  assert sorted_set::range(odd, 7, 3) == []
}
```

### `split`

```cheby
pub fn split<T: Compare>(s: SortedSet<T>, item: T) -> (SortedSet<T>, Bool, SortedSet<T>)
```

Splits `s` at `item`. It returns the set of the elements less than `item`, whether some element compares `Equal` to `item`, and the set of the elements greater than `item`. `item` need not be in the set.

- **Cost:** O(log n), and it calls `compare` O(log n) times. Required (D-300).

```cheby
test "split cuts the set at an element" {
  let odd = sorted_set::from_list([1, 3, 5, 7])
  let (below, present, above) = sorted_set::split(odd, 5)
  assert sorted_set::to_list(below) == [1, 3]
  assert present
  assert sorted_set::to_list(above) == [7]
  let (smaller, missing, larger) = sorted_set::split(odd, 4)
  assert sorted_set::last(smaller) == Some(3)
  assert !missing
  assert sorted_set::first(larger) == Some(5)
}
```

As with `sorted_map::split`, `split` with `first`, `last` and `size` gives the nearest elements on either side of `item`, and the rank of `item`, the number of elements less than it, which is `size` of the lower part (D-P04).

### `at`

```cheby
pub fn at<T: Compare>(s: SortedSet<T>, index: Int) -> Option<T>
```

Returns the element at `index` in order, counting from 0, so `at(s, 0)` is `first(s)`.

- **Fails:** `None` when `index` is outside `0` to `size - 1`, negative indexes included (D-318).
- **Cost:** O(log n), required (D-300). It never calls `compare`.

```cheby
test "at counts from the smallest element" {
  let odd = sorted_set::from_list([7, 1, 5, 3])
  assert sorted_set::at(odd, 0) == Some(1)
  assert sorted_set::at(odd, 3) == Some(7)
  assert sorted_set::at(odd, 4) == None
  assert sorted_set::at(odd, -1) == None
}
```

## Updating

### `insert`

```cheby
pub fn insert<T: Compare>(s: SortedSet<T>, item: T) -> SortedSet<T>
```

Returns the set with `item` added. When an element that compares `Equal` to `item` is already present, the set is returned unchanged and keeps that element (D-341).

- **Cost:** O(log n), required (D-300).

```cheby
test "insert adds an element once" {
  let start = sorted_set::from_list([1, 3])
  let added = sorted_set::insert(start, 2)
  assert sorted_set::to_list(added) == [1, 2, 3]
  assert sorted_set::insert(added, 2) == added
  assert sorted_set::to_list(start) == [1, 3]
}
```

The second example uses an element type whose `compare` looks only at `id`, so two badges with the same `id` are the same element.

```cheby
import std::int
import std::sorted_set

/// An element ordered by `id` alone. Two badges with the same `id` and
/// different labels compare `Equal` without being `==`.
type Badge { id: Int, label: String }

fn compare(a: Badge, b: Badge) -> Order {
  int::compare(a.id, b.id)
}

test "elements that compare Equal are the same element" {
  let stored = Badge { id: 1, label: "stored" }
  let later = Badge { id: 1, label: "later" }
  let badges = sorted_set::insert(sorted_set::from_list([stored]), later)
  assert sorted_set::to_list(badges) == [stored]
  assert sorted_set::contains(badges, Badge { id: 1, label: "other" })
}
```

### `remove`

```cheby
pub fn remove<T: Compare>(s: SortedSet<T>, item: T) -> SortedSet<T>
```

Returns the set without the element that compares `Equal` to `item`. The set is returned unchanged when there is no such element.

- **Cost:** O(log n), required (D-300).

```cheby
test "remove drops an element and ignores a missing one" {
  let digits = sorted_set::from_list([1, 2, 3])
  assert sorted_set::remove(digits, 2) == sorted_set::from_list([1, 3])
  assert sorted_set::remove(digits, 9) == digits
}
```

## Iterating

### `to_list`

```cheby
pub fn to_list<T: Compare>(s: SortedSet<T>) -> List<T>
```

Returns the elements in order. `sorted_set::from_list(sorted_set::to_list(s))` is `s`.

- **Cost:** O(n), required (D-300).

```cheby
test "to_list gives the elements in order" {
  assert sorted_set::to_list(sorted_set::from_list(["b", "c", "a"])) == ["a", "b", "c"]
}
```

### `fold`

```cheby
pub fn fold<T: Compare, A>(s: SortedSet<T>, initial: A, f: fn(A, T) -> A) -> A
```

Combines the elements in order, starting from `initial` and calling `f` once per element.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "fold visits the elements in order" {
  let letters = sorted_set::from_list(["b", "c", "a"])
  assert sorted_set::fold(letters, "", fn(text, letter) { text + letter }) == "abc"
}
```

### `each`

```cheby
pub fn each<T: Compare>(s: SortedSet<T>, f: fn(T))
```

Calls `f` once per element, in order, for its effects.

- **Cost:** O(n), plus the calls to `f`. Required (D-300).

```cheby
test "each calls the function once per element" {
  sorted_set::each(sorted_set::from_list([2, 4, 6]), fn(n) { assert n % 2 == 0 })
}
```

### `try_fold`

```cheby
pub fn try_fold<T: Compare, A, E>(s: SortedSet<T>, initial: A, f: fn(A, T) -> Result<A, E>) -> Result<A, E>
```

Combines the elements like `fold`, in order, with a callback that can fail. It calls `f` once per element, stops calling it at the first `Err` and returns that `Err`, and otherwise returns `Ok` of the last result, which is `Ok(initial)` for an empty set (D-P01).

- **Fails:** the first `Err` that `f` returns.
- **Cost:** O(n), plus the calls to `f`, and O(log n + k) when it stops after k elements. Informative. It never calls `compare`.

```cheby
test "try_fold stops at the first error" {
  fn add_positive(total: Int, n: Int) -> Result<Int, Int> {
    case n > 0 {
      True => Ok(total + n)
      False => Err(n)
    }
  }
  assert sorted_set::try_fold(sorted_set::from_list([3, 1, 2]), 0, add_positive) == Ok(6)
  assert sorted_set::try_fold(sorted_set::from_list([-1, -2, 3]), 0, add_positive) == Err(-2)
  assert sorted_set::try_fold(sorted_set::new(), 7, add_positive) == Ok(7)
  let stops = sorted_set::try_fold(sorted_set::from_list([1, 2, 3]), 0, fn(total, n) {
    case n {
      3 => panic as "not reached"
      2 => Err(total)
      _ => Ok(total + n)
    }
  })
  assert stops == Err(1)
}
```

`try_fold`, `any`, `all` and `find` walk the tree and stop where the callback does, so the elements after the stop are never visited (D-P03).

### `any`

```cheby
pub fn any<T: Compare>(s: SortedSet<T>, predicate: fn(T) -> Bool) -> Bool
```

Tells whether `predicate` returns `True` for some element. It calls `predicate` in order, stops at the first such element, and is `False` for an empty set.

- **Cost:** O(n), plus the calls to `predicate`, and O(log n + k) when it stops after k elements. Informative. It never calls `compare`.

```cheby
test "any stops at the first match" {
  let digits = sorted_set::from_list([3, 1, 2])
  assert sorted_set::any(digits, fn(n) {
    case n {
      3 => panic as "not reached"
      _ => n == 2
    }
  })
  assert !sorted_set::any(digits, fn(n) { n > 3 })
  assert !sorted_set::any(sorted_set::new::<Int>(), fn(_) { True })
}
```

### `all`

```cheby
pub fn all<T: Compare>(s: SortedSet<T>, predicate: fn(T) -> Bool) -> Bool
```

Tells whether `predicate` returns `True` for every element. It calls `predicate` in order, stops at the first element for which it returns `False`, and is `True` for an empty set.

- **Cost:** O(n), plus the calls to `predicate`, and O(log n + k) when it stops after k elements. Informative. It never calls `compare`.

```cheby
test "all needs every element to match" {
  let digits = sorted_set::from_list([3, 1, 2])
  assert sorted_set::all(digits, fn(n) { n > 0 })
  assert !sorted_set::all(digits, fn(n) { n < 2 })
  assert sorted_set::all(sorted_set::new::<Int>(), fn(_) { False })
}
```

### `find`

```cheby
pub fn find<T: Compare>(s: SortedSet<T>, predicate: fn(T) -> Bool) -> Option<T>
```

Returns the smallest element for which `predicate` returns `True`. It calls `predicate` in order and stops calling it at that element.

- **Fails:** `None` when no element matches.
- **Cost:** O(n), plus the calls to `predicate`, and O(log n + k) when it stops after k elements. Informative. It never calls `compare`.

```cheby
test "find returns the first match in order" {
  let digits = sorted_set::from_list([3, 1, 2])
  assert sorted_set::find(digits, fn(n) { n > 1 }) == Some(2)
  assert sorted_set::find(digits, fn(n) { n > 3 }) == None
}
```

## Transforming

### `filter`

```cheby
pub fn filter<T: Compare>(s: SortedSet<T>, keep: fn(T) -> Bool) -> SortedSet<T>
```

Returns the elements for which `keep` returns `True`. `keep` is called once per element, in order, and `compare` is not called, because the kept elements are already in order.

- **Cost:** O(n), plus the calls to `keep`. Required (D-300).

```cheby
test "filter keeps the matching elements" {
  let digits = sorted_set::from_list([1, 2, 3, 4])
  assert sorted_set::to_list(sorted_set::filter(digits, fn(n) { n % 2 == 0 })) == [2, 4]
}
```

### `map`

```cheby
pub fn map<T: Compare, U: Compare>(s: SortedSet<T>, f: fn(T) -> U) -> SortedSet<U>
```

Returns the set of the results of `f`. `f` is called once per element, in order, and results that compare `Equal` become one element, the first one produced (D-341).

- **Cost:** O(n log n), plus the calls to `f`. Required (D-300).

```cheby
test "map can merge elements" {
  let digits = sorted_set::from_list([1, 2, 3, 4])
  assert sorted_set::to_list(sorted_set::map(digits, fn(n) { n / 2 })) == [0, 1, 2]
}
```

## Combining

### `union`

```cheby
pub fn union<T: Compare>(s: SortedSet<T>, other: SortedSet<T>) -> SortedSet<T>
```

Returns the elements that are in `s` or in `other`. Of two elements that compare `Equal`, the one from `s` is kept (D-341).

- **Cost:** O(k log (n + k)), where k is the size of `other`. Required (D-300).

```cheby
test "union joins two sets" {
  let low = sorted_set::from_list([1, 2, 3])
  let high = sorted_set::from_list([3, 4])
  assert sorted_set::to_list(sorted_set::union(low, high)) == [1, 2, 3, 4]
}
```

### `intersection`

```cheby
pub fn intersection<T: Compare>(s: SortedSet<T>, other: SortedSet<T>) -> SortedSet<T>
```

Returns the elements of `s` that are also in `other`.

- **Cost:** O(n log k), where k is the size of `other`. Required (D-300).

```cheby
test "intersection keeps the shared elements" {
  let low = sorted_set::from_list([1, 2, 3])
  let high = sorted_set::from_list([2, 3, 4])
  assert sorted_set::to_list(sorted_set::intersection(low, high)) == [2, 3]
}
```

### `difference`

```cheby
pub fn difference<T: Compare>(s: SortedSet<T>, other: SortedSet<T>) -> SortedSet<T>
```

Returns the elements of `s` that are not in `other`.

- **Cost:** O(n log k), where k is the size of `other`. Required (D-300).

```cheby
test "difference removes the elements of the second set" {
  let low = sorted_set::from_list([1, 2, 3])
  let high = sorted_set::from_list([2, 3, 4])
  assert sorted_set::to_list(sorted_set::difference(low, high)) == [1]
}
```

### `is_subset`

```cheby
pub fn is_subset<T: Compare>(s: SortedSet<T>, other: SortedSet<T>) -> Bool
```

Tells whether every element of `s` is in `other`.

- **Cost:** O(n log k), where k is the size of `other`. Required (D-300).

```cheby
test "is_subset checks every element" {
  let small = sorted_set::from_list([1, 2])
  let large = sorted_set::from_list([1, 2, 3])
  assert sorted_set::is_subset(small, large)
  assert !sorted_set::is_subset(large, small)
  assert sorted_set::is_subset(sorted_set::new(), small)
}
```

## Text

### `show`

```cheby
pub fn show<T: Compare + Show>(s: SortedSet<T>) -> String
```

Makes `SortedSet<T>` satisfy `Show` when `T` does (§8.8, D-064, D-344). The text follows D-355: `sorted_set::from_list([`, the elements in order written with `show`, and `])`.

- **Cost:** O(n), plus the `show` of every element. Informative.

```cheby
test "a sorted set shows its elements in order" {
  let words = sorted_set::from_list(["b", "a"])
  assert sorted_set::show(words) == "sorted_set::from_list([a, b])"
  assert "{words}" == "sorted_set::from_list([a, b])"
}
```
