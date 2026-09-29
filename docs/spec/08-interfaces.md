# 8. Interfaces

Interfaces provide ad-hoc polymorphism. They are **structural**: a type satisfies an interface implicitly, by having the required functions, without any `impl` declaration (D-020, ADR-0010). Because Cheby has no methods, "having a function" means that the module declaring the type also declares a function with the right name and signature (D-061, ADR-0023).

## 8.1 Declaration

```ebnf
interface_decl = [ visibility ] "interface" UPPER [ ":" bound ] [ "{" [ NL ] { interface_fn NL } "}" ] ;
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
- An interface may have no functions, written `interface Any` or `interface Any {}`. Such an interface is satisfied by every type, including tuples and function types, and `dyn` of it is a universal box that can hold any value, except that a type parameter in the value's type needs a bound ([§8.6.1](#861-type-descriptors)) (D-166, D-237). The requirement that every function mention `Self` is vacuously met.

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
import std::ops::{Add, Div, Mul, Sub}

pub interface Num: Add + Sub + Mul + Div
```

If two embedded interfaces require a function with the same name, the signatures must be identical, and the requirement is counted once. Otherwise the declaration is a compile error. Embedding must not be cyclic.

## 8.4 Bounds

A type parameter of a top-level function may be bounded by one or more interfaces, combined with `+` (D-063, D-117):

```cheby
import std::ops::{Add, Compare}

pub fn describe_all<T: Show>(items: List<T>) -> String { … }
pub fn clamp<T: Compare>(x: T, low: T, high: T) -> T { … }
pub fn sum_and_show<T: Add + Show>(xs: List<T>, zero: T) -> String { … }
```

