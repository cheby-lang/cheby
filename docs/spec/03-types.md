# 3. Types

Cheby is statically typed (D-011). Every expression has a type known at compile time. User-defined data types are nominal algebraic data types ([chapter 4](04-declarations.md)). Interfaces are structural ([chapter 8](08-interfaces.md)).

## 3.1 Type syntax

```ebnf
type        = path_type
            | tuple_type
            | fn_type
            | dyn_type ;
path_type   = [ LOWER "::" ] UPPER [ type_args ] ;   (* Int, List<T>, map::Map<K, V> *)
type_args   = "<" type { "," type } [ "," ] ">" ;
tuple_type  = "(" type "," type { "," type } [ "," ] ")" ;
fn_type     = "fn" "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;
dyn_type    = "dyn" [ LOWER "::" ] UPPER ;
```

- A `path_type` names a type in scope, either unqualified or through an imported module (`map::Map<K, V>`) ([chapter 7](07-modules-and-packages.md)).
- Type arguments use angle brackets (D-049).
- In `fn_type`, a missing `-> type` means `-> Nil` (D-083).
- `dyn I` is the runtime-value form of an interface ([§8.6](08-interfaces.md#86-dyn-values)).

## 3.2 Built-in types

These types are known to the compiler. The ones marked _prelude_ are in scope in every module without an import (D-112, [§7.6](07-modules-and-packages.md#76-prelude)).

| Type                                | Kind                                       | Prelude                     | Defined in                            |
| ----------------------------------- | ------------------------------------------ | --------------------------- | ------------------------------------- |
| `Int`                               | 64-bit signed integer                      | yes                         | [§3.3](#33-numeric-types)             |
| `Float`                             | IEEE 754 binary64                          | yes                         | [§3.3](#33-numeric-types)             |
| `I8 I16 I32 I64 U8 U16 U32 U64 F32` | sized numbers                              | yes                         | [§3.3](#33-numeric-types)             |
| `Bool`                              | `True \| False`                            | yes                         | [§3.4](#34-bool)                      |
| `String`                            | immutable Unicode text                     | yes                         | [§3.5](#35-string)                    |
| `Nil`                               | the unit type, with the single value `Nil` | yes                         | [§3.6](#36-nil)                       |
| `List<T>`                           | persistent vector                          | yes                         | [§3.8](#38-list)                      |
| `Result<T, E>`                      | `Ok(T) \| Err(E)`                          | yes                         | [chapter 11](11-errors-and-panics.md) |
| `Option<T>`                         | `Some(T) \| None`                          | yes                         | [chapter 11](11-errors-and-panics.md) |
| `Order`                             | `Less \| Equal \| Greater`                 | yes                         | [§3.14](#314-ordering)                |
| `(A, B, …)`                         | tuples                                     | syntax                      | [§3.7](#37-tuples)                    |
| `fn(A) -> B`                        | functions                                  | syntax                      | [§3.9](#39-function-types)            |
| `Never`                             | the empty type                             | yes                         | [§3.10](#310-never)                   |
| `Map<K, V>`, `Set<T>`               | persistent hash map and set                | no (`std::map`, `std::set`) | [§3.8](#38-list)                      |
| `Bytes`                             | immutable byte sequence                    | no (`std::bytes`)           | [§3.5](#35-string)                    |
| `Sender<T>`, `Receiver<T>`          | channel handles                            | no (`std::channel`)         | [chapter 10](10-concurrency.md)       |

`Never` is in the prelude (D-131), because it is a writable return type ([§3.10](#310-never)).

## 3.3 Numeric types

### 3.3.1 Int

`Int` is a signed 64-bit integer (D-105). All arithmetic on `Int` is checked: a result that does not fit panics (D-025). Intentional wrapping uses explicit functions in `std::int`, such as `int::wrapping_add` (D-025).

On the JS target, `Int` is a JS number, and every operation panics if its result leaves the safe-integer range −(2⁵³−1)…2⁵³−1 (D-037). A program therefore panics at the same points or earlier on JS, but never computes a different result ([§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

Integer division `/` truncates toward zero, and `%` is the remainder with the sign of the dividend (D-069, D-092). Division or remainder by zero panics (D-069). `Int` division of the minimum value by `-1` overflows and panics.

### 3.3.2 Float

`Float` is an IEEE 754 binary64 number (D-105). Arithmetic follows IEEE 754 with round-to-nearest-even, so it can produce infinities and NaN and never panics (D-069).

Equality on `Float` is **not** IEEE equality (D-069):

- `==` is reflexive: every NaN equals every other NaN, including itself.
- `-0.0 == 0.0`.
- Equal floats hash the same, so all NaNs hash alike and both zeros hash alike.

This keeps structural equality and hashing consistent for `Map`/`Set` keys and for records that contain floats ([§3.13](#313-equality-hashing-and-debug-printing)).

### 3.3.3 Sized numeric types

The sized types exist for FFI and binary data (D-105):

| Type                   | Values                                         |
| ---------------------- | ---------------------------------------------- |
| `I8` `I16` `I32` `I64` | two's-complement signed integers of that width |
| `U8` `U16` `U32` `U64` | unsigned integers of that width                |
| `F32`                  | IEEE 754 binary32                              |

Sized integers follow the same checked-overflow rules as `Int` at their own width (D-025). `F32` follows the same equality rules as `Float`.

Sized types behave identically on every target (D-106). In particular, `I64` and `U64` are exact on JS, where they are represented as `BigInt` ([§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

`Int` and `I64` are distinct types (D-106), and there is no `F64` because `Float` is already binary64 (D-106).

Each numeric type has a lower-case module in the standard library with its conversions and helpers: `std::int`, `std::float`, `std::i8`, …, `std::u64`, `std::f32` (D-105).

### 3.3.4 Numeric literals

A numeric literal has no type of its own. Its type is taken from the expected type in context (D-084):

- An integer literal may have any integer type. If the context does not fix one, it is `Int`.
- A float literal may have type `Float` or `F32`. If the context does not fix one, it is `Float`.
- Defaulting happens only after the whole function body has been inferred (D-133). A constraint that appears later in the body still fixes the literal's type, so `let x = 1` followed by a use of `x` as a `U8` makes `x` a `U8`.
- An integer literal is never implicitly a float, and a float literal is never an integer.
- A literal whose value does not fit its type is a compile error (D-084), for example `let b: U8 = 256`. On the JS target, an `Int` literal outside the safe-integer range is also a compile error.

```cheby
let a = 10          // Int
let b: U8 = 255     // U8
let c: F32 = 1.5    // F32
let d = 1.5e3       // Float
```

### 3.3.5 Conversions

There are no implicit numeric conversions (D-047). Explicit conversions are functions in the numeric modules (D-085):

- A **widening** conversion, where every source value fits the target type, returns the target type directly, for example `i64::from_i32(x)`.
- A **narrowing** conversion returns `Result<T, Nil>`, for example `u8::from_int(x) -> Result<U8, Nil>` (D-132).
- `Float → Int` has explicit rounding functions such as `float::truncate` and `float::round`. They return `Result<Int, Nil>`, with `Err(Nil)` for NaN, infinities and values out of `Int`'s range (D-132).
- `Int ↔ I64` is a narrowing conversion in the `I64 → Int` direction, because it can fail on JS (ADR-0030).

### 3.3.6 Text of floats

`show` for `Float` and `F32`, and therefore `{x}` interpolation, produces the same text on every target (D-203):

- A finite value is written as the shortest decimal that reads back as the same value of that type. For `F32` this is judged at `F32` precision, so `0.1` as an `F32` shows as `0.1`, not `0.10000000149011612` (D-228).
- The text always contains a `.` or an exponent, so a float never looks like an integer: `2.0`, `0.1`, `-2.25`.
- Very large and very small magnitudes use an exponent, written like a float literal ([§2.5.2](02-lexical-structure.md#252-float-literals)): `1.0e21`, `1.0e-7`.
- The special values are written `NaN`, `inf`, `-inf` and `-0.0` (D-219).

_Note:_ on JS, the compiler cannot use `String(x)` directly, because `String(2.0)` is `"2"`. The exact thresholds for switching to an exponent are fixed in the standard-library spec for `std::float` (D-204).

## 3.4 Bool

`Bool` is an ordinary built-in ADT with the constructors `True` and `False` (D-065). It is matched with `case` like any other ADT. There is no truthiness: only `Bool` values may be used where a condition is expected. The logical operators are defined in [§5.4](05-expressions.md#54-operators).

## 3.5 String

`String` is immutable Unicode text (D-038):

- A string is a sequence of Unicode scalar values.
- There is no integer indexing and no O(1) access by position. Strings are processed by grapheme or code point through `std::string`, or through explicit byte views.
- Converting a string to bytes always produces UTF-8, on every target.
- Strings are concatenated with `+` ([§5.4.3](05-expressions.md#543-overloaded-arithmetic)) (D-067).

The representation is UTF-8 on native targets and a JS string on JS (D-038). Because the API has no positional indexing, the two representations are not observable.

`Bytes` (in `std::bytes`) is an immutable sequence of bytes, with builder and parsing functions in the standard library (D-095).

## 3.6 Nil

`Nil` is the unit type. It has exactly one value, also written `Nil` (D-065). A function whose return type is omitted returns `Nil` (D-083). A block whose last statement is not an expression, or that is empty, has the value `Nil` ([§5.1](05-expressions.md#51-blocks-and-statements)).

`()` is not a type or a value.

## 3.7 Tuples

A tuple groups a fixed number of values of possibly different types (D-076). The type is written `(A, B)`, `(A, B, C)` and so on, and values are written the same way with expressions ([§5.12](05-expressions.md#512-tuples)). Tuple elements are read with `.0`, `.1`, … ([§5.11.3](05-expressions.md#5113-field-access)) or with a tuple pattern.

A tuple has at least two elements. `(x)` is a parenthesized expression, and there is no one-element tuple.

## 3.8 List

`List<T>` is the built-in sequence type. It is a persistent, immutable RRB-tree vector (D-039), not a linked list. All elements have the same type `T`.

| Operation                 | Cost (informative)                   |
| ------------------------- | ------------------------------------ |
| index, update at index    | O(log n), effectively constant       |
| append at either end      | amortized O(1) to O(log n)           |
| concatenate, slice        | O(log n)                             |
| `[first, ..rest]` pattern | O(1), `rest` is a slice view (D-053) |

List literals and patterns are defined in [§5.13](05-expressions.md#513-list-literals) and [§6.6](06-patterns.md#66-list-patterns).

`Map<K, V>` and `Set<T>` are persistent hash array mapped tries in `std::map` and `std::set` (D-039). Keys are compared with structural `==` and hashed with the built-in structural hash ([§3.13](#313-equality-hashing-and-debug-printing)). Iteration order depends on a per-process random seed and changes between runs (D-093). Two maps or sets are `==` when they contain the same entries, regardless of order (D-093).

## 3.9 Function types

`fn(A, B) -> C` is the type of functions taking an `A` and a `B` and returning a `C` (D-083). `fn(A)` is `fn(A) -> Nil`. Top-level functions, local functions, closures and function captures all have function types. A generic top-level function is instantiated to a function type when it is used as a value.

Functions are values, but they have no structural equality: comparing values that contain functions with `==` panics ([§3.13](#313-equality-hashing-and-debug-printing)) (D-070).

## 3.10 Never

`Never` is the type of expressions that never produce a value (D-161): `panic`, `todo`, and calls to functions declared `-> Never`. `Never` is a subtype of every type, so an expression of type `Never` can be used wherever any type is expected.

A function declared `-> Never` must not return normally. Its body must have type `Never`, which in practice means it ends by panicking, by calling another `-> Never` function, or by a tail call to itself.

```cheby
fn unreachable(what: String) -> Never {
  panic as "unreachable: " + what
}
```

## 3.11 User-defined types and aliases

User-defined types are declared with `type` ([§4.3](04-declarations.md#43-type-declarations)). Each declaration introduces a new nominal type: two declarations with the same shape are different types.

A type alias `type UserId = Int` gives another name to an existing type (D-081). The alias and the aliased type are the same type and are interchangeable everywhere ([§4.4](04-declarations.md#44-type-aliases)).

## 3.12 Generics and type inference

### 3.12.1 Type parameters

Top-level functions and type declarations may have type parameters, written with angle brackets (D-049):

```cheby
type Pair<A, B> { first: A, second: B }

fn swap<A, B>(pair: Pair<A, B>) -> Pair<B, A> {
  Pair { first: pair.second, second: pair.first }
}
```

A type parameter of a function may have interface bounds ([§8.4](08-interfaces.md#84-bounds)).

Generic code is compiled once, over a uniform boxed representation (D-021). Specialization is an implementation optimization and is not observable.

### 3.12.2 Where types are required

- Every top-level function must declare the types of all its parameters and its return type. A missing return type means `Nil` (D-011, D-083).
- Every `const` must declare its type (D-121).
- Every field of a type declaration must declare its type.

### 3.12.3 Inference inside bodies

Inside a function body, the types of `let` bindings, closure parameters, closure return types and local functions are inferred (D-011). Inference is local to one top-level function: it never looks at the bodies of other functions.

Local bindings are **monomorphic** (D-123). A closure bound with `let`, and a local named function, each have exactly one type, fixed by how they are used within the body. Only top-level functions can be generic.

```cheby
fn main() {
  let wrap = fn(x) { [x] }
  let a = wrap(1)       // fixes wrap: fn(Int) -> List<Int>
  let b = wrap("hi")    // compile error: expected Int, found String
}
```

### 3.12.4 Explicit type arguments

When a generic function's type arguments cannot be inferred, they are given with the turbofish `::<…>` (D-049):

```cheby
let empty = list::new::<Int>()
```

## 3.13 Equality, hashing and debug printing

Every type except function types has built-in structural equality, hashing and debug printing (D-033). None of the three can be overridden (D-047).

**Equality** (`==` and `!=`):

- Two values of an ADT are equal when they have the same constructor and all fields are equal.
- Tuples and lists are equal element by element. Maps and sets are equal when they have the same entries, regardless of order (D-093).
- Strings are equal when they have the same sequence of scalar values.
- Floats use reflexive equality ([§3.3.2](#332-float)) (D-069).
- Both operands must have the same type. Comparing values of different types is a compile error.
- If a comparison reaches a function value, it panics (D-070). When the compiler knows statically that a compared type contains a function type, it emits a warning (D-070).

**Hashing** is consistent with equality: equal values hash equally. Hashes are seeded per process (D-093), so hash values are not stable across runs and are not exposed as a stable API.

**Debug printing** produces a textual representation of any value, used by `{x:?}` interpolation ([§5.3.2](05-expressions.md#532-string-interpolation)) and by `assert` failure messages ([§11.5](11-errors-and-panics.md#115-assert)).

The debug format is Rust-like (D-134): `Point { x: 1.0, y: 2.0 }`, `Some(3)`, `[1, 2]`, `"text"`. It shows the fields of opaque types from other packages too, because debug output is for developers (D-134).

## 3.14 Ordering

`Order` is the built-in ADT `Less | Equal | Greater`. The comparison operators `< <= > >=` are defined through the `compare` function of the operand type ([§8.7](08-interfaces.md#87-operator-interfaces)) (D-047). The following built-in types have built-in `compare` functions:

- The numeric types. On `Float` and `F32`, `compare` is a total order consistent with `==` ([§3.3.2](#332-float)): `-0.0` and `0.0` compare `Equal`, and all NaNs compare `Equal` to each other and `Greater` than every other float (D-135). This keeps sorting and ordered collections sound.
- `Bool`, with `False < True` (D-136).
- `String`, ordered by Unicode scalar values on every target (D-136).
- Tuples and `List`, ordered lexicographically when their elements are comparable (D-136).
