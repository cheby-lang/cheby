# 11. Errors and panics

Cheby has two kinds of failure (D-018, ADR-0008):

- **Recoverable errors** are ordinary values of type `Result<T, E>` or `Option<T>`. They are handled with `case` and `use`.
- **Panics** are for bugs and unrecoverable situations. A panic ends the current fiber, not the whole program, and surfaces as an `Err` at the enclosing scope ([§11.7](#117-panics-and-fibers)).

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

When a fiber panics or is cancelled, it **unwinds**: its stack frames are removed from the innermost outwards, and for each frame the runtime releases the references the frame holds (D-077, ADR-0026). As a consequence:

- no memory held by the fiber leaks because of a panic,
- the drop function of every handle whose last reference is released runs ([§9.5](09-memory-model.md#95-handles-and-drop-functions)),
- channels whose last `Sender` or `Receiver` was held by the fiber close ([§10.5.3](10-concurrency.md#1053-closing)).

User code cannot run during unwinding, except for handle drop functions. There is no `finally` and no `defer` (ADR-0029).

On native targets, unwinding uses Cranelift `try_call` landing pads and its unwinder (D-077). On the JS target, the garbage collector reclaims ordinary memory, and the JS backend still releases handles at the same points ([§9.8](09-memory-model.md#98-memory-on-the-js-target)).

## 11.4 let assert

```ebnf
let_assert_stmt = "let" "assert" pattern [ ":" type ] "=" expr [ "as" expr ] ;
```

`let assert` binds with a **refutable** pattern (D-068). If the value matches, the pattern's names are bound for the rest of the block, as with `let`. If it does not match, the fiber panics.

```cheby
let assert Ok(config) = load_config("app.toml")
let assert [first, ..] = arguments
```

The panic message includes the source location and the debug-printed value that failed to match.

An optional custom message follows `as`. It is an expression of type `String`, evaluated only when the match fails, and it is included in the panic message (D-159):

```cheby
let assert Some(user) = find_user(id) as "user {id} must exist"
```

A `let assert` whose pattern is irrefutable is a warning, since a plain `let` would do (D-159).

## 11.5 assert

```ebnf
assert_stmt = "assert" expr [ "as" expr ] ;
```

`assert expr` evaluates an expression of type `Bool` and panics if it is `False` (D-096). It may be used anywhere a statement may appear, not only in tests (D-096).

When the asserted expression is a comparison (`==`, `!=`, `<`, `<=`, `>`, `>=`), the panic message shows the debug-printed values of both operands (D-096). Otherwise it shows the source text of the expression.

An optional custom message follows `as`. It is an expression of type `String`, evaluated only when the assertion fails, and it is shown alongside the automatically printed operands of a failing comparison (D-160):

```cheby
assert list::length(items) == 3 as "expected one item per slot"
```

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

Every panic message includes the source location where the panic started.

## 11.7 Panics and fibers

A panic affects only the fiber in which it happens (D-018):

1. The fiber unwinds ([§11.3](#113-unwinding)) and ends.
2. If the fiber belongs to a scope, the scope cancels its other fibers and its body, and returns `Err` describing the panic once they have all ended ([§10.3](10-concurrency.md#103-scopes), D-040).
3. If the fiber is detached, its panic is printed to standard error and nothing else happens ([§10.4](10-concurrency.md#104-detached-fibers)).
4. If the fiber is the root fiber running `main`, the program prints the panic and exits with status 101 ([§10.10](10-concurrency.md#1010-program-entry-and-exit), D-073, D-182).

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
| stack overflow                                                         | see below           |

### 11.8.1 Stack overflow

Deep non-tail recursion can exhaust a fiber's stack. Exhausting it panics the fiber, which then unwinds like any other panic (D-094):

- On native targets, every function entry compares the stack pointer with the fiber's stack limit, in the same check as the preemption yield check (D-028, D-124, ADR-0032). When the limit is reached, the fiber panics. There are no per-fiber guard pages. The default limit is 8 MiB per fiber, with more for the root fiber (D-125).
- On the JS target, the engine's stack limit is much lower (roughly ten thousand frames), so a program may overflow on JS where it does not natively (D-094, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

Tail calls never grow the stack ([§5.15.3](05-expressions.md#5153-guaranteed-tail-calls)), and all standard-library functions are written so that their stack use does not depend on the size of their input (D-094).

Foreign functions do not perform the entry check. The runtime must leave enough headroom below the limit for foreign calls, or run them on a separate stack (ADR-0032).
