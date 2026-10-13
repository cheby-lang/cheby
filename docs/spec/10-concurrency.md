# 10. Concurrency

Cheby programs are concurrent through **fibers**: lightweight, stackful threads of execution managed by the runtime (D-003, ADR-0002). Fibers run in parallel across OS threads (D-012, ADR-0004), communicate through typed channels (D-013), and are organized by structured concurrency (D-040, ADR-0019).

There is no `async`, no `await` and no function coloring. Any function may block, for example on a channel or on IO, without its type changing (D-003).

_Note:_ this chapter defines the semantics of fibers, scopes, channels and selectors. The names and signatures used here, such as `fiber::scope`, `fiber::spawn`, `fiber::join`, `channel::new`, `channel::send`, `channel::receive` and `selector::select`, are those of the standard-library modules [`std::fiber`](../stdlib/fiber.md), [`std::channel`](../stdlib/channel.md) and [`std::selector`](../stdlib/selector.md) (D-646). Durations come from [`std::duration`](../stdlib/duration.md) (D-650), and the scheduler's settings from [`std::runtime`](../stdlib/runtime.md) (D-657).

## 10.1 Fibers

A fiber runs a function to completion on its own stack. Every Cheby program starts with one fiber running `main` ([§10.10](#1010-program-entry-and-exit)).

- Fibers share no mutable state, because there is none (D-004). Values passed to a fiber, captured by it or sent to it are immutable, and become shared for reference-counting purposes ([§9.4](09-memory-model.md#94-sharing-across-threads)) (D-072). So does a fiber's result, which another fiber reads with `join` (D-674).
- A fiber ends when its function returns, when it panics ([§11.7](11-errors-and-panics.md#117-panics-and-fibers)), or when it is cancelled ([§10.7](#107-cancellation)).
- A fiber's result is observed only through its scope or by joining it ([§10.3](#103-scopes)).

The body of a scope and the function of a timeout are not fibers. They run on the fiber that calls `fiber::scope` or `fiber::timeout` ([§10.3](#103-scopes), [§10.8](#108-timeouts)) (D-665, ADR-0057).

On native targets, each fiber's stack is a fixed virtual-memory reservation carved out of large shared regions and committed lazily by the operating system, so only touched pages use physical memory (D-124, ADR-0032). Stacks are never moved, copied or split. The default reservation is 8 MiB per fiber, with a larger reservation for the root fiber running `main` (D-125). A package sets both sizes with the `[runtime]` table of its manifest ([§13.3](13-tooling.md#133-manifest)), from 1 MiB to 16 GiB, and `fiber::spawn_with_stack` sets the size for one spawn, clamped to the same range (D-125, D-659, D-660). Detached fibers always get the default. Overflowing a stack panics the fiber ([§11.8.1](11-errors-and-panics.md#1181-stack-overflow)).

## 10.2 Scheduling

The runtime schedules fibers **M:N**: many fibers are multiplexed onto a pool of OS threads, and fibers may run truly in parallel (D-012).

A fiber gives up its thread at **suspension points**:

- channel sends and receives that cannot complete immediately, and waiting in `select` ([§10.6](#106-selectors)),
- waiting for a scope or joining a fiber,
- IO and timers ([§10.9](#109-blocking-io)),
- **yield checks**, which the compiler inserts at the entry of every function (D-028). On native targets the yield check is the same compare as the stack-limit check ([§11.8.1](11-errors-and-panics.md#1181-stack-overflow)) (D-124).

Because there are no loops, every repetition goes through a function call, so on native targets the entry yield check guarantees that no fiber can hold a thread indefinitely without passing a suspension point (D-028). Preemption only happens at these points; a fiber is never interrupted in the middle of a function body without calls.

The JS target inserts no entry yield checks, so a JS fiber runs until its next real suspension point. Timers, including selector timer arms, fire only when the running fiber suspends, and a CPU-bound fiber can starve all others (D-185, D-232, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

The scheduling order of runnable fibers is unspecified. Programs must not depend on it.

By default the scheduler uses one OS thread per CPU core, as `runtime::cpu_count()` reports it (D-175, D-657). The thread count is always from 1 to 1024, and it changes only timing and scheduling, never a result that does not depend on them (D-232). It can be overridden in two ways:

- The environment variable `CHEBY_THREADS`, like Go's `GOMAXPROCS`. The runtime reads it once, before `main` starts, in `cheby run`, `cheby test` and the REPL alike. Unset or empty, it gives the default, capped at 1024. A decimal whole number of at least 1 sets the count, clamped to 1024: ASCII digits only, leading zeros allowed, with no sign and no surrounding whitespace, and a number too large for the machine word clamps too (D-740). Any other value stops the program before `main` runs, with `error: CHEBY_THREADS must be a whole number of at least 1, not "<value>"` on standard error and exit status `1`. It is independent of `-j`, which sizes the compiler's own workers ([§13.1](13-tooling.md#131-the-cheby-command)) (D-622, D-658).
- The function `runtime::set_thread_count`, which clamps its argument to 1 to 1024 and applies to the whole program at once. Added threads start taking fibers at once, and a removed thread stops after its fiber reaches a suspension point (D-298, D-657).

On JS, fibers share the one JS thread, so `runtime::thread_count()` is 1, `set_thread_count` has no effect and `CHEBY_THREADS` is not read (D-657).

## 10.3 Scopes

Every fiber belongs to a **scope** (D-040). A scope is opened with `use` ([§5.10](05-expressions.md#510-use)):

```cheby
fn fetch_both(a: Url, b: Url) -> Result<(Page, Page), fiber::Panic> {
  use scope <- fiber::scope()
  let first = fiber::spawn(scope, fn() { http::get(a) })
  let second = fiber::spawn(scope, fn() { http::get(b) })
  (fiber::join(first), fiber::join(second))
}
```

`fiber::scope` calls its body as an ordinary call on the calling fiber, with that fiber's stack. It does not start a fiber for the body, so values that the body captures or returns are not marked shared ([§9.4](09-memory-model.md#94-sharing-across-threads)) (D-665, ADR-0057). A scope's **members** are its body, which is the calling fiber's frames above the `scope` call, and the fibers spawned into it (D-666).

A scope is **open** from the call of its body until the body has finished and every fiber spawned into it has ended. Then it **ends** (D-666).

The rules of a scope (D-040):

1. **Spawning.** Fibers are spawned into a scope. The scope value can be passed to other functions, which can spawn into it. Any fiber that holds the scope value may spawn into it while the scope is open, also while the scope waits for its fibers, and those fibers are waited for too (D-666). Spawning into a scope that has failed or been cancelled creates a fiber that is cancelled before it starts, so its function is never called. Spawning into a scope that has ended panics with the message `spawn into a scope that has ended` (D-666).
2. **Waiting.** When the scope's body finishes, the scope waits for all fibers spawned into it before returning. A scope never returns while any of its fibers is still running, and its frame is never removed, by return or by unwinding, while any of its fibers runs (D-666).
3. **Failure.** If any member of the scope panics, all other members, fibers and body alike, are cancelled ([§10.7](#107-cancellation)). Once they have all ended, the scope returns `Err` describing the panic. A panic in the scope's own body is treated the same way: the calling fiber unwinds down to the `scope` call, the scope's fibers are cancelled and the scope returns `Err`. The panic does not propagate past the scope (D-176). The scope records the first panic and cancels the other members when that panic starts, before the panicking member unwinds, and later panics are not recorded (D-666).
4. **Success.** If the body and all fibers finish without panicking, the scope returns `Ok` with the value of the body.

A panic that a scope catches is not printed. The `fiber::Panic` that the scope returns is its only report ([§11.6](11-errors-and-panics.md#116-panic-and-todo)) (D-666, D-701).

Scopes nest. A fiber may open its own scopes, and a scope's fibers are cancelled together with it when an enclosing scope is cancelled ([§10.7](#107-cancellation)).

`fiber::join(f)` waits for the fiber `f` and returns its value directly, not wrapped in a `Result` (D-177). Any fiber may join `f`, inside or outside `f`'s scope, any number of times, and each call returns the same value. If `f` panicked or was cancelled, there is no value, and `join` never returns (D-177, D-668):

- A joiner that is being cancelled unwinds instead ([§10.7](#107-cancellation)). This is always the case for a member of `f`'s scope, because that scope is failing or has been cancelled.
- Any other joiner, such as a fiber of an unrelated scope that received `f` over a channel, panics with the message `joined fiber failed`.

The error value of a failed scope, `fiber::Panic`, carries the message and source location of the **first** panic in the scope. Panics caused by the resulting cancellation are not reported (D-178). The message is exactly the message line of the panic report ([§11.6](11-errors-and-panics.md#116-panic-and-todo)), without the location or any prefix, and the location is held separately (D-473). A `fiber::Panic` is built on the fiber that reads it, so it never crosses fibers (D-674).

The fiber API releases the values it holds at fixed points, so that the channels their handles reach close predictably ([§10.5.3](#1053-closing)) (D-670):

- `scope` releases its body when the call of the body returns, before it waits for the scope's fibers.
- A spawned fiber releases its function no later than when the call returns or unwinds.
- A fiber's result is kept only while a `Fiber` value for it exists. Otherwise it is released when the fiber's function returns.

_Note:_ the panics `spawn into a scope that has ended` and `joined fiber failed` come from the standard library, and the reference implementation reports them at `<unknown location>`, as it does the panics of kernels ([§11.6](11-errors-and-panics.md#116-panic-and-todo)) (D-732).

## 10.4 Detached fibers

A fiber can also be spawned **detached**, outside any user-visible scope, with `fiber::spawn_detached(f)` (D-040). It returns `Nil`, not a `Fiber`, so a detached fiber cannot be joined, and `f`'s result is released when `f` returns (D-669). A detached fiber:

- is not waited for by anyone,
- is not reached by the cancellation of any scope or timeout, including one around the code that spawned it (D-667, D-669),
- does not cancel anything when it panics. Its panic is printed to standard error as a panic report ([§11.6](11-errors-and-panics.md#116-panic-and-todo)), after standard output has been flushed, with the two lines written together, and the fiber ends (D-669),
- is killed when the program exits ([§10.10](#1010-program-entry-and-exit)) (D-073), or, when a test spawned it, when that test ends ([§13.6](13-tooling.md#136-tests)). A test is a root like `main`, so its detached fibers end with it. They are cancelled and unwind, and no user code runs while they do (D-040, D-073, D-723). On JS, a detached fiber that never suspends keeps the program from finishing, because `main` never gets the thread back (D-232).

Detached fibers are the escape hatch for long-lived background work. Leaked detached fibers, and fibers blocked forever, are not collected (D-029). Erlang-style links, monitors and supervision are provided by a library on top of scopes and detached fibers (D-040).

**Deadlock.** A program is **deadlocked** when no fiber is running or runnable, no timer is pending, no IO operation or `@blocking` foreign call is in progress ([§10.9](#109-blocking-io)), and the root fiber has not finished (D-724). No fiber can then make progress. Every build detects a deadlock and treats it as fatal:

- `cheby run` writes `fatal error: all fibers are blocked` to standard error, then one line per blocked fiber that names the operation it waits in and the location of that call, in a deterministic order, and exits with status `103` ([§10.10](#1010-program-entry-and-exit)).
- A deadlock is not a panic. It cannot be caught by a scope or a timeout, and nothing unwinds before the exit.
- `cheby test` fails the test with that report and goes on with the other tests ([§13.6](13-tooling.md#136-tests)). The REPL reports it and continues the session ([§13.5.4](13-tooling.md#1354-fibers-in-the-repl)).

A root that has finished while detached fibers are blocked is not deadlocked: the program exits ([§10.10](#1010-program-entry-and-exit)).

Detection does not depend on the build, because a program that fails this way in testing must not hang in production. A deadlock is not a panic, because one scope must not catch a failure of the whole program, and its status differs from a panic's `101` so that scripts can tell the two apart (D-724).

_Example:_ the root fiber still holds the `Sender`, so its receive can never complete.

```cheby
import std::channel

fn main() {
  let (tx, rx) = channel::new::<Int>(0)
  let assert Ok(n) = channel::receive(rx)
  let _ = channel::send(tx, n)
}
```

`cheby run` writes this to standard error and exits with status `103`:

```
fatal error: all fibers are blocked
  blocked in channel::receive at src/main.cheby:5:22
```

> **Open (OQ-10-11):** How the runtime finds the location of the call a blocked fiber waits in. D-732 gives the panics of standard-library operations no caller location in M2, and a fiber blocked in `channel::receive`, `select`, `join` or at the end of a scope waits inside such an operation. _Proposed:_ the report gives the location of the innermost call outside package `std` on the blocked fiber's stack, found by mapping that frame's return address to its source location, and `<unknown location>` where none is found. The location in the example above is illustrative until a run produces it.

## 10.5 Channels

A channel is a typed, bounded queue between fibers (D-013, D-054). Creating a channel returns a pair of handles:

```cheby
let (tx, rx) = channel::new::<Job>(16)   // capacity 16
```

- `Sender<T>` sends values of type `T`, and `Receiver<T>` receives them (D-054). They are external types of `std::channel` ([§12.5.3](12-targets-and-ffi.md#1253-external-types)) (D-647).
- The **capacity** is the number of values the channel buffers. Capacity `0` means a synchronous handoff: a send completes only when a receiver takes the value (D-054). A negative capacity counts as `0`, as a clamped count of the standard library does (D-298, D-672).
- A channel delivers values in the order they were enqueued or handed off, so values are received in the order they were sent by any one sender. Values from different senders are interleaved in an unspecified order, and which of several waiting receivers gets a value is unspecified (D-672).
- Handles are ordinary immutable values, but they cannot be compared with `==` or used as `Map` keys or `Set` elements ([§9.5](09-memory-model.md#95-handles-and-drop-functions)) (D-233). Passing a `Sender` to several fibers gives several producers, and passing a `Receiver` to several fibers gives several competing consumers.
- The drop functions of `Sender` and `Receiver` close the channel and wake the fibers waiting on it. They never block or suspend, because they run during unwinding and on any fiber (D-171, D-672).

### 10.5.1 Sending

`channel::send(tx, value)` (D-087):

- completes immediately if the buffer has room (or, for capacity 0, when a receiver takes the value),
- blocks the fiber while the buffer is full,
- returns `Ok(Nil)` once the value is enqueued or handed off,
- returns `Err(value)`, giving the unsent value back, if every `Receiver` for the channel has been dropped, including while it was blocked.

Its result type is `Result<Nil, T>`, so no value is lost when the receivers are gone (D-179).

### 10.5.2 Receiving

`channel::receive(rx)` (D-054):

- returns `Ok(value)` with the next value,
- blocks the fiber while the channel is empty and some `Sender` still exists,
- returns `Err(Nil)` once the channel is empty and every `Sender` has been dropped.

Its result type is `Result<T, Nil>` (D-179).

### 10.5.3 Closing

There is no `close` function. A channel is closed exactly when its last `Sender` or its last `Receiver` is dropped (D-054, D-087). "Dropped" means the last reference to any handle of that kind is released, which the runtime detects precisely through reference counting ([§9.5](09-memory-model.md#95-handles-and-drop-functions)), on every target (D-100, ADR-0028). A binding releases its handle right after its last use ([§9.2](09-memory-model.md#92-reference-counting)) (D-205, ADR-0036), so a channel closes as soon as no code can use its last handle of that kind again, not when the enclosing function returns.

When the last `Receiver` of a channel is dropped, the values still in its buffer are released at once, on the fiber that dropped it, although senders may still hold the channel (D-670).

_Example:_ a producer that ends closes the channel for its consumer.

```cheby
fn produce(tx: Sender<Int>, n: Int) {
  case n {
    0 => Nil   // tx is dropped here, which closes the channel
    _ => {
      let assert Ok(_) = channel::send(tx, n)
      produce(tx, n - 1)
    }
  }
}

fn consume(rx: Receiver<Int>, total: Int) -> Int {
  case channel::receive(rx) {
    Ok(n) => consume(rx, total + n)
    Err(_) => total
  }
}
```

## 10.6 Selectors

There is no `select` statement. Waiting on several channels is done with a **selector** from the standard library (D-041). A selector is built from several receive arms, each mapping the result of a receive to a common result type, and then waited on:

```cheby
let events =
  selector::new()
  |> selector::receive(_, jobs, fn(r) { Job(r) })
  |> selector::receive(_, control, fn(r) { Control(r) })
  |> selector::after(_, duration::seconds(5), fn() { Idle })

case selector::select(events) {
  Job(Ok(job)) => run(job)
  Job(Err(Nil)) => Nil          // jobs is closed
  Control(Ok(cmd)) => handle(cmd)
  Control(Err(Nil)) => Nil      // control is closed
  Idle => Nil
}
```

- A selector is an immutable value. Adding an arm returns a new selector, and the same selector can be passed to `select` any number of times (D-673). `Selector<E>` is an external type of `std::selector`, a handle that holds its receivers and mapping functions (D-647).
- `select` blocks until at least one arm is ready, then takes exactly one value from exactly one ready arm and returns its mapped result.
- If several arms are ready, `select` chooses one of them uniformly at random, so it favors none (D-041, D-673).
- The chosen arm's mapping function is called exactly once, on the calling fiber, after the value has been taken. Before that call, `select` releases its reference to the selector, and so to the receivers it holds (D-670, D-673). A panic in the mapping function is an ordinary panic of the calling fiber at that point.
- A timer arm ([§10.8](#108-timeouts)) is ready when its duration has elapsed since `select` started waiting (D-103). Its clock starts anew in each call of `select`, not when the arm is added, and a duration of zero or less is ready at once (D-652, D-673). A selector may have any number of timer arms.
- A selector without arms is never ready, so `select` waits until the fiber is cancelled (D-673). If no other fiber can then make progress, the program is deadlocked ([§10.4](#104-detached-fibers)) (D-724).
- Each receive arm's mapping function takes a `Result<T, Nil>`, like `channel::receive`. A receive arm on a closed, empty channel is ready, and passes `Err(Nil)` to its mapping function, so selectors can observe closure (D-180).
- A fiber waiting in `select` counts as a receiver on every channel it has a receive arm for (D-206). On a capacity-0 channel, a receive arm is ready while a sender is blocked on it, and a `send` does not block for want of a receiver while a selector waits on its channel. When the selector chooses that arm, the handoff completes: the value is taken and the sender's `send` returns `Ok(Nil)`. When it chooses another arm, no value is taken from the other channels, and their senders stay blocked.

`select` takes one parameter, so it is piped bare: `|> selector::select` (D-244, D-673).

## 10.7 Cancellation

A fiber is **cancelled** when its scope fails ([§10.3](#103-scopes)), when an enclosing scope is cancelled, or when a timeout expires ([§10.8](#108-timeouts)). Cancellation (D-102, ADR-0029):

- is delivered at the fiber's next suspension point or function-entry yield check ([§10.2](#102-scheduling)); on JS, which has no entry yield checks, only at the next real suspension point (D-185, D-232, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)),
- makes the fiber unwind exactly like a panic: its stack is released and handle drop functions run ([§11.3](11-errors-and-panics.md#113-unwinding)),
- cannot be caught, ignored or delayed by the fiber,
- means that a blocking operation that is cancelled never returns to its caller.

Every cancellation has a **target**, an open scope: a failing scope targets itself, and an expired timeout targets its own scope ([§10.8](#108-timeouts)). Cancelling a scope cancels its fibers and its body. The target decides which frames unwind (D-667):

- A cancelled body unwinds the calling fiber down to the target's `scope` or `timeout` call, which then returns `Err`. A cancelled fiber unwinds completely and ends.
- Scopes that a cancelled fiber or body has open are cancelled too, with the same target. The unwinding passes through their frames without returning from them, and at each one it first waits until every fiber of that scope has ended. So an inner scope never returns `Err` to code that a cancellation has already reached (D-102).
- A fiber with several cancellations pending unwinds to the outermost target among them, and a cancellation that arrives while the fiber unwinds for an inner target extends the unwinding to the new target.
- Detached fibers are never reached ([§10.4](#104-detached-fibers)).

A cancellation therefore stops only at the boundary of the scope it targets, and a scope cancelled from outside never returns.

A cancelled operation takes and delivers nothing (D-670). A cancelled `send` has not enqueued its value, which unwinding releases, and a cancelled `receive`, `select` or `join` takes nothing, so no value is lost. Work that the runtime cannot interrupt, which is file IO, standard input and foreign calls marked `@blocking` ([§10.9](#109-blocking-io)), finishes first. The cancellation is delivered when it returns, and its result is released, so a timeout waits for that work (D-670, D-683).

A cancelled fiber is not reported as having panicked. Its scope reports the original cause.

Cleanup therefore belongs in handle drop functions and in scope boundaries (`use` functions such as `file::with_open`), not in user code that runs "finally" (ADR-0029).

## 10.8 Timeouts

The standard library provides timeouts built on cancellation (D-103):

```cheby
case fiber::timeout(duration::seconds(2), fn() { http::get(url) }) {
  Ok(page) => render(page)
  Err(_) => render_timeout()
}
```

`fiber::timeout(d, f)` calls `f` in a new scope and returns `Result<T, fiber::Timeout>`, where `fiber::Timeout` is the exposed ADT `Timeout { TimedOut, Panicked(Panic) }` of `std::fiber`, with structural `==` (D-181, D-649). Like a scope's body, `f` is an ordinary call on the calling fiber, with that fiber's stack, so a deep recursion that works without `timeout` works inside it too (D-665, ADR-0057). The scope has no `Scope` value, so its only member is `f`. Fibers that `f` spawns into scopes it opens are inside the timeout, because those scopes are open in `f`'s frames ([§10.7](#107-cancellation)).

- If `f` returns before a cancellation reaches it, `timeout` returns `Ok` with its value, even when `d` has passed by then (D-671).
- If `f` panics before a cancellation reaches it, `timeout` returns `Err(Panicked(p))` with the panic's `fiber::Panic` ([§10.3](#103-scopes)), so callers can tell a timeout from a crash (D-181). The panic is not printed (D-666).
- Otherwise, once `d` has passed, `f` is cancelled, with the timeout's scope as the target ([§10.7](#107-cancellation)). `timeout` returns `Err(TimedOut)` once everything inside has unwound, and never earlier than `d` after the call. A panic during that unwinding does not change the result (D-103, D-671).
- A `d` of zero or less has already passed: `timeout` returns `Err(TimedOut)` without calling `f`, on every target (D-652, D-671).
- Fibers that `f` spawns into a scope opened outside `f`, whose `Scope` it captured, and detached fibers are not cancelled (D-671).
- `timeout` releases `f` when the call returns, before it waits or returns (D-670).

A function of the standard library that waits for a duration, such as `fiber::timeout` and `selector::after`, treats a duration of zero or less as already elapsed, and one longer than the runtime's clock can represent as one that never elapses (D-652).

On JS, a timeout cannot interrupt `f` between real suspension points, so a CPU-bound `f` may run past `d`, finish and return `Ok`. To bound CPU work on JS, the work must suspend, for example on a channel operation or a timer (D-185, D-232, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

Selectors have a timer arm for deadlines ([§10.6](#106-selectors)). Timers use the runtime's poller ([§10.9](#109-blocking-io)).

## 10.9 Blocking IO

On native targets, the runtime performs IO so that a blocking operation suspends only the calling fiber, not its OS thread (D-042):

- Network IO and timers use an OS event poller (kqueue, epoll or IOCP) (D-680, ADR-0059). A fiber waiting on a socket is suspended until the poller reports it ready.
- File IO, reading standard input, and foreign functions marked `@blocking` ([§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls)), run on a separate pool of blocking threads while the calling fiber is suspended (D-683). No Cheby code runs on those threads (D-191).
- Work on the blocking pool cannot be interrupted. A cancellation that reaches its fiber meanwhile is delivered when the work returns, and the work's result is released ([§10.7](#107-cancellation)) (D-670, D-683).
- Foreign functions without `@blocking` are assumed to return quickly, and are called directly on the fiber's thread (D-042).

On the JS target, IO is performed through the host's asynchronous APIs, and a fiber waiting on IO is suspended in the same way ([§12.3](12-targets-and-ffi.md#123-javascript)).

## 10.10 Program entry and exit

A program runs a `main` function ([§4.2.1](04-declarations.md#421-main)) in a fiber that is the root scope of the program (D-073). Any module's `main` can be run by module path, as in `cheby run my_app::tools::migrate`, and the `main` of the package's root module, `src/main.cheby`, is the default (D-137, D-142). `main` may have any visibility, and is usually written `fn main()` without `pub` (D-163).

1. `main` runs as the body of the root scope. Fibers spawned into scopes opened by `main` belong to it.
2. The program ends when `main` and all fibers in its scopes have finished.
3. Any detached fibers still running at that point are killed without unwinding (D-073).
4. The exit status is:
   - `0` if `main` returns `Nil` or `Ok(Nil)`,
   - `1` if `main` returns `Err(e)`, after printing `e` to standard error (D-073, D-182). `e` is printed with `Show` if its type satisfies `Show` ([§8.8](08-interfaces.md#88-standard-interfaces)), and with debug printing otherwise ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-182). The text is followed by a newline and has no prefix, because the program chose it (D-529). A `fiber::Panic` shows as its panic report without the final line ending, so `main` returning `Err(p)` prints the same two lines that the panic would have printed (D-648),
   - `101` if the root fiber panics, after printing the panic ([§11.6](11-errors-and-panics.md#116-panic-and-todo)) (D-182, D-449),
   - `103` if the program deadlocks, after printing the blocked fibers ([§10.4](#104-detached-fibers)) (D-724).

_Note:_ an implementation may instead cancel the detached fibers of item 3 and let them unwind, as debug toolchains of the reference implementation do to check for leaks. No user code runs while they unwind, the drop functions of user external types are not called, and standard output has been flushed and the exit status fixed before, so a program cannot observe the difference (D-073, D-710). For the same reason, a detached fiber that fails to write after that point, while it is being stopped, leaves the exit status as it is and prints nothing more (D-785).

_Note:_ a toolchain built with debug assertions also checks, when `main` returns with status `0` or `1` and when the root fiber panics, that no objects are still live. If some are, it prints `error: leak check failed: N objects are still live at exit` to standard error, after the panic report if there is one, and exits with status `102` (D-458, D-710). The check is skipped when a fiber cannot be stopped because it is still in work on the blocking pool ([§10.9](#109-blocking-io)) (D-710), or, until the reference implementation runs input and output on that pool, because it is blocked in a read or a file operation on its own thread. Such a fiber is left as it is, and the program still ends with the status already fixed (D-786). It is also skipped when the system refused memory to the program ([§11.8.2](11-errors-and-panics.md#1182-out-of-memory)), because the refusal can leave a value that was being updated in place unreachable, so a leak there never turns the status into `102` (D-771).

`main` takes no parameters and no type parameters ([§4.2.1](04-declarations.md#421-main)) (D-491). A program reads its command-line arguments and environment variables through the standard-library module `std::env`, which any module may use (D-183). `env::args()` does not include the program name, which `env::program_name()` returns (D-347).

Standard output and standard error behave the same way on every target:

- Standard output is line-buffered. It is also flushed before the program reads standard input, so a prompt written without a newline appears in time (D-346).
- Standard error is not buffered (D-346).
- All buffered output is flushed when the program ends, whatever its exit status (D-346).
- When a write or flush that does not return a `Result`, such as `io::println`, cannot complete, for example because the reader of a pipe has gone, the program ends at once with exit status `1` and prints nothing (D-306).
- After `io::write_stdout` returns `Err`, the text it could not write may stay unwritten, and a later flush of standard output that fails does not end the program, whether it is the flush at exit, before a panic report or before reading standard input. The `Err` has already reported that text, so a program that handles it keeps its exit status. This lasts until `io::print` or `io::println` adds text to standard output or a later `io::write_stdout` succeeds, after which the previous rule applies again (D-574).
- A standard stream that is closed or missing when the program starts behaves as a null device: writes and flushes succeed and discard the text, and reads see the end of input. The previous rule applies only to streams that are open at start (D-474).
