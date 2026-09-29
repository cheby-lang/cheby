# 3. Types

Cheby is statically typed (D-011). Every expression has a type known at compile time. User-defined data types are nominal algebraic data types ([chapter 4](04-declarations.md)). Interfaces are structural ([chapter 8](08-interfaces.md)).

## 3.1 Type syntax

```ebnf
type        = path_type
            | tuple_type
            | fn_type
            | dyn_type
            | "Self" ;                                  (* only inside interface_decl *)
path_type   = [ LOWER "::" ] UPPER [ type_args ] ;   (* Int, List<T>, map::Map<K, V> *)
type_args   = "<" type { "," type } [ "," ] ">" ;
tuple_type  = "(" type "," type { "," type } [ "," ] ")" ;
fn_type     = "fn" "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;
dyn_type    = "dyn" interface_ref ;                  (* interface_ref: §8.1 *)
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
| `List<T>`                           | persistent vector                          | yes                         | [§3.8](#38-list-map-and-set)          |
| `Result<T, E>`                      | `Ok(T) \| Err(E)`                          | yes                         | [chapter 11](11-errors-and-panics.md) |
| `Option<T>`                         | `Some(T) \| None`                          | yes                         | [chapter 11](11-errors-and-panics.md) |
| `Order`                             | `Less \| Equal \| Greater`                 | yes                         | [§3.14](#314-ordering)                |
| `(A, B, …)`                         | tuples                                     | syntax                      | [§3.7](#37-tuples)                    |
| `fn(A) -> B`                        | functions                                  | syntax                      | [§3.9](#39-function-types)            |
| `Never`                             | the empty type                             | yes                         | [§3.10](#310-never)                   |
| `Map<K, V>`, `Set<T>`               | persistent hash map and set                | no (`std::map`, `std::set`) | [§3.8](#38-list-map-and-set)          |
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
- Defaulting happens only after the whole function body has been inferred (D-133). A constraint that appears later in the body still fixes the literal's type, so `let x = 1` followed by a use of `x` as a `U8` makes `x` a `U8`. Operators and interface calls are resolved after defaulting ([§3.12.4](#3124-order-of-inference)).
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

A tuple groups a fixed number of values of possibly different types (D-076). The type is written `(A, B)`, `(A, B, C)` and so on, and values are written the same way with expressions ([§5.12](05-expressions.md#512-tuples)). Tuple elements are read only with a tuple pattern ([§6.5](06-patterns.md#65-tuple-patterns)) in `let`, `case`, `use` binders or the parameters of closures and local functions. There is no positional access such as `pair.0` (D-241), so a tuple is taken apart where it is used and its parts get names.

```cheby
let (name, age) = person
list::map(pairs, fn((key, value)) { key + "=" + value })
```

A written type that contains a tuple with a tuple element, such as `((Int, Int), Bool)`, or a tuple with four or more elements, is a warning that suggests a named type instead ([§13.8](13-tooling.md#138-diagnostics-and-warnings)) (D-243).

A tuple has at least two elements. `(x)` is a parenthesized expression, and there is no one-element tuple.

## 3.8 List, Map and Set

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

Functions are values, but they have no structural equality or hash: comparing or hashing values that contain functions panics, and a function debug-prints as `<fn>` ([§3.13](#313-equality-hashing-and-debug-printing)) (D-070, D-233).

## 3.10 Never

`Never` is the type of expressions that never produce a value (D-161): `panic`, `todo`, and calls to functions declared `-> Never`. An expression of type `Never` takes the expected type when there is one, so it can be used wherever a value of any type is expected ([§3.12.5](#3125-expected-types-and-conversions)) (D-235). When there is no expected type, its type is `Never`. Arms of type `Never` do not constrain the type of a `case`: the `case` has the type of its other arms, and if every arm is `Never`, the `case` is `Never`.

A function declared `-> Never` must not return normally. Its body must have type `Never`, which in practice means it ends by panicking, by calling another `-> Never` function, or by a tail call to itself.

```cheby
fn unreachable(what: String) -> Never {
  panic as "unreachable: " + what
}
```

`Never` is not a subtype of other types, and the conversion does not reach inside other types (D-235). The function `unreachable` has type `fn(String) -> Never` and does not fit where `fn(String) -> Int` is expected. Instead, write a closure such as `fn(s) { unreachable(s) }`, whose body converts. Likewise, a `List<Never>` does not fit `List<Int>`.

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

Generic code is compiled once, over a uniform boxed representation (D-021). Type arguments are not kept at run time, except in the type descriptors carried by `dyn` values and by the function packages passed for bounds ([§8.6.1](08-interfaces.md#861-type-descriptors)) (D-236). Specialization is an implementation optimization and is not observable.

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

### 3.12.4 Order of inference

Within a body, statements are checked in order, and within an expression, subexpressions are checked from left to right, with one exception (D-234):

- **Closures last.** In a call, including the call that a `|>` or a `use` desugars to and a constructor call, the arguments that are not closures or function captures are checked first, from left to right. Then the closure and capture arguments are checked, from left to right. Their expected parameter types come from the callee's signature, as instantiated by the other arguments. The position of a closure among the arguments therefore does not matter for inference. A local named function is checked where it is declared, like any other statement.

This order concerns type checking only. Evaluation is always strictly left to right ([§5.15.1](05-expressions.md#5151-evaluation-order)) (D-120).

Some constructs need a known type at the point where they are checked. Others are resolved at the end of the body (D-234). Here "the body" is always the body of the enclosing top-level function or `test`, including every local function and closure inside it, so a constraint in a local function can be resolved by a call that comes after the local function's declaration:

- **Known at that point.** Field access `x.name` and record update `T { ..base, … }` require the type of `x` or `base` to be known when they are checked in this order ([§5.11.3](05-expressions.md#5113-field-access)) (D-213). Otherwise it is a compile error that asks for a type annotation.
- **Resolved at the end of the body.** Operators whose meaning depends on the operand type ([§5.4](05-expressions.md#54-operators)): arithmetic, unary `-`, `<`, `<=`, `>`, `>=` and the bitwise operators. Also interface-qualified calls such as `Compare::compare(x, y)` ([§8.5](08-interfaces.md#85-calling-interface-functions)), the `Show` requirement of `{x}` interpolation ([§5.3.2](05-expressions.md#532-string-interpolation)), and conversions to `dyn I`: the check that the value's type satisfies `I` and the choice of its function package and type descriptor ([§3.12.5](#3125-expected-types-and-conversions)) (D-237). These are recorded and resolved after the whole top-level body has been inferred and numeric literals have been defaulted ([§3.3.4](#334-numeric-literals)) (D-133). Their result types are known before that: arithmetic, unary `-` and bitwise operators return the operands' type (D-118), comparisons return `Bool`, and an interface call's type follows from the interface signature. Interfaces have no type parameters and function names are not overloaded (D-045, D-165), so resolving such a constraint never changes a type. `==` and `!=` only require both operands to have the same type.
- **Still unknown at the end.** If a type that a deferred operator, interface call, interpolation or conversion to `dyn` depends on is still unknown after the top-level body has been inferred and literals defaulted, it is a compile error that asks for a type annotation.

For built-in numeric types, operators use the built-in operations ([§5.4.2](05-expressions.md#542-built-in-arithmetic)), and for other types the functions of the operand type's module ([§8.7](08-interfaces.md#87-operator-interfaces)). The rules above only fix when that choice is made.

```cheby
fn sum(numbers: List<Int>) -> Int {
  fn step(acc, rest) {
    case rest {
      [] => acc
      [first, ..tail] => step(acc + first, tail)   // `+` is resolved at the end: Int
    }
  }
  step(0, numbers)
}

