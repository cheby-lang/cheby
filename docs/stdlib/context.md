# `std::context`

Values that a program provides for the duration of a callback, and that the code running meanwhile reads without being passed them (D-1044, ADR-0068). A logger, a clock or a database pool is provided once, near `main` or in a test, and read wherever it is needed. In the form `cheby doc` prints:

```cheby
pub type Key<T>
```

- **Keys.** A `Key<T>` names one value of type `T` and has a default. It is made by [`key`](#key) or [`key_without_default`](#key_without_default) as the whole initializer of a `const`, and any other call of them is a compile error. A key is identified by its constant's path, so two constants are two keys even when their defaults are equal. A key is as visible as its constant: a `priv` key is read and provided only in its own module, which then offers its own functions around it. Two keys are `==` when they are the same constant, and a key's debug text is `Key(<path>)`, as in `Key(my_app::db::pool)` (D-1045).
- **A fiber's context.** Each fiber has a context, which holds a value for each key that is provided to it ([§10.11](../spec/10-concurrency.md#1011-context-values)). [`provide`](#provide) binds a key while its body runs, and [`get`](#get) reads the innermost value or the default. When the body returns or unwinds, the fiber's context is again what it was before the call. So a `fiber::scope` or `fiber::timeout` that catches a panic continues with the context it was called with (D-1044).
- **Spawns.** Every spawn, scoped or detached, gives the new fiber the spawning fiber's context at the moment of the spawn. The new fiber keeps it after `provide` returns in the spawning fiber. The fiber that calls `spawn` decides, not the scope's owner (D-666). The body of a scope, the function of a timeout and a selector's mapping function run on the calling fiber, so they see its context (D-665, D-675, D-1044).
- **Entries.** `main`, each test and each REPL entry start with an empty context, in which every key has its default. Tests are root fibers of their own (D-723), so a value one test provides never reaches another test that runs at the same time.
- **Channels.** Context follows spawns, not messages. A fiber that was spawned earlier, or from elsewhere, keeps its own context when it receives a message, so an actor or a worker pool never sees its senders' context. To carry a value to such a fiber, send it with the message, and provide it in the receiver around the work it does for that message. See the example under [`provide`](#provide).
- **Immutable.** A provided value is an ordinary immutable value. Nothing in a context is mutated, and a value that another fiber inherits becomes shared, as a capture does (§9.4, D-072). The one exception is std's own: a seeded random stream, which each draw advances in the drawing fiber only ([`random::with_generator`](random.md#with_generator), D-1048).
- **Readers in std.** Std reads three dependencies from context: the clock of [`std::time`](time.md#clocks-in-context), the stream of `std::random`'s host functions, and the logger of `std::log`, once it is specified. No other std function reads context (D-1046).
- **Constants.** `key` and `key_without_default` run in constants, and they must, since keys are constants. Reaching `get` or `provide` while computing a constant is E0077 (D-533, D-1045).
- **Status.** This module starts tier 4: it is implemented in M4, natively and on JS together. Until then this page is a draft, and no code exists (D-1049).

## Keys

### `key`

```cheby
pub fn key<T>(default: T) -> Key<T>
```

Returns a key whose [`get`](#get) gives `default` when no value is provided. It must be the whole initializer of a `const` (D-1045).

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

Returns a key whose default is `None`, for a dependency that has no sensible default, such as a database pool. A value is provided as `Some(v)`, and a program that needs one asserts it with `let assert Some(v) = context::get(k)`, which panics with the location of the `let assert` when none is provided (§11.4). It must be the whole initializer of a `const` (D-1045).

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

fn submit(jobs: channel::Sender<(String, Option<String>)>, job: String) -> Result<Nil, (String, Option<String>)> {
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
