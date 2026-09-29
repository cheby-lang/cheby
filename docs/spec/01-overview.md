# 1. Overview

Cheby is a statically typed, expression-oriented programming language with immutable data, lightweight fibers and deterministic memory management. Its syntax is inspired by Rust (D-001), its semantics by Gleam and Erlang, its concurrency by Go, and its compilation strategy by Koka and C++ toolchains. Programs compile to native code through Cranelift and to JavaScript (D-006).

This chapter is informative. It summarizes the language and points to the normative chapters.

## 1.1 Design principles

1. **One way to do each thing.** There are no loops and no `if`. Iteration is recursion, and branching is `case` (D-002).
2. **Immutability everywhere.** Every value is immutable, and there is no mutable escape hatch (D-004, D-017). Long-lived state lives in fibers that carry it through recursion.
3. **No function coloring.** Concurrency uses stackful fibers. Any function may block without changing its signature (D-003).
4. **Deterministic memory.** Memory is managed with Perceus-style reference counting, not a tracing GC. Uniquely owned values are updated in place (D-005).
5. **The same program computes the same result everywhere.** Native and JS targets share semantics. A program whose result does not depend on timing or scheduling computes the same result on every target, and a failure that happens only on JS is a panic, never a different value. Timing, scheduling and liveness may differ, because JS fibers are not preempted (D-037, D-094, D-232, [chapter 12](12-targets-and-ffi.md)).
6. **Tooling is part of the language.** One `cheby` binary ships the compiler, REPL, test runner, formatter, language server, documentation generator and package manager from v1 (D-031).
7. **Stability after 1.0.** After 1.0 the language and standard library only grow (D-075).

## 1.2 What Cheby has

| Feature                                                                 | Chapter                                               | Decisions                  |
| ----------------------------------------------------------------------- | ----------------------------------------------------- | -------------------------- |
| Algebraic data types as the only user-defined data type                 | [3](03-types.md), [4](04-declarations.md)             | D-022, D-035, D-082        |
| Types opaque by default, `exposed` to publish constructors              | [4](04-declarations.md)                               | D-035, D-048               |
| Exhaustive `case` with guards, multiple subjects and a subjectless form | [5](05-expressions.md), [6](06-patterns.md)           | D-052, D-109               |
| Guaranteed tail calls, including mutual and indirect calls              | [5](05-expressions.md), [12](12-targets-and-ffi.md)   | D-015, D-120               |
| Gleam-style `use` for callbacks, errors and resources                   | [5](05-expressions.md), [11](11-errors-and-panics.md) | D-018, D-023               |
| Pipe operator `\|>` and function capture `f(a, _)`                      | [5](05-expressions.md)                                | D-050, D-244, D-245        |
| Structural interfaces, satisfied implicitly by module functions         | [8](08-interfaces.md)                                 | D-020, D-061, D-063        |
| Operator overloading through interfaces                                 | [8](08-interfaces.md)                                 | D-034, D-047, D-118        |
| Persistent collections: RRB-tree `List`, HAMT `Map` and `Set`           | [3](03-types.md), [9](09-memory-model.md)             | D-039                      |
| Fibers on an M:N scheduler, structured concurrency, channels            | [10](10-concurrency.md)                               | D-003, D-012, D-040, D-054 |
| Compile-time evaluated constants                                        | [4](04-declarations.md)                               | D-090                      |
| Inline `test` blocks                                                    | [4](04-declarations.md), [13](13-tooling.md)          | D-055                      |
| Per-target FFI through `@external` and `@target`                        | [12](12-targets-and-ffi.md)                           | D-030, D-056               |

## 1.3 What Cheby deliberately does not have

Each of these is a decision, not an omission. Proposals to add them should first read the cited decision.

| Absent feature                     | Instead                                             | Decision               |
| ---------------------------------- | --------------------------------------------------- | ---------------------- |
| `for`, `while`, `loop`             | Recursion with guaranteed tail calls, library folds | D-002, ADR-0001        |
| `if` / `else`                      | `case`, including subjectless `case { … }`          | D-002, D-109           |
| Mutable variables, cells or arrays | Immutable values, fiber-held state, Perceus reuse   | D-017, ADR-0007        |
| Shadowing (`let x = x + 1`)        | A new name for each step                            | D-051, D-062, ADR-0021 |
| Methods and `impl` blocks          | Module functions and `\|>`                          | D-061, ADR-0023        |
| Traits with explicit `impl`        | Structural interfaces                               | D-020, ADR-0010        |
| The `?` operator and exceptions    | `Result`, `Option` and `use`                        | D-018, ADR-0008        |
| `async`/`await`                    | Stackful fibers                                     | D-003, ADR-0002        |
| Macros and derive annotations      | Built-in equality, hashing and debug printing       | D-057, ADR-0022        |
| Semicolons                         | Newline-separated statements                        | D-051                  |
| Labelled arguments                 | Positional arguments                                | D-066                  |
| Block comments                     | `//` line comments                                  | D-078                  |
| `null`                             | `Option<T>`                                         | D-091                  |

## 1.4 A first look

A complete program that counts words in a file:

```cheby
//! Prints how often each word appears in input.txt.

import std::io
import std::list
import std::map
import std::result
import std::string

fn main() -> Result<Nil, io::Error> {
  use text <- result::try(io::read_file("input.txt"))
  text
  |> string::split(_, " ")
  |> count_words
  |> map::to_sorted_list
  |> list::each(_, print_entry)
  Ok(Nil)
}

fn count_words(words: List<String>) -> map::Map<String, Int> {
  list::fold(words, map::new(), fn(counts, word) {
    map::upsert(counts, word, fn(previous) {
      case previous {
        Some(n) => n + 1
        None => 1
      }
    })
  })
}

fn print_entry(entry: (String, Int)) {
  let (word, count) = entry
  io::println("{word}: {count}")
}
```

Recursion replaces loops. A local function can carry the loop state:

```cheby
fn sum(numbers: List<Int>) -> Int {
  fn step(acc, rest) {
    case rest {
      [] => acc
      [first, ..tail] => step(acc + first, tail)
    }
  }
  step(0, numbers)
}
```

Fibers run inside scopes. The scope waits for its children, and a panic in any child surfaces as an `Err`:

```cheby
import std::fiber

fn parallel_sum(left: List<Int>, right: List<Int>) -> Result<Int, fiber::Panic> {
  use scope <- fiber::scope()
  let a = fiber::spawn(scope, fn() { sum(left) })
  let b = fiber::spawn(scope, fn() { sum(right) })
  fiber::join(a) + fiber::join(b)
}
```

_Note:_ the standard-library function names in these examples (`fiber::spawn`, `fiber::join`, `map::upsert` and so on) are illustrative. The fiber and channel API names are provisional and will be refined in a standard-library specification (D-174, [chapter 10](10-concurrency.md)).

## 1.5 Implementation architecture (informative)

The reference implementation is written in Rust (D-010). A single mid-level IR feeds three backends (D-019):

- the Cranelift JIT, used by `cheby repl` and `cheby run`,
- Cranelift object output for ahead-of-time native binaries,
- a JavaScript emitter producing ES modules.

There is no interpreter tier. Generic code uses a uniform boxed representation, with small integers tagged and unboxed (D-021).

## 1.6 Stability and licensing

Before 1.0, any part of this specification may change. From 1.0 on, changes are additive only, with no editions (D-075). The compiler, runtime and standard library are dual-licensed under MIT and Apache-2.0 (D-089).
