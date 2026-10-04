# `std::result`

Helpers for the built-in `Result` type (D-317). `Result` and its constructors are in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub exposed type Result<T, E> {
  Ok(T),
  Err(E),
}
```

There are no functions that panic on an `Err`, such as Rust's `unwrap` and `expect`. `let assert Ok(value) = r` unwraps with a message instead (D-068, D-159, D-317).

## Chaining

### `try`

```cheby
pub fn try<T, E, U>(r: Result<T, E>, next: fn(T) -> Result<U, E>) -> Result<U, E>
```

Calls `next` with the value of `r` when `r` is `Ok`, and returns the `Err` of `r` unchanged otherwise. With `use`, it stops a sequence of steps at the first error (§11.2).

- **Cost:** O(1), plus the call to `next`. Informative.

```cheby
test "try stops at the first error" {
  fn add(a: String, b: String) -> Result<Int, Nil> {
    use x <- result::try(int::parse(a))
    use y <- result::try(int::parse(b))
    Ok(x + y)
  }
  assert add("1", "2") == Ok(3)
  assert add("1", "two") == Err(Nil)
}
```

### `map`

```cheby
pub fn map<T, E, U>(r: Result<T, E>, f: fn(T) -> U) -> Result<U, E>
```

Applies `f` to the value of an `Ok`, and returns an `Err` unchanged.

- **Cost:** O(1), plus the call to `f`. Informative.

```cheby
test "map changes only an Ok" {
  let good: Result<Int, String> = Ok(2)
  let bad: Result<Int, String> = Err("no")
  assert result::map(good, fn(n) { n * 10 }) == Ok(20)
  assert result::map(bad, fn(n) { n * 10 }) == Err("no")
}
```

### `map_error`

```cheby
pub fn map_error<T, E, F>(r: Result<T, E>, f: fn(E) -> F) -> Result<T, F>
```

Applies `f` to the error of an `Err`, and returns an `Ok` unchanged. This is how one error type is converted into another, since there is no implicit conversion (§11.2).

- **Cost:** O(1), plus the call to `f`. Informative.

```cheby
test "map_error changes only an Err" {
  let good: Result<Int, String> = Ok(2)
  let bad: Result<Int, String> = Err("no")
  assert result::map_error(good, string::length) == Ok(2)
  assert result::map_error(bad, string::length) == Err(2)
}
```

### `flatten`

```cheby
pub fn flatten<T, E>(r: Result<Result<T, E>, E>) -> Result<T, E>
```

Removes one level of nesting: `Ok(inner)` becomes `inner`, and an outer `Err` is returned unchanged.

- **Cost:** O(1). Informative.

```cheby
test "flatten removes one level" {
  let nested: Result<Result<Int, String>, String> = Ok(Ok(1))
  let inner_error: Result<Result<Int, String>, String> = Ok(Err("inner"))
  let outer_error: Result<Result<Int, String>, String> = Err("outer")
  assert result::flatten(nested) == Ok(1)
  assert result::flatten(inner_error) == Err("inner")
  assert result::flatten(outer_error) == Err("outer")
}
```

## Defaults

### `unwrap_or`

```cheby
pub fn unwrap_or<T, E>(r: Result<T, E>, default: T) -> T
```

Returns the value of an `Ok`, or `default` for an `Err`.

- **Cost:** O(1). Informative.

```cheby
test "unwrap_or falls back on an error" {
  assert result::unwrap_or(int::parse("12"), 0) == 12
  assert result::unwrap_or(int::parse("twelve"), 0) == 0
}
```

### `lazy_unwrap_or`

```cheby
pub fn lazy_unwrap_or<T, E>(r: Result<T, E>, default: fn() -> T) -> T
```

Returns the value of an `Ok`, or calls `default` for an `Err`. `default` is called only when it is needed.

- **Cost:** O(1), plus the call to `default`. Informative.

```cheby
test "lazy_unwrap_or calls the default only for an error" {
  assert result::lazy_unwrap_or(int::parse("12"), fn() { panic as "not called" }) == 12
  assert result::lazy_unwrap_or(int::parse("twelve"), fn() { 0 }) == 0
}
```

### `or`

```cheby
pub fn or<T, E>(first: Result<T, E>, second: Result<T, E>) -> Result<T, E>
```

Returns `first` if it is `Ok`, and `second` otherwise.

- **Cost:** O(1). Informative.

```cheby
test "or picks the first success" {
  assert result::or(int::parse("1"), int::parse("2")) == Ok(1)
  assert result::or(int::parse("one"), int::parse("2")) == Ok(2)
  assert result::or(int::parse("one"), int::parse("two")) == Err(Nil)
}
```

## Tests

### `is_ok`

```cheby
pub fn is_ok<T, E>(r: Result<T, E>) -> Bool
```

Tells whether `r` is an `Ok`.

- **Cost:** O(1). Informative.

```cheby
test "is_ok tells Ok from Err" {
  assert result::is_ok(int::parse("1"))
  assert !result::is_ok(int::parse("one"))
}
```

### `is_error`

```cheby
pub fn is_error<T, E>(r: Result<T, E>) -> Bool
```

Tells whether `r` is an `Err`.

- **Cost:** O(1). Informative.

```cheby
test "is_error tells Err from Ok" {
  assert result::is_error(int::parse("one"))
  assert !result::is_error(int::parse("1"))
}
```

## Lists of results

### `all`

```cheby
pub fn all<T, E>(results: List<Result<T, E>>) -> Result<List<T>, E>
```

Returns `Ok` with every value, in order, when every element is `Ok`, and otherwise the first `Err`.

- **Cost:** O(n). Informative.

```cheby
test "all needs every result to succeed" {
  assert result::all(list::map(["1", "2"], int::parse)) == Ok([1, 2])
  assert result::all(list::map(["1", "two", "x"], int::parse)) == Err(Nil)
  assert result::all::<Int, Nil>([]) == Ok([])
}
```

### `values`

```cheby
pub fn values<T, E>(results: List<Result<T, E>>) -> List<T>
```

Returns the values of the `Ok` elements, in order, and drops the errors.

- **Cost:** O(n). Informative.

```cheby
test "values keeps the successes" {
  assert result::values(list::map(["1", "two", "3"], int::parse)) == [1, 3]
}
```

### `partition`

```cheby
pub fn partition<T, E>(results: List<Result<T, E>>) -> (List<T>, List<E>)
```

Splits the elements into the values of the `Ok`s and the errors of the `Err`s, each in their original order.

- **Cost:** O(n). Informative.

```cheby
test "partition splits successes from errors" {
  let results: List<Result<Int, String>> = [Ok(1), Err("a"), Ok(2), Err("b")]
  assert result::partition(results) == ([1, 2], ["a", "b"])
}
```

## Conversions

### `to_option`

```cheby
pub fn to_option<T, E>(r: Result<T, E>) -> Option<T>
```

Returns `Some` with the value of an `Ok`, and `None` for an `Err`, dropping the error.

- **Cost:** O(1). Informative.

```cheby
test "to_option drops the error" {
  assert result::to_option(int::parse("1")) == Some(1)
  assert result::to_option(int::parse("one")) == None
}
```

### `show`

```cheby
pub fn show<T: Show, E: Show>(r: Result<T, E>) -> String
```

Makes `Result<T, E>` satisfy `Show` when `T` and `E` do (§8.8, D-064). The text follows D-355: `Ok(` or `Err(`, the `show` of the value or error, and `)`.

- **Cost:** O(1), plus the `show` of the value or error. Informative.

```cheby
test "a result shows its value or error" {
  let good: Result<Int, String> = Ok(3)
  let bad: Result<Int, String> = Err("not found")
  assert "{good}" == "Ok(3)"
  assert "{bad}" == "Err(not found)"
}
```
