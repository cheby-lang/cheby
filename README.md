# Cheby

Cheby is a statically typed, expression-oriented programming language with immutable data, lightweight fibers and deterministic memory management. Its syntax is inspired by Rust, its semantics by Gleam and Erlang, and its concurrency by Go. Programs compile to native code through Cranelift and to JavaScript.

> **Status:** pre-1.0. The language specification is a draft. The reference implementation has finished milestones M1, the single-fiber core, M2, the runtime, and M3, AOT output and the build cache: `cheby check`, `run`, `test` and `repl` work through a Cranelift JIT, with fibers on an M:N scheduler, channels, selectors, scopes, deadlock detection and Perceus reference counting, `cheby build --target native` writes executables, and a global cache keeps compiled modules between commands. The JavaScript backend comes in M4. Breaking changes are allowed until 1.0.

## A taste

```cheby
//! Prints the numbers from 1 to 100, replacing multiples of 3 with
//! "Fizz", multiples of 5 with "Buzz" and multiples of both with
//! "FizzBuzz".

import std::io

fn main() {
  // There are no loops, so a local function counts by calling itself.
  // The recursive call is in tail position, so the stack does not grow.
  fn count(n) {
    case {
      n > 100 => Nil
      _ => {
        io::println(fizzbuzz(n))
        count(n + 1)
      }
    }
  }
  count(1)
}

fn fizzbuzz(n: Int) -> String {
  case n % 3, n % 5 {
    0, 0 => "FizzBuzz"
    0, _ => "Fizz"
    _, 0 => "Buzz"
    _, _ => "{n}"
  }
}

test "multiples of both are FizzBuzz" {
  assert fizzbuzz(15) == "FizzBuzz"
}
```

## Core ideas

- **One way to do each thing:** no loops and no `if`. Iteration is recursion with guaranteed tail calls, and branching is `case`.
- **Immutable everywhere:** no mutable escape hatch. Long-lived state lives in fibers.
- **No function coloring:** stackful fibers on an M:N scheduler with structured concurrency. There is no `async`/`await`.
- **Deterministic memory:** Perceus-style reference counting instead of a tracing GC.
- **Same results on every target:** native and JavaScript share semantics.
- **Tooling ships with the language:** one `cheby` binary includes the compiler, REPL, test runner, formatter, language server, doc generator and package manager.
- **Fast compiles:** type-checking a module needs only the interfaces of its dependencies.

The [overview](docs/spec/01-overview.md) lists what Cheby has, what it deliberately leaves out, and why.

## Repository layout

| Path                                              | Contents                                                |
| ------------------------------------------------- | ------------------------------------------------------- |
| [`docs/spec/`](docs/spec/README.md)               | The language specification                              |
| [`docs/stdlib/`](docs/stdlib/README.md)           | The standard-library specification                      |
| [`docs/decisions/LOG.md`](docs/decisions/LOG.md)  | The decision log (`D-NNN`), cited throughout the spec   |
| [`docs/adr/`](docs/adr/)                          | Architecture decision records for the major choices     |
| [`docs/plan/`](docs/plan/README.md)               | Implementation plan, milestones M0-M6                   |
| [`docs/examples/`](docs/examples/)                | 32 example programs with goldens, the conformance suite |
| [`docs/diagnostics/`](docs/diagnostics/README.md) | Explanations of diagnostic codes, for `cheby explain`   |
| [`crates/`](crates/)                              | The compiler, runtime and tools, one Cargo workspace    |
| [`std/`](std/)                                    | The standard library's Cheby sources                    |
| [`tests/`](tests/)                                | Conformance, UI, REPL and language server harnesses     |
| [`xtask/`](xtask/)                                | Developer commands, run as `cargo xtask`                |

## Editor support

