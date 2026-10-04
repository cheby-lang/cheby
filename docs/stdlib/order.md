# `std::order`

The module of the built-in `Order` type, the result of every `compare` function (§3.14, D-353). `Order` and its constructors are in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub exposed type Order {
  Less,
  Equal,
  Greater,
}
```

### `reverse`

```cheby
pub fn reverse(o: Order) -> Order
```

Swaps `Less` and `Greater`, and keeps `Equal`. It turns an ascending comparison into a descending one.

- **Cost:** O(1). Informative.

```cheby
test "reverse flips a comparison" {
  assert order::reverse(int::compare(1, 2)) == Greater
  assert order::reverse(Equal) == Equal
  assert list::sort_with([1, 3, 2], fn(a, b) { order::reverse(int::compare(a, b)) }) == [3, 2, 1]
}
```

### `then`

```cheby
pub fn then(first: Order, next: fn() -> Order) -> Order
```

Returns `first` unless it is `Equal`, in which case it calls `next`. It compares by several keys in turn, and `next` is called only when the earlier keys tie.

- **Cost:** O(1), plus the call to `next`. Informative.

```cheby
test "then breaks ties with the next key" {
  fn by_length_then_text(a: String, b: String) -> Order {
    order::then(int::compare(string::length(a), string::length(b)), fn() { string::compare(a, b) })
  }
  assert list::sort_with(["bb", "c", "aa"], by_length_then_text) == ["c", "aa", "bb"]
}
```

### `compare`

```cheby
pub fn compare(a: Order, b: Order) -> Order
```

Orders `Less` before `Equal` before `Greater`. It makes `Order` satisfy `Compare` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "orders compare in declaration order" {
  assert order::compare(Less, Greater) == Less
  assert order::compare(Greater, Equal) == Greater
}
```

### `show`

```cheby
pub fn show(o: Order) -> String
```

Returns `"Less"`, `"Equal"` or `"Greater"`. It makes `Order` satisfy `Show` (§8.8).

- **Cost:** O(1). Informative.

```cheby
test "an order shows as its constructor" {
  assert order::show(Less) == "Less"
  assert order::show(Greater) == "Greater"
}
```
