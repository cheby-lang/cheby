# 13. Tooling

Cheby ships as one `cheby` binary that contains the compiler, runtime, REPL, test runner, formatter, language server, documentation generator and package manager. All of them are part of v1 (D-031). This chapter specifies the parts of the tooling that affect how programs are written, built and shared. Command-line details that do not affect programs (flag spellings, output formats) are informative.

## 13.1 The cheby command

| Command                                    | Purpose                                                       | Decisions           |
| ------------------------------------------ | ------------------------------------------------------------- | ------------------- |
| `cheby new <name>`                         | create a package                                              | D-097               |
| `cheby build`                              | compile the package, for `native` (AOT) or `js`               | D-006, D-007        |
| `cheby run [module_path]`                  | compile with the JIT and run a module's `main`                | D-007, D-019, D-137 |
| `cheby repl`                               | interactive session ([§13.5](#135-repl))                      | D-008, D-043, D-098 |
| `cheby test`                               | run `test` blocks ([§13.6](#136-tests))                       | D-055               |
| `cheby fmt`                                | format source files ([§13.7](#137-formatter))                 | D-079               |
| `cheby lsp`                                | language server over stdio ([§13.11](#1311-language-server))  | D-031               |
| `cheby doc`                                | generate HTML documentation ([§13.10](#1310-documentation))   | D-078               |
| `cheby add`, `cheby update`, `cheby fetch` | manage dependencies ([§13.4](#134-dependencies-and-lockfile)) | D-044, D-097        |

`cheby build --target native` produces an executable. Object files from Cranelift are linked with the system linker in early versions (`cc` or `link.exe`), and with a linker bundled in the `cheby` binary before 1.0 (D-088). `cheby build --target js` produces a directory of ES modules (D-074).

`cheby run` runs the `main` of the module named by its module path argument, for example `cheby run my_app::tools::migrate`. Without an argument it runs the root module's `main` (D-137). `main` may have any visibility, including `priv` (D-163).

`cheby run` and `cheby repl` produce no output files: they run code in memory with the JIT (D-007, D-019). Like every command, they may read and write the build cache ([§13.12.4](#13124-build-cache-and-standard-library)) (D-254, D-255).

## 13.2 Project layout

```
my_app/
  cheby.toml      manifest (D-097)
  cheby.lock      lockfile (D-097)
  src/            modules (D-162)
    main.cheby    root module, path my_app (D-142)
    web.cheby     my_app::web
    web/
      router.cheby  my_app::web::router
  build/          output of cheby build, not checked in
```

Module paths follow from file paths ([§7.2](07-modules-and-packages.md#72-modules-and-files)). The root module, whose path is just the package name, is `src/main.cheby`, and it cannot also be imported as `my_app::main` (D-142). There are no `internal` directories. Visibility is controlled only by `pub` and `priv` (D-162). Tests live inside the modules they test, so there is no separate test directory (D-055).

## 13.3 Manifest

`cheby.toml` describes a package (D-097):

```toml
name = "my_app"
version = "0.3.1"
description = "An example application"
licences = ["MIT", "Apache-2.0"]

targets = ["native", "js"]

[dependencies]
json = { git = "https://github.com/example/cheby-json", tag = "v1.2.0" }
json_next = { git = "https://github.com/example/cheby-json", tag = "v2.0.0" }
other_json = { git = "https://github.com/other/cheby-json", tag = "v1.0.0" }
http = { git = "https://github.com/example/cheby-http", rev = "4f2a9c1" }

[const-eval]
time-limit = "10s"
memory-limit = "1GiB"
```

Here `json` and `json_next` are majors 1 and 2 of one repository, which are two different packages, and `other_json` is a different repository and so a different package, whatever its own name is ([§7.1](07-modules-and-packages.md#71-packages), D-238). The spelling of the `[const-eval]` table is illustrative.

- `name` is the package name, a `LOWER` identifier ([§7.1](07-modules-and-packages.md#71-packages)). It must not be `std`.
- `version` is a semantic version (D-097).
- `targets` is optional and lists the targets the package supports, `"native"` and/or `"js"`. It defaults to both. Cross-target checks ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) cover only the declared targets (D-190).
- Each dependency is keyed by the name used in import paths: the key is the first segment of the dependency's module paths in this package, as in `import json_next::decode` (D-238). In v1 a dependency is a git URL plus a `tag` or a commit `rev` (D-044, D-097).
- A key must be a `LOWER` identifier, must not be `std` and must differ from the package's own `name`. Keys are local to each manifest: two packages may use different keys for the same dependency, and the same key for different ones. Renaming a dependency is just choosing its key, so the dependency's own `name` never has to be unique in a build (D-192, D-238).
- The dependency table is designed so that a later registry form, such as `json = "1.2"`, can be added without changing existing entries (D-044).
- The optional compile-time evaluation limits ([§13.9](#139-compile-time-evaluation)) can be raised or lowered for the package (D-195).

## 13.4 Dependencies and lockfile

Dependencies are fetched directly from their git URLs. There is no central registry in v1 (D-044).

- `cheby.lock` records, for every package identity in the build ([§7.1](07-modules-and-packages.md#71-packages)), direct or indirect, its URL, its major version, the chosen version, the exact commit and a checksum of its contents (D-044, D-097, D-238). Builds use the lockfile and fail if a checksum does not match.
- `cheby add` adds a dependency to the manifest and lockfile. `cheby update` re-resolves tags and refreshes the lockfile. `cheby fetch` downloads what the lockfile names.
- A package identity is a git URL together with a major version, so different majors of one URL are different packages and may all be in one build under different keys ([§7.1](07-modules-and-packages.md#71-packages), D-238).
- The major version is the first non-zero part of the version, as in Cargo (D-239). `1.4.2` and `1.9.0` share major `1`, `0.1.3` and `0.1.8` share major `0.1`, while `0.1` and `0.2` are different majors, and every `0.0.z` release is a major of its own. A version whose parts are all zero, `0.0.0`, is its own major too.
- A build contains exactly one version of each package identity. Versions are chosen by minimal version selection, as in Go, so requirements within one major never conflict (D-192, D-238).
- The version of a dependency is the `version` in its `cheby.toml` at the fetched tag or commit, and it is authoritative. A tag should spell the same version, such as `v1.2.0`. For a `rev`, the version at that commit is used for version selection. Two required commits of one identity that claim the same version, or a `rev` chosen by version selection while another requirement pins a different commit, are an error reported by `cheby update`, as is a `rev` whose `version` is not in the requested major (D-238).
- Applications should commit `cheby.lock`.
- The standard library is not a dependency. It is part of the toolchain and versioned with it (D-058).

A central registry may be added later. Its introduction must not break git dependencies (D-044).

## 13.5 REPL

`cheby repl` is an interactive session backed by the Cranelift JIT (D-008, D-019).

### 13.5.1 Inputs

The REPL accepts (D-098):

- expressions, whose value is printed with its type,
- `let` and `let assert` statements, whose bindings stay available for the rest of the session,
- declarations: `fn`, `type`, type aliases, `interface` and `const`,
- `import` statements.

Top-level `fn` declarations in the REPL need full signatures, as in a module (D-011). Declarations are type-checked one by one, which the signature rule makes possible (D-011).

Inside a package directory, the REPL loads all of the package's modules and dependencies, and they can be imported (D-098).

### 13.5.2 Redefinition

In a module, names cannot be redefined. The REPL is the one exception, because it is a session rather than a scope:

- **Functions** are late-bound (D-043). Redefining a function replaces it for every existing caller, through a JIT indirection table. If the signature changes, the REPL re-checks every definition that depends on it and reports each one that no longer type-checks.
- **Types** can be redefined. Existing values keep the old type, which is now a different type from the new one, and the REPL marks it as stale when printing (D-043).
- **`let` bindings** at the REPL prompt may be rebound with the same name, an explicit REPL exception to the no-shadowing rule, since forbidding it would make the REPL unusable (D-193). The no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)) still applies inside every expression and declaration, but not between prompt entries.

### 13.5.3 Commands

| Command          | Meaning                                                                              |
| ---------------- | ------------------------------------------------------------------------------------ |
| `:type expr`     | show the type of an expression without evaluating it (D-098)                         |
| `:reload`        | recompile changed modules of the package (D-098)                                     |
| `:module path`   | enter a module, so its `priv` items are visible and its imports are in scope (D-098) |
| `:help`, `:quit` | informative                                                                          |

Entering a module with `:module` makes the REPL behave as if the input were written at the end of that module: its `priv` items are accessible, package-visible items of its package are accessible (D-162), and the package boundary for `exposed` is that module's package (D-098).

### 13.5.4 Fibers in the REPL

Each REPL entry runs in its own root scope, as if it were the body of `main` ([§10.10](10-concurrency.md#1010-program-entry-and-exit)). A panic in an entry is printed and does not end the session.

Detached fibers ([§10.4](10-concurrency.md#104-detached-fibers)) started from the REPL keep running after their entry finishes, until the session ends or `:reload` is used, so servers and background work can be tried interactively (D-194).

## 13.6 Tests

`cheby test` compiles the package with its `test` blocks and runs them (D-055):

- Every test runs in its own fiber, in its own scope. A test body has type `Nil` or `Result<Nil, E>`. A test passes if its body finishes without panicking and, for a `Result` body, returns `Ok` ([§4.7](04-declarations.md#47-tests)) (D-140).
- Code examples in doc comments are compiled and run as tests ([§13.10](#1310-documentation)) (D-197).
- Tests may run in parallel. They cannot interfere through shared state, because there is none (D-004).
- Test output reports each failure with its panic message, which for `assert` includes both sides of a failing comparison (D-096).
- Tests can be filtered by name and by module.
- Test blocks are not compiled into any other build (D-055). They and doc examples are compiled as separate units per module, outside the module's interface artifact and normal cached code, so editing a test never rebuilds other modules ([§13.12.3](#13123-interface-artifacts-and-recompilation)) (D-265).

Test blocks are type-checked by `cheby build` as well, so a broken test is caught even when tests are not run.

## 13.7 Formatter

`cheby fmt` rewrites source files into the one canonical style. It has no configuration options (D-079).

- The output does not depend on the input's layout beyond what the style keeps (such as blank lines between statements and comments).
- Formatting never changes the meaning of a program.
- `cheby fmt --check` exits non-zero if any file would change, for CI.
- The formatter is part of the compatibility promise: after 1.0, rule changes to the canonical style are allowed, but `cheby fmt` must be able to migrate code when a language change needs it (D-075).

The style itself is defined by the formatter, not by this specification. Examples in this specification use it: two-space indentation, one statement per line, trailing commas in multi-line lists, and multi-line pipelines with `|>` at the start of each line.

## 13.8 Diagnostics and warnings

Compile errors stop compilation. Warnings do not, unless `--deny-warnings` is passed, which is intended for CI (D-080).

The following are warnings (D-080 and the cited decisions):

| Warning                                                                 | Decision     |
| ----------------------------------------------------------------------- | ------------ |
| unused local binding, parameter or import                               | D-080        |
| unused `priv` item                                                      | D-080, D-152 |
| unused package-visible item, when nothing in the package uses it        | D-080, D-152 |
| discarding a `Result` in statement position (silenced with `let _ = …`) | D-153        |
| a local declaration or import that hides a prelude name                 | D-146        |
| `let assert` with an irrefutable pattern                                | D-159        |
| `todo` in compiled code                                                 | D-068        |
| unreachable `case` arm                                                  | D-022        |
| `==` on a type statically known to contain a function or handle type    | D-070, D-233 |
| use of a `@deprecated` item                                             | D-200        |
| a written tuple type with a tuple element or with four or more elements | D-243        |

Unused `pub` items are never reported (D-152). Warnings that need the whole package, such as unused package-visible items, are computed in a separate pass that does not delay `cheby run` (D-266). Bindings whose names start with `_` are not reported as unused ([§2.3](02-lexical-structure.md#23-identifiers)).

The compiler should report errors with the source span, a short explanation, and a suggested fix where one exists, for example a new name for a shadowing binding (D-062) or `{x:?}` for a type without `Show` (D-086).

## 13.9 Compile-time evaluation

Constant initializers ([§4.5](04-declarations.md#45-constants)) are run by the compiler with the JIT (D-090). Because the language server compiles code continuously, evaluation must be bounded (ADR-0027):

- Each constant's evaluation is limited to 10 seconds and 1 GiB by default. The limits can be adjusted for a package in `cheby.toml` ([§13.3](#133-manifest)). Exceeding a limit is a compile error on that constant (D-195).
- Evaluation is deterministic: it must not depend on the time, the environment, the file system or randomness, and any attempt to use them is an error (D-090).
- The hash seed used during evaluation is not random. It is derived from the constant's identity (package, module path and name) and the content hash of what it depends on, so a constant that depends on `Map` or `Set` iteration order gets the same value on every build of the same inputs, but may get a different one when the code it depends on changes (D-269, [§13.12.6](#13126-deterministic-builds)).
- Results are cached, so a constant is re-evaluated only when something it depends on changes.

Hash-based collections (`Map`, `Set`) may appear in constants, even though the hash seed is random per process (D-093) and the constant is built in the compiler's process. The runtime rebuilds or re-seeds such maps and sets at program start, so their iteration order is still randomized per run (D-196).

## 13.10 Documentation

`cheby doc` generates HTML documentation for a package from its `pub` items and their `///` and `//!` doc comments (D-078):

- Doc comments are Markdown.
- The pages show each item's signature, the variants and fields of `exposed` types, and the interfaces each type satisfies structurally ([chapter 8](08-interfaces.md)).
- Code blocks in doc comments are highlighted as Cheby.
- Code examples in doc comments are compiled and run as tests by `cheby test`, so they cannot go stale (D-197).

## 13.11 Language server

`cheby lsp` implements the Language Server Protocol over standard input and output (D-031). It provides at least diagnostics, hover with types, go to definition, find references, rename, completion and formatting through `cheby fmt`.

The language server evaluates constants under the limits of [§13.9](#139-compile-time-evaluation), and never runs `main` or tests on its own.

### 13.11.1 Generating JSON code

Cheby has no derive (D-057, ADR-0022). Instead, the language server offers a code action on a type declaration that writes a `to_json` function and a decoder for the type against `std::json` (D-210, D-215). The generated functions are ordinary source code in the type's module. They are meant to be read and edited by hand, and nothing regenerates or checks them afterwards: there is no CLI generator and no staleness check (D-215).

The generated code follows these conventions:

- Each field becomes a JSON key with the field's name unchanged, such as `first_name` (D-225). Renaming a key is a hand edit.
- A variant without fields is encoded as a JSON string of its name in snake_case, such as `"paperback"`.
- A variant with named fields is encoded as an internally tagged object: a `"type"` key with the variant's name in snake_case, followed by its fields, such as `{"type":"circle","radius":1.0}` (D-224). If a field is itself called `type`, the generated code must be edited by hand.
- A field whose type is declared in another module is encoded and decoded through that module's own `to_json` and decoder.
- The encoding function has the signature that `std::json`'s encoding interface requires, and the visibility of the type, so that the type satisfies the interface ([§8.2](08-interfaces.md#82-satisfaction)).

The code action is part of the language server, not of the language. Its exact output is informative, and it may improve between toolchain versions without affecting existing code.

## 13.12 Build model

This section describes how the toolchain compiles packages. [§13.12.1](#13121-the-locality-rule) and [§13.12.6](#13126-deterministic-builds) are normative. The rest is informative and describes the reference implementation.

### 13.12.1 The locality rule

Type-checking a module needs only the interfaces of the modules it depends on, never their function bodies (D-246, ADR-0043). An interface consists of the module's items as seen from outside: function signatures, types, interfaces, aliases and the declared types of constants. Every language feature must keep this rule. The current language meets it because top-level signatures are required (D-011), inference is local to one top-level body (D-234), there is no metaprogramming (D-057) and imports are acyclic (D-145).

### 13.12.2 Units of compilation

The module, one source file, is the unit of compilation, caching and parallelism (D-248). Modules are compiled in the order of the import graph, and independent modules in parallel. Within a module, type-checking and code generation run per function, in parallel, apart from a few per-module steps such as borrowing inference and per-type layout tables (D-263).

### 13.12.3 Interface artifacts and recompilation

Compiling a module produces an **interface artifact** with its signatures, types, interfaces, constant types and, in release builds, small inlinable bodies, together with its compiled code (D-249, ADR-0044). A module is recompiled when its source changes or when the hash of an interface it uses changes. Editing a function body leaves the interface unchanged, so only that module is recompiled.

Nothing that depends on function bodies is part of an interface in debug builds:

- Borrowing is inferred only within a module, and calls across modules use a fixed owned convention (D-250, ADR-0045).
- Constant values are linked in as data. Only their declared types are in the interface. Release builds may also export the values of small scalar constants (D-257).
- Whether a function suspends on JS is computed at link time from small per-module summaries, not stored in interfaces ([§12.3.1](12-targets-and-ffi.md#1231-fibers-on-js)) (D-258, ADR-0048).

When a module needs a fact about a type from a module it does not import, for example whether a `geometry::Point` received through another module's signature satisfies `Show`, it reads that module's interface directly, and the dependency is recorded for recompilation (D-260). Interface artifacts do not copy facts about other modules' types.

Test blocks and doc examples are compiled as separate units, outside interface artifacts and normal cached code (D-265).

### 13.12.4 Build cache and standard library

Compiled artifacts are stored in one cache per machine, shared by all projects and addressed by the content hash of their inputs, the compiler version and the build flags (D-255). The toolchain ships `std` precompiled, with its interface artifacts, native code for every supported native platform and its JS output (D-256).

`cheby run` loads cached code for every unchanged module and compiles changed modules lazily with the JIT, each function on its first call, through the same indirection table the REPL uses for redefinition (D-254, ADR-0047, [§13.5.2](#1352-redefinition)).

### 13.12.5 Build profiles

There are two build profiles (D-251, D-252, D-259, ADR-0046):

| Profile         | Optimization               | Across modules                                   | Equality, hashing, debug printing                    | Debug info                               |
| --------------- | -------------------------- | ------------------------------------------------ | ---------------------------------------------------- | ---------------------------------------- |
| debug (default) | Cranelift optimization off | no specialization, no inlining                   | one runtime routine driven by per-type layout tables | line tables only, full debug info opt-in |
| release         | Cranelift optimization on  | specialization and inlining within a size budget | may be generated per type                            | line tables only, full debug info opt-in |

Both profiles insert and optimize reference counting and unwind cleanup on the mid-level IR, before Cranelift, with cleanup blocks shared between calls (D-262). Whole-program, LTO-style optimization is not used. The flag that selects the release profile is informative.

### 13.12.6 Deterministic builds

The toolchain must be deterministic: the same inputs, compiler version and flags produce bit-identical artifacts, including interface artifacts, object code and JS output (D-264). Compile-time evaluation of constants uses a hash seed derived from the constant's inputs ([§13.9](#139-compile-time-evaluation)) (D-269). Output must not depend on the order of file-system listings, on absolute paths, on the time or on the number of threads used.

### 13.12.7 Process model

Every `cheby` command runs as its own process, and the build cache is the only state shared between commands. The language server keeps its own state in memory. A persistent build server may be added later, but only as an accelerator whose results are bit-identical to those of a fresh process. Correctness never depends on it (D-268, ADR-0049).

### 13.12.8 Compile-speed budgets

The reference compiler tracks these budgets in CI from its first milestone (D-247, D-267):

- A clean debug build checks and generates code for at least 100,000 lines per second per core, and scales with the number of cores.
- After an edit to one function body in a project of one million lines, `cheby run` starts running the program in under 500 ms.
- A clean release build takes at most three times as long as a clean debug build of the same project.

The budgets are estimates and will be revised against measurements.

Unused imports remain warnings ([§13.8](#138-diagnostics-and-warnings)) (D-253), and AOT builds use the system linker, then a bundled lld before 1.0 (D-088, D-261).

## 13.13 Build order (informative)

All tools ship in v1, but the reference implementation is built in this order (D-099):

1. parser, type checker, IR, JIT and REPL for the single-fiber core,
2. Perceus and the runtime, including the M:N scheduler and channels,
3. AOT object output and linking,
4. the JS backend,
5. formatter, language server, documentation and package manager, grown alongside the others.
