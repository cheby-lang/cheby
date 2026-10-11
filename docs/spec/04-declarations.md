# 4. Declarations

A module ([chapter 7](07-modules-and-packages.md)) is a sequence of top-level declarations, also called **items**. This chapter defines each kind of item.

```ebnf
module      = { module_doc NL } { inner_attribute NL } { import_decl NL } { item NL } ;   (* imports: §7.5, inner_attribute: §4.8; end of file ends the last line, D-426 *)
item        = { doc_comment NL } { attribute NL } item_body ;   (* doc comments before attributes, D-434 *)
item_body   = fn_decl | type_decl | alias_decl | const_decl | interface_decl | test_decl ;
```

## 4.1 General rules

### 4.1.1 Order and recursion

Top-level items may appear in any order. Any item may refer to any other item in the same module, regardless of position. Functions may be directly or mutually recursive, and types may be recursive.

Constants may refer to other constants, but the references must not form a cycle ([§4.5](#45-constants)).

### 4.1.2 Names

All top-level names in a module must be distinct within their namespace ([§7.7](07-modules-and-packages.md#77-namespaces-and-name-resolution)). Types and interfaces share the type namespace. Functions, constants and constructors share the value namespace. A type and a constructor may therefore have the same name, which is what the single-variant shorthand relies on ([§4.3.3](#433-single-variant-shorthand)) (D-111).

Names follow the case rules of [§2.3](02-lexical-structure.md#23-identifiers) (D-059):

| Item                                             | Case                   |
| ------------------------------------------------ | ---------------------- |
| functions, constants, parameters                 | `snake_case` (`LOWER`) |
| types, type parameters, constructors, interfaces | `PascalCase` (`UPPER`) |

### 4.1.3 Visibility

An item has one of three visibility levels ([§7.3](07-modules-and-packages.md#73-visibility)) (D-162):

```ebnf
visibility  = "pub" | "priv" ;   (* default: package-visible *)
```

| Prefix   | Visible to                                                      |
| -------- | --------------------------------------------------------------- |
| `priv`   | its own module only                                             |
| _(none)_ | every module of the same package (package-visible, the default) |
| `pub`    | every module of every package                                   |

Package visibility is the default, like Rust's `pub(package)`. `pub` makes an item visible to other packages, and `priv` restricts it to its own module (D-162). There are no other visibility mechanisms.

`pub` and `priv` may appear on functions, types, aliases, constants and interfaces. Tests can be neither `pub` nor `priv` ([§4.7](#47-tests)).

An item's signature must not mention a type less visible than the item itself (D-148, D-149), because code that can see the item could not name the type. So a `pub` item must not mention a type that is not `pub` (for example `pub fn make() -> Secret` where `Secret` is package-visible), and a package-visible item must not mention a `priv` type. Either is a compile error. The signature of a function is its parameter and return types and bounds, the signature of an alias is its definition, the signature of a constant is its declared type, and the signature of an interface is its embedded interfaces and its functions. Interfaces named in a signature, in bounds, `dyn` types and embeddings, count like types (D-488). An alias is transparent ([§4.4](#44-type-aliases)), so a signature that names an alias mentions the types the alias stands for, and the alias's own visibility does not matter: a `pub` function may use a `priv` alias of `Int` (D-488). The fields of a type count as its signature only where they are visible ([§4.3.4](#434-opacity-and-exposed)): a `pub exposed` type's fields must mention only `pub` types, a package-visible type's fields must not mention `priv` types, and a `pub` type without `exposed` may have fields of package-visible types, since its representation is hidden outside the package (D-198). Making such a type `exposed` later requires those field types to become `pub`.

Unused items warn according to their visibility (D-152, D-080): an unused `priv` item always warns, an unused package-visible item warns when nothing in the package uses it, and a `pub` item never warns. What counts as a use is defined in [§13.8](13-tooling.md#138-diagnostics-and-warnings) (D-516).

### 4.1.4 Doc comments

`///` comments before an item document it, and `//!` comments at the top of a file document the module (D-078). `///` comments may also document a variant or a field of a type declaration (D-211), and a function of an interface declaration (D-227). They are used by `cheby doc` and the language server ([chapter 13](13-tooling.md)). Doc comments are Markdown.

```cheby
/// Why a request failed.
pub exposed type Error {
  /// The server did not answer in time.
  TimedOut,
  /// The server answered with a status other than 2xx.
  Status {
    /// The HTTP status code.
    code: Int,
  },
}
```

## 4.2 Functions

```ebnf
fn_decl     = [ visibility ] "fn" LOWER [ type_params ] "(" [ params ] ")" [ "->" type ] [ block ] ;
type_params = "<" type_param { "," type_param } [ "," ] ">" ;
type_param  = UPPER [ ":" bound ] ;
bound       = interface_ref { "+" interface_ref } ;
params      = param { "," param } [ "," ] ;
param       = LOWER ":" type ;
```

- Every parameter must have a type, and the return type must be written unless it is `Nil` (D-011, D-083).
- A missing `-> type` means `-> Nil` (D-083). A function declared `-> Never` must not return ([§3.10](03-types.md#310-never)).
- The body is a block ([§5.1](05-expressions.md#51-blocks-and-statements)). Its type must be the declared return type, which is its expected type ([§3.12.5](03-types.md#3125-expected-types-and-conversions)).
- A function without a body is allowed only with an `@external` attribute ([§12.5](12-targets-and-ffi.md#125-foreign-functions)).
- Parameters are bindings and follow the no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)).
- Type parameters are `UPPER` names and may have interface bounds ([§8.4](08-interfaces.md#84-bounds)).

Parameters of top-level functions are plain names, so signatures and generated documentation always name them. Destructure with `let` in the body. Closures and local functions may take irrefutable patterns as parameters ([§5.7](05-expressions.md#57-anonymous-functions)) (D-242).

```cheby
import std::io
import std::list
import std::ops

pub fn clamp(value: Int, low: Int, high: Int) -> Int {
  case {
    value < low => low
    value > high => high
    _ => value
  }
}

fn greet(name: String) {
  io::println("hello {name}")
}

pub fn largest<T: ops::Compare>(first: T, rest: List<T>) -> T {
  list::fold(rest, first, fn(best, x) {
    case x > best {
      True => x
      False => best
    }
  })
}
```

Local functions, declared inside a function body, are expressions and are covered in [§5.8](05-expressions.md#58-local-named-functions).

### 4.2.1 `main`

An entry point is a function named `main` with no parameters, returning either `Nil` or `Result<Nil, E>` for some `E` (D-073). Its behavior is defined in [§10.10](10-concurrency.md#1010-program-entry-and-exit).

Any module's `main` can be run by module path, for example `cheby run my_app::tools::migrate` (D-137). Every top-level function named `main`, in any module, is therefore an entry point and must have this signature. A `main` with parameters, type parameters or another return type is a compile error (D-448, D-491). `cheby run` could not choose type arguments for `main` or pass function packages for its bounds. The default entry point is the `main` of the package's root module, `src/main.cheby` ([§7.2](07-modules-and-packages.md#72-modules-and-files)) (D-137, D-142).

`main` may have any visibility, including `priv`, because `cheby run` finds it by module path (D-163). It is usually written without a visibility prefix, `fn main()`, since `pub` would export it as part of the package's API.

## 4.3 Type declarations

```ebnf
type_decl   = [ "pub" [ "exposed" ] | "priv" ] "type" UPPER [ plain_params ] [ type_body ] ;   (* no body: external type, §12.5.3 *)
plain_params = "<" UPPER { "," UPPER } [ "," ] ">" ;
type_body   = "{" [ NL ] variant { "," [ NL ] variant } [ "," ] [ NL ] "}"   (* variants *)
            | "{" [ NL ] field { "," [ NL ] field } [ "," ] [ NL ] "}" ;    (* single-variant shorthand *)
variant     = { doc_comment NL } variant_body ;   (* D-211 *)
variant_body = UPPER
            | UPPER "(" type { "," type } [ "," ] ")"
            | UPPER "{" [ NL ] field { "," [ NL ] field } [ "," ] [ NL ] "}" ;
field       = { doc_comment NL } LOWER ":" type ;   (* D-211 *)
```

Every user-defined data type is an algebraic data type: a set of variants, each with its own fields (D-022). There is no `struct` or `enum` keyword. A record is a type with one variant ([§4.3.3](#433-single-variant-shorthand)).

Type parameters of a type declaration have no bounds.

A type declaration without a body declares an external type, whether or not it has an `@external` attribute ([§12.5.3](12-targets-and-ffi.md#1253-external-types)) (D-491).

### 4.3.1 Variants

A variant is one of:

- a **unit variant** with no fields: `Red`, `None`,
- a **positional variant**: `Some(T)`, `Pair(A, B)`,
- a **named-field variant**: `Circle { radius: Float }`.

Field names must be distinct within a variant. The same field name may appear in several variants, and if it has the same type in all of them it can be accessed directly ([§5.11.3](05-expressions.md#5113-field-access)) (D-071).

A type declaration must have at least one variant. Variant names must be distinct within the module's value namespace.

A positional variant has at least one type, and a named-field variant at least one field. `Foo()` and `Foo {}` are compile errors that suggest the unit variant `Foo` (D-437).

```cheby
pub exposed type Shape {
  Circle { radius: Float },
  Rect { width: Float, height: Float },
  Dot,
}

pub type Tree<T> {
  Leaf,
  Node(Tree<T>, T, Tree<T>),
}
```

### 4.3.2 Constructors

Each variant introduces a **constructor** with the variant's name in the value namespace. Construction syntax is defined in [§5.11](05-expressions.md#511-constructors-and-records):

- `Red` is a value of the type.
- A positional constructor `Some` is called like a function, `Some(1)`. It can also be used as a function value, for example `list::map(xs, Some)`.
- A named-field constructor is only used with braces, `Circle { radius: 1.0 }`. It cannot be called positionally (`Circle(1.0)` is a compile error) or used as a function value (D-138). Field order is therefore never significant, and reordering fields never breaks callers.

### 4.3.3 Single-variant shorthand

A type body that starts with a field (`name: Type`) declares a type with exactly one named-field variant whose constructor has the same name as the type (D-082):

```cheby
pub type Point { x: Float, y: Float }

// is equivalent to

pub type Point { Point { x: Float, y: Float } }
```

The type `Point` lives in the type namespace and the constructor `Point` in the value namespace, so the names do not clash (D-111).

### 4.3.4 Opacity and `exposed`

Types are opaque outside their package by default (D-035, D-048). The package is the only opacity boundary, and there is no separate module-level opacity (D-151). For a type `T` declared in module `m` of package `p`:

| Declaration                | Type name usable             | Constructors, patterns and fields usable |
| -------------------------- | ---------------------------- | ---------------------------------------- |
| `priv type T`              | in module `m` only           | in module `m` only                       |
| `type T` (package-visible) | in any module of package `p` | in any module of package `p`             |
| `pub type T`               | in any module of any package | only in modules of package `p`           |
| `pub exposed type T`       | in any module of any package | in any module of any package             |

"Constructors, patterns and fields" means constructing values with the type's constructors, matching on them in patterns, reading fields with `.field`, and record update (D-048). `priv type T` hides the type together with its constructors and fields outside its module, and a package-visible type is fully transparent inside its package (D-151). `exposed` requires `pub` (D-121, D-150), since it only matters across packages. A use of an opaque type's constructors or fields from another package is a compile error reported at the use, which is the code its author can change (D-487).

Code outside package `p` can still pass values of an opaque type around, compare them with `==`, hash them and debug-print them ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)), with the same panics on functions and handles as any other value (D-233), and use any `pub` functions the package provides.

_Rationale:_ the default lets a library change its representation without breaking users (ADR-0016). This reverses Gleam, where types are transparent unless marked `opaque`.

## 4.4 Type aliases

```ebnf
alias_decl  = [ visibility ] "type" UPPER [ plain_params ] "=" type ;
```

A type alias gives another name to an existing type (D-081). The alias is transparent: it is the same type as its definition, interchangeable everywhere, and it satisfies the same interfaces. An alias may have type parameters, which must all be used in the definition. An alias must not refer to itself, directly or indirectly.

```cheby
pub type UserId = Int
pub type Table<V> = map::Map<String, V>
```

To make a distinct type, declare a single-variant type instead:

```cheby
pub type UserId { UserId(Int) }
```

`exposed` is not allowed on aliases.

## 4.5 Constants

```ebnf
const_decl  = [ visibility ] "const" LOWER ":" type "=" expr ;
```

A constant is a named, immutable, module-level value. Its type must be written (D-121).

The initializer is an ordinary expression, evaluated **at compile time** (D-090):

- It may call ordinary Cheby functions, including functions from other modules and packages.
- The compiler runs it with the JIT and embeds the resulting value in the program. It is never evaluated at run time.
- If evaluation panics, it is a compile error that shows the panic.
- If evaluation reaches an IO operation, a fiber or channel operation, or an `@external` function, it is a compile error (D-090). Effects are untracked (D-016), so this is detected during evaluation, not by the type checker. The exception is the effect-free kernels of `std::list`, `std::map`, `std::set`, `std::string` and `std::bytes` (D-360), and of the numeric modules `std::int`, `std::float`, `std::f32` and the sized integer modules, so `const label: String = "{n}"` works (D-537). The compiler runs them itself.
- Implementations must limit the time and memory that evaluation may use and report a compile error when a limit is exceeded ([§13.9](13-tooling.md#139-compile-time-evaluation)).
- A constant that exists on JS, meaning that its targets intersected with the package's targets ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) contain `js`, must not hold an `Int` outside the JS safe-integer range anywhere in its value. Such a value is a compile error (D-037, D-540, ADR-0027). Constants restricted to native, such as `int::max_value`, are never checked, because they never reach JS (D-540). The initializer's own `Int` literals are not checked against that range, because it runs on the host (D-511). _Note:_ the reference implementation walks a graph of the constant's type with at most 65,536 nodes. Only a type that grows as it unfolds, through polymorphic recursion, can reach that bound, and an `Int` inside the types past it is not checked (D-619).
- Constants may refer to other constants, but not cyclically. A cycle of constants that name each other directly in their initializers is found by the type checker, and a cycle through a function call when the constants are evaluated. Imports are acyclic, so no cycle crosses modules (D-511).
- Each constant is evaluated exactly once, and its value does not depend on the order in which constants are evaluated ([§13.9](13-tooling.md#139-compile-time-evaluation)) (D-535, D-538).

There are no comptime parameters and types are not values (D-090, D-057).

A constant's value must not contain function values (including references to top-level functions), channels, fibers or external/handle types (D-139). The handles of the concurrency modules, `Sender`, `Receiver`, `Fiber`, `Scope` and `Selector`, are external types, so this rule covers them (D-647). Baking these into a binary is either impossible or meaningless. The type checker rejects a declared type that contains them, and evaluation rejects any such value it produces. A type contains what its tuples, built-in collections and the fields of every constructor of its named types contain, transitively and with the type arguments in place, so `Option<Db>` and a record with a function field are rejected like `Db` and `fn() -> Int`. A type argument that no field uses holds no value and is not looked at (D-511).

In v1 a constant's value must not contain `dyn` values either, because a `dyn` value carries a function table and a type descriptor ([§8.6](08-interfaces.md#86-dyn-values)). The type checker rejects a declared type that contains `dyn`. Allowing it later breaks no program (D-237).

```cheby
pub const max_connections: Int = 1024
const primes: List<Int> = sieve(1000)
const greeting: String = "hello, " + default_name
const default_name: String = "world"
```

Constants are never reference counted at run time and may be shared by all fibers freely ([§9.7](09-memory-model.md#97-constants)).

## 4.6 Interfaces

```ebnf
interface_decl = [ visibility ] "interface" UPPER [ ":" bound ] [ "{" [ NL ] [ interface_fn { NL interface_fn } ] [ NL ] "}" ] ;   (* D-427 *)
interface_fn   = { doc_comment NL } "fn" LOWER "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;
```

Interfaces are defined in [chapter 8](08-interfaces.md) (D-063, D-117).

## 4.7 Tests

```ebnf
test_decl   = "test" STRING block ;
```

A `test` item declares a named test (D-055):

- The name is a string literal without interpolation. Names must be distinct within a module.
- The body is a block, type-checked like a function body. It can use every item of the module, including `priv` ones (D-055, D-162).
- The body has type `Nil` or `Result<Nil, E>` for some `E` (D-140). A test passes if its body completes without panicking and, for a `Result` body, returns `Ok(Nil)`. An `Err` fails the test, so `use x <- result::try(…)` works directly in tests. Assertions use `assert` and `let assert` ([chapter 11](11-errors-and-panics.md)).
- A body of type `Never`, such as one that ends in `panic` or `todo`, counts as `Nil`, so the test is valid and fails when it runs. A body whose type nothing has determined at its end has type `Nil`. In a `Result` body, the body must determine the error type `E`, like any other type in a body (D-493).
- Tests are compiled only by `cheby test` (and the REPL and LSP). They are removed from all other builds (D-055).
- A test cannot be `pub` or `priv` and cannot be referred to by name.

```cheby
test "clamp keeps values in range" {
  assert clamp(5, 0, 10) == 5
  assert clamp(-1, 0, 10) == 0
  assert clamp(11, 0, 10) == 10
}
```

## 4.8 Attributes

```ebnf
attribute   = "@" LOWER [ "(" [ attr_arg { "," attr_arg } [ "," ] ] ")" ] ;   (* the name may be a reserved word, D-435 *)
attr_arg    = LOWER | STRING ;
```

The set of attributes is fixed, and user-defined attributes do not exist (D-200, D-057):

| Attribute                | Allowed on                                              | Meaning                               | Defined in                                                |
| ------------------------ | ------------------------------------------------------- | ------------------------------------- | --------------------------------------------------------- |
| `@external(target, …)`   | functions, types without a body                         | foreign implementation for one target | [§12.5](12-targets-and-ffi.md#125-foreign-functions)      |
| `@target(target)`        | functions, types, aliases, constants, interfaces, tests | item exists only on that target       | [§12.6](12-targets-and-ffi.md#126-target-specific-code)   |
| `@blocking`              | `@external` functions                                   | the foreign call may block            | [§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls) |
| `@async`                 | functions with `@external(js, …)`                       | the JS function returns a `Promise`   | [§12.5.2](12-targets-and-ffi.md#1252-js-abi)              |
| `@deprecated("message")` | `pub` items                                             | using the item warns with the message | below                                                     |

An unknown attribute name, a malformed attribute, a duplicated attribute (other than `@external` with different targets), or an attribute in a place it is not allowed is a compile error (D-461):

- `@external` takes `native` and one symbol string, or `js` and two strings, the module and the export, none of them empty ([§12.5](12-targets-and-ffi.md#125-foreign-functions)). An item has at most one `@external` per target.
- `@target` and `@!target` take exactly one target, `native` or `js`.
- `@blocking` and `@async` take no arguments, and `@deprecated` takes one message string.
- No attribute is allowed on a variant, a record field or an interface function. A run of attributes there is one error, and the member after it is still read (D-514).
- Every attribute other than `@external` appears at most once on an item, and `@!target` at most once in a module.
- An item must exist on at least one target ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) (D-513).

An attribute name binds nothing, so it is not checked against the reserved words or the naming convention of [§2.3](02-lexical-structure.md#23-identifiers) and [§2.4](02-lexical-structure.md#24-keywords). This is what lets `@async` exist although `async` is a reserved word. A name outside the table is an unknown attribute (D-435).

The standard library also uses three attributes that user code cannot write (D-482, D-692):

- `@std_clamped(param, …)` on a function names one or more distinct parameters of the function, which it clamps (D-298). A negative integer literal written for one of them is a warning ([§13.8](13-tooling.md#138-diagnostics-and-warnings)) (D-303). The warning also covers a literal that a capture such as `list::take(_, -1)` moves into its closure, and it does not look through constants or other expressions (D-518).
- `@std_hooks(equal_fn, hash_fn, debug_fn)` on a type names three functions of the type's module that implement its equality, hashing and debug printing ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-342). Their shapes are `equal_fn(a: T, b: T) -> Bool`, `hash_fn(value: T) -> K` for any `K`, and `debug_fn(value: T) -> String`, where `T` is the type and no type parameter has a bound. The runtime hashes the `K` that `hash_fn` returns with the built-in structural hash, so a hook may return a canonical value such as a sorted list of entries.
- `@std_context` on an `@external` function takes no arguments. It marks a kernel that receives the running fiber's context, so it may suspend the fiber, start fibers or use the fiber's state, as the functions of the concurrency modules and the suspending IO functions do (D-692). Such a kernel never calls a Cheby closure (D-484, D-675).

Only package `std` may write them. Outside it they are unknown attributes, and inside it a misplaced one or one that breaks these rules is a compile error like any other attribute error.

`@deprecated("message")` makes every use of the item from another module a warning that includes the message. It exists so that, under the compatibility promise, items can be discouraged without being removed (D-075, D-200). Every written name that refers to a deprecated item of another module warns, and so does a constructor of a deprecated type or a function of a deprecated interface. An item import of a deprecated item does not warn, but each use of the name it binds does. Uses in the item's own module and implicit uses, such as a deprecated `show` that an interpolation calls, do not warn (D-517).

A whole module is restricted to one target with the inner attribute `@!target(…)`, for example `@!target(js)`, placed after the `//!` module docs and before the imports (D-141, D-201, D-056). It mirrors `//!` and applies to every item in the file.

```ebnf
inner_attribute = "@!" LOWER [ "(" [ attr_arg { "," attr_arg } [ "," ] ] ")" ] ;   (* only @!target(...) *)
```
