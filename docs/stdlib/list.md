# `std::list`

Functions for the built-in `List<T>` (§3.8). `List` is in the prelude (§3.2), list literals and list patterns are syntax (§5.13, §6.6), and this module is where the type is declared (§8.2):

```cheby
pub type List<T>
```

A `List` is a persistent, immutable RRB-tree vector, not a linked list (D-039, ADR-0018). Reading or updating at an index, adding at either end, concatenating and slicing all take O(log n) at most (§3.8). The kernels, the functions that build, read or cut the tree, are implemented in the runtime, and the other functions are written in Cheby over them (D-274). The kernels' costs are required and the others' informative (D-300).

The kernels have no effects, so compile-time evaluation runs them, and they may be called while a constant is computed (D-360, §4.5).

The functions that return a changed list, such as `push`, `prepend`, `append` and `set`, update their first argument in place when it is unique (§9.3). A list built with `push` in a fold therefore costs amortized O(1) per element.

Lists compare with `==` element by element and debug-print as `[1, 2]` (§3.13). `List<T>` satisfies `Compare` and `Show` when `T` does (§8.7, §8.8, D-064, D-136).

## Building

### `new`

```cheby
pub fn new<T>() -> List<T>
```

Returns the empty list, the same as `[]`. It names the element type where `[]` cannot, as in `list::new::<Int>()`.

- **Cost:** O(1), required (D-300).

```cheby
test "new is the empty list" {
  assert list::new::<Int>() == []
  assert list::length(list::new::<String>()) == 0
}
```

### `range`

```cheby
pub fn range(start: Int, end: Int) -> List<Int>
```

Returns the integers from `start` up to but not including `end`, in increasing order, and `[]` when `end <= start` (D-319). `range(0, n)` gives the indexes of an `n`-element list.

- **Cost:** O(n), where n is the length of the result. Informative.

```cheby
test "range excludes its end" {
  assert list::range(0, 4) == [0, 1, 2, 3]
  assert list::range(-2, 1) == [-2, -1, 0]
  assert list::range(3, 3) == []
  assert list::range(5, 1) == []
}
```

### `repeat`

```cheby
pub fn repeat<T>(item: T, times: Int) -> List<T>
```

Returns a list of `times` copies of `item`.

- **Clamps:** `times` below 0 to 0 (D-298).
- **Cost:** O(n), where n is `times`. Informative.

```cheby
test "repeat clamps its count" {
  let negative = 0 - 2
  assert list::repeat("ab", 3) == ["ab", "ab", "ab"]
  assert list::repeat("ab", 0) == []
  assert list::repeat("ab", negative) == []
}
```

## Size and access

### `length`

```cheby
pub fn length<T>(xs: List<T>) -> Int
```

Returns the number of elements in `xs` (D-301).

- **Cost:** O(1), required (D-300).

```cheby
test "length counts the elements" {
  assert list::length([7, 8, 9]) == 3
  assert list::length(list::new::<Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<T>(xs: List<T>) -> Bool
```

Tells whether `xs` has no elements.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells whether there are elements" {
  assert list::is_empty(list::new::<Int>())
  assert !list::is_empty([0])
}
```

### `get`

```cheby
pub fn get<T>(xs: List<T>, index: Int) -> Option<T>
```

Returns the element at `index`, counting from 0.

- **Fails:** `None` when `index` is outside `0` to `length - 1`, including negative indexes (D-318).
- **Cost:** O(log n), required (D-300).

```cheby
test "get reads by position" {
  assert list::get([10, 20, 30], 0) == Some(10)
  assert list::get([10, 20, 30], 2) == Some(30)
  assert list::get([10, 20, 30], 3) == None
  assert list::get([10, 20, 30], -1) == None
}
```

### `first`

```cheby
pub fn first<T>(xs: List<T>) -> Option<T>
```

Returns the first element of `xs`.

- **Fails:** `None` when `xs` is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "first reads the front" {
  assert list::first([1, 2, 3]) == Some(1)
  assert list::first(list::new::<Int>()) == None
}
```

### `last`

```cheby
pub fn last<T>(xs: List<T>) -> Option<T>
```

Returns the last element of `xs`.

- **Fails:** `None` when `xs` is empty.
- **Cost:** O(log n), required (D-300).

