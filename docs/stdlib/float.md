# `std::float`

The module of the built-in `Float` type, an IEEE 754 binary64 number (§3.3.2, D-105). `Float` is in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub type Float
```

Arithmetic on `Float` follows IEEE 754 with rounding to the nearest value, ties to even, so it can produce infinities and NaN and never panics (D-069). Equality is not IEEE equality: `==` is reflexive, so every NaN equals every other NaN, and `-0.0 == 0.0` (D-069). The examples below rely on this when they compare with `float::nan`.

This module has every function of the float families in [Numeric modules](README.md#numeric-modules), written out in full, plus `clamp`, the conversions to `Int`, the tests for special values, square roots, powers and a few transcendental functions. `compare`, `show` and the arithmetic functions make `Float` satisfy `Compare`, `Show`, `Add`, `Sub`, `Mul`, `Div` and `Neg` (§8.7, §8.8). On `Float` operands the compiler uses the built-in operations directly (§5.4.2), so these functions exist to be passed as values and to satisfy bounds.

The rounding functions `truncate`, `round`, `floor` and `ceil` return `Result<Int, Nil>` (D-132). There are no rounding functions that return a `Float`. `x % 1.0` gives the fractional part of `x`.

## Constants

### `infinity`

```cheby
pub const infinity: Float = 1.0 / 0.0
```

Positive infinity, which shows as `inf` (D-219).

```cheby
test "infinity is above every finite float" {
  assert float::infinity > float::max_value
  assert float::show(float::infinity) == "inf"
}
```

### `neg_infinity`

```cheby
pub const neg_infinity: Float = -1.0 / 0.0
```

Negative infinity, which shows as `-inf` (D-219).

```cheby
test "neg_infinity is below every finite float" {
  assert float::neg_infinity < float::min_value
  assert float::neg_infinity == -float::infinity
}
```

### `nan`

```cheby
pub const nan: Float = 0.0 / 0.0
```

A NaN, which shows as `NaN` (D-219). Every NaN is `==` to every other NaN (D-069), so a program cannot tell one NaN from another.

```cheby
test "nan equals itself" {
  assert float::nan == float::nan
  assert float::is_nan(float::nan)
  assert float::sqrt(-1.0) == float::nan
}
```

### `pi`

```cheby
pub const pi: Float = 3.141592653589793
```

The `Float` closest to π.

```cheby
test "pi is the float closest to pi" {
  assert float::pi == 3.141592653589793
}
```

### `e`

```cheby
pub const e: Float = 2.718281828459045
```

The `Float` closest to Euler's number e.

```cheby
test "e is the float closest to e" {
  assert float::e == 2.718281828459045
  assert float::exp(1.0) == float::e
}
```

### `epsilon`

```cheby
pub const epsilon: Float = 2.220446049250313e-16
```

The difference between 1.0 and the next larger `Float`, 2⁻⁵².

```cheby
test "epsilon is the gap above 1.0" {
  assert 1.0 + float::epsilon > 1.0
  assert 1.0 + float::epsilon / 2.0 == 1.0
}
```

### `max_value`

```cheby
pub const max_value: Float = 1.7976931348623157e308
```

The largest finite `Float`, (2−2⁻⁵²)×2¹⁰²³.

```cheby
test "max_value is the largest finite float" {
  assert float::is_finite(float::max_value)
  assert float::max_value * 2.0 == float::infinity
}
```

### `min_value`

```cheby
pub const min_value: Float = -1.7976931348623157e308
```

The smallest finite `Float`, the negation of `max_value`.

```cheby
test "min_value is the smallest finite float" {
  assert float::min_value == -float::max_value
}
```

These are the constants of `std::float` (D-393). `min_value` and `max_value` are the extreme finite values, as Rust's `f64::MIN` and `f64::MAX` are.

## Conversions

Every integer type converts to `Float` without failing, and so does `F32` (D-359). The integer types whose every value is exact as a `Float` (`I8` to `I32` and `U8` to `U32`) convert exactly. `Int`, `I64` and `U64` values beyond ±2⁵³ are rounded to the nearest `Float`, ties to even (D-359). `Float` becomes an integer only through [`truncate`](#truncate), [`round`](#round), [`floor`](#floor) and [`ceil`](#ceil).

### `from_int`

```cheby
pub fn from_int(n: Int) -> Float
```

Returns `n` as a `Float`, rounded to the nearest value, ties to even, when `n` is beyond ±2⁵³. On JS every `Int` is inside that range, so the result is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_int converts" {
  assert float::from_int(-3) == -3.0
}

@target(native)
test "from_int rounds beyond 2^53" {
  assert float::from_int(int::max_safe + 2) == 9007199254740992.0
}
```

