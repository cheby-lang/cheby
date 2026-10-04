# `std::int`

The module of the built-in `Int` type, a signed 64-bit integer (§3.3.1, D-105). `Int` is in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub type Int
```

All arithmetic on `Int` is checked: a result that does not fit panics (D-025). On JS, an `Int` result outside ±(2⁵³−1) also panics (D-037), so a program panics at the same points or earlier on JS, but never computes a different value (§12.4, D-232).

This module has every function of the integer families in [Numeric modules](README.md#numeric-modules), written out in full, plus `clamp`, `pow`, `checked_pow` and `to_base`. `compare`, `show` and the arithmetic functions make `Int` satisfy `Compare`, `Show`, `Add`, `Sub`, `Mul`, `Div` and `Neg` (§8.7, §8.8). On `Int` operands the compiler uses the built-in operations directly (§5.4.2), so these functions exist to be passed as values and to satisfy bounds.

`clamp`, `pow`, `checked_pow` and `to_base` exist only in `std::int`, because the sized types are for FFI and binary data (D-105, D-389).

The bitwise operators `&`, `|`, `^`, `<<` and `>>` are built in on every integer type and cannot be overloaded (§5.4.6, D-092, D-118), so there are no functions for them. There is no `sign`, `is_even` or `gcd`: `int::compare(n, 0)` and `n % 2 == 0` say the same, and Euclid's algorithm is a three-line function (example 028).

There are no bit-manipulation functions such as `count_ones` or `rotate_left` in tier 1. They can be written with the built-in operators (D-391).

## Constants

### `min_value`

```cheby
@target(native)
pub const min_value: Int = -9_223_372_036_854_775_808
```

The smallest `Int`, −2⁶³. It exists only on native, because it is outside JS's safe-integer range, so code that uses it is restricted to native (D-358, §12.6, D-037). `min_safe` is the portable limit.

```cheby
@target(native)
test "min_value is the smallest Int" {
  assert int::min_value == -int::max_value - 1
  assert int::checked_sub(int::min_value, 1) == Err(Nil)
}
```

### `max_value`

```cheby
@target(native)
pub const max_value: Int = 9_223_372_036_854_775_807
```

The largest `Int`, 2⁶³−1. Like `min_value`, it exists only on native (D-358, D-037). `max_safe` is the portable limit.

```cheby
@target(native)
test "max_value is the largest Int" {
  assert int::max_value == int::pow(2, 62) - 1 + int::pow(2, 62)
  assert int::min_value + int::max_value == -1
}
```

### `min_safe`

```cheby
pub const min_safe: Int = -9_007_199_254_740_991
```

The smallest `Int` that works on every target, −(2⁵³−1) (D-037).

```cheby
test "min_safe is the negated max_safe" {
  assert int::min_safe == -9_007_199_254_740_991
  assert int::min_safe == -int::max_safe
}
```

### `max_safe`

```cheby
pub const max_safe: Int = 9_007_199_254_740_991
```

The largest `Int` that works on every target, 2⁵³−1 (D-037).

```cheby
test "max_safe is the largest portable Int" {
  assert int::max_safe == 9_007_199_254_740_991
  assert int::checked_sub(int::max_safe, 1) == Ok(9_007_199_254_740_990)
}
```

## Conversions

Conversions follow D-308: from each sized integer type there is a function `from_source`, which returns `Int` when every value of the source fits and `Result<Int, Nil>` otherwise. There is no `from_float` or `from_f32`. A float becomes an `Int` through `float::truncate`, `round`, `floor` and `ceil`, and the same functions of `std::f32` (D-359).

### `from_i8`

```cheby
pub fn from_i8(n: I8) -> Int
```

Returns `n` as an `Int`. Every `I8` fits.

- **Cost:** O(1). Informative.

```cheby
test "from_i8 widens" {
  assert int::from_i8(-128) == -128
  assert int::from_i8(i8::max_value) == 127
}
```

### `from_i16`

```cheby
pub fn from_i16(n: I16) -> Int
```

Returns `n` as an `Int`. Every `I16` fits.

- **Cost:** O(1). Informative.

```cheby
test "from_i16 widens" {
  assert int::from_i16(-32768) == -32768
}
```

### `from_i32`

```cheby
pub fn from_i32(n: I32) -> Int
```

Returns `n` as an `Int`. Every `I32` fits.

- **Cost:** O(1). Informative.

```cheby
test "from_i32 widens" {
  assert int::from_i32(i32::max_value) == 2_147_483_647
}
```

### `from_i64`

```cheby
pub fn from_i64(n: I64) -> Result<Int, Nil>
```

Returns `n` as an `Int`. The conversion is narrowing on every target, because it can fail on JS (ADR-0030).

- **Fails:** no `I64` gives `Err(Nil)`, because every `I64` is inside `Int`'s 64-bit range (D-357).
- **Panics:** on JS, when `n` is outside ±(2⁵³−1), like any other `Int` result (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "from_i64 converts a small value on every target" {
  assert int::from_i64(-42) == Ok(-42)
}

@target(native)
test "from_i64 keeps every I64 on native" {
  assert int::from_i64(i64::max_value) == Ok(int::max_value)
}
```

