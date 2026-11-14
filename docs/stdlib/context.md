# `std::context`

Values that a program provides for the duration of a callback, and that the code running meanwhile reads without being passed them (D-1044, ADR-0068). A logger, a clock or a database pool is provided once, near `main` or in a test, and read wherever it is needed. In the form `cheby doc` prints:

```cheby
pub type Key<T>

pub type Context
```

- **Keys.** A `Key<T>` names one value of type `T` and has a default. It is made by [`key`](#key) or [`key_without_default`](#key_without_default) as the whole initializer of a `const`. Every other mention of the two functions is a compile error: a call anywhere else, a function value such as `let make = context::key`, and a pipe stage (D-1045, D-1565). A key is as visible as its constant: a `priv` key is read and provided only in its own module, which then offers its own functions around it.
- **A key's identity.** A key is identified by its constant's path and the type of its values, so two constants are two keys even when their defaults are equal. A constant initialized from another key constant, as in `const alias: context::Key<Int> = port`, is the same key. Two keys are `==` when they are the same key, and a key's debug text is `Key(<path>)`, as in `Key(my_app::db::pool)`. In the REPL, redefining a key's constant with the same type keeps the key and changes its default. Redefining it with another type makes a new key, for which no fiber has a value, as a redefined type is a new type (§13.5.2, D-1045, D-1564).
- **A fiber's context.** Each fiber has a context, which holds a value for each key that is provided to it ([§10.11](../spec/10-concurrency.md#1011-context-values)). [`provide`](#provide) binds a key while its body runs, and [`get`](#get) reads the innermost value or the default. When the body returns or unwinds, the fiber's context is again what it was before the call. So a `fiber::scope` or `fiber::timeout` that catches a panic continues with the context it was called with (D-1044).
- **Spawns.** Every spawn, scoped or detached, gives the new fiber the spawning fiber's context at the moment of the spawn. The new fiber keeps it after `provide` returns in the spawning fiber. The fiber that calls `spawn` decides, not the scope's owner (D-666). The body of a scope, the function of a timeout and a selector's mapping function run on the calling fiber, so they see its context (D-665, D-675, D-1044).
- **Entries.** `main`, each test and each REPL entry start with an empty context, in which every key has its default. Tests are root fibers of their own (D-723), so a value one test provides never reaches another test that runs at the same time.
- **Channels.** Context follows spawns, not messages. A fiber that was spawned earlier, or from elsewhere, keeps its own context when it receives a message, so an actor or a worker pool never sees its senders' context. To carry one value to such a fiber, send it with the message, and provide it in the receiver around the work it does for that message, as in the example under [`provide`](#provide). To carry the whole context, send a captured one and run the work under it, as in the example under [`with`](#with).
- **Captured contexts.** A `Context` is a fiber's whole context as a value. [`capture`](#capture) takes it, [`with`](#with) runs a body under it in place of the caller's context, and [`empty`](#empty) returns the context in which every key has its default. A `Context` is a handle (§9.5): it prints as `<Context>`, `==` and hashing panic when they reach one, and no constant may hold one (§3.13, §4.5). It may be sent on a channel, and it keeps its values alive as a spawned fiber does (D-1566, ADR-0080).
- **Immutable.** A provided value is an ordinary immutable value. Nothing in a context is mutated, and a value that another fiber inherits becomes shared, as a capture does (§9.4, D-072). The one exception is std's own: a seeded random stream, which each draw advances in the drawing fiber only ([`random::with_generator`](random.md#with_generator), D-1048). No key names the stream, and only `std::random` reaches it (D-1574).
- **Readers in std.** Std reads three dependencies from context: the clock of [`std::time`](time.md#clocks-in-context), the stream of `std::random`'s host functions, and the logger of `std::log`, once it is specified. No other std function reads context (D-1046).
- **Constants.** `key` and `key_without_default` run in constants, and they must, since keys are constants. Reaching `get`, `provide`, `capture`, `with` or `empty` while computing a constant is E0077 (D-533, D-1045, D-1566).
- **Written in Cheby.** `provide` and `with` are Cheby over private kernels that swap the fiber's context and swap it back when the body returns. When the body unwinds, the scope that catches the failure puts back the context it was opened with. No Cheby code runs while a fiber unwinds, so a program sees the context restored in both cases (D-1573). A fiber's bindings are a persistent map in the runtime, so a spawn and a capture copy one pointer (D-1571).
- **On JS.** A Cheby function that a fiber passes to foreign code takes that fiber's context with it, as [`capture`](#capture) would, and the detached fiber of each callback starts with it (§12.5, D-191, D-1569).
- **Status.** This module starts tier 4. It is implemented in M4, on native first and on JS with the JS runtime. Until then this page is a draft, and no code exists (D-1049, D-1563).

## Keys

### `key`

```cheby
pub fn key<T>(default: T) -> Key<T>
```

Returns a key whose [`get`](#get) gives `default` when no value is provided. It must be the whole initializer of a `const`, and `key` cannot be used in any other way (D-1045, D-1565).

```cheby
const greeting: context::Key<String> = context::key("hello")

test "get gives the default when nothing is provided" {
  assert context::get(greeting) == "hello"
}
```

### `key_without_default`

```cheby
pub fn key_without_default<T>() -> Key<Option<T>>
```

Returns a key whose default is `None`, for a dependency that has no sensible default, such as a database pool. A value is provided as `Some(v)`, and a program that needs one asserts it with `let assert Some(v) = context::get(k)`, which panics with the location of the `let assert` when none is provided (§11.4). It must be the whole initializer of a `const`, as [`key`](#key) must (D-1045, D-1565).

```cheby
const port: context::Key<Option<Int>> = context::key_without_default()

test "a key without a default gives None until a value is provided" {
  assert context::get(port) == None
  use <- context::provide(port, Some(8080))
  assert context::get(port) == Some(8080)
}
```

## Reading and providing

### `get`

```cheby
pub fn get<T>(key: Key<T>) -> T
```

Returns the innermost value that the calling fiber has for `key`, or the key's default when it has none.

- **Cost:** O(log k) in the number of keys the fiber has values for, effectively constant. Informative.

The examples of [`key`](#key) and [`provide`](#provide) use it.

### `provide`

```cheby
pub fn provide<T, R>(key: Key<T>, value: T, body: fn() -> R) -> R
```

Calls `body` on the calling fiber with `key` bound to `value`, and returns what `body` returns. Inside `body`, `get(key)` gives `value`, unless a nested `provide` binds the key again. Every fiber spawned while `body` runs inherits the binding and keeps it after `body` returns. When `body` returns or unwinds, the fiber's context is again what it was before the call. It is written with `use` (§5.10).

- **Cost:** O(log k) in the number of keys the fiber has values for, plus the call of `body`. Informative.

```cheby
test "provide binds a value while its body runs" {
  let inside = context::provide(greeting, "hi", fn() { context::get(greeting) })
  assert inside == "hi"
  assert context::get(greeting) == "hello"
}

test "the innermost value wins" {
  use <- context::provide(greeting, "outer")
  let inner = context::provide(greeting, "inner", fn() { context::get(greeting) })
  assert inner == "inner"
  assert context::get(greeting) == "outer"
}

test "a spawned fiber inherits the context" {
  use <- context::provide(greeting, "hi")
  let outcome = {
    use owner <- fiber::scope()
    fiber::join(fiber::spawn(owner, fn() { context::get(greeting) }))
  }
  assert outcome == Ok("hi")
}

test "a caught panic restores the context" {
  let outcome = {
    use _ <- fiber::scope()
    context::provide(greeting, "inside", fn() { panic as "boom" })
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "boom"
  assert context::get(greeting) == "hello"
}
```

_Example_, not a `test` because it needs a worker that runs until its channel closes. A worker spawned before the request keeps its own context, so the sender passes the value with each job:

```cheby
const request_id: context::Key<Option<String>> = context::key_without_default()

fn submit(
  jobs: channel::Sender<(String, Option<String>)>,
  job: String,
) -> Result<Nil, (String, Option<String>)> {
  channel::send(jobs, (job, context::get(request_id)))
}

fn work(jobs: channel::Receiver<(String, Option<String>)>) -> Nil {
  case channel::receive(jobs) {
    Ok((job, id)) => {
      context::provide(request_id, id, fn() { run(job) })
      work(jobs)
    }
    Err(Nil) => Nil
  }
}
```

## Captured contexts

### `capture`

```cheby
pub fn capture() -> Context
```

Returns the calling fiber's whole context as a value: every binding the fiber has at the call. Later calls of [`provide`](#provide) do not change it.

While a seeded random stream is in context, `capture` counts as a spawn: the captured context holds the stream state that a fiber spawned at that point would start from, and the calling fiber's own stream is left alone ([`random::with_generator`](random.md#with_generator), D-1567). A context captured with no stream holds none.

- **Cost:** O(1). Informative.

The examples of [`with`](#with) use it.

### `with`

```cheby
pub fn with<R>(captured: Context, body: fn() -> R) -> R
```

Calls `body` on the calling fiber with `captured` in place of the fiber's context, and returns what `body` returns. It is written with `use` (§5.10). Inside `body`:

1. `get(key)` gives the value `captured` has for `key`, or the key's default. The caller's own bindings are not visible, as they would not be in a fiber spawned where `captured` was taken.
2. `provide` binds on top of `captured`, and every fiber spawned meanwhile inherits the context `body` has then.
3. If `captured` holds a seeded stream, the host functions of `std::random` draw from it, starting from the captured state, and each draw advances it for `body` alone. If it holds none, they draw from the host, also when the calling fiber has a stream of its own (D-1567).

When `body` returns or unwinds, the fiber's context and its random stream are again what they were before the call. So every `with` of one captured context starts its stream from the same state, and two of them draw the same numbers. A program that wants different numbers captures once per job (D-1566, D-1567, ADR-0080).

- **Cost:** O(1), plus the call of `body`. Informative.

```cheby
test "with runs a body under a captured context" {
  let captured = context::provide(greeting, "hi", fn() { context::capture() })
  assert context::get(greeting) == "hello"
  let inside = context::with(captured, fn() { context::get(greeting) })
  assert inside == "hi"
  assert context::get(greeting) == "hello"
}

test "with hides the caller's own bindings" {
  let captured = context::capture()
  use <- context::provide(greeting, "outer")
  let inside = context::with(captured, fn() { context::get(greeting) })
  assert inside == "hello"
  assert context::get(greeting) == "outer"
}

test "provide binds on top of a captured context" {
  let captured = context::provide(greeting, "hi", fn() { context::capture() })
  use <- context::with(captured)
  assert context::get(greeting) == "hi"
  let inner = context::provide(greeting, "inner", fn() { context::get(greeting) })
  assert inner == "inner"
}

test "a fiber spawned under with inherits the captured context" {
  let captured = context::provide(greeting, "hi", fn() { context::capture() })
  let outcome = {
    use <- context::with(captured)
    use owner <- fiber::scope()
    fiber::join(fiber::spawn(owner, fn() { context::get(greeting) }))
  }
  assert outcome == Ok("hi")
}

test "two bodies under one captured context draw the same numbers" {
  use <- random::with_generator(random::from_seed(7))
  let captured = context::capture()
  let first = context::with(captured, fn() { random::fraction() })
  let again = context::with(captured, fn() { random::fraction() })
  assert first == again
}
```

_Example_, not a `test` because it needs a worker that runs until its channel closes. The sender captures its context with each job, and the worker runs the job under it, so `run` reads every key as the sender would:

```cheby
fn submit(
  jobs: channel::Sender<(String, context::Context)>,
  job: String,
) -> Result<Nil, (String, context::Context)> {
  channel::send(jobs, (job, context::capture()))
}

fn work(jobs: channel::Receiver<(String, context::Context)>) -> Nil {
  case channel::receive(jobs) {
    Ok((job, captured)) => {
      context::with(captured, fn() { run(job) })
      work(jobs)
    }
    Err(Nil) => Nil
  }
}
```

### `empty`

```cheby
pub fn empty() -> Context
```

Returns the context in which every key has its default and no seeded stream is set, which is the context of `main` when it starts. `with(context::empty(), body)` runs `body` apart from everything provided around it: a long-lived detached fiber spawned there keeps none of a request's values alive. It is a function, not a constant, because a constant may not hold a handle (§4.5, D-1568).

- **Cost:** O(1). Informative.

```cheby
test "the empty context has every default" {
  use <- context::provide(greeting, "hi")
  let inside = context::with(context::empty(), fn() { context::get(greeting) })
  assert inside == "hello"
}
```