### `from_i8`

```cheby
pub fn from_i8(n: I8) -> Float
```

Returns `n` as a `Float`. Every `I8` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_i8 is exact" {
  assert float::from_i8(-128) == -128.0
}
```

### `from_i16`

```cheby
pub fn from_i16(n: I16) -> Float
```

Returns `n` as a `Float`. Every `I16` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_i16 is exact" {
  assert float::from_i16(-32768) == -32768.0
}
```

### `from_i32`

```cheby
pub fn from_i32(n: I32) -> Float
```

Returns `n` as a `Float`. Every `I32` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_i32 is exact" {
  assert float::from_i32(i32::max_value) == 2147483647.0
}
```

### `from_i64`

```cheby
pub fn from_i64(n: I64) -> Float
```

Returns `n` as a `Float`, rounded to the nearest value, ties to even, when `n` is beyond ±2⁵³. `I64` is exact on JS (ADR-0030), so the rounding is the same on every target.

- **Cost:** O(1). Informative.

```cheby
test "from_i64 rounds beyond 2^53" {
  assert float::from_i64(-42) == -42.0
  assert float::from_i64(9_007_199_254_740_993) == 9007199254740992.0
}
```

### `from_u8`

```cheby
pub fn from_u8(n: U8) -> Float
```

Returns `n` as a `Float`. Every `U8` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_u8 is exact" {
  assert float::from_u8(255) == 255.0
}
```

### `from_u16`

```cheby
pub fn from_u16(n: U16) -> Float
```

Returns `n` as a `Float`. Every `U16` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_u16 is exact" {
  assert float::from_u16(65535) == 65535.0
}
```

### `from_u32`

```cheby
pub fn from_u32(n: U32) -> Float
```

Returns `n` as a `Float`. Every `U32` is exact.

- **Cost:** O(1). Informative.

```cheby
test "from_u32 is exact" {
  assert float::from_u32(u32::max_value) == 4294967295.0
  assert 0.5 * float::from_u32(10) == 5.0
}
```

### `from_u64`

```cheby
pub fn from_u64(n: U64) -> Float
```

Returns `n` as a `Float`, rounded to the nearest value, ties to even, when `n` is beyond 2⁵³. `U64` is exact on JS (ADR-0030), so the rounding is the same on every target.

- **Cost:** O(1). Informative.

```cheby
test "from_u64 rounds beyond 2^53" {
  assert float::from_u64(u64::max_value) == 18446744073709551616.0
}
```

### `from_f32`

```cheby
pub fn from_f32(n: F32) -> Float
```

Returns `n` as a `Float`. Every `F32` is exact, including the infinities, NaN and `-0.0`.

- **Cost:** O(1). Informative.

```cheby
test "from_f32 is exact" {
  assert float::from_f32(1.5) == 1.5
  assert float::from_f32(f32::infinity) == float::infinity
}
```

## Parsing and text

### `parse`

```cheby
pub fn parse(text: String) -> Result<Float, Nil>
```

Reads a `Float` written in decimal (D-313). `text` must match `float_text` below, with nothing before or after it:

```ebnf
float_text = [ "-" ] digits [ "." digits ] [ ( "e" | "E" ) [ "+" | "-" ] digits ]
           | "NaN" | "inf" | "-inf" ;
digits     = ascii_digit { ascii_digit } ;
```

The value is the decimal number rounded to the nearest `Float`, ties to even, so `"0.1"` gives the same value as the literal `0.1`. A value too large for `Float` gives an infinity, and one too small gives a zero with its sign (D-394). Every text `show` gives reads back as the same value (D-313).

- **Fails:** `Err(Nil)` when `text` does not match `float_text`, including `_`, `.5`, `1.`, a leading `+`, whitespace, an empty string, and other spellings of the special values such as `nan` or `Infinity` (D-313).
- **Cost:** O(n), where n is the length of `text`. Informative.

```cheby
test "parse reads decimal floats" {
  assert float::parse("42") == Ok(42.0)
  assert float::parse("-1.5e-3") == Ok(-0.0015)
  assert float::parse("1E+3") == Ok(1000.0)
  assert float::parse("NaN") == Ok(float::nan)
  assert float::parse("-inf") == Ok(float::neg_infinity)
}