```cheby
test "last reads the back" {
  assert list::last([1, 2, 3]) == Some(3)
  assert list::last(list::new::<Int>()) == None
}
```

## Adding and updating

### `push`

```cheby
pub fn push<T>(xs: List<T>, item: T) -> List<T>
```

Returns `xs` with `item` added at the end, the same as `[..xs, item]`.

- **Cost:** O(log n), required (D-300). Amortized O(1) when `xs` is unique (§9.3), informative.

```cheby
test "push adds at the end" {
  assert list::push([1, 2], 3) == [1, 2, 3]
  assert list::fold([1, 2, 3], [], list::push) == [1, 2, 3]
}
```

### `prepend`

```cheby
pub fn prepend<T>(xs: List<T>, item: T) -> List<T>
```

Returns `xs` with `item` added at the front, the same as `[item, ..xs]`. The subject comes first, as in every function here (D-296).

- **Cost:** O(log n), required (D-300). Amortized O(1) when `xs` is unique (§9.3), informative.

```cheby
test "prepend adds at the front" {
  assert list::prepend([2, 3], 1) == [1, 2, 3]
  assert list::fold([1, 2, 3], [], list::prepend) == [3, 2, 1]
}
```

### `append`

```cheby
pub fn append<T>(xs: List<T>, other: List<T>) -> List<T>
```

Returns the elements of `xs` followed by those of `other`, the same as `[..xs, ..other]`. The name matches `bytes::append` (D-316).

- **Cost:** O(log n), where n is the combined length, required (D-300).

```cheby
test "append concatenates two lists" {
  assert list::append([1, 2], [3]) == [1, 2, 3]
  assert list::append([1], []) == [1]
}
```

### `set`

```cheby
pub fn set<T>(xs: List<T>, index: Int, item: T) -> List<T>
```

Returns `xs` with the element at `index` replaced by `item`. When `index` is outside `0` to `length - 1`, including a negative index, it returns `xs` unchanged (D-318).

- **Cost:** O(log n), required (D-300).

```cheby
test "set replaces one element" {
  assert list::set([1, 2, 3], 1, 20) == [1, 20, 3]
  assert list::set([1, 2, 3], 3, 40) == [1, 2, 3]
  assert list::set([1, 2, 3], -1, 0) == [1, 2, 3]
}
```

`set` stays total. A caller who needs to know checks `index < list::length(xs)` first (D-377).

## Slicing

### `slice`

```cheby
pub fn slice<T>(xs: List<T>, start: Int, end: Int) -> List<T>
```

Returns the elements from `start` up to but not including `end` (D-319), sharing structure with `xs`.

- **Clamps:** `start` and `end` to `0` to `length`. The result is `[]` when `end <= start` (D-298).
- **Cost:** O(log n), required (D-300).

```cheby
test "slice cuts out a range" {
  assert list::slice([0, 1, 2, 3, 4], 1, 3) == [1, 2]
  assert list::slice([0, 1, 2], 2, 10) == [2]
  assert list::slice([0, 1, 2], 2, 1) == []
}
```

`slice` clamps like `bytes::slice` and `string::slice`, so `slice(xs, a, b)` equals `take(drop(xs, a), b - a)` (D-375).

### `take`

```cheby
pub fn take<T>(xs: List<T>, count: Int) -> List<T>
```

Returns the first `count` elements of `xs`.

- **Clamps:** `count` to `0` to `length`, so a negative count gives `[]` and a count past the end gives `xs` (D-298).
- **Cost:** O(log n), required (D-300).

```cheby
test "take keeps a prefix" {
  let negative = 0 - 1
  assert list::take([1, 2, 3], 2) == [1, 2]
  assert list::take([1, 2, 3], 10) == [1, 2, 3]
  assert list::take([1, 2, 3], negative) == []
}
```

### `drop`

```cheby
pub fn drop<T>(xs: List<T>, count: Int) -> List<T>
```

Returns `xs` without its first `count` elements.

- **Clamps:** `count` to `0` to `length`, so a negative count gives `xs` and a count past the end gives `[]` (D-298).
- **Cost:** O(log n), required (D-300).

```cheby
test "drop removes a prefix" {
  let negative = 0 - 1
  assert list::drop([1, 2, 3], 2) == [3]
  assert list::drop([1, 2, 3], 10) == []
  assert list::drop([1, 2, 3], negative) == [1, 2, 3]
}
```