- A call to a bounded generic function type-checks only if every type argument satisfies its bounds. The error reports which function the type's module is missing, or which function is too little visible to count ([§8.2](#82-satisfaction)).
- Inside the function body, a bounded type parameter may be used with the interfaces' functions and with any operator sugar they provide ([§8.7](#87-operator-interfaces)) and interpolation (`Show`).
- A bound also supplies the type's descriptor for conversions to `dyn` ([§8.6](#86-dyn-values)), so a type parameter that appears in the type of a value converted to `dyn` needs a bound. An empty interface suffices (D-236).
- Type parameters of type declarations have no bounds ([§4.3](04-declarations.md#43-type-declarations)).
- A type parameter may be bounded only in the function's type parameter list. There is no `where` clause.

## 8.5 Calling interface functions

Inside a generic function, the concrete type of a bounded parameter is not known, so its module cannot be named. Interface functions are therefore called by name **through the interface**, as `Show::show(x)`, or with a module-qualified interface name, as `shape::Shape::area(x)` (D-173).

The call is resolved from the type of the argument in the first `Self` position of the function's signature (D-173):

- for a bounded type parameter, through the bound (the dictionary passed for it, [§8.5.1](#851-implementation-informative)),
- for a concrete type, through that type's module, which must satisfy the interface ([§8.2](#82-satisfaction)),
- for a `dyn I` value, dynamically, through the functions packaged with the value ([§8.6](#86-dyn-values)).

The argument's type need not be known when the call is checked. The call is resolved at the end of the body, and its result type follows from the interface signature before that ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234).

This form adds no names to the scope (D-062), and it disambiguates bounds that require functions with the same name: with `T: A + B`, `A::f(x)` and `B::f(x)` are distinct calls. Operators ([§8.7](#87-operator-interfaces)) and `Show` interpolation remain sugar for these calls.

```cheby
pub fn total_area<T: Shape>(shapes: List<T>) -> Float {
  list::fold(shapes, 0.0, fn(acc, s) { acc + Shape::area(s) })
}
```

For a concrete type, the type's module function can always be called directly, for example `point::show(p)`.

### 8.5.1 Implementation (informative)

Bounds are resolved by dictionary passing: a generic function with bounds receives, for each bound, a record of the satisfying type's functions (D-046). Each such function package also carries the type descriptor of the type it was built for, which `dyn` conversions inside the function use ([§8.6](#86-dyn-values)) (D-236). A type parameter without bounds receives nothing. With the uniform representation (D-021), each generic function is compiled once. An implementation may specialize generic functions for known types as an optimization.

Descriptors of generic types such as `List<T>` are built at run time from the descriptors of their type arguments. An implementation may cache or intern them.

## 8.6 dyn values

`dyn I` is a type whose values are a value of some type satisfying `I`, packaged with that type's functions for `I` and a descriptor of its type ([§8.6.1](#861-type-descriptors)) (D-046, D-063, D-236). It gives dynamic dispatch and lets values of different types share one collection:

```cheby
let shapes: List<dyn Shape> = [circle, square, triangle]
let areas = list::map(shapes, fn(s) { Shape::area(s) })
```

- `dyn I` is allowed only if every function of `I`, including embedded ones, uses `Self` only as its **first parameter**, and nowhere else: not in other parameters, not in the return type, not nested inside other types (D-117). Otherwise `dyn I` is a compile error that names the offending function.
- `dyn I` satisfies `I` itself, calling through to the packaged functions.
- `dyn I` satisfies every interface embedded in `I`, and no others.
- Two `dyn I` values are equal under `==` when their underlying values have the same type and are structurally equal, which is decided with type descriptors ([§8.6.1](#861-type-descriptors)) (D-033, D-236).

A value becomes a `dyn I` implicitly: a value of a type satisfying `I` is packaged with its type's functions for `I` where `dyn I` is the expected type when the value is checked, such as an annotated `let`, a function argument, a list element when the list type is expected, or a constructor field ([§3.12.5](03-types.md#3125-expected-types-and-conversions)) (D-168, D-235). There is no explicit syntax for this conversion. It applies one level only, at the value itself, and never inside another type: `List<Circle>` is never a `List<dyn Shape>`, and `fn() -> Circle` is never a `fn() -> dyn Shape`. Each conversion is packaging, O(1) in the size of the value, with no hidden traversal. Building the descriptor of a generic type such as `List<T>` from its arguments' descriptors takes time proportional to the size of the type, not of the value, and may be cached (D-237). A whole list is converted with `list::map(circles, fn(c) -> dyn Shape { c })`. Where no expected type is known, nothing converts, so `let mixed = [circle, square]` is a type mismatch. There is no conversion from `dyn I` back to the underlying type, and no downcasting in v1 (D-168).

`dyn` of an empty interface ([§8.1](#81-declaration)) is a universal box: any value can be converted to it, except that a value whose type mentions a type parameter needs a bound on that parameter ([§8.6.1](#861-type-descriptors)). Since the interface has no functions, no interface function can be called on it (D-166, D-237).

### 8.6.1 Type descriptors

Every `dyn` value carries, next to its packaged functions, a runtime **type descriptor** of its underlying value's full type, including all type arguments (D-236). Descriptors identify nominal types exactly: two declarations with the same shape, or types with the same name in different modules or packages, including different major versions of the same package ([§7.1](07-modules-and-packages.md#71-packages)), have different descriptors ([§3.11](03-types.md#311-user-defined-types-and-aliases)). An alias has the descriptor of the aliased type, and a type redefined in the REPL gets a new descriptor ([§13.5.2](13-tooling.md#1352-redefinition)) (D-043).

- Two `dyn I` values are equal under `==` when their descriptors are equal and their underlying values are structurally equal. Values of different types are never equal, even when they print the same.
- Hashing a `dyn` value hashes its descriptor together with its value, so equal `dyn` values hash equally, and `dyn` values can be `Map` keys and `Set` elements.
- If the comparison or the hash reaches a function value or a handle inside the value, it panics, as for any other value ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-233).

```cheby
let a: dyn Show = circle
let b: dyn Show = circle
a == b        // True: same type, equal values
let c: dyn Show = list::new::<Int>()
let d: dyn Show = list::new::<String>()
c == d        // False: List<Int> and List<String> are different types
```

For a type known at the conversion, the compiler supplies the descriptor statically. When the type of the converted value mentions a type parameter `T` of the enclosing top-level function, `T`'s descriptor comes from one of `T`'s bounds ([§8.4](#84-bounds)): every function package passed for a bound carries the descriptor of the type it was built for ([§8.5.1](#851-implementation-informative)). Converting to `dyn` a value whose type mentions a type parameter without any bound is a compile error, and the diagnostic suggests adding a bound. An empty interface works as such a bound, because every type satisfies it, so adding it never breaks a caller (D-166, D-236). A value that is already a `dyn` value keeps its descriptor, so `circle` converted to `dyn Any` equals `circle` converted first to `dyn Shape` and then to `dyn Any`.

```cheby
interface Any

fn wrap<T: Any>(xs: List<T>) -> dyn Any { xs }      // OK: `T`'s descriptor comes from its bound
fn label<T: Show>(xs: List<T>) -> dyn Show { xs }   // OK: the `Show` bound carries it
fn wrap_bad<T>(xs: List<T>) -> dyn Any { xs }       // error: `T` has no bound, add one such as `T: Any`
```

The check that a converted value's type satisfies `I`, and the choice of its function package and descriptor, are constraints resolved at the end of the enclosing top-level body, after numeric literals are defaulted, like operators and interface calls ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234, D-237). After `let x = 1` and `let d: dyn Show = x`, a later use of `x` as a `U8` gives `d` the descriptor of `U8` and `U8`'s `show`. If the converted value's type is still not fully known at the end, as in `let a: dyn Any = []` or `let b: dyn Any = None` with nothing fixing the element type, it is a compile error that asks for a type annotation. Only numeric literals are defaulted.

Descriptors, and therefore `==` and hashing on `dyn` values, are the same on every target: `Int`, `I64`, `F32` and `Float` have distinct descriptors on JS too, even where they share a JS representation (D-232, D-237).

Descriptors are observable only through `==` and hashing. Through them, a bound on a type parameter, even an empty interface, makes that parameter's type identity observable. This is intended: comparing an empty `xs: List<T>` with `[]: List<Int>`, both as `dyn Any`, reveals whether `T` is `Int` (D-237). Type parameters without bounds stay fully parametric, because values whose type mentions them cannot be converted to `dyn`. There is still no conversion back from `dyn`, no downcasting and no type reflection in v1 (D-168), and debug printing of a `dyn` value prints the value inside it (D-233). Only `dyn` values and function packages for bounds carry descriptors, so generic code without bounds, and code that never converts to `dyn`, pays nothing (D-236).

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

Equality, hashing and debug printing are **not** interfaces. They are built in and cannot be customized. Debug printing works for all types, and equality and hashing work for all types but panic on functions and handles (D-033, D-233, [§3.13](03-types.md#313-equality-hashing-and-debug-printing)).

Other standard interfaces (for example for iteration, hashing into custom structures or serialization) are standard-library API and are specified in the standard-library spec, [`../stdlib/`](../stdlib/) (D-204). JSON encoding and decoding live in `std::json` (D-210, ADR-0038). Because of the `Self` rule of [§8.1](#81-declaration), encoding can be an interface, but decoding uses decoder values (D-209).
