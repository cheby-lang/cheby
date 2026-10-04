# 7. Modules and packages

## 7.1 Packages

A **package** is the unit of distribution and versioning. It is a directory with a `cheby.toml` manifest and a `src/` directory of source files ([§13.2](13-tooling.md#132-project-layout), D-097).

- A package name is a `LOWER` identifier. The name `std` is reserved for the standard library (D-058).
- Inside a package, the first segment of its module paths is its own `name` from the manifest (D-111, D-142). Another package imports those modules with the key it gave the package in its own manifest ([§13.3](13-tooling.md#133-manifest)), so a dependency's own name need not be unique in a build (D-238).
- A package depends on other packages through its manifest ([§13.3](13-tooling.md#133-manifest)). Dependency cycles between packages are a compile error.
- The package is the **opacity boundary**: every module of a package can see the constructors and fields of the package's types that are not `priv`, while other packages can see those of a `pub` type only if it is `exposed` (D-048, D-151, [§4.3.4](04-declarations.md#434-opacity-and-exposed)).

A dependency is identified by its git URL together with its major version (D-238). URLs are compared after normalization: the same repository written in different ways, for example with or without a trailing `.git` or `/`, or with the host name in a different case, is one URL, while different repositories, including forks, are different packages. Different major versions of one URL are different packages, and a build may contain several of them under different dependency keys ([§13.4](13-tooling.md#134-dependencies-and-lockfile)). The major version is the first non-zero part of the version, so `0.1` and `0.2` are different majors (D-239). The package being built is identified by itself, and `std` by being the standard library (D-058).

"Same package" in this specification means the same identity. Two majors of one URL are different packages, so neither can see the other's non-`exposed` constructors and fields. Types from different packages are different types even when their module paths and names are the same: `json::Value` from major 1 and from major 2 of one URL are distinct and have different type descriptors ([§8.6.1](08-interfaces.md#861-type-descriptors)). Diagnostics that mention such types should say which package and version each comes from (D-238).

The standard library is the package `std`. It ships with the compiler and is versioned with it (D-058). It needs no manifest entry.

## 7.2 Modules and files

Each source file is a module (D-162). A module's path is the package name followed by the file's path relative to `src/`, with directories separated by `::` and the file extension removed:

| File in package `my_app`  | Module path                |
| ------------------------- | -------------------------- |
| `src/main.cheby`          | `my_app` (the root module) |
| `src/web.cheby`           | `my_app::web`              |
| `src/web/router.cheby`    | `my_app::web::router`      |
| `src/tools/migrate.cheby` | `my_app::tools::migrate`   |

These are the paths inside `my_app`. A package that depends on `my_app` writes its own key for `my_app` as the first segment instead ([§7.1](#71-packages)).

There are no `mod` declarations. The file system is the single source of truth (D-162). A file and a directory may share a name: `src/web.cheby` and `src/web/router.cheby` are the modules `my_app::web` and `my_app::web::router`, and neither contains the other.

The names of `.cheby` files and of directories under `src/` must be valid `LOWER` identifiers. Such files and directories whose names are not valid identifiers are a compile error, except for hidden files, which are ignored.

Other files under `src/`, such as the JS helper modules named by `@external(js, "./…")`, are not modules. The naming rule does not apply to them, and module discovery ignores them (D-208). A JS build copies every file named by a relative `@external(js, …)` path into its output, at the same location relative to the compiled module ([§12.5](12-targets-and-ffi.md#125-foreign-functions)).

The package's **root module** is `src/main.cheby`. Its path is just the package name (`my_app`), and it cannot also be imported as `my_app::main` (D-142). Its `main` function is the program's default entry point. Any other module's `main` can be run by its module path, for example `cheby run my_app::tools::migrate` ([§4.2.1](04-declarations.md#421-main), D-137).

## 7.3 Visibility

Every item has one of three visibility levels (D-162):

| Declaration                      | Visible to                       |
| -------------------------------- | -------------------------------- |
| `priv fn f`, `priv type T`, …    | its own module only              |
| `fn f`, `type T`, … (no keyword) | every module of the same package |
| `pub fn f`, `pub type T`, …      | every module of every package    |

The default, with no keyword, is **package-visible**, like Rust's `pub(package)`. `pub` makes an item visible to other packages, and `priv` restricts it to its own module (D-162). There are no directory-based import restrictions (D-162).

An item that is not visible to a module cannot be named there, whether qualified or through an item import ([§7.5](#75-imports)). A `priv` item of `my_app::web` is not visible in `my_app::web::router`, and a package-visible item of `my_app::web` is not visible in another package.

`main` may have any visibility, including `priv`, because `cheby run` targets it by module path (D-163).

Unused items are warned about according to their visibility: an unused `priv` item always warns, an unused package-visible item warns when nothing in the package uses it, and `pub` items never warn (D-152).

```cheby
// src/web/session.cheby, package my_app
pub type SessionId { SessionId(String) }

type Store { Store(List<SessionId>) }          // package-visible

priv fn hash_token(token: String) -> String {  // this module only
  …
}

pub fn new_id(token: String) -> SessionId {
  SessionId(hash_token(token))
}
```

## 7.4 Visibility of types

An item's signature must not mention a type that is less visible than the item itself (D-148, D-149). "Signature" means a function's parameter and return types, a constant's type, an alias's right-hand side, a type's field types and an interface's function signatures. Violations are compile errors:

- a `pub` item must not mention a type that is not `pub` (D-148),
- a package-visible item must not mention a `priv` type (D-149).

```cheby
priv type Secret { Secret(String) }
type Token { Token(String) }

fn reveal() -> Secret { … }    // error: package-visible `reveal` mentions `priv` type `Secret`
pub fn issue() -> Token { … }  // error: `pub` `issue` mentions package-visible type `Token`
```

The visibility of a type also governs its constructors, patterns and fields (D-048, D-151):

- `priv type T` hides the type together with its constructors and fields outside its module.
- A package-visible `type T` is fully transparent inside its package: every module of the package can construct, match, read fields and update records. There is no separate module-level opacity (D-151).
- A `pub type T` is additionally usable by name in other packages, but its constructors, patterns and fields remain restricted to its own package unless it is `pub exposed` ([§4.3.4](04-declarations.md#434-opacity-and-exposed)). `exposed` requires `pub` (D-121, D-150).

## 7.5 Imports

```ebnf
import_decl = "import" module_path [ "as" LOWER ]
            | "import" module_path "::" "{" [ NL ] import_item { "," [ NL ] import_item } [ "," ] [ NL ] "}" ;
module_path = LOWER { "::" LOWER } ;
import_item = LOWER [ "as" LOWER ]
            | UPPER [ "as" UPPER ] ;
```

Imports use Rust-style paths with the `import` keyword (D-060). All imports of a module must come before its first item (D-143). An import after an item is a compile error.

Any module of the current package, of a dependency or of `std` may be imported. The first segment of a module path is the current package's own name, a dependency key from its manifest ([§13.3](13-tooling.md#133-manifest)) or `std` (D-238). What an import gives access to is limited by item visibility ([§7.3](#73-visibility)): from a module of the same package, all items that are not `priv`, and from a module of another package, only its `pub` items. `priv` items of another module are never accessible.

### 7.5.1 Module imports

`import a::b::c` makes the module `a::b::c` available under the name of its last segment, `c`. Its items visible to the importing module are then accessed as `c::name` (D-060). `import a::b::c as d` uses the name `d` instead (D-111).

A path without braces always names a module, never an item. The root module is imported by the first segment alone: the package's own name inside the package, as in `import my_app`, and the dependency key in other packages ([§7.1](#71-packages)).

```cheby
import std::list
import std::io
import my_app::web::router
import json::decode as json_decode

fn main() {
  io::println(list::length([1, 2]) |> int::show)   // error: `int` is not imported
}
```

A module name brought in by an import is a name in scope for the whole module. It follows the no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)) (D-062): two imports with the same last segment need an `as` alias, and a local binding cannot use an imported module's name.

### 7.5.2 Item imports

`import a::b::{x, Y, …}` brings the listed items of `a::b` into scope unqualified. Each listed item must be visible to the importing module ([§7.3](#73-visibility)). Each item may be renamed with `as`, keeping its case.

- Importing an `UPPER` name brings in every item with that name: the type or interface from the type namespace and the constructor from the value namespace (D-111). `import geometry::{Point}` makes both the type `Point` and its constructor usable.
- Importing a `LOWER` name brings in the function or constant with that name.

An item import binds only the listed items. It does not make the module name `b` available (D-144). To use both, write `import a::b` and `import a::b::{x}`.

Every path must be imported before use. `std::list::map(xs, f)` without `import std::list` is a compile error: a qualified path in an expression starts with exactly one module segment, `module::item` (D-060). The only longer form is an interface function named through its module, `shape::Shape::area` ([§8.5](08-interfaces.md#85-calling-interface-functions), D-173).

### 7.5.3 Import cycles

Modules must not import each other cyclically, directly or through other modules. An import cycle is a compile error that lists the modules on the cycle (D-145). This keeps the evaluation order of compile-time constants ([§4.5](04-declarations.md#45-constants)), REPL reloading and incremental builds simple.

An unused import is a warning (D-080).

## 7.6 Prelude

Every module implicitly imports the prelude. It contains only types, constructors and one interface, and no functions (D-112):

| Kind                | Names                                                      |
| ------------------- | ---------------------------------------------------------- |
| Types               | `Int Float String Bool Nil List Result Option Order Never` |
| Sized numeric types | `I8 I16 I32 I64 U8 U16 U32 U64 F32`                        |
| Constructors        | `True False Nil Ok Err Some None Less Equal Greater`       |
| Interfaces          | `Show`                                                     |

`Nil` is both the type and its only value ([§3.6](03-types.md#36-nil)). `Never` is in the prelude because it is a writable return type (D-131, D-161).

Functions on these types come from their modules, which must be imported: `std::int`, `std::float`, `std::string`, `std::list`, `std::result`, `std::option` and so on. The operator interfaces live in `std::ops`, not in the prelude (D-169).

A module may declare, or explicitly import, a type or constructor with the same name as a prelude item, for example a type called `Order` in a shop application. The local declaration or import hides the prelude item of the same name in that module, and doing so is a warning (D-146). Hiding is per namespace ([§7.7](#77-namespaces-and-name-resolution)) (D-212): a type hides only a prelude type, and a constructor only a prelude constructor. A constructor named `String` or `Bool`, as in `Value { String(String), Bool(Bool) }`, hides nothing and does not warn, because `String` and `Bool` are prelude types, not prelude constructors. A constructor named `Some` or `Ok` does hide a prelude constructor and warns. This is not shadowing in the sense of [§5.2.3](05-expressions.md#523-no-shadowing), which is about value bindings.

## 7.7 Namespaces and name resolution

Each module has two namespaces:

- the **type namespace**, holding types, aliases, interfaces and type parameters,
- the **value namespace**, holding functions, constants, constructors, imported module names and local bindings.

A name may be in both, as with a single-variant type and its constructor (D-082, D-111).

An unqualified name in an expression resolves to, in order:

1. a local binding or parameter,
2. a top-level item of the current module,
3. an item or module name brought in by an import,
4. a prelude constructor.

Because the no-shadowing rule forbids a name from being bound twice in overlapping scopes, the first three sources never disagree: a conflict between them is reported where the second name is introduced. Only the prelude can be hidden (D-146).

An unqualified name in a type resolves to a type parameter in scope, a type of the current module, an imported type, or a prelude type, in that order.

A qualified name `m::x` resolves `m` to an imported module and `x` to an item of it that is visible to the current module ([§7.3](#73-visibility)): a package-visible or `pub` item if `m` is in the same package, only a `pub` item otherwise, and never a `priv` item. A qualified constructor or type uses the same form: `shape::Circle`, `map::Map<K, V>`.
