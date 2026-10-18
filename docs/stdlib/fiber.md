# `std::fiber`

Fibers, scopes, joining, detached fibers, cancellation and timeouts ([§10.1](../spec/10-concurrency.md#101-fibers) to [§10.4](../spec/10-concurrency.md#104-detached-fibers), [§10.7](../spec/10-concurrency.md#107-cancellation), [§10.8](../spec/10-concurrency.md#108-timeouts)). This file fixes the names that chapter 10 left provisional (D-174, D-199, D-646). In the form `cheby doc` prints:

```cheby
pub type Scope

pub type Fiber<T>

pub type Panic

pub exposed type Timeout {
  TimedOut,
  Panicked(Panic),
}
```

- **Handles.** `Scope` and `Fiber<T>` are handles (§9.5). A scope prints as `<Scope>` and a fiber as `<Fiber>`, `==` and hashing panic when they reach one, and no constant may hold one (§3.13, §4.5, D-233, D-647).
- **`Panic`.** The error of a failed scope: the message and the location of the first panic in the scope (D-178, D-473). It is opaque and plain data. Two panics are `==` when their messages and their locations are equal, and equal panics hash equally. Its debug text is `Panic { message: "…", location: "…" }`, with both texts escaped as debug text escapes a `String` (D-648). It satisfies `Show` through [`show`](#show).
- **`Timeout`.** The error of [`timeout`](#timeout). It is `exposed`, so other packages match `fiber::TimedOut` and `fiber::Panicked(p)` in patterns and compare with `==` (D-181, D-199, D-649).
- **Written in Cheby.** Every function is written in Cheby over private runtime operations, kernels and compiler intrinsics, that only `std` can call. None of them calls a Cheby closure, so D-484 holds, and their costs are informative (D-675, D-692, D-693).
- **Sharing.** A value becomes shared when it moves to another fiber (§9.4): the captures of a spawned function and the result of a fiber. The body of `scope` and the function of `timeout` run on the calling fiber, so nothing they capture or return is marked. A `Panic` is built on the fiber that reads it, so it never crosses fibers (D-072, D-665, D-674).
- **Deadlock.** When no fiber is running or can run, no timer, blocking job or IO registration is pending, and the root fiber has not finished, every fiber would wait forever. Every build detects this as a deadlock: `cheby run` writes `fatal error: all fibers are blocked` and one line per blocked fiber to standard error, and exits with status 103. A deadlock is not a panic, so no scope or timeout catches it, and in `cheby run` nothing unwinds before the exit. `cheby test` judges each test's domain on its own and fails only the deadlocked test, and the REPL reports a deadlock of the entry and the session's detached fibers, cancels the entry and continues (§10.4, §13.5.4, §13.6, D-724, D-857, D-859). Each line names the operation, `channel::send`, `channel::receive`, `selector::select`, `fiber::join`, `fiber::scope` or `fiber::timeout`, and the location of the innermost call outside `std` (D-856, D-858).
- **On JS.** A fiber runs until its next real suspension point, so cancellation and timeouts take effect only there (D-185, D-232). Everything else in this file holds on every target.

## Scopes

### `scope`

```cheby
pub fn scope<T>(body: fn(Scope) -> T) -> Result<T, Panic>
```

Opens a new scope, calls `body` with it on the calling fiber, and returns `Ok` with `body`'s value once `body` has returned and every fiber spawned into the scope has ended. It is written with `use` (§5.10).

A scope's **members** are its body, which is the calling fiber's frames above the `scope` call, and the fibers spawned into it (D-040, D-665, D-666):

1. **Open.** The scope is open from the call of `body` until it **ends**, which is when `body` has finished and every fiber spawned into it has ended. While it is open, any fiber that holds the `Scope` value may spawn into it, including while the scope waits for its fibers. A fiber spawned during the wait is waited for too.
2. **Waiting.** `scope` releases `body` as soon as the call returns, before it waits, so the captures of `body` close their channels at that point (D-205, D-670). A scope never returns, and its frame is never removed by unwinding, while any of its fibers is running.
3. **Failure.** The first member to panic makes the scope **fail**. The scope records that panic when it starts, before the panicking member unwinds, and cancels every other member. Once every member has ended, `scope` returns `Err` with the recorded `Panic`. A panic of the body itself stops unwinding at the `scope` call. A panic of a member after the scope has failed or has been cancelled is not recorded (D-176, D-178, D-666).
4. **Silence.** A panic that a scope catches is not printed. Its report is the `Panic` that `scope` returns (§11.7, D-666, D-701).
5. **Cancellation from outside.** When the fiber that opened the scope is cancelled for an enclosing scope or a timeout, the scope's fibers are cancelled with it, and `scope` never returns ([Cancellation](#cancellation)).

When the scope succeeds, the value of `body` is returned. When it fails, the value is released.

- **Fails:** `Err(p)` when a member panicked, with the first panic `p`.
- **Cost:** O(1), plus the wait. Informative.

```cheby
test "a scope returns its body's value once its fibers have ended" {
  let (tx, rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() { channel::send(tx, 1) })
    channel::receive(rx)
  }
  assert outcome == Ok(Ok(1))
}

test "a scope waits for every fiber spawned into it" {
  let (tx, rx) = channel::new::<Int>(3)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() {
      fiber::spawn(owner, fn() { channel::send(tx, 2) })
    })
    Nil
  }
  assert outcome == Ok(Nil)
  assert channel::receive(rx) == Ok(2)
  assert channel::receive(rx) == Err(Nil)
}

test "a panic in the body is caught by its scope" {
  let outcome = {
    use _scope <- fiber::scope()
    panic as "boom"
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "boom"
}

test "a panic in a fiber cancels the body" {
  let (keep, never) = channel::new::<Int>(1)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() { panic as "first" })
    // Nothing is ever sent on `never`, so only the cancellation ends
    // this wait.
    channel::receive(never)
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "first"
  // Unwinding the body released `never`, the only `Receiver`.
  assert channel::send(keep, 1) == Err(1)
}

test "a nested scope keeps a fiber's panic to itself" {
  let outcome = {
    use outer <- fiber::scope()
    let inner = fiber::spawn(outer, fn() {
      use owner <- fiber::scope()
      fiber::spawn(owner, fn() { panic as "inner" })
    })
    case fiber::join(inner) {
      Ok(_) => "no panic"
      Err(failure) => fiber::message(failure)
    }
  }
  assert outcome == Ok("inner")
}

test "fibers and scopes print as placeholders" {
  let outcome = {
    use owner <- fiber::scope()
    let child = fiber::spawn(owner, fn() { Nil })
    "{owner:?} {child:?}"
  }
  assert outcome == Ok("<Scope> <Fiber>")
}
```

## Spawning

### `spawn`

```cheby
pub fn spawn<T>(owner: Scope, f: fn() -> T) -> Fiber<T>
```

Starts a new fiber in the scope `owner` that calls `f`, and returns the fiber at once. The new fiber runs concurrently with the caller, possibly in parallel, and has the default stack (§10.1, D-125).

- `f` is called exactly once, on the new fiber. The fiber releases `f` no later than when the call returns or unwinds, so `f`'s captures do not outlive it (D-205, D-670).
- The fiber's result is kept for [`join`](#join) while a `Fiber` value for it exists. When none is left, for example because the result of `spawn` was discarded, the result is released as soon as `f` returns (D-670).
- If `owner` has failed or has been cancelled, the new fiber is cancelled before it starts: `f` is never called, and it is released at once (D-666).
- **Panics:** with the message `spawn into a scope that has ended` when `owner` has ended (§10.3, D-666). This is a rule of scopes, not a bad argument: no fiber may outlive its scope. _Note:_ the reference implementation reports this panic at `<unknown location>`, like the panics of kernels (§11.6, D-732).
- **Cost:** O(1), plus marking the captures of `f` as shared (§9.4). Informative.

```cheby
test "spawning into a scope that has ended panics" {
  let assert Ok(ended) = {
    use owner <- fiber::scope()
    owner
  }
  let outcome = {
    use _scope <- fiber::scope()
    fiber::spawn(ended, fn() { Nil })
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "spawn into a scope that has ended"
}
```

The parameter is named `owner` because a parameter cannot share its name with a function of its module, here `scope` (D-051). The tests in this file bind their scopes as `owner` for the same reason: they live in `std::fiber` (D-630, D-662).

### `spawn_with_stack`

```cheby
pub fn spawn_with_stack<T>(owner: Scope, stack_bytes: Int, f: fn() -> T) -> Fiber<T>
```

Like [`spawn`](#spawn), but the new fiber's stack reserves `stack_bytes` bytes of address space instead of the default (D-125, D-659). Only touched pages use memory (D-124).

- **Clamps:** `stack_bytes` to 1 MiB to 16 GiB, the range of the `[runtime]` table's sizes, and then rounded up to the page size (D-298, D-660). _Note:_ the reference implementation reserves the next power of two, so the reservation is at least the request (D-677).
- **Panics:** as `spawn`.
- **Cost:** as `spawn`. Informative.

On JS the stack is the engine's, and `stack_bytes` has no effect (D-094, D-232).

```cheby
@target(native)
test "a fiber can have a larger stack" {
  fn depth(n) {
    case n {
      0 => 0
      _ => 1 + depth(n - 1)
    }
  }
  let outcome = {
    use owner <- fiber::scope()
    let deep = fiber::spawn_with_stack(owner, 64 * 1024 * 1024, fn() { depth(100000) })
    fiber::join(deep)
  }
  assert outcome == Ok(100000)
}
```

### `spawn_detached`

```cheby
pub fn spawn_detached<T>(f: fn() -> T)
```

Starts a new **detached** fiber that calls `f`, outside every scope, and returns at once (§10.4, D-040, D-669). `f`'s result is released when it returns. `f` may return `Never`, as a fiber that runs until the program ends does.

- Nobody waits for a detached fiber, and no cancellation reaches it, including that of a scope or a timeout around the code that spawned it. Its own scopes work as usual.
- `f` is released no later than when the call returns or unwinds (D-670).
- A panic of a detached fiber is printed to standard error in the form of a panic report, after standard output has been flushed (§11.6, D-449, D-701). The two lines are written together, never interleaved with other output on standard error. The fiber then ends, and nothing else happens (§11.7).
- The program kills detached fibers that are still running when `main` and its scopes have finished, without unwinding (§10.10, D-073). _Note:_ the reference implementation's debug builds cancel and unwind them instead, before the leak check, which no program can observe (D-710).
- A detached fiber spawned by a test ends with that test: when the test ends, its detached fibers are cancelled and unwound, in every build (§10.4, §13.6, D-723).
- **Cost:** O(1), plus marking the captures of `f` as shared (§9.4). Informative.

```cheby
test "a detached fiber runs on its own" {
  let (tx, rx) = channel::new::<Int>(0)
  fiber::spawn_detached(fn() { channel::send(tx, 5) })
  assert channel::receive(rx) == Ok(5)
}
```

`spawn_detached` returns `Nil`, not a `Fiber`, so no fiber outside every scope can be joined (D-669).

## Joining

### `join`

```cheby
pub fn join<T>(fiber: Fiber<T>) -> T
```

Waits until `fiber` has ended and returns its value directly (D-177). Any fiber may join, any number of times, and each call returns the same value.

If `fiber` panicked or was cancelled, `join` never returns (D-177, D-668):

- A joiner that is a member of the fiber's scope, directly or through scopes nested in a member, is being cancelled with that scope, so it unwinds instead of returning.
- Any other joiner, such as one in an unrelated scope that received the `Fiber` over a channel, panics with the message `joined fiber failed`.

- **Panics:** with `joined fiber failed` in the second case above. This is a rule of `join` (§10.3), not a bad argument: a fiber that failed has no value to return (D-668). _Note:_ the reference implementation reports this panic at `<unknown location>`, like the panics of kernels (§11.6, D-732).
- **Cost:** O(1), plus the wait. Informative.

```cheby
test "join returns the fiber's value every time" {
  let outcome = {
    use owner <- fiber::scope()
    let answer = fiber::spawn(owner, fn() { 6 * 7 })
    fiber::join(answer) + fiber::join(answer)
  }
  assert outcome == Ok(84)
}

test "joining a failed fiber from outside its scope panics" {
  let (tx, rx) = channel::new::<fiber::Fiber<Never>>(1)
  let (go_tx, go_rx) = channel::new::<Nil>(1)
  let first = {
    use owner <- fiber::scope()
    let doomed = fiber::spawn(owner, fn() {
      let _ = channel::receive(go_rx)
      panic as "doomed"
    })
    let assert Ok(Nil) = channel::send(tx, doomed)
    let assert Ok(Nil) = channel::send(go_tx, Nil)
    Nil
  }
  let assert Err(_) = first
  let assert Ok(doomed) = channel::receive(rx)
  let second = {
    use _scope <- fiber::scope()
    fiber::join(doomed)
  }
  let assert Err(failure) = second
  assert fiber::message(failure) == "joined fiber failed"
}
```

## Cancellation

Cancellation follows §10.7 (D-102, ADR-0029). This section fixes which frames a cancellation unwinds, so that scopes and timeouts nest (D-667):

- Every cancellation has a **target**, an open scope. A scope that fails targets itself, and a timeout whose limit passes targets its own scope.
- Cancelling a scope's members cancels each fiber spawned into it, and its body. A cancelled body unwinds the calling fiber's frames down to the target's `scope` or `timeout` call, which then returns `Err`. A cancelled fiber unwinds completely and ends.
- A cancelled fiber or body that has scopes of its own open cancels them too, with the same target. Unwinding passes through their frames without returning from them, and at each one it first waits until every fiber of that scope has ended.
- When a fiber has several cancellations pending, it unwinds to the outermost target among them. A cancellation that arrives while the fiber unwinds for an inner target extends the unwinding to the new target.
- A cancelled `send`, `receive`, `select` or `join` takes and delivers nothing: a blocked `send` has not enqueued its value, which unwinding releases, and a blocked `receive` or `select` leaves every value in its channel (D-670).
- Work that the runtime cannot interrupt, which is file IO, standard input and foreign functions marked `@blocking` (§10.9, §12.7), runs to the end first. The cancellation is delivered when it returns, and its result is released (D-670).

A cancellation therefore stops only at the boundary of the scope it targets, and a scope cancelled from outside never returns. Detached fibers have no enclosing scope, so no cancellation reaches them.

### `timeout`

```cheby
pub fn timeout<T>(limit: duration::Duration, f: fn() -> T) -> Result<T, Timeout>
```

Calls `f` on the calling fiber, in a new scope, and cancels it if it has not finished within `limit` (D-103, D-181, D-665, D-671). The scope has no `Scope` value, so its only member is `f`. The fibers that `f` spawns into scopes it opens are inside that scope.

- If `f` returns before a cancellation reaches it, `timeout` returns `Ok` with its value, even when `limit` has passed by then.
- If `f` panics before a cancellation reaches it, `timeout` returns `Err(Panicked(p))` with the panic `p`, and prints nothing.
- Otherwise, once `limit` has passed, `f` is cancelled (see [Cancellation](#cancellation)), and `timeout` returns `Err(TimedOut)` after every frame of `f` has unwound and every fiber in the scopes it opened has ended. A panic during that unwinding does not change the result. `Err(TimedOut)` never comes earlier than `limit` after the call.
- A `limit` of zero or less has already passed: `timeout` returns `Err(TimedOut)` without calling `f`, on every target (D-652).
- A `limit` longer than 2⁶³ − 1 nanoseconds, about 292 years, never passes: `timeout` calls `f` without a timer, on every target (D-652, D-891).
- `timeout` releases `f` before it returns, whatever the result (D-670).
- Fibers that `f` spawns into a scope opened outside `f`, whose `Scope` it captured, and detached fibers are not cancelled.

`f` runs on the calling fiber, with its stack, so a deep recursion that works without `timeout` also works inside it (D-665).

- **Fails:** `Err(TimedOut)` when `limit` passed first, and `Err(Panicked(p))` when `f` panicked first.
- **Cost:** O(1), plus the time `f` takes. Informative.

```cheby
test "timeout returns the value of work that finishes in time" {
  assert fiber::timeout(duration::seconds(10), fn() { 6 * 7 }) == Ok(42)
}

test "timeout cancels work that takes too long" {
  let (keep, never) = channel::new::<Int>(1)
  let outcome = fiber::timeout(duration::milliseconds(10), fn() { channel::receive(never) })
  assert outcome == Err(fiber::TimedOut)
  // `f` was released before `timeout` returned, and with it `never`.
  assert channel::send(keep, 1) == Err(1)
}

test "timeout reports a panic as Panicked" {
  let outcome = fiber::timeout(duration::seconds(10), fn() { panic as "crash" })
  let assert Err(fiber::Panicked(failure)) = outcome
  assert fiber::message(failure) == "crash"
}

test "a limit of zero times out without calling f" {
  let outcome = fiber::timeout(duration::seconds(0), fn() { panic as "not called" })
  assert outcome == Err(fiber::TimedOut)
}

test "a timeout cancels the scopes inside it without their returning" {
  let (keep, never) = channel::new::<Int>(1)
  let outcome = fiber::timeout(duration::milliseconds(10), fn() {
    let inner = {
      use owner <- fiber::scope()
      fiber::spawn(owner, fn() { channel::receive(never) })
    }
    // Not reached: a scope that is cancelled from outside never
    // returns.
    case inner {
      Ok(_) => "returned"
      Err(_) => "failed"
    }
  })
  assert outcome == Err(fiber::TimedOut)
  assert channel::send(keep, 1) == Err(1)
}
```

## Panics

### `message`

```cheby
pub fn message(failure: Panic) -> String
```

Returns the message of the panic: exactly the message line of its panic report, without the `panicked at` line, the location or any prefix (§11.6, D-449, D-473).

- **Cost:** O(1). Informative.

```cheby
test "message is the panic's message line" {
  let outcome = {
    use _scope <- fiber::scope()
    todo as "later"
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "not implemented: later"
}
```

The parameter is named `failure` because `panic` is a keyword.

### `show`

```cheby
pub fn show(failure: Panic) -> String
```

Makes `Panic` satisfy `Show` (§8.8). The text is the panic report of §11.6 without its final line ending: `panicked at <location>:`, a line ending, and the message. The location is written as in the report, a path with line and column, `<unknown location>` or `<builtin>` (D-449). So `main` returning `Err(p)` prints the same two lines as the panic would have, and exits with status 1 (§10.10, D-182, D-648).

- **Cost:** O(n) in the length of the text. Informative.

```cheby
test "a panic shows like a panic report" {
  let assert Err(failure) = {
    use _scope <- fiber::scope()
    panic as "boom"
  }
  let text = fiber::show(failure)
  assert string::starts_with(text, "panicked at ")
  assert string::ends_with(text, ":\nboom")
  assert string::starts_with("{failure:?}", "Panic \{ message: \"boom\", location: \"")
}

test "panics compare by message and location" {
  fn fail(text) {
    let assert Err(failure) = {
      use _scope <- fiber::scope()
      panic as text
    }
    failure
  }
  assert fail("same") == fail("same")
  assert fail("same") != fail("other")
}
```
