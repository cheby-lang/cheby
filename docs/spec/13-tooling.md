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

`-j N` or `--jobs N` sets the number of worker threads for the build graph and code generation. The default is the available parallelism. Output never depends on it ([§13.12.6](#13126-deterministic-builds)) (D-264, D-551). It is a global option, accepted before or after the subcommand, so scripts may pass it to every command. `cheby check`, `cheby run`, `cheby test` and `cheby repl` use it, and the other commands ignore it. `N` must be greater than zero, and `-j 0` is a usage error (D-622).

`cheby run` runs the `main` of the module named by its module path argument, for example `cheby run my_app::tools::migrate`. Without an argument it runs the root module's `main` (D-137). `main` may have any visibility, including `priv` (D-163). A module path that is malformed, names no module of the package, or names a module without `main` is a tool error (D-464). Compile errors are reported first, and then nothing runs. Warnings are printed to standard error before the program starts (D-464).

`cheby run` and `cheby test` accept `--reuse-report`, which writes to standard error one line for each place where reuse in place ([§9.3](09-memory-model.md#93-reuse)) was considered, saying whether it happens and, when it does not, why. The lines are sorted by location. A standard-library kernel that updates in place when its argument is unique at run time gets a line that states that condition (D-170, D-717). The exact text of the lines is informative.

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

[runtime]
fiber-stack = "16MiB"
main-stack = "256MiB"
```

Here `json` and `json_next` are majors 1 and 2 of one repository, which are two different packages, and `other_json` is a different repository and so a different package, whatever its own name is ([§7.1](07-modules-and-packages.md#71-packages), D-238).

- `name` is the package name, a `LOWER` identifier ([§7.1](07-modules-and-packages.md#71-packages)). It must not be `std`.
- `version` is a semantic version (D-097).
- `description` is a string, and `licences` is a list of strings.
- `targets` is optional and lists the targets the package supports, `"native"` and/or `"js"`. It defaults to both. When present it must not be empty or name a target twice. Cross-target checks ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) cover only the declared targets (D-190, D-444).
- Each dependency is keyed by the name used in import paths: the key is the first segment of the dependency's module paths in this package, as in `import json_next::decode` (D-238). In v1 a dependency is a git URL plus a `tag` or a commit `rev` (D-044, D-097).
- A key must be a `LOWER` identifier, must not be `std` and must differ from the package's own `name`. Keys are local to each manifest: two packages may use different keys for the same dependency, and the same key for different ones. Renaming a dependency is just choosing its key, so the dependency's own `name` never has to be unique in a build (D-192, D-238).
- The dependency table is designed so that a later registry form, such as `json = "1.2"`, can be added without changing existing entries (D-044).
- The optional `[const-eval]` table raises or lowers the compile-time evaluation limits of [§13.9](#139-compile-time-evaluation) for the package (D-195). `time-limit` is an integer followed by `ms` or `s`, such as `"10s"`, and `memory-limit` is an integer followed by `KiB`, `MiB` or `GiB`, such as `"1GiB"` (D-536). The integer is written in decimal digits and may have leading zeros. It must be greater than zero, because a zero limit would fail every constant and no value means "no limit", and the limit in milliseconds or bytes must fit in 64 bits (D-616).
- The optional `[runtime]` table sets the stack reservations of fibers on native targets ([§10.1](10-concurrency.md#101-fibers)) (D-125, D-660). `fiber-stack` is the reservation of every fiber that does not set its own, 8 MiB by default. `main-stack` is the reservation of the root fiber running `main`, of each test and of each REPL entry, 64 MiB by default in the reference implementation. A value has the form of `memory-limit`, must be from 1 MiB to 16 GiB, and is rounded up to whole pages. Only the table of the package being run or tested applies. In a dependency it is checked and has no effect. It has no effect on JS (D-660).
- The top-level keys are exactly `name`, `version`, `description`, `licences`, `targets`, `[dependencies]`, `[const-eval]` and `[runtime]`, a dependency has only `git`, `tag` and `rev`, `[const-eval]` has only `time-limit` and `memory-limit`, and `[runtime]` has only `fiber-stack` and `main-stack`. Any other key is a compile error, so a misspelled key such as `verison` is never ignored silently (D-444, D-536, D-660). A value of the wrong form in `[const-eval]` or `[runtime]`, or a `[runtime]` value outside its range, is a compile error too (D-536, D-660).

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

The REPL reads one entry at a time (D-605):

- An entry goes on to the next line while a bracket, a block, a string, a raw string or an interpolation is open, or while its last non-blank line starts with `@` or `///`, which an item must follow. Text inside `//` comments and strings does not count, except the code of an interpolation.
- A closing bracket that matches nothing ends the entry, and the checker reports it. At the end of input, an unfinished entry is still checked, so its error is reported.
- Text of only blanks and comments is not an entry and takes no number.
- An entry is declarations and imports when its first line that is neither blank nor a `//` comment starts with `@`, `///` or `//!`, or with one of the words `fn` not followed by `(`, `pub`, `priv`, `type`, `interface`, `const`, `import` or `test`. It is checked as a module.
- Any other entry is statements, checked as the body of a block. When its last statement is an expression whose type is not `Never`, its value is shown ([§13.5.5](#1355-prompt-output-and-standard-streams)). The names bound by its top-level `let` and `let assert` statements become session values.
- A shown `Result` is not a discarded result, and a session value is not an unused binding ([§13.8](#138-diagnostics-and-warnings)).
- An entry with an error binds nothing, and its unused-binding warnings are not shown.

Inside a package directory, the REPL loads all of the package's modules and dependencies, and they can be imported (D-098). Each entry is checked as a module `<repl:N>` of the local package, so the package's package-visible items are usable at the prompt after an import, without `:module` (D-606). Outside a package, the session's package is `repl`, version `0.0.0`, with no modules. Every shipped standard-library module can be imported. A `priv` item of an entry is visible only inside that entry, because it never becomes a session name. Types and diagnostics write session items without a `<repl:N>::` qualifier, as in `Point`, while locations keep `<repl:N>:line:col` (D-606). Diagnostics call a statements entry "the entry", and an entry module named on its own "the session", as in "the session has no function `show(Cat) -> String`", because users never write the wrapper of an entry or a module `<repl:N>`, and the entries count as one module for satisfaction (D-629).

Entries are checked for the `native` target only, because the session runs on the native JIT. So a native-only item of the package and an `@external(native, …)` entered at the prompt are usable (D-606). This holds whatever targets the package declares, also when they lack `native`. A package's targets narrow where its code is checked, not whether its items exist (D-190), so in a package for `js` only, its portable items, the standard library and the session's own definitions are usable, while an item that exists only on `js`, such as a function with only an `@external(js, …)`, is E0067 at the prompt: "`alert` exists only on `js`, but the entry runs on `native`" (D-628).

In a session, `env::program_name()` returns the package's name, or `repl` outside a package, and `env::args()` is empty, so transcripts are deterministic (D-612).

### 13.5.2 Redefinition

In a module, names cannot be redefined. The REPL is the one exception, because it is a session rather than a scope:

- **Functions** are late-bound (D-043). Redefining a function with an equal type signature replaces it for every existing caller, through a JIT indirection table. Signatures are equal when they have the same generics, parameter types, clamped parameters and result, whatever the parameter names (D-607). If the signature changes, the new function replaces the old one for later entries, and the REPL re-checks every definition that depends on it and reports each one that no longer type-checks. Until a caller is checked and compiled again, it keeps calling the old function, never code of another signature (D-456, D-607).
- **Foreign functions** follow the same rule. The rule that every `@external(native, …)` naming one symbol gives it the same C signature ([§12.5](12-targets-and-ffi.md#125-foreign-functions)) applies to the session's current definitions only. Replaced definitions and entries that failed to compile do not count, so a redefinition may change a foreign symbol's C signature, while two live definitions with different C signatures are still an error (D-475).
- **Types** can be redefined. Existing values keep the old type, which is now a different type from the new one, and the REPL marks it as stale when printing (D-043).
- **Interfaces** are redefined like types. The old interface and its implementations stay with the old values, and the new interface is a different interface (D-547).
- **Constants** are redefined like functions without parameters. With the same type, readers see the new value. With another type, the definitions that depend on the constant are checked again (D-547, D-607).
- **Type aliases** are replaced for later entries. Definitions already checked keep what they were checked against (D-547).
- **`let` bindings** at the REPL prompt may be rebound with the same name, an explicit REPL exception to the no-shadowing rule, since forbidding it would make the REPL unusable (D-193). The no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)) still applies inside every expression and declaration, but not between prompt entries.

A prompt-level `let` rebinding shadows. Each `let` or `let assert` creates a new session value, whatever its type, and later entries read the new one. Functions, closures and constants checked or evaluated before the rebinding keep reading the binding they were checked against, so a value never changes once it is shown. A `let` rebinding never checks other definitions again (D-608):

```
> let x = 1
> let f = fn() -> Int { x }
> fn read_x() -> Int { x }
> let x = 2
> f()
1 : Int
> read_x()
1 : Int
> x
2 : Int
```

When a function or constant is redefined with another signature or type, every current function and constant whose signature or body names it is checked again from its own text, in the order the definitions entered the session. That includes those that named an earlier definition of the name, and those that relied on the function for interface satisfaction. Each one that checks is compiled again and replaces itself. Each one that fails keeps its old definition, and each of its errors gets the note "`f` was checked again because a definition it uses changed its signature; it keeps its old definition". A redefinition with the same signature or type checks nothing again, and neither does a `let` rebinding or a redefined type, interface or alias. Definitions already checked keep the old type, interface or alias, and their values of an old type are stale (D-609). `:reload` checks definitions again in the same way when it replaces a module they use ([§13.5.3](#1353-commands)) (D-625).

An entry that is rejected, because evaluating one of its constants failed or its body panicked, changes nothing in the session: the definitions it replaced are restored (D-607).

For interface satisfaction ([§8.2](08-interfaces.md#82-satisfaction)), the prompt entries of a session count as one module (D-610):

- A type entered at the prompt satisfies an interface through the session's current functions, whichever entries declared them. After `type Cat { Cat(Int) }` and, in a later entry, `fn show(c: Cat) -> String`, `Cat` satisfies `Show` in every entry after the one with `show`. Definitions checked before keep what they were checked against.
- The visibility rule of [§8.2](08-interfaces.md#82-satisfaction) applies unchanged, and a `priv` function counts only in its own entry.
- Redefining `show` with the same signature replaces the implementation for code checked earlier too. With another signature, the new function counts for later entries, and the definitions that relied on the old one are checked again.
- A session name means one function, so the session satisfies an interface for one type at a time, as a module does (D-045). Entering `fn show(d: Dog) -> String` after `Cat`'s `show` replaces it: later entries see `Dog` satisfy `Show`, and `Cat` no longer.
- A redefined type is a new type. A function that names the old type satisfies the interface for the old type only, and the new type satisfies it once a function that names the new type is entered.
- For types of the package and of the standard library, the rule of [§8.2](08-interfaces.md#82-satisfaction) is unchanged, and functions entered at the prompt never count for them.

### 13.5.3 Commands

| Command        | Meaning                                                                                     |
| -------------- | ------------------------------------------------------------------------------------------- |
| `:type expr`   | show the type of an expression without evaluating it (D-098, D-624)                         |
| `:reload`      | recompile changed modules of the package (D-098, D-625)                                     |
| `:module path` | enter a module, so its `priv` items are visible and its imports are in scope (D-098, D-624) |
| `:module`      | leave the entered module (D-624)                                                            |
| `:help`        | list the commands (D-624)                                                                   |
| `:quit`        | end the session, as the end of input does (D-624)                                           |

A command that succeeds prints nothing, except `:type`, `:help` and the report of `:reload`. An unknown command is the tool error "there is no command `:name`; `:help` lists the commands", and an argument given to `:quit`, `:help` or `:reload` is the tool error "`:name` takes no argument" (D-464, D-624).

`:type expr` checks `expr` as the next entry `<repl:N>`, so it uses up an entry number and its diagnostics have a location of their own. It runs nothing, keeps nothing and shows only errors. On success it prints `<expr text> : <type>`, the type of the final expression written as in diagnostics, also when it is `Never`. Since nothing runs, `:type 1 / 0` prints `1 / 0 : Int` (D-546, D-624). Empty text or a declaration is the tool error "`:type` needs an expression, as in `:type 1 + 2`", and statements whose last one is not an expression, such as `:type let y = 1`, are the tool error "`:type` needs an expression, and the last statement here is not one" (D-624).

`:module path` enters a module of the package or of the standard library, written as in an import, such as `bakery::oven`, `bakery` or `std::list`, and `:module` alone leaves it. A path that names no module is the tool error "there is no module `path`". The prompt stays `> ` (D-624).

Entering a module with `:module` makes the REPL behave as if the input were written at the end of that module: its `priv` items are accessible, package-visible items of its package are accessible (D-162), and the package boundary for `exposed` is that module's package (D-098). For a module of the standard library, every package-visible item of the standard library is visible and every type of it is `exposed`. Only checking changes. Later entries are still modules of the local package ([§13.5.1](#1351-inputs)), but they see the entered module's items, `priv` ones included, and its imports. These names rank as if the session had defined them when the module was entered, so a name the session defines later wins. A definition entered inside a module is checked again inside it ([§13.5.2](#1352-redefinition)) (D-624). _Note:_ the reference implementation checks entries against interfaces in which the entered module's `priv` items are package-visible, and generates code from the real interfaces (D-624).

`:reload` runs package discovery again and compares the text of each module file with the loaded version (D-098, D-625):

- Each module whose text changed, each new module, and each module of the package that imports one of them, transitively, is checked again as a new version, in import order, all or nothing. A module that imports one that failed is not checked, since its errors would only repeat.
- The reload is rejected when a module has a syntax or type error, when the reloaded modules import each other in a cycle (E0040), when package discovery reports an error, or when there is any warning under `--deny-warnings` (D-549). A rejected reload reports its diagnostics and changes nothing, not even how values print.
- When it succeeds, the REPL prints "reloaded `path`" for each new version, in check order. Imports then reach the new versions, including the session's earlier imports and the module entered with `:module`. The types of the old versions are stale ([§13.5.5](#1355-prompt-output-and-standard-streams)), and E0054 is checked again over the current modules (D-475).
- Every current session definition that refers to any earlier version of a reloaded module is checked again from its text, in the order the definitions entered the session, as after a redefinition ([§13.5.2](#1352-redefinition)) (D-609). This includes a definition that an earlier `:reload` broke, which still runs code compiled against an older version. Each one that checks is compiled again. Each error of the others gets the note "`f` was checked again because `:reload` changed a module it uses; it keeps its old definition", so a definition that stays broken is reported again at every later `:reload` of the module it uses.
- Package-discovery diagnostics, such as E0037 for a bad file name, are shown even when no module text changed, and an error among them rejects the reload. When no text changed and nothing rejected the reload, `:reload` prints "no module changed".
- Outside a package, `:reload` is the tool error "`:reload` reloads the modules of the package, and this session has no package". A module file that cannot be read is a tool error that changes nothing.

The old versions stay loaded, because session values and compiled code still refer to their items (D-547). Two limits hold in M1: a module whose file was deleted stays loaded, and `cheby.toml` is not read again (D-625). _Note:_ the reference implementation gives a new version an internal module path whose last segment carries the reload's generation, as in `store::prices<r1>`. No import can name it, so the new version's symbols never collide with the old version's. The generation counts the `:reload` commands that found a change, and diagnostics show the original path (D-547, D-625).

### 13.5.4 Fibers in the REPL

Each REPL entry runs in its own root scope, as if it were the body of `main` ([§10.10](10-concurrency.md#1010-program-entry-and-exit)). A panic in an entry is printed and does not end the session. The entry unwinds, so the memory it held is released (D-712). A deadlock in an entry ([§10.4](10-concurrency.md#104-detached-fibers)) is reported as `cheby run` reports it, the entry's fibers are cancelled and unwind, and the session continues (D-724). _Note:_ the reference implementation runs the body of each entry on a 64 MiB stack, as it runs `main` and tests, unless the manifest's `main-stack` sets another size ([§13.3](#133-manifest)), and evaluates constants entered at the prompt on the 8 MiB stack of other entries (D-613, D-660).

Detached fibers ([§10.4](10-concurrency.md#104-detached-fibers)) started from the REPL keep running after their entry finishes, until the session ends or `:reload` is used, so servers and background work can be tried interactively (D-194). At `:reload` and at the end of the session, they are cancelled and unwind, in every build, before any session value is released. No user code runs while they unwind (D-712).

### 13.5.5 Prompt, output and standard streams

The REPL shows its session as follows (D-546):

- The prompt is `> `, and `. ` while an entry goes on to the next line ([§13.5.1](#1351-inputs)) (D-605).
- An expression's value prints as `<debug text> : <type>`, with the type written as in diagnostics ([§13.8](#138-diagnostics-and-warnings)). A `Nil` value prints nothing. A value whose type is stale prints its type followed by ` (stale)`, as described below.
- Declarations and imports print nothing when they succeed.
- A location in an entry is shown as `<repl:N>:line:col` in diagnostics and panic reports, with entries counted from 1.
- On a terminal the REPL has line editing. When standard input is not a terminal, there is no line editing, and each input line is echoed after its prompt, so the output of a session reads as a transcript.
- There is no leak check ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-458).

A type is stale when a later entry redefined it, when it is a `dyn` type whose interface a later entry redefined, or when it belongs to a module of the package that `:reload` replaced ([§13.5.3](#1353-commands)). Wherever a stale type is written, in a value's type, in the output of `:type` and in diagnostics, ` (stale)` follows it, after its type arguments: `Shape (stale)`, `prices::Item<Int> (stale)`, `dyn Named (stale)`. A type that holds a stale type is not stale itself, as in `Holder<Shape (stale)>` and `List<prices::Item (stale)>`. Old and new types share a name, so the marker is what tells them apart. Mixing an old type with its new version is an ordinary type error, such as "expected `List<prices::Item>`, found `List<prices::Item (stale)>`" (D-626).

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

_Note:_ the reference implementation tests the REPL with transcripts in `tests/repl/`, in which lines that start with `> ` or `. ` are input and every other line is expected output, with standard output and standard error merged in the order of writes (D-546, D-548). A line that starts with `< ` is a line of standard input that an entry reads. It is fed right after the input line before it and is not echoed, so it is not expected output (D-614).

## 13.6 Tests

`cheby test` compiles the package with its `test` blocks and runs them (D-055):

- Every test runs as a root fiber, like `main`, in its own root scope ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-040, D-723). A test body has type `Nil` or `Result<Nil, E>`. A test passes if its body finishes without panicking and, for a `Result` body, returns `Ok` ([§4.7](04-declarations.md#47-tests)) (D-140).
- Code examples in doc comments are compiled and run as tests ([§13.10](#1310-documentation)) (D-197).
- Tests run in parallel, at most as many at once as the scheduler has threads ([§10.2](10-concurrency.md#102-scheduling)), so `CHEBY_THREADS` bounds them as it bounds `cheby run` (D-658, D-723). They cannot interfere through shared state, because there is none (D-004).
- Detached fibers that a test spawns end when the test ends: they are cancelled and unwind, in every build, and no user code runs while they do ([§10.4](10-concurrency.md#104-detached-fibers)) (D-723).
- A deadlock in a test ([§10.4](10-concurrency.md#104-detached-fibers)) fails the test, whose failure block holds the deadlock report. The test's fibers are then cancelled and unwind, as at the end of a test, and the other tests go on (D-724).
- Test output reports each failure with its panic message, which for `assert` includes both sides of a failing comparison (D-096). A test whose body returns `Err(e)` shows `e` as `main` prints it ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-529).
- Tests can be filtered by name and by module: `cheby test [FILTER] [--module PATH]`. A test's full name is `<module path> "<test name>"`. `FILTER` selects the tests whose full name contains it, and `--module` selects the tests of that module and its submodules (D-541). `PATH` must be a well-formed module path that is a module of the package or the prefix of one. Otherwise the command stops with status 1 before any test is compiled, with the tool error "`x` is not a module path" or "there is no module `x` in this package", as `cheby run` does ([§13.1](#131-the-cheby-command)) (D-464, D-597).
- The native test command selects only the tests that exist on `native`: a test's targets, including its module's `@!target`, and for a local test the package's `targets` too, must contain `native`. A test that exists only on `js` is neither run nor counted as filtered out ([§12.6](12-targets-and-ffi.md#126-target-specific-code)) (D-597).
- Test blocks are not compiled into any other build (D-055). They and doc examples are compiled as separate units per module, outside the module's interface artifact and normal cached code, so editing a test never rebuilds other modules ([§13.12.3](#13123-interface-artifacts-and-recompilation)) (D-265).

> **Open (OQ-13-8):** How a deadlock is detected while several tests run at once. [§10.4](10-concurrency.md#104-detached-fibers) defines a deadlock for the whole program, so a deadlocked test would not be detected while any other test runs or waits to start, and D-724 does not say which tests fail. _Proposed:_ detection is per test. A test is deadlocked when none of its own fibers, those of its accounting domain, is running or runnable, none of their timers, IO operations or `@blocking` calls is pending, and its root has not finished. Only that test fails, and tests still running or not yet started are not affected. A test's fibers can be woken only by fibers of the same test, because tests share no state.

Test blocks are type-checked by `cheby build` as well, so a broken test is caught even when tests are not run.

`cheby test` writes to standard output one line per test, `test <module path> "<name>" ... ok` or `... FAILED`. For each failure, a block follows that starts with `---- <module path> "<name>" ----` and holds the panic report or the `Err` text. A summary ends the output, such as `test result: ok. 3 passed; 0 failed; 1 filtered out`, with `FAILED` in place of `ok` when a test failed. A test's own output on standard output and standard error, including that of the fibers it spawns, is captured. Once the test has finished and every test before it has been written, its output is written to the same streams, followed by its line. Tests are written in order, by module path and then in source order, so the output is the same as if the tests ran one after another, whatever order they finish in (D-541, D-598, D-723). The failure blocks follow all the lines, each preceded by an empty line. The summary is preceded by an empty line when a test line was written, and when no test is selected it is the only output (D-598). A test's panic report appears only in its failure block, never on standard error, and standard output is flushed before it is taken (D-595). The exit status is 0 when every selected test passes, also when none is selected, and 1 when one fails. A compile error stops the command with status 1 before any test runs (D-541). A write or flush that ends the program ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) also ends the test run at once: nothing more is printed, and the exit status is 1 (D-306, D-783).

_Note:_ the reference implementation runs each test on a 64 MiB stack like `main`, unless the manifest's `main-stack` sets another size ([§13.3](#133-manifest)) (D-596, D-660). In M1 it ran tests one after another. A toolchain with debug assertions counts the live objects of each test separately, after the test's detached fibers have ended, and objects still live fail that test with the message of the leak check ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-458, D-541, D-723).

The standard library's `test` blocks sit next to the code they test, in its source files. A build of a user package neither checks them nor compiles them, and the warnings of [§13.8](#138-diagnostics-and-warnings) ignore them. They are checked and run when the standard library itself is tested (D-479). `cheby` has no flag for this. _Note:_ the reference implementation tests the standard library with `cargo xtask test-std`, one module at a time, so a module that does not build does not hide the results of the others (D-542). It checks the standard library through an in-memory package `std_self_test` whose root module imports every shipped std module, and while its tests run `env::program_name()` returns `std_self_test` (D-599). Where its std tests live and how their helper items count as used is in the [stdlib README](../stdlib/README.md) (D-630, D-631).

## 13.7 Formatter

`cheby fmt` rewrites source files into the one canonical style. It has no configuration options (D-079).

- The output does not depend on the input's layout beyond what the style keeps (such as blank lines between statements and comments).
- Formatting never changes the meaning of a program.
- `cheby fmt --check` exits non-zero if any file would change, for CI.
- The formatter is part of the compatibility promise: after 1.0, rule changes to the canonical style are allowed, but `cheby fmt` must be able to migrate code when a language change needs it (D-075).

The style itself is defined by the formatter, not by this specification. Examples in this specification use it: two-space indentation, one statement per line, trailing commas in multi-line lists, and multi-line pipelines with `|>` at the start of each line.

## 13.8 Diagnostics and warnings

Compile errors stop compilation. Warnings do not, unless `--deny-warnings` is passed, which is intended for CI (D-080). With it, each warning is rendered as an error with its own code, as in `error[W0002]: unused binding …`, and the first one has the note "warnings are denied by `--deny-warnings`". The command then fails with exit status 1, and `cheby run` and `cheby test` run nothing (D-549). An error found while reading the manifest, discovering modules, lexing, parsing or checking syntax stops the later checks, such as name resolution and type checking, so their diagnostics never come from a broken file (D-452). A denied warning of those phases, such as one about the manifest, is rendered as an error but does not stop the later checks, because a warning never comes from a broken file. The command still fails with status 1 after them, `cheby check` evaluates no constants, and `cheby run` and `cheby test` compile and run nothing (D-621).

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

- An import is used only through names written in the module ([§7.5](07-modules-and-packages.md#75-imports)) (D-515), except that in a standard-library module an import of a built-in type's home module is also used when satisfaction takes a function from it (D-762).
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

A type in a diagnostic is written as users write it at that place: prelude types and the current module's own types by their bare names, such as `Option<Int>`, other types qualified with the last segment of their module path, such as `map::Map<String, Int>`, and type parameters by their names (D-463). In the REPL, a stale type is followed by ` (stale)` ([§13.5.5](#1355-prompt-output-and-standard-streams)) (D-626).

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
- The time limit is wall-clock time for one evaluation of a constant at the top level, including the constants it forces. It stops a loop at its next function entry or tail call, so a single long call into a standard-library kernel cannot be interrupted. The memory limit is the peak number of heap bytes that the evaluation keeps live at once, including the constants it forces and what kernels allocate, so a constant that builds and drops large temporaries can finish (D-536). Constants evaluated earlier and REPL session values do not count, so a constant's verdict never depends on what was evaluated before it. Every allocation is checked against the limit before memory is requested, so a single request over the limit exceeds it too (D-617), and so does a request whose size overflows. A standard-library list update that takes its arguments over checks the limit first against an upper bound of what it allocates, so the limit may stop it up to that bound before the peak would reach the limit (D-738). A request under the limit that the system refuses is not over the limit: it panics with `out of memory`, which is E0075 ([§11.8.2](11-errors-and-panics.md#1182-out-of-memory), D-642).
- An exceeded limit is reported at the constant whose evaluation started at the top level, also when the limit ran out in a constant that it forced, which a note then names. The message names the limit and its value as `[const-eval]` spells it (D-618).
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

The module, one source file, is the unit of compilation, caching and parallelism (D-248). Modules are compiled in the order of the import graph, and independent modules in parallel. Within a module, type-checking and code generation run per function, in parallel, apart from a few per-module steps such as borrowing inference and per-type layout tables (D-263). Results are taken in a fixed order, so diagnostics and generated code are the same for any number of threads (D-264). _Note:_ in M1 the reference implementation lowers each module to MIR in parallel and then compiles the functions of each batch in parallel, taking the results in build order and declaration order. When several fail, it reports the error of the first one in that order (D-632).

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

_Note:_ M1's measurements confirmed the throughput budget as written: the reference compiler checks and generates code for 104,000 to 107,000 lines per second per core on projects of 100,000 lines or more. The budgets are reported, not enforced, until M6 (D-644).

Unused imports remain warnings ([§13.8](#138-diagnostics-and-warnings)) (D-253), and AOT builds use the system linker, then a bundled lld before 1.0 (D-088, D-261).

## 13.13 Build order (informative)

All tools ship in v1, but the reference implementation is built in this order (D-099):

1. parser, type checker, IR, JIT and REPL for the single-fiber core,
2. Perceus and the runtime, including the M:N scheduler and channels,
3. AOT object output and linking,
4. the JS backend,
5. formatter, language server, documentation and package manager, grown alongside the others.