### `from_u8`

```cheby
pub fn from_u8(n: U8) -> Int
```

Returns `n` as an `Int`. Every `U8` fits.

- **Cost:** O(1). Informative.

```cheby
test "from_u8 widens" {
  assert int::from_u8(255) == 255
}
```

### `from_u16`

```cheby
pub fn from_u16(n: U16) -> Int
```

Returns `n` as an `Int`. Every `U16` fits.

- **Cost:** O(1). Informative.

```cheby
test "from_u16 widens" {
  assert int::from_u16(u16::max_value) == 65535
}
```

### `from_u32`

```cheby
pub fn from_u32(n: U32) -> Int
```

Returns `n` as an `Int`. Every `U32` fits, on JS too.

- **Cost:** O(1). Informative.

```cheby
test "from_u32 widens" {
  assert int::from_u32(u32::max_value) == 4_294_967_295
  assert list::get(["a", "b", "c"], int::from_u32(1)) == Some("b")
}
```

### `from_u64`

```cheby
pub fn from_u64(n: U64) -> Result<Int, Nil>
```

Returns `n` as an `Int`.

- **Fails:** `Err(Nil)` when `n` is greater than 2⁶³−1.
- **Panics:** on JS, when `n` is greater than 2⁵³−1 and at most 2⁶³−1 (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "from_u64 fails above Int's range" {
  assert int::from_u64(42) == Ok(42)
  assert int::from_u64(u64::max_value) == Err(Nil)
}
```

## Parsing and text

### `parse`

```cheby
pub fn parse(text: String) -> Result<Int, Nil>
```

Reads an `Int` written in decimal: an optional `-` followed by one or more ASCII digits, and nothing else (D-312). Leading zeros are allowed.

- **Fails:** `Err(Nil)` when `text` has any other character, including `+`, `_`, whitespace and a base prefix such as `0x`, when it has no digits, and when the value is outside `Int`'s 64-bit range (D-312).
- **Panics:** on JS, when the value is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(n), where n is the length of `text`. Informative.

```cheby
test "parse reads an optional minus and decimal digits" {
  assert int::parse("42") == Ok(42)
  assert int::parse("-007") == Ok(-7)
  assert int::parse("+42") == Err(Nil)
  assert int::parse(" 42") == Err(Nil)
  assert int::parse("1_000") == Err(Nil)
  assert int::parse("0x2A") == Err(Nil)
  assert int::parse("-") == Err(Nil)
  assert int::parse("") == Err(Nil)
  assert int::parse("9223372036854775808") == Err(Nil)
}
```

### `parse_base`

```cheby
pub fn parse_base(text: String, base: Int) -> Result<Int, Nil>
```

Reads an `Int` written in `base`, from 2 to 36: an optional `-` followed by one or more digits of that base, and nothing else (D-312). The digits are `0` to `9` and then the letters `a` to `z` in either case, for the values 10 to 35.

- **Fails:** `Err(Nil)` when `base` is outside 2 to 36, when `text` has a character that is not a digit of `base` (other than the leading `-`), including `+`, `_`, whitespace and a prefix such as `0x`, when it has no digits, and when the value is outside `Int`'s 64-bit range.
- **Panics:** on JS, when the value is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(n), where n is the length of `text`. Informative.

```cheby
test "parse_base reads other bases" {
  assert int::parse_base("ff", 16) == Ok(255)
  assert int::parse_base("FF", 16) == Ok(255)
  assert int::parse_base("-101", 2) == Ok(-5)
  assert int::parse_base("z", 36) == Ok(35)
  assert int::parse_base("12", 2) == Err(Nil)
  assert int::parse_base("0xff", 16) == Err(Nil)
  assert int::parse_base("1", 37) == Err(Nil)
}
```

### `show`

```cheby
pub fn show(n: Int) -> String
```

Returns `n` in decimal, with `-` for a negative value and no leading zeros. It makes `Int` satisfy `Show` (§8.8), so `{n}` interpolation uses it. `parse` reads the text back as `n`.

- **Cost:** O(1). Informative.

```cheby
test "show writes decimal digits" {
  let count = 3
  assert int::show(-42) == "-42"
  assert int::show(0) == "0"
  assert "{count} items" == "3 items"
  assert int::parse(int::show(-1234)) == Ok(-1234)
}
```

### `to_base`

```cheby
pub fn to_base(n: Int, base: Int) -> Result<String, Nil>
```

Returns `n` written in `base`, from 2 to 36, with the digits `0` to `9` and then lowercase `a` to `z`, `-` for a negative value, no prefix and no leading zeros. `parse_base` reads the text back as `n`.

- **Fails:** `Err(Nil)` when `base` is outside 2 to 36.
- **Cost:** O(1). Informative.

```cheby
test "to_base writes other bases" {
  assert int::to_base(255, 16) == Ok("ff")
  assert int::to_base(-5, 2) == Ok("-101")
  assert int::to_base(0, 8) == Ok("0")
  assert int::to_base(10, 1) == Err(Nil)
  assert int::parse_base("7b", 16) == Ok(123)
  assert int::to_base(123, 16) == Ok("7b")
}
```

`to_base` is the inverse of `parse_base`. Text in another base is a different operation from `show`, so it does not break D-307 (D-390).

## Comparison

### `compare`

```cheby
pub fn compare(a: Int, b: Int) -> Order
```

Orders integers by value. It makes `Int` satisfy `Compare` (§8.7). The operators `<`, `<=`, `>` and `>=` on `Int` give the same answers.

- **Cost:** O(1). Informative.

```cheby
test "compare orders by value" {
  assert int::compare(1, 2) == Less
  assert int::compare(2, 2) == Equal
  assert int::compare(-1, -2) == Greater
  assert list::sort_with([3, 1, 2], fn(a, b) { int::compare(b, a) }) == [3, 2, 1]
}
```

### `min`

```cheby
pub fn min(a: Int, b: Int) -> Int
```

Returns the smaller of `a` and `b`.

- **Cost:** O(1). Informative.

```cheby
test "min picks the smaller" {
  assert int::min(3, 7) == 3
  assert int::min(-3, -7) == -7
}
```

### `max`

```cheby
pub fn max(a: Int, b: Int) -> Int
```

Returns the larger of `a` and `b`.

- **Cost:** O(1). Informative.

```cheby
test "max picks the larger" {
  assert int::max(3, 7) == 7
  assert list::fold([4, 9, 2], int::min_safe, int::max) == 9
}
```

### `clamp`

```cheby
pub fn clamp(n: Int, low: Int, high: Int) -> Int
```

Returns `n` limited to the range from `low` to `high`, both included: `low` when `n` is below it, `high` when `n` is above it, and `n` otherwise. It is `int::max(int::min(n, high), low)`, so when `low` is greater than `high` the result is `low`.

- **Cost:** O(1). Informative.

```cheby
test "clamp limits to a range" {
  assert int::clamp(15, 0, 10) == 10
  assert int::clamp(-3, 0, 10) == 0
  assert int::clamp(4, 0, 10) == 4
  assert int::clamp(4, 10, 0) == 10
}
```

## Arithmetic

### `add`

```cheby
pub fn add(a: Int, b: Int) -> Int
```

Returns `a + b`. It makes `Int` satisfy `Add` (§8.7).

- **Panics:** when the result is outside `Int`'s range (D-025), and on JS outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
test "add is plus as a function" {
  assert int::add(2, 3) == 5
  assert list::fold([1, 2, 3], 0, int::add) == 6
}
```

### `sub`

```cheby
pub fn sub(a: Int, b: Int) -> Int
```

Returns `a - b`. It makes `Int` satisfy `Sub` (§8.7).

- **Panics:** when the result is outside `Int`'s range (D-025), and on JS outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
test "sub is minus as a function" {
  assert int::sub(2, 3) == -1
}
```

### `mul`

```cheby
pub fn mul(a: Int, b: Int) -> Int
```

Returns `a * b`. It makes `Int` satisfy `Mul` (§8.7).

- **Panics:** when the result is outside `Int`'s range (D-025), and on JS outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
test "mul is times as a function" {
  assert int::mul(-4, 5) == -20
  assert list::fold([1, 2, 3, 4], 1, int::mul) == 24
}
```

### `div`

```cheby
pub fn div(a: Int, b: Int) -> Int
```

Returns `a / b`, truncated toward zero (D-069). It makes `Int` satisfy `Div` (§8.7).

- **Panics:** when `b` is 0 (D-069), and for `min_value / -1`, whose result does not fit (§3.3.1).
- **Cost:** O(1). Informative.

```cheby
test "div truncates toward zero" {
  assert int::div(7, 2) == 3
  assert int::div(-7, 2) == -3
}
```

### `rem`

```cheby
pub fn rem(a: Int, b: Int) -> Int
```

Returns `a % b`, the remainder of `div`, with the sign of `a` (D-092). `%` cannot be overloaded (D-118), so this function exists only to be passed as a value.

- **Panics:** when `b` is 0 (D-069).
- **Cost:** O(1). Informative.

```cheby
test "rem keeps the sign of the dividend" {
  assert int::rem(7, 2) == 1
  assert int::rem(-7, 2) == -1
  assert int::rem(7, -2) == 1
}
```

### `neg`

```cheby
pub fn neg(n: Int) -> Int
```

Returns `-n`. It makes `Int` satisfy `Neg` (§8.7).

- **Panics:** for `min_value`, whose negation does not fit (D-025).
- **Cost:** O(1). Informative.

```cheby
test "neg flips the sign" {
  assert int::neg(5) == -5
  assert int::neg(-5) == 5
  assert int::neg(0) == 0
}
```

### `abs`

```cheby
pub fn abs(n: Int) -> Int
```

Returns `n` without its sign.

- **Panics:** for `min_value`, like `neg` (D-025).
- **Cost:** O(1). Informative.

```cheby
test "abs drops the sign" {
  assert int::abs(-5) == 5
  assert int::abs(5) == 5
}
```

### `pow`

```cheby
pub fn pow(n: Int, exponent: Int) -> Int
```

Returns `n` multiplied by itself `exponent` times, so `int::pow(n, 0)` is 1 for every `n`, including 0.

- **Clamps:** `exponent` below 0 counts as 0, so the result is 1 (D-298, D-392).
- **Panics:** when the result is outside `Int`'s range (D-025), and on JS outside ±(2⁵³−1) (D-037).
- **Cost:** O(log `exponent`). Informative.

```cheby
test "pow multiplies repeatedly" {
  let below_zero = -2
  assert int::pow(2, 10) == 1024
  assert int::pow(-3, 3) == -27
  assert int::pow(0, 0) == 1
  assert int::pow(10, below_zero) == 1
}
```

The exponent is a clamped count, so the functions stay total, and a negative literal warns (D-303, D-392).

## Checked arithmetic

### `checked_add`

```cheby
pub fn checked_add(a: Int, b: Int) -> Result<Int, Nil>
```

Returns `Ok(a + b)` when the sum fits (D-309).

- **Fails:** `Err(Nil)` when the sum is outside `Int`'s 64-bit range.
- **Panics:** on JS, when the sum is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "checked_add returns a sum that fits" {
  assert int::checked_add(1, 2) == Ok(3)
}

@target(native)
test "checked_add reports overflow" {
  assert int::checked_add(int::max_value, 1) == Err(Nil)
  assert int::checked_add(int::min_value, -1) == Err(Nil)
}
```

### `checked_sub`

```cheby
pub fn checked_sub(a: Int, b: Int) -> Result<Int, Nil>
```

Returns `Ok(a - b)` when the difference fits (D-309).

- **Fails:** `Err(Nil)` when the difference is outside `Int`'s 64-bit range.
- **Panics:** on JS, when the difference is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "checked_sub returns a difference that fits" {
  assert int::checked_sub(1, 3) == Ok(-2)
}

@target(native)
test "checked_sub reports overflow" {
  assert int::checked_sub(int::min_value, 1) == Err(Nil)
}
```

### `checked_mul`

```cheby
pub fn checked_mul(a: Int, b: Int) -> Result<Int, Nil>
```

Returns `Ok(a * b)` when the product fits (D-309).

- **Fails:** `Err(Nil)` when the product is outside `Int`'s 64-bit range.
- **Panics:** on JS, when the product is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "checked_mul returns a product that fits" {
  assert int::checked_mul(-4, 5) == Ok(-20)
}

@target(native)
test "checked_mul reports overflow" {
  assert int::checked_mul(int::max_value, 2) == Err(Nil)
}
```

### `checked_div`

```cheby
pub fn checked_div(a: Int, b: Int) -> Result<Int, Nil>
```

Returns `Ok(a / b)`, truncated toward zero, when the division is defined and its result fits (D-309).

- **Fails:** `Err(Nil)` when `b` is 0 (D-356), and for `min_value / -1`.
- **Cost:** O(1). Informative.

```cheby
test "checked_div reports a zero divisor" {
  assert int::checked_div(7, 2) == Ok(3)
  assert int::checked_div(-7, 2) == Ok(-3)
  assert int::checked_div(7, 0) == Err(Nil)
}

@target(native)
test "checked_div reports overflow" {
  assert int::checked_div(int::min_value, -1) == Err(Nil)
}
```

### `checked_rem`

```cheby
pub fn checked_rem(a: Int, b: Int) -> Result<Int, Nil>
```

Returns `Ok(a % b)`, with the sign of `a`, when `b` is not 0 (D-309).

- **Fails:** `Err(Nil)` when `b` is 0 (D-356).
- **Cost:** O(1). Informative.

```cheby
test "checked_rem reports a zero divisor" {
  assert int::checked_rem(-7, 2) == Ok(-1)
  assert int::checked_rem(7, 0) == Err(Nil)
}
```

### `checked_neg`

```cheby
pub fn checked_neg(n: Int) -> Result<Int, Nil>
```

Returns `Ok(-n)` when the negation fits (D-309).

- **Fails:** `Err(Nil)` for `min_value`.
- **Cost:** O(1). Informative.

```cheby
test "checked_neg negates" {
  assert int::checked_neg(5) == Ok(-5)
}

@target(native)
test "checked_neg reports overflow" {
  assert int::checked_neg(int::min_value) == Err(Nil)
}
```

### `checked_pow`

```cheby
pub fn checked_pow(n: Int, exponent: Int) -> Result<Int, Nil>
```

Returns `Ok(int::pow(n, exponent))` when the result fits (D-309, D-392).

- **Fails:** `Err(Nil)` when the result is outside `Int`'s 64-bit range.
- **Clamps:** `exponent` below 0 counts as 0, so the result is `Ok(1)` (D-298, D-392).
- **Panics:** on JS, when the result is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(log `exponent`). Informative.

```cheby
test "checked_pow returns a power that fits" {
  assert int::checked_pow(2, 10) == Ok(1024)
}

@target(native)
test "checked_pow reports overflow" {
  assert int::checked_pow(2, 62) == Ok(int::max_value / 2 + 1)
  assert int::checked_pow(2, 63) == Err(Nil)
}
```

## Wrapping arithmetic

### `wrapping_add`

```cheby
pub fn wrapping_add(a: Int, b: Int) -> Int
```

Returns `a + b` modulo 2⁶⁴, as a signed value (D-025).

- **Panics:** on JS, when the result is outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
@target(native)
test "wrapping_add wraps around" {
  assert int::wrapping_add(2, 3) == 5
  assert int::wrapping_add(int::max_value, 1) == int::min_value
}
```

### `wrapping_sub`

```cheby
pub fn wrapping_sub(a: Int, b: Int) -> Int
```

Returns `a - b` modulo 2⁶⁴, as a signed value (D-025).

- **Panics:** on JS, when the result is outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
@target(native)
test "wrapping_sub wraps around" {
  assert int::wrapping_sub(2, 3) == -1
  assert int::wrapping_sub(int::min_value, 1) == int::max_value
}
```

### `wrapping_mul`

```cheby
pub fn wrapping_mul(a: Int, b: Int) -> Int
```

Returns `a * b` modulo 2⁶⁴, as a signed value (D-025).

- **Panics:** on JS, when the result is outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
@target(native)
test "wrapping_mul wraps around" {
  assert int::wrapping_mul(-4, 5) == -20
  assert int::wrapping_mul(int::max_value, 2) == -2
}
```

### `wrapping_neg`

```cheby
pub fn wrapping_neg(n: Int) -> Int
```

Returns `-n` modulo 2⁶⁴, as a signed value, so the negation of `min_value` is `min_value` (D-025).

- **Panics:** on JS, when the result is outside ±(2⁵³−1) (D-037).
- **Cost:** O(1). Informative.

```cheby
@target(native)
test "wrapping_neg wraps around" {
  assert int::wrapping_neg(5) == -5
  assert int::wrapping_neg(int::min_value) == int::min_value
}
```