### `split_at`

```cheby
pub fn split_at<T>(xs: List<T>, count: Int) -> (List<T>, List<T>)
```

Splits `xs` after its first `count` elements, the same as `(take(xs, count), drop(xs, count))`.

- **Clamps:** `count` to `0` to `length` (D-298).
- **Cost:** O(log n), required (D-300).

```cheby
test "split_at splits after a prefix" {
  assert list::split_at([1, 2, 3, 4], 1) == ([1], [2, 3, 4])
  assert list::split_at([1, 2], 5) == ([1, 2], [])
}
```

## Searching

### `contains`

```cheby
pub fn contains<T>(xs: List<T>, item: T) -> Bool
```

Tells whether some element of `xs` is `==` to `item`. It stops at the first match.

- **Cost:** O(n). Informative.

```cheby
test "contains looks for an equal element" {
  assert list::contains(["a", "b"], "b")
  assert !list::contains([1, 2], 3)
}
```

### `find`

```cheby
pub fn find<T>(xs: List<T>, predicate: fn(T) -> Bool) -> Option<T>
```

Returns the first element for which `predicate` returns `True`. It stops calling `predicate` at that element.

- **Fails:** `None` when no element matches.
- **Cost:** O(n), plus the calls to `predicate`. Informative.

```cheby
test "find returns the first match" {
  assert list::find([1, 4, 9], fn(n) { n > 3 }) == Some(4)
  assert list::find([1, 4, 9], fn(n) { n > 10 }) == None
  let stops = list::find([1, 2, 3], fn(n) {
    case n {
      3 => panic as "not reached"
      _ => n == 2
    }
  })
  assert stops == Some(2)
}
```

### `position`

```cheby
pub fn position<T>(xs: List<T>, predicate: fn(T) -> Bool) -> Option<Int>
```

Returns the index of the first element for which `predicate` returns `True`. It stops calling `predicate` at that element.

- **Fails:** `None` when no element matches.
- **Cost:** O(n), plus the calls to `predicate`. Informative.

```cheby
test "position returns the index of the first match" {
  assert list::position(["a", "b", "c", "b"], fn(s) { s == "b" }) == Some(1)
  assert list::position(["a"], fn(s) { s == "z" }) == None
}
```

There is no `index_of`, because `position(xs, fn(x) { x == item })` covers it (D-378).

### `any`

```cheby
pub fn any<T>(xs: List<T>, predicate: fn(T) -> Bool) -> Bool
```

Tells whether `predicate` returns `True` for some element. It stops at the first such element, and is `False` for `[]`.

- **Cost:** O(n), plus the calls to `predicate`. Informative.

```cheby
test "any stops at the first match" {
  assert list::any([1, 2, 3], fn(n) {
    case n {
      3 => panic as "not reached"
      _ => n == 2
    }
  })
  assert !list::any([1, 2], fn(n) { n > 5 })
  assert !list::any(list::new::<Int>(), fn(n) { n > 5 })
}
```

### `all`

```cheby
pub fn all<T>(xs: List<T>, predicate: fn(T) -> Bool) -> Bool
```

Tells whether `predicate` returns `True` for every element. It stops at the first element for which it returns `False`, and is `True` for `[]`.

- **Cost:** O(n), plus the calls to `predicate`. Informative.

```cheby
test "all needs every element to match" {
  assert list::all([2, 4], fn(n) { n % 2 == 0 })
  assert !list::all([2, 3, 4], fn(n) { n % 2 == 0 })
  assert list::all(list::new::<Int>(), fn(n) { n > 5 })
}
```

### `count`

```cheby
pub fn count<T>(xs: List<T>, predicate: fn(T) -> Bool) -> Int
```

Returns the number of elements for which `predicate` returns `True`.

- **Cost:** O(n), plus the calls to `predicate`. Informative.

```cheby
test "count counts the matches" {
  assert list::count([1, 2, 3, 4], fn(n) { n % 2 == 0 }) == 2
  assert list::count(["a"], fn(s) { s == "b" }) == 0
}
```

## Transforming

### `map`

```cheby
pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U>
```

