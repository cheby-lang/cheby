# `std::bool`

The module of the built-in `Bool` type (D-353). `Bool` and its constructors are in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub exposed type Bool {
  False,
  True,
}
```

The logical operators `&&`, `||` and `!` are built in and cannot be overloaded (§5.4.5, D-118). There is no truthiness, and there are no functions that repeat the operators.

### `compare`

```cheby
pub fn compare(a: Bool, b: Bool) -> Order
```

Orders `False` before `True` (D-136). It makes `Bool` satisfy `Compare` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "False comes before True" {
  assert bool::compare(False, True) == Less
  assert bool::compare(True, True) == Equal
  assert list::sort([True, False, True]) == [False, True, True]
}
```

### `show`

```cheby
pub fn show(b: Bool) -> String
```

Returns `"True"` or `"False"`. It makes `Bool` satisfy `Show` (§8.8).

- **Cost:** O(1). Informative.

```cheby
test "a bool shows as its constructor" {
  let yes = True
  assert bool::show(False) == "False"
  assert "{yes}" == "True"
}
```
