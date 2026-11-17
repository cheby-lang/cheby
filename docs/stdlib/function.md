# `std::function`

Helpers that repeat a step (D-1611). Cheby has no loops (D-002), so a simulation that runs for a number of rounds, or until nothing changes, would otherwise need a local recursive function each time. Both functions take the value first, so they pipe: `grid |> function::iterate(_, 100, step)`.

The module is in tier 4. It is Cheby with no kernels, so it runs in constants and on every target unchanged.

### `iterate`

```cheby
pub fn iterate<T>(x: T, n: Int, f: fn(T) -> T) -> T
```

Applies `f` to `x` `n` times and returns the result, so `iterate(x, 0, f)` is `x` and `iterate(x, 2, f)` is `f(f(x))` (D-1611).

- **Panics:** when `n` is negative.
- **Cost:** n calls to `f`. Informative.

```cheby
test "iterate applies a function n times" {
  assert function::iterate(1, 10, fn(n) { n * 2 }) == 1024
  assert function::iterate("a", 0, fn(_) { "b" }) == "a"
  assert function::iterate([1], 2, fn(xs) { [0, ..xs] }) == [0, 0, 1]
}
```

### `fixpoint`

```cheby
pub fn fixpoint<T>(x: T, f: fn(T) -> T) -> T
```

Applies `f`, starting from `x`, until a result is `==` to the value it came from, and returns that value. When `f(x) == x`, it returns `x` after one call. It never stops when no fixed point is reached, as for `fixpoint(0, fn(n) { n + 1 })` (D-1611).

- **Panics:** when `==` panics on the values, as it does on functions (D-233).
- **Cost:** one call to `f` and one `==` per step. Informative.

```cheby
test "fixpoint stops when the value stops changing" {
  assert function::fixpoint(100, fn(n) { n / 2 }) == 0
  assert function::fixpoint(7, fn(n) { n }) == 7
  let halve_evens = fn(n) {
    case n % 2 {
      0 => n / 2
      _ => n
    }
  }
  assert function::fixpoint(96, halve_evens) == 3
}
```