Returns the results of calling `f` on each element, in order.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "map transforms every element" {
  assert list::map([1, 2, 3], fn(n) { n * 10 }) == [10, 20, 30]
  assert list::map(["a", "bc"], string::length) == [1, 2]
}
```

### `filter`

```cheby
pub fn filter<T>(xs: List<T>, keep: fn(T) -> Bool) -> List<T>
```

Returns the elements for which `keep` returns `True`, in order.

- **Cost:** O(n), plus the calls to `keep`. Informative.

```cheby
test "filter keeps the matching elements" {
  assert list::filter([1, 2, 3, 4], fn(n) { n % 2 == 0 }) == [2, 4]
  assert list::filter(["a", ""], fn(s) { s != "" }) == ["a"]
}
```

### `filter_map`

```cheby
pub fn filter_map<T, U>(xs: List<T>, f: fn(T) -> Option<U>) -> List<U>
```

Calls `f` on each element and returns the values of the `Some` results, in order.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "filter_map keeps the Some results" {
  assert list::filter_map([0, 2, 5], list::get(["a", "b", "c"], _)) == ["a", "c"]
  assert list::filter_map(["1", "x", "3"], fn(text) { result::to_option(int::parse(text)) }) == [1, 3]
}
```

The callback returns `Option`, as `get` and `find` do (D-379). A function that returns a `Result`, such as `int::parse`, is passed through `result::to_option`.

### `flat_map`

```cheby
pub fn flat_map<T, U>(xs: List<T>, f: fn(T) -> List<U>) -> List<U>
```

Calls `f` on each element and concatenates the resulting lists, in order.

- **Cost:** O(n log m), where m is the length of the result, plus the calls to `f`. Informative.

```cheby
test "flat_map concatenates the results" {
  assert list::flat_map([1, 2], fn(n) { [n, n * 10] }) == [1, 10, 2, 20]
  assert list::flat_map([1, 2], fn(_) { list::new::<Int>() }) == []
}
```

### `flatten`

```cheby
pub fn flatten<T>(lists: List<List<T>>) -> List<T>
```

Concatenates the lists in `lists`, in order.

- **Cost:** O(n log m), where m is the length of the result. Informative.

```cheby
test "flatten concatenates a list of lists" {
  assert list::flatten([[1], [], [2, 3]]) == [1, 2, 3]
  assert list::flatten(list::new::<List<Int>>()) == []
}
```

### `reverse`

```cheby
pub fn reverse<T>(xs: List<T>) -> List<T>
```

Returns the elements of `xs` in the opposite order.

- **Cost:** O(n). Informative.

```cheby
test "reverse flips the order" {
  assert list::reverse([1, 2, 3]) == [3, 2, 1]
  assert list::reverse(list::new::<Int>()) == []
}
```

### `unique`

```cheby
pub fn unique<T>(xs: List<T>) -> List<T>
```

Returns `xs` without repeated elements, keeping the first of each group of `==` elements, in order. Elements are hashed, like `Set` elements (§3.13).

- **Cost:** O(n log n), effectively O(n). Informative.

```cheby
test "unique keeps the first of each element" {
  assert list::unique([3, 1, 3, 2, 1]) == [3, 1, 2]
  assert list::unique(["a", "a"]) == ["a"]
}
```

### `partition`

```cheby
pub fn partition<T>(xs: List<T>, keep: fn(T) -> Bool) -> (List<T>, List<T>)
```

Splits `xs` into the elements for which `keep` returns `True` and the others, each in their original order.

- **Cost:** O(n), plus the calls to `keep`. Informative.

```cheby
test "partition splits by a predicate" {
  assert list::partition([1, 2, 3, 4], fn(n) { n > 2 }) == ([3, 4], [1, 2])
  assert list::partition([1], fn(n) { n > 2 }) == ([], [1])
}
```

### `enumerate`

```cheby
pub fn enumerate<T>(xs: List<T>) -> List<(Int, T)>
```

Pairs each element with its index, counting from 0.

- **Cost:** O(n). Informative.

```cheby
test "enumerate pairs elements with their indexes" {
  assert list::enumerate(["a", "b"]) == [(0, "a"), (1, "b")]
  let labels = list::map(list::enumerate(["x", "y"]), fn((index, name)) { "{index}:{name}" })
  assert labels == ["0:x", "1:y"]
}
```

`enumerate` works with every other list function, so there are no indexed copies such as Gleam's `index_map` (D-380).

### `zip`