fn total(items: List<Item>) -> Int {
  list::fold(items, 0, fn(acc, item) { acc + item.price })   // `item` is an Item
}

fn sums(ps: List<Point>, qs: List<Point>) -> List<Float> {
  list::zip_with(fn(a, b) { a.x + b.x }, ps, qs)   // the closure is checked after `ps` and `qs`
}
```

```cheby
fn main() {
  let get_x = fn(p) { p.x }   // compile error: type of `p` unknown, annotate `fn(p: Point)`
  let x = get_x(origin)
}

fn unused() {
  let f = fn(a, b) { a + b }   // compile error: type of `a` unknown at the end of the body
}
```

### 3.12.5 Expected types and conversions

Type inference has no subtyping (D-235). The only implicit type changes are the two conversions below, and both happen only at the point where an expression is checked against an expected type, never inside another type.

When an expression is checked, it may have an **expected type**, known at that point in the order of [§3.12.4](#3124-order-of-inference). Expected types come from:

- a `let` annotation, for the bound expression;
- a parameter type of the callee's signature, as instantiated so far, for a function or closure argument;
- a function's declared result type, for its body block, and a closure's annotated return type, for its body;
- a constructor's declared field types, with the type's parameters as instantiated so far, for the field values, and the field types of a record update, for the updated fields;
- the element type of a list, for each element of a list literal, when the list type is expected;
- the element types of a tuple, for the elements of a tuple expression, when the tuple type is expected.

An expected type passes into the result of a block and into every arm body of a `case`.

The two conversions are:

- **`Never`.** An expression of type `Never` takes the expected type when there is one. Otherwise its type is `Never`, and as a `case` arm it does not constrain the type of the `case` ([§3.10](#310-never)).
- **`dyn`.** A value whose type satisfies `I` converts to `dyn I` where `dyn I` is the expected type when the value is checked ([§8.6](08-interfaces.md#86-dyn-values)). The conversion packages the value in O(1) in the size of the value, with no hidden traversal. Only the descriptor of a generic type such as `List<T>` is built from its arguments' descriptors, in time proportional to the size of the type ([§8.6](08-interfaces.md#86-dyn-values)) (D-237). A value of type `dyn I` where `dyn I` is expected is not packaged again. A value of type `dyn I` where `dyn J` is expected and `I` embeds `J` converts, because `dyn I` satisfies `J`. Otherwise the value's type is unified with the expected type as usual, so a type that is still unknown becomes `dyn I`, and `let shapes: List<dyn Shape> = []` is fine. If the value's type mentions a type parameter of the enclosing top-level function, that parameter must have a bound, which supplies its type descriptor ([§8.6.1](08-interfaces.md#861-type-descriptors)) (D-236). The check that the type satisfies `I` and the choice of package and descriptor are made at the end of the body ([§3.12.4](#3124-order-of-inference)), so `let x = 1` followed by `let d: dyn Show = x` and a use of `x` as a `U8` packages a `U8`. The type of a numeric literal is already known to be numeric, so it converts rather than becoming `dyn I`, and its concrete type is still fixed later or defaulted ([§3.3.4](#334-numeric-literals)) (D-237). If the value's type is still not fully known at the end, as in `let a: dyn Any = []` with nothing fixing the element type, it is a compile error that asks for an annotation (D-237).

Nothing converts inside another type. `List<Circle>` never becomes `List<dyn Shape>`, `fn() -> Circle` never becomes `fn() -> dyn Shape`, and an already-built `Option<Circle>` never becomes `Option<dyn Shape>`. `Some(circle)` checked against `Option<dyn Shape>` works, because the expected type of its field is `dyn Shape`. A whole list is converted with `list::map`, and a function with a closure.

Where no expected type is known, nothing converts. `let mixed = [circle, square]` is a type mismatch between `Circle` and `Square`, and the diagnostic suggests an annotation such as `List<dyn Shape>`.

```cheby
let n = case x {
  Some(v) => v
  None => panic
}                                                // Int: the `panic` arm takes the arm type