test "parse rejects other spellings" {
  assert float::parse(".5") == Err(Nil)
  assert float::parse("1.") == Err(Nil)
  assert float::parse("+1.0") == Err(Nil)
  assert float::parse(" 1.0") == Err(Nil)
  assert float::parse("1_000.0") == Err(Nil)
  assert float::parse("nan") == Err(Nil)
}

test "parse reads back what show writes" {
  let third = 1.0 / 3.0
  assert float::parse(float::show(third)) == Ok(third)
  assert float::parse("1e400") == Ok(float::infinity)
}
```

A number beyond `Float`'s range rounds to an infinity or a zero with its sign, as in Rust and JS's `JSON.parse`, so every well-formed number parses (D-394).

### `show`

```cheby
pub fn show(x: Float) -> String
```

Returns the text of `x`, identically on every target (§3.3.6, D-203). It makes `Float` satisfy `Show` (§8.8), so `{x}` interpolation uses it.

- NaN is `NaN`, the infinities are `inf` and `-inf`, and negative zero is `-0.0` (D-219).
- Any other value is written with the fewest significant decimal digits that `parse` reads back as `x`. When several such digit strings exist, the one closest to `x` is used, and of two equally close the one whose last digit is even.
- With those digits written as d.ddd × 10ᵖ, the text is in decimal form when p is from −6 to 20, such as `0.000001`, `2.0` and `100000000000000000000.0`, and in exponent form otherwise, such as `1.0e-7` and `1.0e21` (D-395).
- The decimal form has at least one digit on each side of the `.`, with `.0` added to a whole number.
- The exponent form is one digit, `.`, the remaining digits or `0` when there are none, `e` and p, with `-` when p is negative and no `+` or leading zeros, as in `1.5e300`. It is a valid float literal (§2.5.2).
- A negative value starts with `-`.

`parse` reads every such text back as `x` (D-313).

- **Cost:** O(1). Informative.

```cheby
test "show writes the shortest text that reads back" {
  let half = 0.5
  let sum = 0.1 + 0.2
  assert float::show(2.0) == "2.0"
  assert float::show(0.1) == "0.1"
  assert float::show(-2.25) == "-2.25"
  assert "{half}" == "0.5"
  assert "{sum}" == "0.30000000000000004"
}

test "show uses an exponent for large and small magnitudes" {
  assert float::show(1.0e20) == "100000000000000000000.0"
  assert float::show(1.0e21) == "1.0e21"
  assert float::show(0.000001) == "0.000001"
  assert float::show(1.0e-7) == "1.0e-7"
  assert float::show(-1.5e300) == "-1.5e300"
}