```cheby
pub fn zip<T, U>(xs: List<T>, other: List<U>) -> List<(T, U)>
```

Pairs the elements of `xs` and `other` by position. When one list is longer, its extra elements are left out.

- **Cost:** O(n), where n is the length of the shorter list. Informative.

```cheby
test "zip pairs by position" {
  assert list::zip([1, 2, 3], ["a", "b"]) == [(1, "a"), (2, "b")]
  assert list::zip([1], list::new::<String>()) == []
}
```

### `unzip`

```cheby
pub fn unzip<T, U>(pairs: List<(T, U)>) -> (List<T>, List<U>)
```

Splits a list of pairs into the list of first elements and the list of second elements, each in order.

- **Cost:** O(n). Informative.

```cheby
test "unzip splits pairs" {
  assert list::unzip([(1, "a"), (2, "b")]) == ([1, 2], ["a", "b"])
}
```

## Folding and visiting

### `fold`

```cheby
pub fn fold<T, A>(xs: List<T>, initial: A, f: fn(A, T) -> A) -> A
```

Combines the elements from front to back: it calls `f` with `initial` and the first element, then with that result and the second element, and so on, and returns the last result, or `initial` for `[]`.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "fold combines from the front" {
  assert list::fold([1, 2, 3], 0, int::add) == 6
  assert list::fold(["a", "b", "c"], "", fn(text, s) { text + s }) == "abc"
}
```

### `sum`

```cheby
pub fn sum<T: Add>(xs: List<T>, zero: T) -> T
```

Adds the elements to `zero`, front to back, so the result for `[]` is `zero` (D-381, D-406). An interface function cannot return `Self` without taking one (D-209), so the caller supplies the starting value.

- **Panics:** where `+` on `T` panics, such as `Int` overflow (D-025).
- **Cost:** O(n), plus the calls to `add`. Informative.

```cheby
test "sum adds to the starting value" {
  assert list::sum([1, 2, 3], 0) == 6
  assert list::sum([0.5, 0.25], 0.0) == 0.75
  assert list::sum([], 0) == 0
}
```

### `product`

```cheby
pub fn product<T: Mul>(xs: List<T>, one: T) -> T
```

Multiplies `one` by the elements, front to back, so the result for `[]` is `one` (D-381, D-406).

- **Panics:** where `*` on `T` panics, such as `Int` overflow (D-025).
- **Cost:** O(n), plus the calls to `mul`. Informative.

```cheby
test "product multiplies from the starting value" {
  assert list::product([2, 3, 4], 1) == 24
  assert list::product([], 1) == 1
}
```

### `max`

```cheby
pub fn max<T: Compare>(xs: List<T>) -> Option<T>
```

Returns the largest element by `compare`, or `None` for `[]` (D-407). Among elements that compare `Equal`, it returns the last, so `max` of a stably sorted list is its last element (D-408).

- **Fails:** `None` for `[]`.
- **Cost:** O(n), plus the calls to `compare`. Informative.

```cheby
test "max finds the largest element" {
  assert list::max([3, 9, 2]) == Some(9)
  assert list::max(["pear", "apple"]) == Some("pear")
  let empty: List<Int> = []
  assert list::max(empty) == None
}
```

### `min`

```cheby
pub fn min<T: Compare>(xs: List<T>) -> Option<T>
```

Returns the smallest element by `compare`, or `None` for `[]` (D-407). Among elements that compare `Equal`, it returns the first, so `min` of a stably sorted list is its first element (D-408).

- **Fails:** `None` for `[]`.
- **Cost:** O(n), plus the calls to `compare`. Informative.

```cheby
test "min finds the smallest element" {
  assert list::min([3, 9, 2]) == Some(2)
  let empty: List<String> = []
  assert list::min(empty) == None
}
```

### `max_with`

```cheby
pub fn max_with<T>(xs: List<T>, compare: fn(T, T) -> Order) -> Option<T>
```

Returns the largest element by `compare`, or `None` for `[]`, as `sort_with` takes a comparison function (D-409). Among elements that compare `Equal`, it returns the last (D-408).

- **Fails:** `None` for `[]`.
- **Cost:** O(n), plus the calls to `compare`. Informative.

```cheby
test "max_with returns the last of equal elements" {
  let by_count = fn((_, a), (_, b)) { int::compare(a, b) }
  assert list::max_with([("a", 2), ("b", 5), ("c", 5)], by_count) == Some(("c", 5))
}
```

### `min_with`

```cheby
pub fn min_with<T>(xs: List<T>, compare: fn(T, T) -> Order) -> Option<T>
```

Returns the smallest element by `compare`, or `None` for `[]` (D-409). Among elements that compare `Equal`, it returns the first (D-408).

- **Fails:** `None` for `[]`.
- **Cost:** O(n), plus the calls to `compare`. Informative.

```cheby
test "min_with returns the first of equal elements" {
  let by_count = fn((_, a), (_, b)) { int::compare(a, b) }
  assert list::min_with([("a", 2), ("b", 1), ("c", 1)], by_count) == Some(("b", 1))
}
```

### `fold_right`

```cheby
pub fn fold_right<T, A>(xs: List<T>, initial: A, f: fn(A, T) -> A) -> A
```

Combines the elements from back to front, like `fold` on the reversed list. The callback takes its arguments in the same order as `fold`'s, as in Gleam.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "fold_right combines from the back" {
  assert list::fold_right(["a", "b", "c"], "", fn(text, s) { text + s }) == "cba"
  assert list::fold_right([1, 2], [], list::push) == [2, 1]
}
```

