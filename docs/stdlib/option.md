# `std::option`

Helpers for the built-in `Option` type (D-317). `Option` and its constructors are in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub exposed type Option<T> {
  Some(T),
  None,
}
```

A value that may be absent has type `Option<T>`. There is no `null` (D-091). There are no functions that panic on `None`, such as Rust's `unwrap` and `expect`. `let assert Some(value) = o` unwraps with a message instead (D-068, D-159, D-317).

## Chaining

### `then`

```cheby
pub fn then<T, U>(o: Option<T>, next: fn(T) -> Option<U>) -> Option<U>
```

Calls `next` with the value of `o` when `o` is `Some`, and returns `None` otherwise. With `use`, it stops a sequence of steps at the first `None`, as `result::try` does for `Result` (§11.2).

- **Cost:** O(1), plus the call to `next`. Informative.

```cheby
test "then stops at the first None" {
  fn second_of_first(rows: List<List<Int>>) -> Option<Int> {
    use row <- option::then(list::get(rows, 0))
    list::get(row, 1)
  }
  assert second_of_first([[1, 2], [3]]) == Some(2)
  assert second_of_first([[1]]) == None
  assert second_of_first([]) == None
}
```

`then` is named as in Gleam, because `try` would suggest an error that `None` does not carry (D-383).

### `map`

```cheby
pub fn map<T, U>(o: Option<T>, f: fn(T) -> U) -> Option<U>
```

Applies `f` to the value of a `Some`, and returns `None` unchanged.

- **Cost:** O(1), plus the call to `f`. Informative.

```cheby
test "map changes only a Some" {
  assert option::map(Some(2), fn(n) { n * 10 }) == Some(20)
  assert option::map(None, fn(n: Int) { n * 10 }) == None
}
```

### `flatten`

```cheby
pub fn flatten<T>(o: Option<Option<T>>) -> Option<T>
```

Removes one level of nesting: `Some(inner)` becomes `inner`, and `None` stays `None`.

- **Cost:** O(1). Informative.

```cheby
test "flatten removes one level" {
  let inner_none: Option<Option<Int>> = Some(None)
  let outer_none: Option<Option<Int>> = None
  assert option::flatten(Some(Some(1))) == Some(1)
  assert option::flatten(inner_none) == None
  assert option::flatten(outer_none) == None
}
```

## Defaults

### `unwrap_or`

```cheby
pub fn unwrap_or<T>(o: Option<T>, default: T) -> T
```

Returns the value of a `Some`, or `default` for `None`.

- **Cost:** O(1). Informative.

```cheby
test "unwrap_or falls back on None" {
  assert option::unwrap_or(list::get([5], 0), 0) == 5
  assert option::unwrap_or(list::get([5], 3), 0) == 0
}
```

### `lazy_unwrap_or`

```cheby
pub fn lazy_unwrap_or<T>(o: Option<T>, default: fn() -> T) -> T
```

Returns the value of a `Some`, or calls `default` for `None`. `default` is called only when it is needed.

- **Cost:** O(1), plus the call to `default`. Informative.

```cheby
test "lazy_unwrap_or calls the default only for None" {
  assert option::lazy_unwrap_or(list::get([5], 0), fn() { panic as "not called" }) == 5
  assert option::lazy_unwrap_or(list::get([5], 3), fn() { 0 }) == 0
}
```

### `or`

```cheby
pub fn or<T>(first: Option<T>, second: Option<T>) -> Option<T>
```

Returns `first` if it is `Some`, and `second` otherwise.

- **Cost:** O(1). Informative.

```cheby
test "or picks the first Some" {
  assert option::or(list::get([1], 0), Some(2)) == Some(1)
  assert option::or(list::get([1], 5), Some(2)) == Some(2)
  assert option::or(list::get([1], 5), list::get([1], 6)) == None
}
```

## Tests

### `is_some`

```cheby
pub fn is_some<T>(o: Option<T>) -> Bool
```

Tells whether `o` is a `Some`.

- **Cost:** O(1). Informative.

```cheby
test "is_some tells Some from None" {
  assert option::is_some(list::get([1], 0))
  assert !option::is_some(list::get([1], 1))
}
```

### `is_none`

```cheby
pub fn is_none<T>(o: Option<T>) -> Bool
```

Tells whether `o` is `None`.

- **Cost:** O(1). Informative.

```cheby
test "is_none tells None from Some" {
  assert option::is_none(list::get([1], 1))
  assert !option::is_none(list::get([1], 0))
}
```

## Lists of options

### `all`

```cheby
pub fn all<T>(options: List<Option<T>>) -> Option<List<T>>
```

Returns `Some` with every value, in order, when every element is `Some`, and `None` otherwise.

- **Cost:** O(n). Informative.

```cheby
test "all needs every option to be Some" {
  assert option::all([Some(1), Some(2)]) == Some([1, 2])
  assert option::all([Some(1), None]) == None
  assert option::all::<Int>([]) == Some([])
}
```

### `values`

```cheby
pub fn values<T>(options: List<Option<T>>) -> List<T>
```

Returns the values of the `Some` elements, in order, and drops the `None`s.

- **Cost:** O(n). Informative.

```cheby
test "values keeps the Somes" {
  assert option::values([Some(1), None, Some(3)]) == [1, 3]
}
```

## Conversions

### `to_result`

```cheby
pub fn to_result<T, E>(o: Option<T>, error: E) -> Result<T, E>
```

Returns `Ok` with the value of a `Some`, and `Err(error)` for `None`.

- **Cost:** O(1). Informative.

```cheby
test "to_result supplies the error" {
  assert option::to_result(list::get([1], 0), "missing") == Ok(1)
  assert option::to_result(list::get([1], 4), "missing") == Err("missing")
}
```

### `show`

```cheby
pub fn show<T: Show>(o: Option<T>) -> String
```

Makes `Option<T>` satisfy `Show` when `T` does (§8.8, D-064). The text follows D-355: `Some(`, the `show` of the value and `)`, or `None`.

- **Cost:** O(1), plus the `show` of the value. Informative.

```cheby
test "an option shows its value" {
  let found = list::get([3], 0)
  let missing = list::get([3], 1)
  assert "{found}" == "Some(3)"
  assert "{missing}" == "None"
}
```
