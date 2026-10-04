# Cheby

Cheby is a statically typed, expression-oriented programming language with immutable data, lightweight fibers and deterministic memory management. Its syntax is inspired by Rust, its semantics by Gleam and Erlang, and its concurrency by Go. Programs compile to native code through Cranelift and to JavaScript.

> **Status:** design phase, pre-1.0. The language specification is a draft and the reference implementation has not started yet. Breaking changes are allowed until 1.0.

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

| Path                                             | Contents                                              |
| ------------------------------------------------ | ----------------------------------------------------- |
| [`docs/spec/`](docs/spec/README.md)              | The language specification                            |
| [`docs/decisions/LOG.md`](docs/decisions/LOG.md) | The decision log (`D-NNN`), cited throughout the spec |
| [`docs/adr/`](docs/adr/)                         | Architecture decision records for the major choices   |
| [`docs/plan/`](docs/plan/README.md)              | Implementation plan, milestones M0-M6                 |
| [`docs/examples/`](docs/examples/)               | 32 example programs, the future conformance suite     |

## Editor support

- [tree-sitter-cheby](https://github.com/cheby-lang/tree-sitter-cheby): tree-sitter grammar and highlight queries
- [zed-cheby](https://github.com/cheby-lang/zed-cheby): Zed extension

## License

The compiler, runtime and standard library will be dual-licensed under MIT and Apache-2.0 (D-089).