let shapes: List<dyn Shape> = [circle, square]   // OK: each element converts
let maybe: Option<dyn Shape> = Some(circle)      // OK: the field converts
let mixed = [circle, square]                     // error: Circle vs Square, suggests List<dyn Shape>
let all: List<dyn Shape> = circles               // error: List<Circle> does not convert
let converted: List<dyn Shape> = list::map(circles, fn(c) -> dyn Shape { c })   // OK

let f: fn(String) -> Int = unreachable           // error: found fn(String) -> Never
let g: fn(String) -> Int = fn(s) { unreachable(s) }   // OK
```

```cheby
fn as_shape(circle: Circle) -> dyn Shape {
  circle   // OK: the body's expected type is `dyn Shape`
}
```

_Rationale:_ inference stays free of subtyping and variance, and every conversion is visible and cheap, as in Rust. Converting inside other types would need a bottom type with variance for `Never`, and hidden O(n) rebuilds for `dyn`, which are impossible for functions and channels (D-235).

### 3.12.6 Explicit type arguments

When a generic function's type arguments cannot be inferred, they are given with the turbofish `::<…>` (D-049):

```cheby
let empty = list::new::<Int>()
```

## 3.13 Equality, hashing and debug printing

Every type has built-in debug printing. Every type also has built-in structural equality and hashing, except that they panic when they reach a function value or a [handle](09-memory-model.md#95-handles-and-drop-functions) (D-033, D-070, D-189, D-233). None of the three can be overridden (D-047).

**Equality** (`==` and `!=`):

- Two values of an ADT are equal when they have the same constructor and all fields are equal.
- Tuples and lists are equal element by element. Maps and sets are equal when they have the same entries, regardless of order (D-093).
- Strings are equal when they have the same sequence of scalar values.
- Floats use reflexive equality ([§3.3.2](#332-float)) (D-069).
- Two `dyn I` values are equal when they hold values of the same type, including all type arguments, and those values are equal ([§8.6.1](08-interfaces.md#861-type-descriptors)) (D-236).
- Both operands must have the same type. Comparing values of different types is a compile error.
- If a comparison reaches a function value or a handle (a `Sender`, `Receiver`, fiber or scope value, or a value of an external type), it panics (D-070, D-189, D-233). Identity is never compared ([§9.1](09-memory-model.md#91-values-and-sharing)). When the compiler knows statically that a compared type contains a function type or a handle type, it emits a warning (D-070, D-233).

**Hashing** is consistent with equality: equal values hash equally. A `dyn` value's hash covers its type as well as its value (D-236). Hashes are seeded per process (D-093), so hash values are not stable across runs and are not exposed as a stable API. Hashing panics on the same values as equality, so a value that contains a function or a handle cannot be a `Map` key or a `Set` element: inserting or looking it up panics (D-233).

**Debug printing** produces a textual representation of any value of any type (D-233), used by `{x:?}` interpolation ([§5.3.2](05-expressions.md#532-string-interpolation)) and by `assert` failure messages ([§11.5](11-errors-and-panics.md#115-assert)).

The debug format is Rust-like (D-134): `Point { x: 1.0, y: 2.0 }`, `Some(3)`, `[1, 2]`, `"text"`. It shows the fields of opaque types from other packages too, because debug output is for developers (D-134).

Values without printable structure print as fixed placeholders (D-233):

- A function value, whether a top-level function, a closure, a capture or a constructor used as a function, prints as `<fn>`.
- A `Sender` prints as `<Sender>` and a `Receiver` as `<Receiver>`.
- A fiber value prints as `<Fiber>` and a scope value as `<Scope>`. These names are provisional, like the rest of the fiber API (D-174).
- A value of an external type ([§12.5.3](12-targets-and-ffi.md#1253-external-types)) prints as its unqualified type name in angle brackets, such as `<Db>`.
- A `dyn I` value prints as the debug form of the value inside it.

Placeholders never show identity, addresses or contents, so identity stays unobservable ([§9.1](09-memory-model.md#91-values-and-sharing)). For example, `(1, fn(x) { x })` prints as `(1, <fn>)`.

## 3.14 Ordering

`Order` is the built-in ADT `Less | Equal | Greater`. The comparison operators `< <= > >=` are defined through the `compare` function of the operand type ([§8.7](08-interfaces.md#87-operator-interfaces)) (D-047). The following built-in types have built-in `compare` functions:

- The numeric types. On `Float` and `F32`, `compare` is a total order consistent with `==` ([§3.3.2](#332-float)): `-0.0` and `0.0` compare `Equal`, and all NaNs compare `Equal` to each other and `Greater` than every other float (D-135). This keeps sorting and ordered collections sound.
- `Bool`, with `False < True` (D-136).
- `String`, ordered by Unicode scalar values on every target (D-136).
- Tuples and `List`, ordered lexicographically when their elements are comparable (D-136).
