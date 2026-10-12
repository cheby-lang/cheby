# 11. Errors and panics

Cheby has two kinds of failure (D-018, ADR-0008):

- **Recoverable errors** are ordinary values of type `Result<T, E>` or `Option<T>`. They are handled with `case` and `use`.
- **Panics** are for bugs and unrecoverable situations. A panic unwinds the current fiber, not the whole program, and surfaces as an `Err` at the enclosing scope ([§11.7](#117-panics-and-fibers)).

There are no exceptions, no `try`/`catch`, and no `?` operator (D-018).

## 11.1 Result and Option

Both types are built-in ADTs in the prelude (D-091, D-112):

```cheby
pub exposed type Result<T, E> {
  Ok(T),
  Err(E),
}

pub exposed type Option<T> {
  Some(T),
  None,
}
```

Their constructors use the Rust names `Ok`/`Err` and `Some`/`None` (D-091). Functions over them live in `std::result` and `std::option`, which must be imported ([§7.6](07-modules-and-packages.md#76-prelude)).

There is no `null`. A value that may be absent has type `Option<T>`.

## 11.2 Propagating errors with use

Early return on error is written with `use` and an ordinary library function (D-018, D-023). `result::try` takes a `Result` and a callback. It calls the callback with the value when the result is `Ok`, and returns the `Err` unchanged otherwise:

```cheby
// in std::result
pub fn try<T, E, U>(r: Result<T, E>, next: fn(T) -> Result<U, E>) -> Result<U, E> {
  case r {
    Ok(value) => next(value)
    Err(e) => Err(e)
  }
}
```

With `use`, a sequence of fallible steps reads top to bottom:

```cheby
fn load_user(id: String) -> Result<User, AppError> {
  use key <- result::try(parse_id(id))
  use row <- result::try(db::find(key))
  decode_user(row)
}
```

Because shadowing is not allowed, every step needs its own name (D-051, ADR-0021).

Converting between error types is done with ordinary functions such as `result::map_error`. There is no implicit conversion.

The same pattern works for `Option` with `option::then`, and for any other callback-taking function ([§5.10](05-expressions.md#510-use)).

## 11.3 Unwinding

When a fiber panics or is cancelled, it **unwinds**: its stack frames are removed from the innermost outwards, and for each frame the runtime releases the references the frame holds (D-733, ADR-0061). Unwinding stops at the `scope` or `timeout` call that catches the panic or that the cancellation targets, because scope bodies and timeout functions run on the calling fiber ([§10.3](10-concurrency.md#103-scopes), [§10.7](10-concurrency.md#107-cancellation)). Otherwise it removes every frame and the fiber ends (D-665, D-667). As a consequence:

- no memory held by the fiber leaks because of a panic,
- the drop function of every handle whose last reference is released runs ([§9.5](09-memory-model.md#95-handles-and-drop-functions)),
- channels whose last `Sender` or `Receiver` was held by the fiber close ([§10.5.3](10-concurrency.md#1053-closing)).

User code cannot run during unwinding, except for handle drop functions. There is no `finally` and no `defer` (ADR-0029).

On native targets, unwinding propagates a flag through returns: a frame that unwinds runs the cleanup code of its mid-level IR and returns to its caller, which tests for the unwinding after each call that may unwind (D-698, D-733). On the JS target, the garbage collector reclaims ordinary memory, and the JS backend still releases handles at the same points ([§9.8](09-memory-model.md#98-memory-on-the-js-target)).

_Note:_ the mechanism releases the same references at the same points as any other would, so it does not change what a program can observe. The reference implementation measured it against Cranelift `try_call` landing pads before choosing it (D-702, ADR-0061).

## 11.4 let assert

```ebnf
let_assert_stmt = "let" "assert" pattern [ ":" type ] "=" expr [ "as" expr ] ;
```

`let assert` binds with a **refutable** pattern (D-068). If the value matches, the pattern's names are bound for the rest of the block, as with `let`. If it does not match, the fiber panics.

```cheby
let assert Ok(config) = load_config("app.toml")
let assert [first, ..] = arguments
```

The panic report includes the source location, which is the start of the pattern (D-570). Its message is `` `let assert` pattern did not match ``, followed by the line `value: <debug text>`, indented by two spaces, with the debug-printed value that failed to match (D-528).

An optional custom message follows `as`. It is an expression of type `String`, evaluated only when the match fails. The first line of the panic message is then `` `let assert` pattern did not match: `` followed by the custom message, and the value line still follows (D-159, D-528):

```cheby
let assert Some(user) = find_user(id) as "user {id} must exist"
```

_Example:_ when `id` is `7` and `find_user` returns `None`, the panic message is:

```
`let assert` pattern did not match: user 7 must exist
  value: None
```

A `let assert` whose pattern is irrefutable is a warning, since a plain `let` would do (D-159). Irrefutability follows the syntactic list of [§6.1](06-patterns.md#61-pattern-syntax), so `let assert True | False = b` does not warn (D-509).

## 11.5 assert

```ebnf
assert_stmt = "assert" expr [ "as" expr ] ;
```

`assert expr` evaluates an expression of type `Bool` and panics if it is `False` (D-096). It may be used anywhere a statement may appear, not only in tests (D-096).

A failed `assert e` has the panic message `assertion failed: <source text of e>`. When `e` is a comparison (`==`, `!=`, `<`, `<=`, `>`, `>=`), two more lines follow with the debug-printed values of both operands, labelled `left:` and `right:`. The labels are right-aligned, so `left:` is indented by two spaces and `right:` by one (D-096, D-528). The panic is located at the start of `e`, not at the `assert` keyword (D-570).

An optional custom message follows `as`. It is an expression of type `String`, evaluated only when the assertion fails. It replaces the source text in the first line, and the operand lines of a failing comparison still follow (D-160, D-528):

```cheby
assert list::length(items) == 3 as "expected one item per slot"
```

_Example:_ when `items` has two elements, the panic message is:

```
assertion failed: expected one item per slot
  left: 2
 right: 3
```

Without the custom message, its first line is `assertion failed: list::length(items) == 3`.

```cheby
test "parse numbers" {
  assert int::parse("42") == Ok(42)
  assert list::is_empty([])
}
```

Assertions are never removed from release builds.

## 11.6 panic and todo

`panic` and `todo` are keyword expressions of type `Never` ([§3.10](03-types.md#310-never)), so they can be used where any type is expected ([§3.12.5](03-types.md#3125-expected-types-and-conversions)) (D-068, D-161, D-235).

```ebnf
panic_expr = "panic" [ "as" expr ] ;
todo_expr  = "todo" [ "as" expr ] ;
```

The optional message follows `as` and is an expression of type `String`, the same message syntax as `assert` and `let assert` (D-161).

- `panic` or `panic as "computation failed"` panics the current fiber with the given message, or a default one (D-068, D-161).
- `todo` or `todo as "area of a dot"` marks unfinished code. The compiler emits a warning for every `todo` it compiles (D-068). At run time, reaching a `todo` panics with a message saying that the code is not implemented.

```cheby
fn area(shape: Shape) -> Float {
  case shape {
    Circle { radius } => 3.14159 * radius * radius
    Rect { width, height } => width * height
    Dot => todo as "area of a dot"
  }
}
```

Every panic report includes the source location where the panic started. Its message is the text after the `panicked at …:` line, without the location, and it is what a `fiber::Panic` carries as its message (D-473). A panic is reported on standard error, after standard output has been flushed, as two lines in the form rustc uses: `panicked at path:line:column:`, with the path and position as in diagnostics ([§13.8](13-tooling.md#138-diagnostics-and-warnings)), then the message (D-449). For example, reaching the `todo` above in `src/main.cheby` prints:

```
panicked at src/main.cheby:5:12:
not implemented: area of a dot
```

A panic is reported where its unwinding ends, so that a panic that is caught prints nothing (D-701):

- A panic that ends the root fiber running `main`, the root of a REPL entry or a detached fiber is reported on standard error once the fiber has unwound ([§10.4](10-concurrency.md#104-detached-fibers), [§10.10](10-concurrency.md#1010-program-entry-and-exit), [§13.5.4](13-tooling.md#1354-fibers-in-the-repl)). The two lines are written together, never interleaved with other output on standard error (D-669).
- A panic that a scope or a timeout catches is not reported. It becomes the `fiber::Panic` that the scope or the timeout returns ([§10.3](10-concurrency.md#103-scopes), [§10.8](10-concurrency.md#108-timeouts)) (D-666).
- A test's panic is reported only in the test's failure block ([§13.6](13-tooling.md#136-tests)) (D-595), and a constant's panic only in the constant's compile error ([§13.9](13-tooling.md#139-compile-time-evaluation)) (D-533).

`panic` without a message reports `explicit panic`, and a message given with `as` replaces it. `todo` reports `not implemented`, and `todo as m` reports `not implemented: m`. The runtime panics of [§11.8](#118-runtime-panics) report `integer overflow`, `division by zero`, `shift amount out of range`, `equality or hashing reached a function value or a handle`, `stack overflow` and `out of memory` (D-642), and the rules of scopes and `join` report `spawn into a scope that has ended` and `joined fiber failed` ([§10.3](10-concurrency.md#103-scopes)) (D-666, D-668). Failed assertions report `assertion failed` and `` `let assert` pattern did not match ``, with the details of [§11.4](#114-let-assert) and [§11.5](#115-assert) (D-449, D-528).

A panic of a built-in operator is located at the start of the whole operator expression: the start of the left operand of a binary operator, and the `-` of a negation (D-570). This covers integer overflow, division by zero, a shift amount out of range, and a `==` or `!=` that reaches a function value or a handle (D-556). In `a + b * c`, an overflow of `*` is located at the start of `b`. A failed `assert e`, with or without `as`, is located at the start of its condition `e`, not at the `assert` keyword, and a failed `let assert` at the start of its pattern (D-570). _Note:_ when the standard library compares or hashes values inside a runtime kernel, as a `Map` lookup does, the reference implementation knows no Cheby location and prints `<unknown location>` in its place (D-449, D-556). An `out of memory` panic is located at `<unknown location>` too (D-642), and so are `spawn into a scope that has ended` and `joined fiber failed` (D-732). Built-in code with no source, the `show` of tuples, is located at `<builtin>`, so a stack overflow inside it reports `panicked at <builtin>` (D-583).

## 11.7 Panics and fibers

A panic affects only the fiber in which it happens (D-018):

1. The fiber unwinds ([§11.3](#113-unwinding)). If the panic happened in the body of a scope or the function of a timeout that the fiber runs, the innermost such `scope` or `timeout` call catches it and the fiber goes on. Otherwise the fiber ends (D-665).
2. The scope that catches the panic, or that the ended fiber belongs to, records it when it starts, cancels its other members, and returns `Err` describing the panic once they have all ended. The panic is not printed ([§10.3](10-concurrency.md#103-scopes)) (D-040, D-666). A timeout returns `Err(Panicked(p))` in the same way ([§10.8](10-concurrency.md#108-timeouts)).
3. If the fiber is detached, its panic is printed to standard error as a panic report and nothing else happens ([§10.4](10-concurrency.md#104-detached-fibers)) (D-669).
4. If the fiber is the root fiber running `main`, the program prints the panic and exits with status 101 ([§10.10](10-concurrency.md#1010-program-entry-and-exit), D-073, D-182).

A deadlock ([§10.4](10-concurrency.md#104-detached-fibers)) is not a panic of any fiber. No scope or timeout can catch it, nothing unwinds, and the program exits with status `103` instead of `101`, so that scripts can tell the two apart (D-724).

Structured concurrency is therefore the way to recover from panics: code that must survive a failure runs its risky part in a scope and inspects the `Result` (ADR-0019).

```cheby
fn handle_request(req: Request) -> Response {
  let outcome = {
    use _scope <- fiber::scope()
    route(req)
  }
  case outcome {
    Ok(response) => response
    Err(_) => response::internal_error()
  }
}
```

## 11.8 Runtime panics

The following are panics, not undefined behavior or silent results:

| Condition                                                              | Decision            |
| ---------------------------------------------------------------------- | ------------------- |
| integer overflow in arithmetic, negation or shifts                     | D-025, D-092        |
| integer division or remainder by zero                                  | D-069               |
| shift by a negative amount or by the type's width or more              | D-092               |
| an `Int` leaving the safe-integer range on JS                          | D-037               |
| `==` or hashing reaching a function value or a handle                  | D-070, D-189, D-233 |
| a failed `let assert` or `assert`                                      | D-068, D-096        |
| reaching `panic` or `todo`                                             | D-068               |
| list index out of range, in the standard library's panicking accessors | D-039               |
| spawning into a scope that has ended                                   | D-666               |
| joining a failed fiber, by a joiner that is not being cancelled        | D-668               |
| stack overflow                                                         | see below           |
| an allocation that cannot be served, on native targets                 | D-642, see below    |

### 11.8.1 Stack overflow

Deep non-tail recursion can exhaust a fiber's stack. Exhausting it panics the fiber, which then unwinds like any other panic (D-094):

- On native targets, every function entry compares the stack pointer with the fiber's stack limit, in the same check as the preemption yield check (D-028, D-124, ADR-0032). When the limit is reached, the fiber panics. Overflow is detected by this check, not by per-fiber guard pages (D-124, D-678). The default limit is 8 MiB per fiber, with more for the root fiber, and a package can change both ([§10.1](10-concurrency.md#101-fibers)) (D-125, D-660).
- On the JS target, the engine's stack limit is much lower (roughly ten thousand frames), so a program may overflow on JS where it does not natively (D-094, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

Tail calls never grow the stack ([§5.15.3](05-expressions.md#5153-guaranteed-tail-calls)), and all standard-library functions are written so that their stack use does not depend on the size of their input (D-094).

Foreign functions do not perform the entry check. The runtime must leave enough headroom below the limit for foreign calls, or run them on a separate stack (ADR-0032).

_Note:_ the reference implementation gives the root fiber a 64 MiB stack and leaves 256 KiB of headroom below the limit (D-457). Each test gets a 64 MiB stack too, so deep recursion that works under `cheby run` also works in a test (D-596), and so does the body of each REPL entry (D-613). The `main-stack` key of the manifest's `[runtime]` table sets all three, and `fiber-stack` sets the stack of other fibers ([§13.3](13-tooling.md#133-manifest)) (D-660). After an overflow, the fiber unwinds within that headroom (D-700). Builds of the reference implementation with debug assertions protect the lowest page of each stack, so that foreign or runtime code that overflows below the last entry check crashes instead of corrupting memory (D-678).

### 11.8.2 Out of memory

On native targets, an allocation that cannot be served panics the running fiber with the message `out of memory`. This covers a size that overflows and a request that the system refuses, for objects, text and the results that standard-library functions such as `string::repeat` compute. It is a panic like any other, not an abort, so a REPL session and a test run survive it: `cheby run` exits with 101, `cheby test` fails the test, and the REPL unwinds the entry and continues (D-642, D-712). One case is fatal instead: if the system refuses memory while the runtime marks a value as shared after its reference count overflowed, the process ends (D-765).

During constant evaluation, a request over the memory limit, including one whose size overflows, is E0076 instead ([§13.9](13-tooling.md#139-compile-time-evaluation), D-617). A request under the limit that the system refuses is the panic, so the constant gets E0075 (D-642).

_Note:_ the reference implementation sets aside a 4 MiB reserve per process and frees it before it reports, so even complete exhaustion ends as this panic. The panic is located at `<unknown location>` (D-642).