test "show spells the special values" {
  assert float::show(float::nan) == "NaN"
  assert float::show(float::infinity) == "inf"
  assert float::show(float::neg_infinity) == "-inf"
  assert float::show(-0.0) == "-0.0"
}
```

These are the thresholds of JS's `Number.prototype.toString`, so numbers print as JSON tools print them, apart from Cheby's `.0` and exponent spelling (D-395).

## Comparison

### `compare`

```cheby
pub fn compare(a: Float, b: Float) -> Order
```

Orders floats by value, in a total order consistent with `==`: `-0.0` and `0.0` are `Equal`, and every NaN is `Equal` to every other NaN and `Greater` than every other value, including `inf` (D-135). It makes `Float` satisfy `Compare` (§8.7), so `<`, `<=`, `>` and `>=` on `Float` follow the same order, and `float::nan > float::infinity` holds.

- **Cost:** O(1). Informative.

```cheby
test "compare is a total order" {
  assert float::compare(1.0, 2.0) == Less
  assert float::compare(-0.0, 0.0) == Equal
  assert float::compare(float::nan, float::infinity) == Greater
  assert float::compare(float::nan, float::nan) == Equal
  assert list::sort([float::nan, 1.0, float::neg_infinity]) == [float::neg_infinity, 1.0, float::nan]
}
```

### `min`

```cheby
pub fn min(a: Float, b: Float) -> Float
```

Returns the smaller of `a` and `b` by `compare`, and `a` when they are `Equal`. A NaN is never smaller than another value, which differs from IEEE 754's `minimum`.

- **Cost:** O(1). Informative.

```cheby
test "min follows compare" {
  assert float::min(1.0, 2.0) == 1.0
  assert float::min(1.0, float::nan) == 1.0
  assert float::show(float::min(-0.0, 0.0)) == "-0.0"
}
```

### `max`

```cheby
pub fn max(a: Float, b: Float) -> Float
```

Returns the larger of `a` and `b` by `compare`, and `a` when they are `Equal`. A NaN is larger than every other value.

- **Cost:** O(1). Informative.

```cheby
test "max follows compare" {
  assert float::max(1.0, 2.0) == 2.0
  assert float::is_nan(float::max(1.0, float::nan))
}
```

### `clamp`

```cheby
pub fn clamp(x: Float, low: Float, high: Float) -> Float
```

Returns `x` limited to the range from `low` to `high`, both included, by `compare`. It is `float::max(float::min(x, high), low)`, so a NaN `x` gives `high`, and when `low` is greater than `high` the result is `low`.

- **Cost:** O(1). Informative.

```cheby
test "clamp limits to a range" {
  assert float::clamp(1.5, 0.0, 1.0) == 1.0
  assert float::clamp(-0.5, 0.0, 1.0) == 0.0
  assert float::clamp(0.25, 0.0, 1.0) == 0.25
  assert float::clamp(float::nan, 0.0, 1.0) == 1.0
}
```

## Arithmetic

These functions never panic. Their results follow IEEE 754, rounded to the nearest value, ties to even (D-069).

### `add`

```cheby
pub fn add(a: Float, b: Float) -> Float
```

Returns `a + b`. It makes `Float` satisfy `Add` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "add is plus as a function" {
  assert float::add(0.5, 0.25) == 0.75
  assert list::fold([0.5, 1.5, 2.0], 0.0, float::add) == 4.0
  assert float::add(float::infinity, float::neg_infinity) == float::nan
}
```

### `sub`

```cheby
pub fn sub(a: Float, b: Float) -> Float
```

Returns `a - b`. It makes `Float` satisfy `Sub` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "sub is minus as a function" {
  assert float::sub(0.5, 0.25) == 0.25
}
```

### `mul`

```cheby
pub fn mul(a: Float, b: Float) -> Float
```

Returns `a * b`. It makes `Float` satisfy `Mul` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "mul is times as a function" {
  assert float::mul(1.5, -2.0) == -3.0
  assert float::mul(float::max_value, 2.0) == float::infinity
}
```

### `div`

```cheby
pub fn div(a: Float, b: Float) -> Float
```

Returns `a / b`. It makes `Float` satisfy `Div` (§8.7). Division by zero gives an infinity, or NaN for `0.0 / 0.0`.

- **Cost:** O(1). Informative.

```cheby
test "div follows IEEE 754" {
  assert float::div(1.0, 4.0) == 0.25
  assert float::div(1.0, 0.0) == float::infinity
  assert float::div(-1.0, 0.0) == float::neg_infinity
  assert float::is_nan(float::div(0.0, 0.0))
}
```

### `rem`

```cheby
pub fn rem(a: Float, b: Float) -> Float
```

Returns `a % b`, the remainder of `a / b` truncated toward zero, with the sign of `a`, like C's `fmod` (D-154). The result is exact. It is NaN when `b` is zero or `a` is infinite. `%` cannot be overloaded (D-118), so this function exists only to be passed as a value.

- **Cost:** O(1). Informative.

```cheby
test "rem keeps the sign of the dividend" {
  assert float::rem(7.5, 2.0) == 1.5
  assert float::rem(-7.5, 2.0) == -1.5
  assert float::is_nan(float::rem(1.0, 0.0))
}
```

### `neg`

```cheby
pub fn neg(x: Float) -> Float
```

Returns `-x`, flipping the sign, so the negation of `0.0` is `-0.0`. It makes `Float` satisfy `Neg` (§8.7).

- **Cost:** O(1). Informative.

```cheby
test "neg flips the sign" {
  assert float::neg(1.5) == -1.5
  assert float::show(float::neg(0.0)) == "-0.0"
}
```

