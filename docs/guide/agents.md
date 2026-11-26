# Writing Cheby: a guide for coding agents

Cheby is a statically typed, expression-oriented language with immutable data, pattern matching and
lightweight fibers. Its syntax looks like Rust and its semantics are closer to Gleam, so code
written from habit is usually wrong in a few places. Check instead of guessing: the commands below
answer most questions in one call.

## The loop

```sh
cheby check                        # check the package around the current directory
cheby check --fix                  # apply the fixes the compiler is sure of, then check again
cheby check --format json          # diagnostics as one JSON object per line, on standard error
cheby doc list::map                # a standard-library function: signature and doc comment
cheby doc std::list                # every item of a module, with one sentence each
cheby doc std                      # the modules of the standard library
cheby doc parse                    # every item with that name, or with it in its name
cheby explain E0042                # the long explanation of a diagnostic code
cheby run                          # run `main`
cheby test                         # run the `test` blocks
cheby test --report-failures-only  # only the failures and the summary
cheby fmt                          # format every file of the package
```

One file needs no package. `cheby check probe.cheby`, `cheby run probe.cheby` and
`cheby test probe.cheby` take a lone file, which may import the standard library only.

Read the `help:` line of a diagnostic before you change anything else: it usually names the Cheby
form of what you wrote. After a syntax error in one module, the modules that import it are not
checked, and a `note:` line names them. Look a function up with `cheby doc` before you call it:
names such as `len`, `reduce` or `to_string` are not Cheby's.

## What is different

- There is no `if`, `for`, `while` or `loop`. Branch with `case`, and iterate with recursion or
  with the functions of `std::list`.
- There is no `return`. A block's value is its last expression. Leave early with
  `guard cond else { … }` or `let guard pattern = value else { … }`.
- Nothing is mutable, and a name cannot be bound twice in a scope: every `let` needs a new name.
- There are no methods. Call the module's function: `list::length(xs)`, not `xs.length()`.
- A pipe needs a hole: `xs |> list::map(_, f)`. Only a function of one parameter is piped bare:
  `xs |> list::length`.
- Errors are values: `Result` and `Option`. There is no `?`, no `null` and no exception. Chain
  with `use x <- result::try(…)`.
- `Bool` is `True` and `False`, the value with no information is `Nil`, and types and constructors
  start with an upper-case letter: `Int`, `String`, `List<Int>`, `Some(1)`.
- A top-level function has a full signature. Types are inferred only inside bodies.
- A string interpolates names and fields only: `"{name}"`, `"{user.name}"`, and `"{value:?}"` for
  debug output. Bind any other expression to a name first.
- A tuple has no `.0`. Take it apart with a pattern: `let (a, b) = pair`.
- Comments are `//`. `///` documents the item below it and `//!` the module.
- Imports come before the first item, in up to three blocks with a blank line between them: `std`,
  other packages, then the package's own modules, each sorted by path. `cheby fmt` and
  `cheby check --fix` put them in order.
- There are no semicolons, and indentation is two spaces. `cheby fmt` gives the one accepted form.

## Data, `case` and recursion

```cheby
//! Shapes and their areas.

import std::io
import std::list

type Shape {
  Square(Int),
  Rectangle { width: Int, height: Int },
}

fn area(shape: Shape) -> Int {
  case shape {
    Square(side) => side * side
    Rectangle { width, height } => width * height
  }
}

/// Adds the areas up by recursion.
fn total(shapes: List<Shape>, sum: Int) -> Int {
  case shapes {
    [] => sum
    [first, ..rest] => total(rest, sum + area(first))
  }
}

/// The same with the standard library, through a pipe.
fn total_piped(shapes: List<Shape>) -> Int {
  shapes
  |> list::map(_, area)
  |> list::sum(_, 0)
}

/// A `case` without a subject takes the first condition that holds.
fn size(n: Int) -> String {
  case {
    n < 10 => "small"
    n < 100 => "medium"
    _ => "large"
  }
}

fn main() {
  let shapes = [Square(2), Rectangle { width: 2, height: 3 }]
  let sum = total(shapes, 0)
  let word = size(sum)
  io::println("{sum} is {word}")
}

test "both totals agree" {
  let shapes = [Square(3), Rectangle { width: 1, height: 4 }]
  assert total(shapes, 0) == 13
  assert total_piped(shapes) == 13
}
```

## Errors and early exits

```cheby
//! Adding numbers that arrive as text.

import std::int
import std::result
import std::string

type ParseError {
  NotANumber(String),
}

fn parse(text: String) -> Result<Int, ParseError> {
  result::map_error(int::parse(string::trim(text)), fn(_) { NotANumber(text) })
}

/// `use` binds the `Ok` value and returns the `Err` at once.
fn add(a: String, b: String) -> Result<Int, ParseError> {
  use x <- result::try(parse(a))
  use y <- result::try(parse(b))
  Ok(x + y)
}

/// `let guard` leaves the function when the pattern does not match.
fn first_or_zero(xs: List<Int>) -> Int {
  let guard [first, ..] = xs else {
    0
  }
  first
}

test "add parses both sides" {
  assert add("1", " 2 ") == Ok(3)
  assert add("1", "x") == Err(NotANumber("x"))
  assert first_or_zero([]) == 0
  assert first_or_zero([7, 8]) == 7
}
```

## Habits and their Cheby forms

| Habit                       | Cheby                                         |
| --------------------------- | --------------------------------------------- |
| `if c { a } else { b }`     | `case c { … }` with the arms `True => a` and `False => b` |
| `for x in xs { … }`         | `list::each(xs, fn(x) { … })`, or recursion   |
| `xs.len()`, `xs.map(f)`     | `list::length(xs)`, `list::map(xs, f)`        |
| `let mut n = 0; n += 1`     | a new name, or a parameter of a recursive `fn` |
| `value?`                    | `use x <- result::try(value)`                 |
| `null`, `nil`               | `None` of `Option`, or `Nil`                  |
| `true`, `false`             | `True`, `False`                               |
| `x as Float`                | a function, such as `float::from_int(x)`      |
| `match x { a -> b }`        | `case x { a => b }`                           |
| `println!("{}", x)`         | `io::println("{x}")` after `import std::io`   |