- [tree-sitter-cheby](https://github.com/cheby-lang/tree-sitter-cheby): tree-sitter grammar and highlight queries
- [zed-cheby](https://github.com/cheby-lang/zed-cheby): Zed extension

## Language server

`cheby lsp` is the language server. It speaks LSP 3.17 over standard input and output. It publishes the diagnostics `cheby check` reports, as you type, and answers hover, go to definition, references, call hierarchy, workspace symbols, semantic tokens, completion, signature help, inlay hints and formatting. It evaluates a package's constants in a child process, so hover shows their values. Rename, quick fixes, lenses and the memory marks come later (see [`docs/plan/m5-tooling.md`](docs/plan/m5-tooling.md)).

### Install

Build `cheby` and put it on your `PATH`:

```sh
cargo install --path crates/cheby_cli --locked
```

Any `cheby` on `PATH` works, or point the editor at a binary by its path.

### Zed

The [zed-cheby](https://github.com/cheby-lang/zed-cheby) extension starts the server. It is not in Zed's extension registry yet, so install it as a dev extension: clone `zed-cheby`, open the Extensions page (`cmd-shift-x`), press Install Dev Extension and choose the clone. Zed builds the extension from source, which needs Rust installed through `rustup`. The extension runs `cheby lsp` from `PATH`. To use another binary, or to change a setting, add a `cheby` entry under `lsp` in Zed's `settings.json`:

```json
{
  "lsp": {
    "cheby": {
      "binary": {
        "path": "/path/to/cheby",
        "arguments": ["lsp"]
      },
      "settings": {
        "analysis": { "debounceMs": 100 },
        "inlayHints": { "parameterNames": true }
      }
    }
  },
  "inlay_hints": { "enabled": true }
}
```

Zed shows no inlay hints until `inlay_hints.enabled` is `true`. The server's settings can also go in `initialization_options`, with the same shape. A key in `settings` wins over the same key there.

### Settings

Every setting sits in one `cheby` object, so `analysis.debounceMs` is written `{ "analysis": { "debounceMs": 100 } }`. A value of the wrong type is ignored with a warning in the server's log, and the key keeps its earlier value.

| Setting                         | Default | What it does                                                           |
| ------------------------------- | ------- | ---------------------------------------------------------------------- |
| `analysis.target`               | `null`  | Check for `"native"` or `"js"` only, as `cheby check --target` does    |
| `analysis.debounceMs`           | `40`    | The quiet time after an edit before the server checks again, 0 to 1000 |
| `analysis.requestWaitMs`        | `10000` | How long a request waits for the check of its text, 100 to 60000       |
| `completion.autoImport`         | `true`  | Offer the items of modules the file does not import, with the import   |
| `completion.typeDriven`         | `true`  | Rank items of the expected type first, and offer functions after `xs.` |
| `inlayHints.enable`             | `true`  | Every inlay hint                                                       |
| `inlayHints.letTypes`           | `true`  | The type after a `let` of one name                                     |
| `inlayHints.patternBinders`     | `true`  | The type after each name of a destructuring `let`                      |
| `inlayHints.useBinders`         | `true`  | The type after each `use` binder                                       |
| `inlayHints.closureTypes`       | `true`  | A closure's parameter and result types                                 |
| `inlayHints.localFnTypes`       | `true`  | A local `fn`'s parameter and result types                              |
| `inlayHints.pipeStages`         | `true`  | The type after each stage of a multi-line pipeline                     |
| `inlayHints.dyn`                | `true`  | An implicit `dyn` conversion                                           |
| `inlayHints.parameterNames`     | `false` | `name: ` before a literal argument                                     |
| `constants.evaluate`            | `true`  | Evaluate constants, for hover and the back-end diagnostics             |
| `constants.childTimeoutSeconds` | `60`    | The time limit of one evaluation, 1 to 3600                            |
| `semanticTokens.enabled`        | `true`  | Colour names by what they are, over the editor's highlighting          |

These keys are accepted but do nothing yet: `diagnostics.explainLinks`, `inlayHints.constants`, `lenses.*`, `ownership.*` and `clientCommands`. Their features come in later waves.

### Tips

- If no list appears after `::`, as after accepting a module's name, type the first letter of the item you want, and the module's items are listed.
- Zed asks for no signature while a completion's placeholders are active. Run `editor: show signature help` (`editor::ShowSignatureHelp`) from the command palette, or bind it to a key, to show it then.
- Constants are evaluated once the package first checks clean, then after each save that changes it. Hover marks a value that may be out of date, and says why a constant has no value.

### Known issues

- This repository's own `std/*.cheby` files show false errors when opened: the server checks them as single files, not as the standard library.
- An unpaired `"` while typing marks string errors down the rest of the file until it is closed.
- On Windows, constants are not evaluated until M6, so hover shows no values and the back-end diagnostics of `cheby check` do not appear.

## License

The compiler, runtime and standard library are dual-licensed under MIT and Apache-2.0 (D-089).
