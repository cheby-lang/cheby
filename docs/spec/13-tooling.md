# 13. Tooling

Cheby ships as one `cheby` binary that contains the compiler, runtime, REPL, test runner, formatter, language server, documentation generator and package manager. All of them are part of v1 (D-031). This chapter specifies the parts of the tooling that affect how programs are written, built and shared. Command-line details that do not affect programs (flag spellings, output formats) are informative.

## 13.1 The cheby command

| Command                                    | Purpose                                                            | Decisions           |
| ------------------------------------------ | ------------------------------------------------------------------ | ------------------- |
| `cheby new <name>`                         | create a package                                                   | D-097               |
| `cheby check`                              | type-check the package without producing artifacts                 | D-282, D-459, D-550 |
| `cheby build`                              | compile the package, for `native` (AOT) or `js`                    | D-006, D-007        |
| `cheby run [module_path]`                  | compile with the JIT and run a module's `main`                     | D-007, D-019, D-137 |
| `cheby repl`                               | interactive session ([§13.5](#135-repl))                           | D-008, D-043, D-098 |
| `cheby test`                               | run `test` blocks ([§13.6](#136-tests))                            | D-055, D-541        |
| `cheby fmt`                                | format source files ([§13.7](#137-formatter))                      | D-079               |
| `cheby lsp`                                | language server over stdio ([§13.11](#1311-language-server))       | D-031               |
| `cheby doc`                                | generate HTML documentation ([§13.10](#1310-documentation))        | D-078               |
| `cheby add`, `cheby update`, `cheby fetch` | manage dependencies ([§13.4](#134-dependencies-and-lockfile))      | D-044, D-097        |
| `cheby explain <code>`                     | explain a diagnostic code ([§13.8](#138-diagnostics-and-warnings)) | D-420               |

`cheby build --target native` produces an executable. Object files from Cranelift are linked with the system linker in early versions (`cc` or `link.exe`), and with a linker bundled in the `cheby` binary before 1.0 (D-088). `cheby build --target js` produces a directory of ES modules (D-074).

`cheby check` parses and type-checks the package, including `test` blocks and the cross-target checks of [§12.6](12-targets-and-ffi.md#126-target-specific-code), and reports diagnostics like `cheby build`. Like `cheby build`, it evaluates constants ([§13.9](#139-compile-time-evaluation)) and reports their panics and exceeded limits (D-459). It evaluates every constant of the local package, and none when an earlier check reported an error (D-534). Until the toolchain can evaluate constants, it checks them without evaluating them ([§13.8](#138-diagnostics-and-warnings)) (D-478). It writes no output files, but may use the build cache ([§13.12.4](#13124-build-cache-and-standard-library)) (D-282).

`cheby check --target native` or `cheby check --target js` checks the code as it runs on that target only, so the cross-target check of [§12.6](12-targets-and-ffi.md#126-target-specific-code) reports only uses that would fail there. A target that the manifest's `targets` leaves out is a tool error, such as "the package does not target `js`" (D-550).

`cheby explain` prints the long explanation of a diagnostic code such as `E0012`, with examples and fixes ([§13.8](#138-diagnostics-and-warnings)) (D-420).

Every command takes a global `--color=auto|always|never` flag, which says whether diagnostics and other errors are colored ([§13.8](#138-diagnostics-and-warnings)) (D-467).

`cheby check`, `cheby run`, `cheby test` and `cheby repl` accept `--deny-warnings`, which turns warnings into errors ([§13.8](#138-diagnostics-and-warnings)) (D-080, D-549).

`-j N` or `--jobs N` sets the number of worker threads for the build graph and code generation. The default is the available parallelism. Output never depends on it ([§13.12.6](#13126-deterministic-builds)) (D-264, D-551).

`cheby run` runs the `main` of the module named by its module path argument, for example `cheby run my_app::tools::migrate`. Without an argument it runs the root module's `main` (D-137). `main` may have any visibility, including `priv` (D-163). A module path that is malformed, names no module of the package, or names a module without `main` is a tool error (D-464). Compile errors are reported first, and then nothing runs. Warnings are printed to standard error before the program starts (D-464).

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

Here `json` and `json_next` are majors 1 and 2 of one repository, which are two different packages, and `other_json` is a different repository and so a different package, whatever its own name is ([§7.1](07-modules-and-packages.md#71-packages), D-238).

- `name` is the package name, a `LOWER` identifier ([§7.1](07-modules-and-packages.md#71-packages)). It must not be `std`.
- `version` is a semantic version (D-097).
- `description` is a string, and `licences` is a list of strings.
- `targets` is optional and lists the targets the package supports, `"native"` and/or `"js"`. It defaults to both. When present it must not be empty or name a target twice. Cross-target checks ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) cover only the declared targets (D-190, D-444).
- Each dependency is keyed by the name used in import paths: the key is the first segment of the dependency's module paths in this package, as in `import json_next::decode` (D-238). In v1 a dependency is a git URL plus a `tag` or a commit `rev` (D-044, D-097).
- A key must be a `LOWER` identifier, must not be `std` and must differ from the package's own `name`. Keys are local to each manifest: two packages may use different keys for the same dependency, and the same key for different ones. Renaming a dependency is just choosing its key, so the dependency's own `name` never has to be unique in a build (D-192, D-238).
- The dependency table is designed so that a later registry form, such as `json = "1.2"`, can be added without changing existing entries (D-044).
- The optional `[const-eval]` table raises or lowers the compile-time evaluation limits of [§13.9](#139-compile-time-evaluation) for the package (D-195). `time-limit` is an integer followed by `ms` or `s`, such as `"10s"`, and `memory-limit` is an integer followed by `KiB`, `MiB` or `GiB`, such as `"1GiB"` (D-536).
- The top-level keys are exactly `name`, `version`, `description`, `licences`, `targets`, `[dependencies]` and `[const-eval]`, a dependency has only `git`, `tag` and `rev`, and `[const-eval]` has only `time-limit` and `memory-limit`. Any other key is a compile error, so a misspelled key such as `verison` is never ignored silently (D-444, D-536). A value of the wrong form in `[const-eval]` is a compile error too (D-536).

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
- A package's URL has a canonical form: without the scheme or a user name, with the host in lower case, without a trailing `.git` or `/`, and with an scp-style `git@host:path` written as `host/path`, as in `github.com/example/cheby-json`. Two URLs are one URL exactly when their canonical forms are equal ([§7.1](07-modules-and-packages.md#71-packages)), and a package identity is displayed, for example in diagnostics, by its canonical URL (D-468).

A central registry may be added later. Its introduction must not break git dependencies (D-044).

## 13.5 REPL

`cheby repl` is an interactive session backed by the Cranelift JIT (D-008, D-019).

### 13.5.1 Inputs

The REPL accepts (D-098):

- expressions, whose value is printed with its type,
- `let` and `let assert` statements, which are evaluated once and whose bindings stay available as session values for the rest of the session (D-547),
- declarations: `fn`, `type`, type aliases, `interface` and `const`,
- `import` statements.

Top-level `fn` declarations in the REPL need full signatures, as in a module (D-011). Declarations are type-checked one by one, which the signature rule makes possible (D-011).

Inside a package directory, the REPL loads all of the package's modules and dependencies, and they can be imported (D-098).

### 13.5.2 Redefinition

In a module, names cannot be redefined. The REPL is the one exception, because it is a session rather than a scope:

- **Functions** are late-bound (D-043). Redefining a function replaces it for every existing caller, through a JIT indirection table. If the signature changes, the REPL re-checks every definition that depends on it and reports each one that no longer type-checks. Until a caller is checked and compiled again, it keeps calling the old function, never code of another signature (D-456).
- **Foreign functions** follow the same rule. The rule that every `@external(native, …)` naming one symbol gives it the same C signature ([§12.5](12-targets-and-ffi.md#125-foreign-functions)) applies to the session's current definitions only. Replaced definitions and entries that failed to compile do not count, so a redefinition may change a foreign symbol's C signature, while two live definitions with different C signatures are still an error (D-475).
- **Types** can be redefined. Existing values keep the old type, which is now a different type from the new one, and the REPL marks it as stale when printing (D-043).
- **Interfaces** are redefined like types. The old interface and its implementations stay with the old values, and the new interface is a different interface (D-547).
- **Constants** are redefined like functions without parameters. With the same type, readers see the new value. With another type, the definitions that depend on the constant are checked again (D-547).
- **Type aliases** are replaced for later entries. Definitions already checked keep what they were checked against (D-547).
- **`let` bindings** at the REPL prompt may be rebound with the same name, an explicit REPL exception to the no-shadowing rule, since forbidding it would make the REPL unusable (D-193). The no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)) still applies inside every expression and declaration, but not between prompt entries.

### 13.5.3 Commands

| Command          | Meaning                                                                              |
| ---------------- | ------------------------------------------------------------------------------------ |
| `:type expr`     | show the type of an expression without evaluating it (D-098)                         |
| `:reload`        | recompile changed modules of the package (D-098)                                     |
| `:module path`   | enter a module, so its `priv` items are visible and its imports are in scope (D-098) |
| `:help`, `:quit` | informative                                                                          |

`:type expr` prints `<expr text> : <type>`, with the type written as in diagnostics (D-546).

Entering a module with `:module` makes the REPL behave as if the input were written at the end of that module: its `priv` items are accessible, package-visible items of its package are accessible (D-162), and the package boundary for `exposed` is that module's package (D-098).

### 13.5.4 Fibers in the REPL

Each REPL entry runs in its own root scope, as if it were the body of `main` ([§10.10](10-concurrency.md#1010-program-entry-and-exit)). A panic in an entry is printed and does not end the session.

Detached fibers ([§10.4](10-concurrency.md#104-detached-fibers)) started from the REPL keep running after their entry finishes, until the session ends or `:reload` is used, so servers and background work can be tried interactively (D-194).

### 13.5.5 Prompt, output and standard streams

The REPL shows its session as follows (D-546):

- The prompt is `> `, and `. ` while a bracket or a block is open.
- An expression's value prints as `<debug text> : <type>`, with the type written as in diagnostics ([§13.8](#138-diagnostics-and-warnings)). A `Nil` value prints nothing. A value whose type a redefinition replaced prints its type followed by ` (stale)`.
- Declarations and imports print nothing when they succeed.
- A location in an entry is shown as `<repl:N>:line:col` in diagnostics and panic reports, with entries counted from 1.
- On a terminal the REPL has line editing. When standard input is not a terminal, there is no line editing, and each input line is echoed after its prompt, so the output of a session reads as a transcript.
- There is no leak check ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-458).

_Example:_

```
> import std::list
> list::length([1, 2, 3])
3 : Int
> :type list::reverse([1, 2])
list::reverse([1, 2]) : List<Int>
```

The REPL reads its input lines through the same buffered reader of standard input as `io::read_line`. An entry that reads standard input therefore consumes the next lines of a piped session in order, and those lines are not echoed as input. On a terminal, the line editor reads the prompt lines and an entry reads the terminal directly. The REPL flushes standard output before it writes a diagnostic or a panic report to standard error, so earlier output stays ahead of the report (D-548).

Before it reads an input line, the REPL flushes standard output, and a failed flush ends it with exit status 1 (D-306). It removes the line's final `\n` or `\r\n` (D-305) and replaces invalid UTF-8 with U+FFFD, as `env::args` does (D-311), because it has no `Result` to report it through. A standard input that cannot be read ends the session as the end of input does (D-575).

_Note:_ the reference implementation tests the REPL with transcripts in `tests/repl/`, in which lines that start with `> ` or `. ` are input and every other line is expected output, with standard output and standard error merged in the order of writes (D-546, D-548).

## 13.6 Tests

`cheby test` compiles the package with its `test` blocks and runs them (D-055):

- Every test runs in its own fiber, in its own scope. A test body has type `Nil` or `Result<Nil, E>`. A test passes if its body finishes without panicking and, for a `Result` body, returns `Ok` ([§4.7](04-declarations.md#47-tests)) (D-140).
- Code examples in doc comments are compiled and run as tests ([§13.10](#1310-documentation)) (D-197).
- Tests may run in parallel. They cannot interfere through shared state, because there is none (D-004).
- Test output reports each failure with its panic message, which for `assert` includes both sides of a failing comparison (D-096). A test whose body returns `Err(e)` shows `e` as `main` prints it ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-529).
- Tests can be filtered by name and by module: `cheby test [FILTER] [--module PATH]`. A test's full name is `<module path> "<test name>"`. `FILTER` selects the tests whose full name contains it, and `--module` selects the tests of that module and its submodules (D-541). `PATH` must be a well-formed module path that is a module of the package or the prefix of one. Otherwise the command stops with status 1 before any test is compiled, with the tool error "`x` is not a module path" or "there is no module `x` in this package", as `cheby run` does ([§13.1](#131-the-cheby-command)) (D-464, D-597).
- The native test command selects only the tests that exist on `native`: a test's targets, including its module's `@!target`, and for a local test the package's `targets` too, must contain `native`. A test that exists only on `js` is neither run nor counted as filtered out ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) (D-597).
- Test blocks are not compiled into any other build (D-055). They and doc examples are compiled as separate units per module, outside the module's interface artifact and normal cached code, so editing a test never rebuilds other modules ([§13.12.3](#13123-interface-artifacts-and-recompilation)) (D-265).

Test blocks are type-checked by `cheby build` as well, so a broken test is caught even when tests are not run.

`cheby test` writes to standard output one line per test, `test <module path> "<name>" ... ok` or `... FAILED`. For each failure, a block follows that starts with `---- <module path> "<name>" ----` and holds the panic report or the `Err` text. A summary ends the output, such as `test result: ok. 3 passed; 0 failed; 1 filtered out`, with `FAILED` in place of `ok` when a test failed. The program's own output is not captured and appears as it is written. A test's line is written when the test finishes, so the test's own output comes before its line. The failure blocks follow all the lines, each preceded by an empty line. The summary is preceded by an empty line when a test line was written, and when no test is selected it is the only output (D-598). A test's panic report appears only in its failure block, never on standard error, and standard output is flushed before it is taken (D-595). The exit status is 0 when every selected test passes, also when none is selected, and 1 when one fails. A compile error stops the command with status 1 before any test runs (D-541).

_Note:_ in M1 the reference implementation runs tests one after another, by module path and then in source order, each on a 64 MiB stack like `main` (D-596). A toolchain with debug assertions counts the live objects before and after each passing test, and a difference fails that test with the message of the leak check ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-458, D-541).

The standard library's `test` blocks sit next to the code they test, in its source files. A build of a user package neither checks them nor compiles them, and the warnings of [§13.8](#138-diagnostics-and-warnings) ignore them. They are checked and run when the standard library itself is tested (D-479). `cheby` has no flag for this. _Note:_ the reference implementation tests the standard library with `cargo xtask test-std`, one module at a time, so a module that does not build does not hide the results of the others (D-542). It checks the standard library through an in-memory package `std_self_test` whose root module imports every shipped std module, and while its tests run `env::program_name()` returns `std_self_test` (D-599).

## 13.7 Formatter

`cheby fmt` rewrites source files into the one canonical style. It has no configuration options (D-079).

- The output does not depend on the input's layout beyond what the style keeps (such as blank lines between statements and comments).
- Formatting never changes the meaning of a program.
- `cheby fmt --check` exits non-zero if any file would change, for CI.
- The formatter is part of the compatibility promise: after 1.0, rule changes to the canonical style are allowed, but `cheby fmt` must be able to migrate code when a language change needs it (D-075).

The style itself is defined by the formatter, not by this specification. Examples in this specification use it: two-space indentation, one statement per line, trailing commas in multi-line lists, and multi-line pipelines with `|>` at the start of each line.

## 13.8 Diagnostics and warnings

Compile errors stop compilation. Warnings do not, unless `--deny-warnings` is passed, which is intended for CI (D-080). With it, each warning is rendered as an error with its own code, as in `error[W0002]: unused binding …`, and the first one has the note "warnings are denied by `--deny-warnings`". The command then fails with exit status 1, and `cheby run` and `cheby test` run nothing (D-549). An error found while reading the manifest, discovering modules, lexing, parsing or checking syntax stops the later checks, such as name resolution and type checking, so their diagnostics never come from a broken file (D-452).

The following are warnings (D-080 and the cited decisions):

| Warning                                                                 | Decision            |
| ----------------------------------------------------------------------- | ------------------- |
| unused local binding, parameter or import                               | D-080, D-515        |
| unused `priv` item                                                      | D-080, D-152, D-516 |
| unused package-visible item, when nothing in the package uses it        | D-080, D-152, D-516 |
| discarding a `Result` in statement position (silenced with `let _ = …`) | D-153               |
| a local declaration or import that hides a prelude name                 | D-146, D-487        |
| `let assert` with an irrefutable pattern                                | D-159, D-509        |
| `todo` in compiled code                                                 | D-068               |
| unreachable `case` arm                                                  | D-022               |
| `==` on a type statically known to contain a function or handle type    | D-070, D-233, D-518 |
| use of a `@deprecated` item                                             | D-200, D-517        |
| a written tuple type with a tuple element or with four or more elements | D-243, D-440        |
| a negative integer literal passed as a standard-library count argument  | D-298, D-303, D-518 |

Unused `pub` items are never reported (D-152). What counts as a use is fixed as follows (D-515, D-516):

- An import is used only through names written in the module ([§7.5](07-modules-and-packages.md#75-imports)) (D-515).
- A type is used when its name or any of its constructors is used, in an expression or a pattern, and an interface when its name or any of its functions is used. Constructors and interface functions are never reported on their own. A standard-library type uses the functions that its `@std_hooks` names ([§4.8](04-declarations.md#48-attributes)).
- An item's uses of itself, as in recursion or a recursive type, and a local function's calls in its own body, do not make it used. An item that only another unused item uses is not reported, because there is no transitive dead-code analysis.
- Bindings whose names start with `_` are not reported as unused ([§2.3](02-lexical-structure.md#23-identifiers)). The exemption covers bindings only, so an unused item named `_helper` is reported.
- Unused items and imports are reported only when every body of the module type-checked, or of the package for package-visible items, since the uses inside a body with an error are not known. Unused bindings need only name resolution.

Warnings that need the whole package, such as unused package-visible items, are computed in a separate pass (D-266). It runs once per `cheby check`, after every module of the package is checked, and never in `cheby run` (D-516).

The compiler should report errors with the source span, a short explanation, and a suggested fix where one exists, for example a new name for a shadowing binding (D-062) or `{x:?}` for a type without `Show` (D-086).

Every diagnostic has a code: `E` and four digits for an error, `W` and four digits for a warning. Codes are assigned in order and never reused, and a code whose check is removed stays reserved (D-419). `cheby explain <code>` prints a code's long explanation (D-420). The first codes, `E0001` to `E0037` and `W0001`, cover the lexer, the parser, the syntax checks and the reading of the manifest and module files. A rule with its own decision gets its own code, and other syntax errors share `E0013` (D-442). The codes `E0038` to `E0055` cover imports and names, attributes, types, `main` and native symbols (D-466), and `E0056` whitespace or a comment between the `-` and the digits of a negative literal (D-470). The codes `E0057` to `E0074` and `W0002` to `W0013` cover type checking, patterns, interfaces, targets and constants, and the warnings of this section (D-486). The codes `E0075` to `E0078` cover the evaluation of constants (D-533). Naming and reserved-word errors (`E0006`, `E0007`) are reported once, where a name is bound or declared (D-469).

While the toolchain is being built, `E0038` reports a valid construct that it cannot check or run yet (D-451). From the end of M1, the front end supports every construct, and reports `E0038` only for an import of a standard-library module that is specified or planned but not shipped, such as `std::fiber`. Paths through such an import report nothing more (D-480). The back end reports `E0038` for each construct it cannot compile yet. It compiles only what the `main` being run reaches, so code that `main` never reaches reports nothing, and it reports all its `E0038`s before compiling anything, after the front end's diagnostics (D-510). From the end of M1, the back end reports no `E0038`, and a construct it cannot compile is an internal compiler error (D-554). Until constants are evaluated at compile time, `cheby check` checks a constant's type, its restrictions and cycles of direct references between constants, but not its value, and `cheby run` reports `E0038` at each constant that `main` reaches (D-478). The code is retired when the toolchain ships every planned module and compiles every construct, and its explanation then says it is no longer emitted (D-480).

Errors that are not about the program, such as an unreadable file, a missing `cheby.toml`, a misused command line or an unknown code given to `cheby explain`, have no code. They are printed as a plain `error: …` line (D-442). A broken invariant of the compiler itself is such an error too, printed as `error: internal compiler error: …` with exit status 1 (D-464).

A diagnostic is rendered as plain text: a header with its kind, code and message, a location, a snippet of the source lines it is about, with `^` under the primary span and `-` under secondary spans, and then `note` and `help` lines. A suggested fix is a `help` line (D-418).

Diagnostics and other errors written to a terminal are colored, unless the user turns color off. Color never changes the text: removing the escape sequences gives the plain rendering exactly, and that plain form is what tests compare. Control characters quoted from the source, in messages as in snippets, are shown as their Unicode control pictures, so a diagnostic never writes a terminal escape of its own. Panic reports ([§11.6](11-errors-and-panics.md#116-panic-and-todo)) and program output are never colored (D-467). _Note:_ the reference implementation colors when standard error is a terminal. It follows the `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR=0` and `TERM=dumb` conventions, in that order of precedence, and the global `--color` flag overrides them.

A location is `path:line:column`. Lines and columns are counted from 1, and columns count Unicode code points. A byte-order mark is not counted. The path of a file in the package is relative to the package root and uses `/` on every platform (D-421, D-443). A file of the standard library is shown as `std/` followed by its module path, such as `std/io.cheby` (D-450). A file of a dependency is shown as its package's URL in the canonical form of [§13.4](#134-dependencies-and-lockfile), then `@` and the package's version, then the file's path relative to that package's root, such as `github.com/example/cheby-json@1.2.0/src/decode.cheby:4:7`. The version is the `version` in the dependency's own `cheby.toml` (D-468).

A type in a diagnostic is written as users write it at that place: prelude types and the current module's own types by their bare names, such as `Option<Int>`, other types qualified with the last segment of their module path, such as `map::Map<String, Int>`, and type parameters by their names (D-463).

The layout follows rustc in its details (D-443):

- Line numbers are left-aligned in the gutter.
- Several labels on one line share one row of markers. The label that ends last is written inline, and the others hang below it.
- A span over several lines is drawn with a rail to the left of the text, from `_…^` on its first line to `|_…^` on its last.
- A gap of one line between shown lines is printed, and a longer gap becomes `...`.
- A tab shows as four spaces, other control characters as their Unicode control pictures, and markers follow the display width of the text.
- Labels in another file get their own `::: path:line:col` section.
- Notes come before helps.
- An empty span at the end of a file that ends with a newline points at the empty last line.

Diagnostics are separated by an empty line, and no summary line, such as a count of errors, follows them (D-443).

_Example:_

```
error[E0029]: comparison operators cannot be chained
 --> src/main.cheby:7:16
  |
7 |   assert 1 < x < 3
  |            -   ^
  |            |
  |            first comparison here
  |
  = help: combine the comparisons with `&&`, such as `a < b && b < c`
```

Codes are listed with their explanations in [`docs/diagnostics/`](../diagnostics/README.md).

## 13.9 Compile-time evaluation

Constant initializers ([§4.5](04-declarations.md#45-constants)) are run by the compiler with the JIT (D-090). Because the language server compiles code continuously, evaluation must be bounded (ADR-0027):

- Each constant's evaluation is limited to 10 seconds and 1 GiB by default. The limits can be adjusted for a package in `cheby.toml` ([§13.3](#133-manifest)). Exceeding a limit is a compile error on that constant (D-195).
- The time limit is wall-clock time for one evaluation of a constant at the top level, including the constants it forces. It stops a loop at its next function entry or tail call, so a single long call into a standard-library kernel cannot be interrupted. The memory limit is the peak number of heap bytes live in the build while the constant is evaluated, including what kernels allocate, so a constant that builds and drops large temporaries can finish (D-536).
- Evaluation is deterministic: it must not depend on the time, the environment, the file system or randomness, and any attempt to use them is an error (D-090).
- The hash seed used during evaluation is not random. It is derived from the constant's identity (package, module path and name) and the content hash of what it depends on, so a constant that depends on `Map` or `Set` iteration order gets the same value on every build of the same inputs, but may get a different one when the code it depends on changes (D-269, [§13.12.6](#13126-deterministic-builds)).
- Results are cached, so a constant is re-evaluated only when something it depends on changes.

Each command evaluates the constants it needs: `cheby run` those that `main` reaches, `cheby test` those that the selected tests reach, and `cheby check` every constant of the local package. Constants of the standard library are evaluated when they are reached (D-534).

Constants are evaluated in the dependency order of their initializers, with ties broken by module order and then by name. The module order is the build order of modules, dependencies first. When only constants in cycles remain, the smallest by the same key is evaluated next, and its evaluation reports the cycle (D-594). An initializer that reads a constant not yet evaluated evaluates that constant first, under that constant's own hash seed, and then goes on under its own seed. A constant that needs its own value while it is computed is a cycle ([§4.5](04-declarations.md#45-constants)). Each constant is therefore evaluated exactly once, and its value never depends on which constant reached it first (D-535).

The seed of a constant is SipHash-1-3 with a zero key over the package name, the module path and the constant's name, followed by, for each module that the initializer reaches, sorted by module path and including standard-library modules, the module path and a hash of its source files. The package name is the first segment of the constant's module path, `std` for a constant of the standard library. The modules that the initializer reaches are the constant's own module and the modules of every function and constant that it reaches, transitively and through the initializers of the constants it reads, dictionary functions included, together with the modules of every constructor that any of them mentions. The builtin module of the prelude types is left out, and a module without a source file hashes as 0 (D-593). A map or set keeps the seed it was built with, so a constant may read and extend another constant's map. A constructor value hashes its type key and its tag, never a number internal to the compiler. The type key is SipHash-1-3 with a zero key over the type's module path written with `::`, empty for builtin types, then one `0xff` byte, then the type's name without type arguments (D-538, D-559). A constant therefore gets the same value under `cheby check`, `cheby run`, `cheby test`, the REPL and any number of threads (D-538). _Note:_ the build cache may replace the hash of the source files with the hash of the interface (D-538).

A constant that fails to evaluate is a compile error at the constant's name. When an initializer panics, the error shows the panic's message and location in a note, and evaluation never prints a panic report of its own (D-533). The error goes to the innermost constant in progress, whose own initializer panicked, which may be a constant that the top-level one forced. A cycle is reported at the constant that a read found in progress. Every constant then in progress fails with it, and an initializer that later reads a failed constant stops without an error of its own, so each failure is reported once (D-592).

Hash-based collections (`Map`, `Set`) may appear in constants, even though the hash seed is random per process (D-093) and the constant is built with its own seed. Before a program, a test or a REPL entry runs, every `Map` and `Set` in the values of the constants is rebuilt with the per-process seed, so their iteration order is still randomized per run (D-196). The rebuild is one pass over all constants together, so a collection that two constants share stays one collection. `cheby check` runs no program, so it skips the rebuild (D-539).

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

An interface's hash covers only what other modules can see: the items that are not `priv` and not tests. It refers to other definitions by module path and name, never by numbering internal to the compiler, so adding a private helper or a test changes no dependent's input (D-460).

### 13.12.4 Build cache and standard library

Compiled artifacts are stored in one cache per machine, shared by all projects and addressed by the content hash of their inputs, the compiler version and the build flags (D-255). The toolchain ships `std` precompiled, with its interface artifacts, native code for every supported native platform and its JS output (D-256). Until that exists, the reference toolchain embeds the `std` sources and compiles the modules a build reaches (D-450).

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