### `each`

```cheby
pub fn each<T>(xs: List<T>, f: fn(T))
```

Calls `f` on each element, front to back, for its effects.

- **Cost:** O(n), plus the calls to `f`. Informative.

```cheby
test "each visits every element" {
  list::each([1, 2, 3], fn(n) { assert n > 0 })
}
```

## Sorting and comparing

### `sort`

```cheby
pub fn sort<T: Compare>(xs: List<T>) -> List<T>
```

Returns the elements of `xs` in increasing order by `T`'s `compare` (D-335). The sort is stable: elements that compare `Equal` keep their order (D-299).

- **Cost:** O(n log n) calls to `compare`. Informative.

```cheby
test "sort orders by compare" {
  assert list::sort([3, 1, 2]) == [1, 2, 3]
  assert list::sort(["pear", "apple"]) == ["apple", "pear"]
  assert list::sort([(2, "b"), (1, "z"), (2, "a")]) == [(1, "z"), (2, "a"), (2, "b")]
}
```

### `sort_with`

```cheby
pub fn sort_with<T>(xs: List<T>, compare: fn(T, T) -> Order) -> List<T>
```

Returns the elements of `xs` in increasing order by `compare`, where `Less` means the first argument goes first (D-335). The sort is stable (D-299).

- **Cost:** O(n log n) calls to `compare`. Informative.

```cheby
test "sort_with is stable" {
  let by_length = fn(a: String, b: String) { int::compare(string::length(a), string::length(b)) }
  assert list::sort_with(["bb", "a", "cc", "d"], by_length) == ["a", "d", "bb", "cc"]
  assert list::sort_with([1, 3, 2], fn(a, b) { int::compare(b, a) }) == [3, 2, 1]
}
```

### `compare`

```cheby
pub fn compare<T: Compare>(a: List<T>, b: List<T>) -> Order
```

Compares two lists lexicographically: the first pair of elements that differ decides, and a list that is a prefix of the other comes first (D-136). It makes `List<T>` satisfy `Compare` when `T` does (§8.7, D-064).

- **Cost:** O(n), where n is the length of the shorter list, plus the calls to `T`'s `compare`. Informative.

```cheby
test "lists compare lexicographically" {
  assert list::compare([1, 2], [1, 3]) == Less
  assert list::compare([1, 2], [1, 2, 0]) == Less
  assert list::compare(["b"], ["a", "z"]) == Greater
  assert list::compare([1], [1]) == Equal
  assert [2] > [1, 9]
}
```

## Text

### `show`

```cheby
pub fn show<T: Show>(xs: List<T>) -> String
```

Makes `List<T>` satisfy `Show` when `T` does (§8.8, D-064). The text follows D-355: `[`, the `show` of each element separated by `, `, and `]`.

- **Cost:** O(n), plus the `show` of each element. Informative.

```cheby
test "a list shows its elements" {
  let numbers = [1, 2]
  let words = ["a", "b"]
  assert "{numbers}" == "[1, 2]"
  assert list::show(words) == "[a, b]"
  assert list::show(list::new::<Int>()) == "[]"
}
```