### `abs`

```cheby
pub fn abs(x: Float) -> Float
```

Returns `x` without its sign, so `-0.0` becomes `0.0` and `-inf` becomes `inf`. The absolute value of a NaN is a NaN.

- **Cost:** O(1). Informative.

```cheby
test "abs drops the sign" {
  assert float::abs(-1.5) == 1.5
  assert float::abs(float::neg_infinity) == float::infinity
  assert float::show(float::abs(-0.0)) == "0.0"
}
```

## Conversion to `Int`

Each of these functions rounds `x` to a whole number and returns it as an `Int` (D-085, D-132). They fail and panic in the same cases.

### `truncate`

```cheby
pub fn truncate(x: Float) -> Result<Int, Nil>
```

Returns the whole number toward zero from `x`, dropping the fraction.

- **Fails:** `Err(Nil)` for NaN, the infinities, and a result outside `Int`'s 64-bit range (D-132).
- **Panics:** on JS, when the result is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "truncate drops the fraction" {
  assert float::truncate(2.7) == Ok(2)
  assert float::truncate(-2.7) == Ok(-2)
  assert float::truncate(float::nan) == Err(Nil)
  assert float::truncate(float::infinity) == Err(Nil)
  assert float::truncate(1.0e19) == Err(Nil)
}
```

### `round`

```cheby
pub fn round(x: Float) -> Result<Int, Nil>
```

Returns the whole number nearest to `x`, with halfway cases rounded away from zero, on every target (D-314).

- **Fails:** `Err(Nil)` for NaN, the infinities, and a result outside `Int`'s 64-bit range (D-132).
- **Panics:** on JS, when the result is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "round takes halves away from zero" {
  assert float::round(2.4) == Ok(2)
  assert float::round(2.5) == Ok(3)
  assert float::round(-2.5) == Ok(-3)
  assert float::round(-0.4) == Ok(0)
}
```

### `floor`

```cheby
pub fn floor(x: Float) -> Result<Int, Nil>
```

Returns the largest whole number that is not greater than `x`.

