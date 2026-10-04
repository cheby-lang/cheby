# The Cheby Standard Library

Status: **draft**, pre-1.0. Breaking changes are allowed until 1.0 (D-075).

This directory specifies the standard library, one file per module (D-204, D-220). The language specification in [`../spec/`](../spec/) fixes the language and the semantics the runtime provides. This specification fixes the names, signatures and behavior of the modules under `std`. Where a rule comes from a logged decision, the decision is cited as `(D-NNN)`.

The library is written in tiers, each before the milestone that needs it (D-276). This draft covers **tier 1**, the modules M1 needs (D-294). Tier 2 (fibers, channels, selectors, durations, randomness) and tier 3 (files, networking, HTTP, time, processes) are specified in later sessions.

## Modules

### Tier 1

| Module                                                                 | Contents                                                                |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| [`std::io`](io.md)                                                     | standard input and output, reading and writing whole files              |
| [`std::env`](env.md)                                                   | command-line arguments and environment variables                        |
| [`std::list`](list.md)                                                 | the built-in `List<T>`                                                  |
| [`std::map`](map.md)                                                   | `Map<K, V>`, the hashed map                                             |
| [`std::set`](set.md)                                                   | `Set<T>`, the hashed set                                                |
| [`std::multimap`](multimap.md)                                         | a hashed map with a list of values per key                              |
| [`std::sorted_map`](sorted_map.md)                                     | a map ordered by key                                                    |
| [`std::sorted_set`](sorted_set.md)                                     | a set ordered by element                                                |
| [`std::sorted_multimap`](sorted_multimap.md)                           | a multimap ordered by key                                               |
| [`std::ordered_map`](ordered_map.md)                                   | a map in insertion order                                                |
| [`std::ordered_set`](ordered_set.md)                                   | a set in insertion order                                                |
| [`std::ordered_multimap`](ordered_multimap.md)                         | a multimap in insertion order                                           |
| [`std::string`](string.md)                                             | the built-in `String`                                                   |
| [`std::bytes`](bytes.md)                                               | `Bytes`, immutable byte sequences                                       |
| [`std::int`](int.md)                                                   | the built-in `Int`                                                      |
| [`std::float`](float.md)                                               | the built-in `Float`                                                    |
| [`std::i8`](i8.md) … [`std::u64`](u64.md), [`std::f32`](f32.md)        | the sized numeric types, defined by [Numeric modules](#numeric-modules) |
| [`std::bool`](bool.md), [`std::nil`](nil.md), [`std::order`](order.md) | the remaining built-in types                                            |
| [`std::result`](result.md), [`std::option`](option.md)                 | helpers for `Result` and `Option`                                       |
| [`std::ops`](ops.md)                                                   | the operator interfaces                                                 |
| [`std::json`](json.md), [`std::json::decode`](json/decode.md)          | JSON values, encoding and decoders                                      |

A submodule's file sits in a directory named after its parent, following the import path (D-351).

`Bool`, `Nil` and `Order` are declared in `std::bool`, `std::nil` and `std::order`, with the functions drafted in their files (D-353).

## Entry format

Each module file starts with a short description of the module and its types, followed by its functions grouped under `##` headings. Each function is one entry (D-295, D-349):

````markdown
### `get`

```cheby
pub fn get<T>(xs: List<T>, index: Int) -> Option<T>
```

Returns the element at `index`, counting from 0.

- **Fails:** `None` when `index` is outside `0` to `length - 1`, including negative indexes (D-318).
- **Cost:** O(log n), required (D-300).

```cheby
test "get reads by position" {
  assert list::get([10, 20, 30], 1) == Some(20)
  assert list::get([10, 20, 30], -1) == None
}
```
````

- **Signature.** A declaration without a body, in the form `cheby doc` prints (D-349). It is not a complete item on its own, because a function without a body needs `@external` (§4.2). Types are declared the same way, with their variants when they are `exposed` and without a body when they are opaque.
- **Description.** What the function returns, in one or two sentences.
- **Fails.** For functions that return `Option` or `Result`: every case that gives `None` or `Err`.
- **Clamps.** The count, width and length parameters that are clamped (D-298). Passing a negative integer literal for one of them is a warning (D-303, §13.8).
- **Panics.** Present only where a language rule makes the function panic, such as overflow (D-025). Never because of a bad argument (D-297).
- **Cost.** In terms of `n`, the size of the first argument, unless the entry says otherwise. A cost marked **required** is an upper bound every implementation must meet. One marked **informative** is not binding (D-300).
- **Example.** At least one `test` block. The examples are written as if the module that contains them imports every module they name, as in `import std::list`. Test names are unique within a file, so a whole file's examples can run as one test module. A test that holds on only one target carries `@target(…)` ([§12.6](../spec/12-targets-and-ffi.md#126-target-specific-code)).
- **Environment-dependent examples.** A function whose result depends on the environment, such as standard input and output, files or the operating system, cannot have a deterministic test. Its example is a top-level function introduced by `_Example_, not a \`test\` because …:`. Where the function can fail, a `test` that checks only the failure, which is the same everywhere, is added when one exists.

Fields that do not apply are left out. Every `cheby` block in this directory must parse with the tree-sitter grammar, as the examples do.

## Conventions

These rules apply to every module. Entries do not repeat them.

### Arguments

- **Subject first.** Every function takes the value it works on, such as the list, map, string or number, as its first argument (D-296), so `xs |> list::map(_, f)` and `list::map(xs, f)` read the same way.
- **Callbacks last.** A function that takes a callback takes it as its last argument, so it works with `use` (§5.10).

### Failure

- A function returns `Option` when the answer may simply be absent, `Result<T, Nil>` when there is only one way to fail, and `Result<T, E>` with the module's own error type when callers need the reason (D-297).
- A function never panics because of a bad argument. It panics only where a language rule already does, such as overflow (D-025) or `==` reaching a function value (D-233) (D-297). Any function that compares or hashes values, such as `list::contains` or `map::insert`, panics where `==` or hashing would ([§3.13](../spec/03-types.md#313-equality-hashing-and-debug-printing)).
- **Counts are clamped.** A count, width or length outside its useful range is clamped, so `list::take(xs, -1)` is `[]` and `list::take(xs, 100)` on a shorter list is the whole list (D-298).
- **Indexes are not counts.** An index outside `0` to `length - 1`, including a negative one, makes the function return `None` or leave its input unchanged, as the entry says. Indexes never count from the end (D-318).
- **Ranges exclude their end.** A range given as `start` and `end` includes `start` and excludes `end` (D-319).
- An error type whose cases an outside specification fixes is `exposed` (`json::Error`). One whose cases follow the operating system is opaque, with predicate functions and `show` (`io::Error`) (D-304, ADR-0051).

### Names

- Sequences (`List`, `String`, `Bytes`) have `length`. Maps and sets have `size` (D-301).
- A type's text comes only from its `show` function. There are no `to_string` functions for this (D-307). `json::to_string` encodes a JSON value, which is a different operation.
- Numeric conversions are named `target::from_source` (D-308).

### Text

- Every length, position and count in `std::string` is in code points (Unicode scalar values) (D-302, ADR-0050). Grapheme clusters are available only through `string::to_graphemes` and `string::grapheme_length`.
- Case mapping and every other Unicode table come from one Unicode version fixed by the toolchain (D-310).

### Behavior on every target

- Every function gives the same result on every target (D-232, §12.4). Functions whose result could depend on the host, such as case mapping, use tables shipped with the runtime (D-310).
- No function grows the stack with the size of its input (D-094).
- Sorting is stable (D-299).

A function that visits elements calls its callback exactly once per element, front to back in the collection's order, unless its entry says it stops early, as `any`, `all` and `find` do. A function that calls a comparison function, such as `list::sort_with`, uses one algorithm fixed by the toolchain on every target, so even a comparison function that is not a consistent order gives the same result everywhere, and the number of its calls is not specified. Functions on `Map`, `Set` and `multimap` visit entries in their per-process order (D-093, D-354).

### Equality, printing and `Show`

- `List`, `Map`, `Set` and every collection in this library compare with `==` by contents. Insertion-ordered collections also compare their order (D-343).
- The sorted, insertion-ordered and multimap collections are written in Cheby (D-336), and supply their own equality, hashing and debug printing through a standard-library-only attribute (D-342, §3.13).
- Debug printing of `Map`, `Set` and `multimap` lists entries sorted by their printed text (D-344). These three types do not satisfy `Show`. The sorted and insertion-ordered collections satisfy `Show` when their elements do (D-344).

`show` of `List`, `Option`, `Result`, tuples and the sorted and insertion-ordered collections uses the punctuation of debug printing (D-134), with each element written by its own `show`, so strings inside appear without quotes: `[1, 2]`, `[a, b]` for `["a", "b"]`, `Some(3)`, `Err(not found)`, `(1, a)`, and `sorted_map::from_list([(a, 1), (b, 2)])` (D-355).

## Numeric modules

`std::int`, `std::float` and the sized modules `std::i8`, `std::i16`, `std::i32`, `std::i64`, `std::u8`, `std::u16`, `std::u32`, `std::u64` and `std::f32` share the families of functions below. [`int.md`](int.md) and [`float.md`](float.md) are written in full. Each sized file gives its type's range and a table of its functions, with one example per family (D-350).

In this section, `T` is the module's type and `t` its module name, such as `U8` and `u8`.

### Constants

Each integer module has the constants `min_value` and `max_value`, its type's smallest and largest values, such as `pub const max_value: U8 = 255`:

| Type  | `min_value`                 | `max_value`                  |
| ----- | --------------------------- | ---------------------------- |
| `Int` | −2⁶³ (−9223372036854775808) | 2⁶³−1 (9223372036854775807)  |
| `I8`  | −128                        | 127                          |
| `I16` | −32768                      | 32767                        |
| `I32` | −2147483648                 | 2147483647                   |
| `I64` | −2⁶³                        | 2⁶³−1                        |
| `U8`  | 0                           | 255                          |
| `U16` | 0                           | 65535                        |
| `U32` | 0                           | 4294967295                   |
| `U64` | 0                           | 2⁶⁴−1 (18446744073709551615) |

`Int`'s limits are the 64-bit ones, but on JS an `Int` outside ±(2⁵³−1) panics (D-037), so `int::min_value` and `int::max_value` exist only on native (D-358). `std::int` also has `min_safe` and `max_safe`, ±(2⁵³−1), the range in which `Int` works on every target. The floating-point constants are listed in [`float.md`](float.md) and [`f32.md`](f32.md).

### Conversions

Every numeric type has a conversion from each of the other numeric types, named `t::from_source`, such as `u8::from_int` and `int::from_u32` (D-308):

- A **widening** conversion, where every source value fits `T` exactly, returns `T`, for example `pub fn from_u8(n: U8) -> U32`.
- A **narrowing** conversion returns `Result<T, Nil>`, with `Err(Nil)` when the value does not fit, for example `pub fn from_int(n: Int) -> Result<U8, Nil>` (D-132).
- `Int` from `I64` or `U64` is narrowing on every target, because it can fail on JS (ADR-0030).
- A conversion from an integer type to a float type, and `f32::from_float`, never fails. It returns the target type, rounded to the nearest value with ties to even, and `f32::from_float` gives an infinity for a value beyond `F32`'s range (D-359).
- Integer types have no `from_float` or `from_f32`. A float becomes an integer through `truncate`, `round`, `floor` or `ceil`, which return `Result<Int, Nil>`, followed by a narrowing conversion if needed (D-132, D-359).

### Parsing and text

```cheby
pub fn parse(text: String) -> Result<T, Nil>
pub fn show(n: T) -> String
```

- `parse` for an integer type accepts an optional `-` followed by one or more ASCII decimal digits, and nothing else. A value out of range is `Err(Nil)` (D-312). Integer types also have `parse_base(text: String, base: Int) -> Result<T, Nil>`.
- `parse` for a float type accepts an optional `-`, decimal digits, an optional fraction of `.` followed by digits, and an optional exponent, as well as `NaN`, `inf` and `-inf`. `_`, `.5`, `1.`, a leading `+` and whitespace are rejected. Every text that `show` gives parses back to the same value (D-313).
- `show` for an integer type gives the decimal digits, with `-` for negative values. `show` for a float type follows §3.3.6 (D-203, D-219, D-228).

### Comparison

```cheby
pub fn compare(a: T, b: T) -> Order
pub fn min(a: T, b: T) -> T
pub fn max(a: T, b: T) -> T
```

`compare` is the built-in order, which on floats is total and consistent with `==`: `-0.0` and `0.0` are `Equal`, and every NaN is `Equal` to every other NaN and `Greater` than every other value (D-135). `min` and `max` follow `compare`, and return `a` when the two are `Equal`.

### Arithmetic

```cheby
pub fn add(a: T, b: T) -> T
pub fn sub(a: T, b: T) -> T
pub fn mul(a: T, b: T) -> T
pub fn div(a: T, b: T) -> T
pub fn rem(a: T, b: T) -> T
pub fn neg(n: T) -> T
pub fn abs(n: T) -> T
```

These are the operators as functions, so that the numeric types satisfy `Add`, `Sub`, `Mul`, `Div` and `Neg` (§8.7) and can be passed as values. They behave exactly like `+`, `-`, `*`, `/`, `%` and unary `-` (§5.4.2), including their panics on integer overflow and division by zero (D-025, D-069). `abs` panics for an integer type's `min_value`, like `neg`.

### Checked arithmetic (integer types)

```cheby
pub fn checked_add(a: T, b: T) -> Result<T, Nil>
pub fn checked_sub(a: T, b: T) -> Result<T, Nil>
pub fn checked_mul(a: T, b: T) -> Result<T, Nil>
pub fn checked_div(a: T, b: T) -> Result<T, Nil>
pub fn checked_rem(a: T, b: T) -> Result<T, Nil>
pub fn checked_neg(n: T) -> Result<T, Nil>
```

Each returns `Err(Nil)` where the matching operator would panic, including for a zero divisor, so no `checked_*` function panics (D-309, D-356).

### `Int` results on JS

A function that returns `Result<Int, Nil>`, such as `checked_add`, `parse`, `int::from_i64` or `float::truncate`, returns `Err(Nil)` only for a value outside the 64-bit range, on every target. A value inside that range but outside ±(2⁵³−1) panics on JS like any other `Int` result (D-037, D-357), so each function returns the same value wherever it returns (D-232).

### Float-only functions

`std::float` and `std::f32` also share functions that integer types do not have. They are defined in [`float.md`](float.md), and [`f32.md`](f32.md) refers to them:

- the constants `infinity`, `neg_infinity`, `nan` and the others listed in `float.md`,
- `truncate`, `round`, `floor` and `ceil`, which return `Result<Int, Nil>`, with `Err(Nil)` for NaN, infinities and values outside `Int`'s range (D-132). `round` breaks ties away from zero (D-314),
- `is_nan`, `is_infinite` and `is_finite`.

### Wrapping arithmetic (integer types)

```cheby
pub fn wrapping_add(a: T, b: T) -> T
pub fn wrapping_sub(a: T, b: T) -> T
pub fn wrapping_mul(a: T, b: T) -> T
pub fn wrapping_neg(n: T) -> T
```

Each computes the result modulo 2ⁿ, where n is the type's width in bits (D-025). For `Int` the width is 64 on every target, and on JS a result outside the safe-integer range panics (D-037).

## Gaps

A gap found while drafting is marked in the module file as `> **Open (OQ-<module>-<n>):** … _Proposed:_ …`, and the draft is written as if the proposal were accepted (D-352). The markers are settled together in a review round, and each settled marker is replaced by normative text that cites its decision.
