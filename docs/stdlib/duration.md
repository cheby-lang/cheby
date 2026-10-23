# `std::duration`

Spans of time, such as a timeout, a timer's wait or the time between two events. `std::fiber` and `std::selector` take a `Duration` wherever they wait (D-103), and tier 3's `std::time` measures one.

```cheby
pub type Duration
```

- **Representation.** `Duration` is opaque. It is a record written in Cheby that holds whole seconds and the nanoseconds after them, with the nanoseconds always from 0 to `999_999_999`, so every span has exactly one representation (D-650). A negative span has negative seconds and non-negative nanoseconds, so −0.5 s is −1 s plus `500_000_000` ns.
- **Resolution and range.** The resolution is one nanosecond. The seconds are any `Int`, so a span reaches about ±292 billion years on native and ±285 million years on JS, where `Int` is limited to ±(2⁵³−1) (D-037). Spans may be negative, and arithmetic outside the range panics like `Int` arithmetic (D-025, D-650).
- **Written in Cheby.** The module has no kernels. Every function runs in constants (D-090), so `const deadline: Duration = duration::seconds(1)` works, and gives the same result on every target (D-232, D-650).
- **Equality, hashing and printing.** `==` and hashing are the built-in structural ones, which compare spans because the representation is unique. Debug printing is the built-in one for records (D-134), such as `Duration { seconds: 1, nanoseconds: 500000000 }`. `show` gives the short text, such as `1.5s` (D-651).
- **Interfaces.** `add`, `sub`, `neg`, `compare` and `show` make `Duration` satisfy `Add`, `Sub`, `Neg`, `Compare` and `Show` (§8.7, §8.8), so `a + b`, `a - b`, `-a`, `a < b` and `"{d}"` work. There is no `mul` or `div`, since a span times a span is not a span (§8.7). [`scale`](#scale) multiplies a span by an `Int` exactly (D-651, D-P22).
- **Waiting.** A function that waits for a `Duration`, such as `fiber::timeout` or `selector::after`, treats a span of zero or less as already elapsed, and a span longer than 2⁶³ − 1 nanoseconds, about 292 years, as never elapsing (D-652, D-891).

Every function costs O(1). The costs are informative.

## Building

### `zero`

```cheby
pub const zero: Duration = seconds(0)
```

The span of length zero, `duration::seconds(0)`.

```cheby
test "zero is no time at all" {
  assert duration::zero == duration::seconds(0)
  assert duration::zero == duration::nanoseconds(0)
  assert duration::show(duration::zero) == "0s"
}
```

### `nanoseconds`

```cheby
pub fn nanoseconds(n: Int) -> Duration
```

Returns the span of `n` nanoseconds. `n` may be negative.

```cheby
test "nanoseconds build exact spans" {
  assert duration::nanoseconds(1_500_000_000) == duration::milliseconds(1_500)
  assert duration::nanoseconds(-1) < duration::zero
  assert duration::to_nanoseconds(duration::nanoseconds(-7)) == -7
}
```

### `microseconds`

```cheby
pub fn microseconds(n: Int) -> Duration
```

Returns the span of `n` microseconds. `n` may be negative.

```cheby
test "microseconds are a thousand nanoseconds" {
  assert duration::microseconds(3) == duration::nanoseconds(3_000)
  assert duration::microseconds(-2_000_000) == duration::seconds(-2)
}
```

### `milliseconds`

```cheby
pub fn milliseconds(n: Int) -> Duration
```

Returns the span of `n` milliseconds. `n` may be negative.

```cheby
test "milliseconds are a thousand microseconds" {
  assert duration::milliseconds(250) == duration::microseconds(250_000)
  assert duration::milliseconds(1_000) == duration::seconds(1)
  assert duration::milliseconds(-500) + duration::milliseconds(500) == duration::zero
}
```

### `seconds`

```cheby
pub fn seconds(n: Int) -> Duration
```

Returns the span of `n` seconds. `n` may be negative. It never panics, because the seconds of a span are any `Int`.

```cheby
test "seconds build whole spans" {
  assert duration::seconds(60) == duration::minutes(1)
  assert duration::to_milliseconds(duration::seconds(2)) == 2_000
}
```

### `minutes`

```cheby
pub fn minutes(n: Int) -> Duration
```

Returns the span of `n` minutes of 60 seconds. `n` may be negative.

- **Panics:** when `n * 60` overflows `Int` (D-025, D-037).

```cheby
test "minutes are sixty seconds" {
  assert duration::minutes(2) == duration::seconds(120)
  assert duration::minutes(-1) == -duration::seconds(60)
}
```

### `hours`

```cheby
pub fn hours(n: Int) -> Duration
```

Returns the span of `n` hours of 3600 seconds. `n` may be negative. There is no `days`, because a calendar day is not always 24 hours, which is tier 3's `std::time` to handle (D-651).

- **Panics:** when `n * 3600` overflows `Int` (D-025, D-037).

```cheby
test "hours are sixty minutes" {
  assert duration::hours(1) == duration::minutes(60)
  assert duration::to_seconds(duration::hours(24)) == 86_400
}
```

## Reading

Each of these returns the span as a whole number of its unit, truncated toward zero like `/` on `Int` (§5.4.2), so a span of −1.5 ms is −1 in milliseconds (D-651).

### `to_nanoseconds`

```cheby
pub fn to_nanoseconds(d: Duration) -> Int
```

Returns `d` in nanoseconds.

- **Panics:** when the result overflows `Int` (D-025): beyond about ±292 years on native, and beyond about ±104 days on JS, where an `Int` outside ±(2⁵³−1) panics (D-037).

```cheby
test "to_nanoseconds is exact" {
  assert duration::to_nanoseconds(duration::milliseconds(3)) == 3_000_000
  assert duration::to_nanoseconds(duration::milliseconds(-1)) == -1_000_000
}
```

### `to_microseconds`

```cheby
pub fn to_microseconds(d: Duration) -> Int
```

Returns `d` in whole microseconds, truncated toward zero.

- **Panics:** when the result overflows `Int` (D-025): beyond about ±292,000 years on native and ±285 years on JS (D-037).

```cheby
test "to_microseconds truncates toward zero" {
  assert duration::to_microseconds(duration::nanoseconds(1_999)) == 1
  assert duration::to_microseconds(duration::nanoseconds(-1_999)) == -1
}
```

### `to_milliseconds`

```cheby
pub fn to_milliseconds(d: Duration) -> Int
```

Returns `d` in whole milliseconds, truncated toward zero.

- **Panics:** when the result overflows `Int` (D-025), beyond about ±285,000 years on JS (D-037).

```cheby
test "to_milliseconds truncates toward zero" {
  assert duration::to_milliseconds(duration::seconds(5)) == 5_000
  assert duration::to_milliseconds(duration::microseconds(1_500)) == 1
  assert duration::to_milliseconds(duration::microseconds(-1_500)) == -1
  assert duration::to_milliseconds(duration::microseconds(-500)) == 0
}
```

### `to_seconds`

```cheby
pub fn to_seconds(d: Duration) -> Int
```

Returns `d` in whole seconds, truncated toward zero. It never panics.

```cheby
test "to_seconds truncates toward zero" {
  assert duration::to_seconds(duration::milliseconds(2_999)) == 2
  assert duration::to_seconds(duration::milliseconds(-2_999)) == -2
  assert duration::to_seconds(duration::minutes(3)) == 180
}
```

## Arithmetic

### `add`

```cheby
pub fn add(a: Duration, b: Duration) -> Duration
```

Returns the sum of two spans. It makes `Duration` satisfy `Add`, so `a + b` calls it (§8.7).

- **Panics:** when the seconds of the result overflow `Int` (D-025, D-037).

```cheby
test "add sums spans" {
  assert duration::add(duration::milliseconds(700), duration::milliseconds(800)) == duration::milliseconds(1_500)
  assert duration::seconds(1) + duration::milliseconds(-250) == duration::milliseconds(750)
}
```

### `sub`

```cheby
pub fn sub(a: Duration, b: Duration) -> Duration
```

Returns `a` minus `b`, which is negative when `b` is the longer span. It makes `Duration` satisfy `Sub` (§8.7).

- **Panics:** when the seconds of the result overflow `Int` (D-025, D-037).

```cheby
test "sub may go below zero" {
  assert duration::sub(duration::seconds(2), duration::milliseconds(500)) == duration::milliseconds(1_500)
  assert duration::seconds(1) - duration::seconds(3) == duration::seconds(-2)
}
```

### `neg`

```cheby
pub fn neg(d: Duration) -> Duration
```

Returns the span of the same length with the other sign. It makes `Duration` satisfy `Neg`, so `-d` calls it (§8.7).

- **Panics:** for the most negative span on native, whose negation overflows `Int` (D-025).

```cheby
test "neg flips the sign" {
  assert duration::neg(duration::milliseconds(1_500)) == duration::milliseconds(-1_500)
  assert -duration::zero == duration::zero
}
```

### `scale`

```cheby
pub fn scale(d: Duration, factor: Int) -> Duration
```

Returns `d` times `factor`, exactly, as in a backoff that doubles a wait (D-P22). It panics only where the result does not fit, as `Duration` arithmetic does (D-650), with the message `integer overflow` on every target. The algorithm keeps every intermediate value within the result's range:

1. Write `d` as `a` seconds plus `b` nanoseconds, such that `a` and `b` never have opposite signs. When `d` is not negative, or is negative with no nanoseconds, `a` and `b` are its seconds and nanoseconds. When `d` is negative and has nanoseconds, `a` is its seconds plus 1 and `b` its nanoseconds minus 10⁹, so −0.5 s is 0 s and `-500_000_000` ns.
2. Write `factor` as `q × 10⁹ + r`, with `q = factor / 10⁹` and `r = factor % 10⁹`, which truncate toward zero (D-069, D-092).
3. Compute `p = b × r` in `I64`. Its size is below 10¹⁸, so it is exact, also on JS, where `I64` is a `BigInt` (ADR-0030).
4. The result is `a × factor + b × q + p / 10⁹` seconds plus `p % 10⁹` nanoseconds, with `/` and `%` truncating, computed left to right in `Int`, and then carried with floor division into the one representation of D-650: a negative nanosecond part borrows one second.

Every partial product and partial sum has the sign of the result or is 0, and none is larger in size than the result, so no step overflows unless the result does. A negative span or a negative factor makes `p % 10⁹` negative, which is why the carry is floored.

- **Panics:** with `integer overflow` when the seconds of the result overflow `Int` (D-025). On JS that means leaving ±(2⁵³−1), and `scale` checks that range itself, so the message is the same as on native (D-037, D-P22).
- **Cost:** O(1). Informative.

```cheby
test "scale multiplies a span" {
  assert duration::scale(duration::milliseconds(1_500), 3) == duration::milliseconds(4_500)
  assert duration::scale(duration::nanoseconds(1), 1_000_000_007) ==
    duration::nanoseconds(1_000_000_007)
  assert duration::scale(duration::seconds(2), 1) == duration::seconds(2)
}

test "scale keeps the sign rules of multiplication" {
  assert duration::scale(duration::milliseconds(-1_500), 3) == duration::milliseconds(-4_500)
  assert duration::scale(duration::milliseconds(1_500), -3) == duration::milliseconds(-4_500)
  assert duration::scale(duration::milliseconds(-1_500), -3) == duration::milliseconds(4_500)
  assert duration::scale(duration::milliseconds(-500), 3) == duration::milliseconds(-1_500)
  assert duration::scale(duration::nanoseconds(-1), 1) == duration::nanoseconds(-1)
  let almost_a_second = duration::nanoseconds(999_999_999)
  assert duration::scale(almost_a_second, -1_000_000_001) ==
    -(duration::seconds(999_999_999) + almost_a_second)
}

test "scale by 0 is zero" {
  assert duration::scale(duration::seconds(5), 0) == duration::zero
  assert duration::scale(duration::milliseconds(-250), 0) == duration::zero
  assert duration::scale(duration::zero, -7) == duration::zero
}

test "scale panics where the result does not fit" {
  let outcome = {
    use _scope <- fiber::scope()
    duration::scale(duration::hours(1), 2_562_047_788_015_216)
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "integer overflow"
}

@target(native)
test "scale is exact up to the ends of Int" {
  // 1.5 s times this is 2⁶³ − 0.5 s.
  let factor = int::max_value / 3 * 2 + 1
  let half = duration::milliseconds(500)
  let one_and_a_half = duration::milliseconds(1_500)
  assert duration::scale(one_and_a_half, factor) == duration::seconds(int::max_value) + half
  assert duration::scale(-one_and_a_half, factor) == duration::seconds(int::min_value) + half
  let one = duration::nanoseconds(1)
  assert duration::scale(one, int::max_value) == duration::nanoseconds(int::max_value)
  assert duration::scale(-one, int::min_value) == duration::nanoseconds(int::max_value) + one
  assert duration::scale(-half, int::min_value) == duration::seconds(-(int::min_value / 2))
}

@target(native)
test "scale panics just past the largest span" {
  // 1.5 s times this is 2⁶³ + 1 s, past the largest span.
  let factor = int::max_value / 3 * 2 + 2
  let fits = duration::scale(duration::milliseconds(1_500), factor - 1)
  assert fits == duration::seconds(int::max_value) + duration::milliseconds(500)
  let outcome = {
    use _scope <- fiber::scope()
    duration::scale(duration::milliseconds(1_500), factor)
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "integer overflow"
}
```

`duration::divide` waits until its exact algorithm is written (D-P22).

## Comparison

### `compare`

```cheby
pub fn compare(a: Duration, b: Duration) -> Order
```

Orders spans by length, negative spans first. It makes `Duration` satisfy `Compare`, so `<`, `<=`, `>` and `>=` work on spans (§8.7). It returns `Equal` exactly when `a == b`.

```cheby
test "compare orders by length" {
  assert duration::compare(duration::milliseconds(999), duration::seconds(1)) == Less
  assert duration::compare(duration::seconds(1), duration::milliseconds(1_000)) == Equal
  assert duration::milliseconds(-1) < duration::nanoseconds(1)
}
```

### `min`

```cheby
pub fn min(a: Duration, b: Duration) -> Duration
```

Returns the shorter of `a` and `b`, and `a` when they are equal.

```cheby
test "min picks the shorter" {
  assert duration::min(duration::seconds(3), duration::milliseconds(10)) == duration::milliseconds(10)
}
```

### `max`

```cheby
pub fn max(a: Duration, b: Duration) -> Duration
```

Returns the longer of `a` and `b`, and `a` when they are equal.

```cheby
test "max picks the longer" {
  assert duration::max(duration::seconds(3), duration::milliseconds(10)) == duration::seconds(3)
}
```

## Text

### `show`

```cheby
pub fn show(d: Duration) -> String
```

Returns the span as a number and a unit. It makes `Duration` satisfy `Show` (§8.8). The text is `0s` for `zero`. Otherwise it is `-` for a negative span, then the length in the largest of `s`, `ms`, `µs` and `ns` in which it is at least 1, written in decimal with the exact fraction, with trailing zeros and an empty fraction left out. The micro sign is U+00B5, as in Go and Rust (D-651).

```cheby
test "show picks the largest whole unit" {
  assert duration::show(duration::seconds(90)) == "90s"
  assert duration::show(duration::milliseconds(1_500)) == "1.5s"
  assert duration::show(duration::milliseconds(100)) == "100ms"
  assert duration::show(duration::nanoseconds(1_001)) == "1.001µs"
  assert duration::show(duration::nanoseconds(7)) == "7ns"
  assert duration::show(duration::milliseconds(-250)) == "-250ms"
  assert duration::show(duration::nanoseconds(1_000_000_001)) == "1.000000001s"
}
```