- **Fails:** `Err(Nil)` for NaN, the infinities, and a result outside `Int`'s 64-bit range (D-132).
- **Panics:** on JS, when the result is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "floor rounds down" {
  assert float::floor(2.7) == Ok(2)
  assert float::floor(-2.5) == Ok(-3)
  assert float::floor(float::neg_infinity) == Err(Nil)
}
```

### `ceil`

```cheby
pub fn ceil(x: Float) -> Result<Int, Nil>
```

Returns the smallest whole number that is not less than `x`.

- **Fails:** `Err(Nil)` for NaN, the infinities, and a result outside `Int`'s 64-bit range (D-132).
- **Panics:** on JS, when the result is inside the 64-bit range but outside ±(2⁵³−1) (D-037, D-357).
- **Cost:** O(1). Informative.

```cheby
test "ceil rounds up" {
  assert float::ceil(2.1) == Ok(3)
  assert float::ceil(-2.5) == Ok(-2)
}
```

## Special values

### `is_nan`

```cheby
pub fn is_nan(x: Float) -> Bool
```

Tells whether `x` is a NaN. `x == float::nan` gives the same answer, because `==` is reflexive (D-069).

- **Cost:** O(1). Informative.

```cheby
test "is_nan finds NaN" {
  assert float::is_nan(float::nan)
  assert !float::is_nan(float::infinity)
}
```

### `is_infinite`

```cheby
pub fn is_infinite(x: Float) -> Bool
```

Tells whether `x` is `inf` or `-inf`.

- **Cost:** O(1). Informative.

```cheby
test "is_infinite finds both infinities" {
  assert float::is_infinite(float::neg_infinity)
  assert !float::is_infinite(float::nan)
  assert !float::is_infinite(float::max_value)
}
```

### `is_finite`

```cheby
pub fn is_finite(x: Float) -> Bool
```

Tells whether `x` is neither a NaN nor an infinity.

- **Cost:** O(1). Informative.

```cheby
test "is_finite excludes NaN and the infinities" {
  assert float::is_finite(-0.0)
  assert !float::is_finite(float::nan)
  assert !float::is_finite(float::infinity)
}
```

## Roots and powers

### `sqrt`

```cheby
pub fn sqrt(x: Float) -> Float
```

Returns the square root of `x`, correctly rounded as IEEE 754 requires, so it is the same on every target. The square root of a negative number is NaN, and that of `-0.0` is `-0.0`.

- **Cost:** O(1). Informative.

```cheby
test "sqrt is correctly rounded" {
  assert float::sqrt(16.0) == 4.0
  assert float::sqrt(2.0) == 1.4142135623730951
  assert float::is_nan(float::sqrt(-1.0))
  assert float::sqrt(float::infinity) == float::infinity
}
```

### `pow`

```cheby
pub fn pow(x: Float, exponent: Float) -> Float
```

Returns `x` raised to `exponent`, correctly rounded (D-396). Special cases follow IEEE 754's `pow` and C's: `pow(x, 0.0)` is 1.0 for every `x`, including NaN, `pow(1.0, y)` is 1.0 for every `y`, including NaN, and a negative `x` with a finite `exponent` that is not a whole number gives NaN. JS's `**` gives NaN for `pow(1.0, nan)`, so the JS runtime implements the rule itself.

- **Cost:** O(1). Informative.

```cheby
test "pow raises to a power" {
  assert float::pow(2.0, 10.0) == 1024.0
  assert float::pow(2.0, -1.0) == 0.5
  assert float::pow(float::nan, 0.0) == 1.0
  assert float::pow(1.0, float::nan) == 1.0
  assert float::is_nan(float::pow(-8.0, 1.0 / 3.0))
}
```

## Exponentials and trigonometry

The functions of this section and `pow` return the correctly rounded result, the `Float` nearest to the exact mathematical value, on every target (D-396).

These functions are correctly rounded on every target, computed by code shipped with the runtime rather than by the host's library (D-396, ADR-0054). Functions such as `asin`, `log2` and the hyperbolic ones can be added later under the same rule.

### `exp`

```cheby
pub fn exp(x: Float) -> Float
```

Returns eˣ. A result too large for `Float` is `inf`.

- **Cost:** O(1). Informative.

```cheby
test "exp raises e to a power" {
  assert float::exp(0.0) == 1.0
  assert float::exp(1.0) == float::e
  assert float::exp(float::neg_infinity) == 0.0
  assert float::exp(1000.0) == float::infinity
}
```

### `ln`

```cheby
pub fn ln(x: Float) -> Float
```

Returns the natural logarithm of `x`. It is `-inf` for zero and NaN for a negative `x`.

- **Cost:** O(1). Informative.

```cheby
test "ln is the natural logarithm" {
  assert float::ln(1.0) == 0.0
  assert float::ln(0.0) == float::neg_infinity
  assert float::is_nan(float::ln(-1.0))
}
```

### `sin`

```cheby
pub fn sin(x: Float) -> Float
```

Returns the sine of `x`, in radians. It is NaN for an infinite `x`.

- **Cost:** O(1). Informative.

```cheby
test "sin takes radians" {
  assert float::sin(0.0) == 0.0
  assert float::sin(float::pi / 2.0) == 1.0
  assert float::is_nan(float::sin(float::infinity))
}
```

### `cos`

```cheby
pub fn cos(x: Float) -> Float
```

Returns the cosine of `x`, in radians. It is NaN for an infinite `x`.

- **Cost:** O(1). Informative.

```cheby
test "cos takes radians" {
  assert float::cos(0.0) == 1.0
  assert float::cos(float::pi) == -1.0
}
```

### `tan`

```cheby
pub fn tan(x: Float) -> Float
```

Returns the tangent of `x`, in radians. It is NaN for an infinite `x`.

- **Cost:** O(1). Informative.

```cheby
test "tan takes radians" {
  assert float::tan(0.0) == 0.0
  assert float::is_nan(float::tan(float::neg_infinity))
}
```

### `atan2`

```cheby
pub fn atan2(y: Float, x: Float) -> Float
```

Returns the angle in radians, from −π to π, between the positive x-axis and the point (`x`, `y`). Special cases follow IEEE 754's `atan2` and C's.

- **Cost:** O(1). Informative.

```cheby
test "atan2 gives the angle of a point" {
  assert float::atan2(1.0, 1.0) == float::pi / 4.0
  assert float::atan2(0.0, -1.0) == float::pi
  assert float::atan2(-1.0, 0.0) == -float::pi / 2.0
}
```
