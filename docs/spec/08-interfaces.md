# 8. Interfaces

Interfaces provide ad-hoc polymorphism. They are **structural**: a type satisfies an interface implicitly, by having the required functions, without any `impl` declaration (D-020, ADR-0010). Because Cheby has no methods, "having a function" means that the module declaring the type also declares a function with the right name and signature (D-061, ADR-0023).

## 8.1 Declaration

```ebnf
interface_decl = [ "pub" | "priv" ] "interface" UPPER [ ":" bound ] [ "{" [ NL ] { interface_fn NL } "}" ] ;
interface_fn   = { doc_comment NL } "fn" LOWER "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;   (* doc comments: D-227 *)
bound          = interface_ref { "+" interface_ref } ;
interface_ref  = [ LOWER "::" ] UPPER ;
```

An interface lists function signatures in terms of `Self`, the type that satisfies it (D-063):

```cheby
pub interface Show {
  fn show(Self) -> String
}

pub interface Shape {
  fn area(Self) -> Float
  fn scale(Self, Float) -> Self
}
```

- Each function has a name and parameter types. Parameter names are not written.
- A missing return type means `Nil` (D-083).
- `Self` may appear anywhere in parameter and return types, including nested (`List<Self>`, `fn(Self) -> Bool`), subject to the `dyn` restriction in [§8.6](#86-dyn-values).
- Every function must mention `Self` in at least one parameter, so that the satisfying type can be found from the arguments. A function with `Self` only in its return type, such as `fn default() -> Self` or `fn from_json(json::Value) -> Result<Self, E>`, is a compile error in v1 (D-209, ADR-0037). Constructing values of an unknown type is done with ordinary values instead, such as a decoder value (`std::json`).
- Each function may have `///` doc comments (D-227).
- Function names must be distinct within the interface, including functions inherited by embedding ([§8.3](#83-embedding)).
- There are no default implementations (D-117).
- Interfaces have no type parameters, and interface functions are not generic: an interface function has no type parameter list of its own, and `interface Into<T> { fn into(Self) -> T }` is a compile error (D-165).
- An interface may have no functions, written `interface Any` or `interface Any {}`. Such an interface is satisfied by every type, including tuples and function types, and `dyn` of it is a universal box that can hold any value ([§8.6](#86-dyn-values)) (D-166). The requirement that every function mention `Self` is vacuously met.

An interface's visibility (package-visible by default, `pub` or `priv`) controls where it can be named, in bounds and in `dyn`, like any other item ([§7.3](07-modules-and-packages.md#73-visibility)) (D-162).

## 8.2 Satisfaction

A type `T` **satisfies** an interface `I` when, for every function `fn f(P1, …, Pn) -> R` of `I`, the module `M` in which `T` is declared has a function named `f` whose signature equals the interface signature with `Self` replaced by `T` (D-020, D-061).

- **The type's own module.** Only functions in the module that declares `T` count (D-061). A function in any other module, even with the right signature, does not make `T` satisfy `I`. To make a foreign type satisfy a new interface, wrap it in a new type (ADR-0023).
- **Visibility.** A function of `M` counts toward `I` only if it is at least as visible as `T` itself, and a function that counts does so everywhere `T` is visible (D-164, ADR-0034):
  - for a `pub` type, only `pub` functions count,
  - for a package-visible type, package-visible and `pub` functions count,
  - for a `priv` type, every function of `M` counts, including `priv` ones.

  Whether `T` satisfies `I` therefore has one answer wherever `T` can be named, independent of the module or package doing the check. A function that is less visible than its type is simply ignored for satisfaction. It can still be called directly where it is visible.

- **Aliases.** For a type alias, the aliased type's module counts ([§4.4](04-declarations.md#44-type-aliases)).
- **Built-in types** are treated as declared in their standard-library module: `Int` in `std::int`, `Float` in `std::float`, `String` in `std::string`, `List` in `std::list`, `U8` in `std::u8`, and so on (D-105).
- **Tuples** are not declared in any module. By a built-in rule, a tuple type satisfies `Show` when all of its element types satisfy `Show`, and `Compare` when all of its element types satisfy `Compare` (comparison is lexicographic, D-136) (D-167). Tuples satisfy no other interface, apart from empty interfaces (D-166).
- **Function types** are not declared in any module and satisfy no interface, apart from empty interfaces (D-166).

_Example:_ a `pub` type whose `show` is only package-visible does not satisfy `Show`, anywhere.

```cheby
// geometry/angle.cheby
pub type Angle { radians: Float }

fn show(a: Angle) -> String {   // package-visible, less visible than Angle
  "{a.radians} rad"
}
```

Interpolating an `Angle`, or passing it where `T: Show` is required, is a compile error, even inside the `geometry` package. The error points at `angle::show` and suggests making it `pub` so that it is as visible as `Angle`, or reducing `Angle`'s visibility (D-164).

### 8.2.1 One type per interface per module

Function names in a module are unique and never overloaded, so a module can satisfy a given interface for only one type (D-045, ADR-0020). A module that declares two types that should both be `Show` must be split into submodules, each declaring one type and its `show`.

```cheby
// geometry/point.cheby
pub type Point { x: Float, y: Float }

pub fn show(p: Point) -> String {
  "({p.x}, {p.y})"
}

pub fn add(a: Point, b: Point) -> Point {
  Point { x: a.x + b.x, y: a.y + b.y }
}
```

`Point` now satisfies `Show` and `Add` ([§8.7](#87-operator-interfaces)), with no declaration saying so.

### 8.2.2 Conditional conformance

For a generic type, the module's function may have bounds on its type parameters. The type then satisfies the interface for exactly those type arguments that meet the bounds (D-064):

```cheby
// in std::list
pub fn show<T: Show>(xs: List<T>) -> String { … }
```

`List<Int>` satisfies `Show` because `Int` does. `List<fn() -> Nil>` does not, because function types do not.

The function's signature must still match the interface after substituting `Self` with the generic type applied to the function's own type parameters: `show(List<T>) -> String` matches `fn show(Self) -> String` with `Self = List<T>`.

## 8.3 Embedding

`interface I: A + B { … }` embeds the interfaces `A` and `B` (D-117). `I` requires all functions of `A`, all functions of `B`, and its own. A type satisfies `I` exactly when it satisfies `A`, `B` and `I`'s own functions.

```cheby
pub interface Num: Add + Sub + Mul + Div
```

If two embedded interfaces require a function with the same name, the signatures must be identical, and the requirement is counted once. Otherwise the declaration is a compile error. Embedding must not be cyclic.

## 8.4 Bounds

A type parameter of a top-level function may be bounded by one or more interfaces, combined with `+` (D-063, D-117):

```cheby
pub fn describe_all<T: Show>(items: List<T>) -> String { … }
pub fn clamp<T: Compare>(x: T, low: T, high: T) -> T { … }
pub fn sum_and_show<T: Add + Show>(xs: List<T>, zero: T) -> String { … }
```

- A call to a bounded generic function type-checks only if every type argument satisfies its bounds. The error reports which function the type's module is missing, or which function is too little visible to count ([§8.2](#82-satisfaction)).
- Inside the function body, a bounded type parameter may be used with the interfaces' functions and with any operator sugar they provide ([§8.7](#87-operator-interfaces)) and interpolation (`Show`).
- Type parameters of type declarations have no bounds ([§4.3](04-declarations.md#43-type-declarations)).
- A type parameter may be bounded only in the function's type parameter list. There is no `where` clause.

## 8.5 Calling interface functions

Inside a generic function, the concrete type of a bounded parameter is not known, so its module cannot be named. Interface functions are therefore called by name **through the interface**, as `Show::show(x)`, or with a module-qualified interface name, as `shape::Shape::area(x)` (D-173).

The call is resolved from the type of the argument in the first `Self` position of the function's signature (D-173):

- for a bounded type parameter, through the bound (the dictionary passed for it, [§8.5.1](#851-implementation-informative)),
- for a concrete type, through that type's module, which must satisfy the interface ([§8.2](#82-satisfaction)),
- for a `dyn I` value, dynamically, through the functions packaged with the value ([§8.6](#86-dyn-values)).

This form adds no names to the scope (D-062), and it disambiguates bounds that require functions with the same name: with `T: A + B`, `A::f(x)` and `B::f(x)` are distinct calls. Operators ([§8.7](#87-operator-interfaces)) and `Show` interpolation remain sugar for these calls.

```cheby
pub fn total_area<T: Shape>(shapes: List<T>) -> Float {
  list::fold(shapes, 0.0, fn(acc, s) { acc + Shape::area(s) })
}
```

For a concrete type, the type's module function can always be called directly, for example `point::show(p)`.

### 8.5.1 Implementation (informative)

Bounds are resolved by dictionary passing: a generic function with bounds receives, for each bound, a record of the satisfying type's functions (D-046). With the uniform representation (D-021), each generic function is compiled once. An implementation may specialize generic functions for known types as an optimization.

## 8.6 dyn values

`dyn I` is a type whose values are a value of some type satisfying `I`, packaged with that type's functions for `I` (D-046, D-063). It gives dynamic dispatch and lets values of different types share one collection:

```cheby
let shapes: List<dyn Shape> = [circle, square, triangle]
let areas = list::map(shapes, fn(s) { Shape::area(s) })
```

- `dyn I` is allowed only if every function of `I`, including embedded ones, uses `Self` only as its **first parameter**, and nowhere else: not in other parameters, not in the return type, not nested inside other types (D-117). Otherwise `dyn I` is a compile error that names the offending function.
- `dyn I` satisfies `I` itself, calling through to the packaged functions.
- `dyn I` satisfies every interface embedded in `I`, and no others.
- Two `dyn I` values are equal under `==` when their underlying values have the same type and are structurally equal (D-033).

A value becomes a `dyn I` implicitly: wherever a value of a type satisfying `I` appears where `dyn I` is expected, such as an annotated `let`, a function argument, a list element with a known element type, or a field, it is packaged with its type's functions for `I` (D-168). There is no explicit syntax for this coercion. There is no conversion from `dyn I` back to the underlying type, and no downcasting in v1 (D-168).

`dyn` of an empty interface ([§8.1](#81-declaration)) is a universal box: any value can be coerced to it, and since the interface has no functions, no interface function can be called on it (D-166).

## 8.7 Operator interfaces

Operators on non-built-in types are overloaded through interfaces (D-034, D-047, D-118):

| Interface | Function                          | Operators                            |
| --------- | --------------------------------- | ------------------------------------ |
| `Add`     | `fn add(Self, Self) -> Self`      | `a + b`                              |
| `Sub`     | `fn sub(Self, Self) -> Self`      | `a - b`                              |
| `Mul`     | `fn mul(Self, Self) -> Self`      | `a * b`                              |
| `Div`     | `fn div(Self, Self) -> Self`      | `a / b`                              |
| `Neg`     | `fn neg(Self) -> Self`            | `-a`                                 |
| `Compare` | `fn compare(Self, Self) -> Order` | `a < b`, `a <= b`, `a > b`, `a >= b` |

These interfaces live in `std::ops`, not in the prelude (D-112, D-169). Operators work without importing `std::ops`, because an operator is resolved through the operand type's module. Only naming one of these interfaces, in a bound or in `dyn`, requires the import (D-169).

Rules:

- Both operands must have the same type (D-118). `Vector * Float` is not an operator; write `vector::scale(v, 2.0)`.
- `a < b` and the other comparisons desugar to `compare` as defined in [§5.4.4](05-expressions.md#544-comparison) (D-047).
- `==` and `!=` are always built-in structural equality and cannot be overloaded (D-047). A `compare` that returns `Equal` for values that are not `==` is legal but is a program bug that sorted collections may expose.
- `%`, `&`, `|`, `^`, `<<`, `>>`, `&&`, `||` and `!` cannot be overloaded (D-118).
- There are no compound assignment operators, since there is no assignment.

Built-in types satisfy the operator interfaces through their standard-library modules:

| Type                  | Interfaces                                                           |
| --------------------- | -------------------------------------------------------------------- |
| `Int`, sized integers | `Add Sub Mul Div Neg Compare`                                        |
| `Float`, `F32`        | `Add Sub Mul Div Neg Compare`                                        |
| `String`              | `Add Compare` (D-067)                                                |
| `Bool`                | `Compare` (D-136)                                                    |
| `List<T>`             | `Compare` when `T` satisfies `Compare` (D-136)                       |
| tuples                | `Compare` when every element type satisfies `Compare` (D-136, D-167) |

For built-in numeric types, the compiler uses the built-in operations directly ([§5.4.2](05-expressions.md#542-built-in-arithmetic)).

## 8.8 Standard interfaces

`Show` is in the prelude (D-112):

```cheby
pub interface Show {
  fn show(Self) -> String
}
```

`Show` produces user-facing text. It is used by string interpolation `{x}` (D-067, D-086), and a type that does not satisfy it cannot be interpolated without `:?` ([§5.3.2](05-expressions.md#532-string-interpolation)).

The built-in numeric types, `String` (whose `show` returns the string unchanged), `Bool` and `Nil` satisfy `Show`. `List`, `Option` and `Result` satisfy it conditionally on their element types (D-064), and tuples satisfy it by a built-in rule when all their element types do (D-167).

Equality, hashing and debug printing are **not** interfaces. They are built in for all types except functions and cannot be customized (D-033, [§3.13](03-types.md#313-equality-hashing-and-debug-printing)).

Other standard interfaces (for example for iteration, hashing into custom structures or serialization) are standard-library API and are specified in the standard-library spec, [`../stdlib/`](../stdlib/) (D-204). JSON encoding and decoding live in `std::json` (D-210, ADR-0038). Because of the `Self` rule of [§8.1](#81-declaration), encoding can be an interface, but decoding uses decoder values (D-209).
