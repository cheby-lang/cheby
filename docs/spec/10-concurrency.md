# 10. Concurrency

Cheby programs are concurrent through **fibers**: lightweight, stackful threads of execution managed by the runtime (D-003, ADR-0002). Fibers run in parallel across OS threads (D-012, ADR-0004), communicate through typed channels (D-013), and are organized by structured concurrency (D-040, ADR-0019).

There is no `async`, no `await` and no function coloring. Any function may block, for example on a channel or on IO, without its type changing (D-003).

_Note:_ this chapter defines the semantics of fibers, scopes, channels and selectors. The standard-library names and signatures used here (`fiber::scope`, `fiber::spawn`, `fiber::join`, `channel::new`, `channel::send`, `channel::receive` and the like) are provisional, and will be refined in a standard-library specification (D-174).

## 10.1 Fibers

A fiber runs a function to completion on its own stack. Every Cheby program starts with one fiber running `main` ([§10.10](#1010-program-entry-and-exit)).

- Fibers share no mutable state, because there is none (D-004). Values passed to a fiber, captured by it or sent to it are immutable, and become shared for reference-counting purposes ([§9.4](09-memory-model.md#94-sharing-across-threads)) (D-072).
- A fiber ends when its function returns, when it panics ([§11.7](11-errors-and-panics.md#117-panics-and-fibers)), or when it is cancelled ([§10.7](#107-cancellation)).
- A fiber's result is observed only through its scope or by joining it ([§10.3](#103-scopes)).

On native targets, each fiber's stack is a fixed virtual-memory reservation carved out of large shared regions and committed lazily by the operating system, so only touched pages use physical memory (D-124, ADR-0032). Stacks are never moved, copied or split. The default reservation is 8 MiB per fiber, with a larger reservation for the root fiber running `main`, and both are configurable at build time and per spawn (D-125). Overflowing a stack panics the fiber ([§11.8.1](11-errors-and-panics.md#1181-stack-overflow)).

## 10.2 Scheduling

The runtime schedules fibers **M:N**: many fibers are multiplexed onto a pool of OS threads, and fibers may run truly in parallel (D-012).

A fiber gives up its thread at **suspension points**:

- channel sends and receives that cannot complete immediately,
- waiting for a scope or joining a fiber,
- IO and timers ([§10.9](#109-blocking-io)),
- **yield checks**, which the compiler inserts at the entry of every function (D-028). On native targets the yield check is the same compare as the stack-limit check ([§11.8.1](11-errors-and-panics.md#1181-stack-overflow)) (D-124).

Because there are no loops, every repetition goes through a function call, so the entry yield check guarantees that no fiber can hold a thread indefinitely without passing a suspension point (D-028). Preemption only happens at these points; a fiber is never interrupted in the middle of a function body without calls.

The scheduling order of runnable fibers is unspecified. Programs must not depend on it.

By default the scheduler uses one OS thread per CPU core. The number can be overridden through an environment variable (like Go's `GOMAXPROCS`) and through a runtime function (D-175).

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

The rules of a scope (D-040):

1. **Spawning.** Fibers are spawned into a scope. The scope value can be passed to other functions, which can spawn into it.
2. **Waiting.** When the scope's body finishes, the scope waits for all fibers spawned into it before returning. A scope never returns while any of its fibers is still running.
3. **Failure.** If any fiber in the scope panics, all other fibers in the scope, and the scope's body, are cancelled ([§10.7](#107-cancellation)). Once they have all ended, the scope returns `Err` describing the panic. A panic in the scope's own body is treated the same way: the scope's fibers are cancelled and the scope returns `Err`. The panic does not propagate past the scope (D-176).
4. **Success.** If the body and all fibers finish without panicking, the scope returns `Ok` with the value of the body.

Scopes nest. A fiber may open its own scopes, and a scope's fibers are cancelled together with it when an enclosing scope is cancelled.

`fiber::join(f)` waits for the fiber `f` and returns its value directly, not wrapped in a `Result` (D-177). If the joined fiber panics or is cancelled, its scope is failing, so the joining fiber is itself cancelled as part of that failure and `join` never returns (D-177).

The error value of a failed scope, `fiber::Panic`, carries the message and source location of the **first** panic in the scope. Panics caused by the resulting cancellation are not reported (D-178).

A scope value must not escape the scope's callback: storing it or returning it and spawning into it after the scope has ended panics.

## 10.4 Detached fibers

A fiber can also be spawned **detached**, outside any user-visible scope, with an explicit function such as `fiber::spawn_detached` (D-040). A detached fiber:

- is not waited for by anyone,
- does not cancel anything when it panics; its panic is reported to standard error and the fiber ends,
- is killed when the program exits ([§10.10](#1010-program-entry-and-exit)) (D-073).

Detached fibers are the escape hatch for long-lived background work. Leaked detached fibers, and fibers blocked forever, are not collected (D-029). Erlang-style links, monitors and supervision are provided by a library on top of scopes and detached fibers (D-040).

## 10.5 Channels

A channel is a typed, bounded queue between fibers (D-013, D-054). Creating a channel returns a pair of handles:

```cheby
let (tx, rx) = channel::new::<Job>(16)   // capacity 16
```

- `Sender<T>` sends values of type `T`, and `Receiver<T>` receives them (D-054).
- The **capacity** is the number of values the channel buffers. Capacity `0` means a synchronous handoff: a send completes only when a receiver takes the value (D-054). A negative capacity panics.
- Values are received in the order they were sent by any one sender. Values from different senders are interleaved in an unspecified order.
- Handles are ordinary immutable values. Passing a `Sender` to several fibers gives several producers, and passing a `Receiver` to several fibers gives several competing consumers.

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
let selector =
  selector::new()
  |> selector::receive(jobs, fn(r) { Job(r) })
  |> selector::receive(control, fn(r) { Control(r) })
  |> selector::after(duration::seconds(5), fn() { Idle })

case selector::select(selector) {
  Job(Ok(job)) => run(job)
  Job(Err(Nil)) => Nil          // jobs is closed
  Control(Ok(cmd)) => handle(cmd)
  Control(Err(Nil)) => Nil      // control is closed
  Idle => Nil
}
```

- `select` blocks until at least one arm is ready, then takes exactly one value from exactly one ready arm and returns its mapped result.
- If several arms are ready, which one is chosen is unspecified and should not systematically favor any arm.
- A timer arm ([§10.8](#108-timeouts)) is ready when its duration has elapsed (D-103).
- Each receive arm's mapping function takes a `Result<T, Nil>`, like `channel::receive`. A receive arm on a closed, empty channel is ready, and passes `Err(Nil)` to its mapping function, so selectors can observe closure (D-180).
- A fiber waiting in `select` counts as a receiver on every channel it has a receive arm for (D-206). On a capacity-0 channel, a receive arm is ready while a sender is blocked on it, and a `send` does not block for want of a receiver while a selector waits on its channel. When the selector chooses that arm, the handoff completes: the value is taken and the sender's `send` returns `Ok(Nil)`. When it chooses another arm, no value is taken from the other channels, and their senders stay blocked.

## 10.7 Cancellation

A fiber is **cancelled** when its scope fails ([§10.3](#103-scopes)), when an enclosing scope is cancelled, or when a timeout expires ([§10.8](#108-timeouts)). Cancellation (D-102, ADR-0029):

- is delivered at the fiber's next suspension point or function-entry yield check ([§10.2](#102-scheduling)),
- makes the fiber unwind exactly like a panic: its stack is released and handle drop functions run ([§11.3](11-errors-and-panics.md#113-unwinding)),
- cannot be caught, ignored or delayed by the fiber,
- means that a blocking operation that is cancelled never returns to its caller.

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

`fiber::timeout(d, f)` runs `f` in a new scope and returns `Result<T, fiber::Timeout>`, where `fiber::Timeout` is an ADT with the variants `TimedOut` and `Panicked(fiber::Panic)` (D-181). The type name is provisional, to be fixed by the standard-library spec (D-199):

- If `f` finishes within `d`, it returns `Ok` with its value.
- Otherwise `f` and every fiber it spawned are cancelled, and it returns `Err(TimedOut)` once they have unwound (D-103).
- If `f`, or a fiber in its scope, panics, it returns `Err(Panicked(p))` with the scope's `fiber::Panic` ([§10.3](#103-scopes)), so callers can tell a timeout from a crash (D-181).

Selectors have a timer arm for deadlines ([§10.6](#106-selectors)). Timers use the runtime's poller ([§10.9](#109-blocking-io)).

## 10.9 Blocking IO

On native targets, the runtime performs IO so that a blocking operation suspends only the calling fiber, not its OS thread (D-042):

- Network IO and timers use an OS event poller (kqueue, epoll, io_uring or IOCP). A fiber waiting on a socket is suspended until the poller reports it ready.
- File IO, and foreign functions marked `@blocking` ([§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls)), run on a separate pool of blocking threads while the calling fiber is suspended.
- Foreign functions without `@blocking` are assumed to return quickly, and are called directly on the fiber's thread (D-042).

On the JS target, IO is performed through the host's asynchronous APIs, and a fiber waiting on IO is suspended in the same way ([§12.3](12-targets-and-ffi.md#123-javascript)).

## 10.10 Program entry and exit

A program runs a `main` function ([§4.2.1](04-declarations.md#421-main)) in a fiber that is the root scope of the program (D-073). Any module's `main` can be run by module path, as in `cheby run my_app::tools::migrate`, and the `main` of the package's root module, `src/main.cheby`, is the default (D-137, D-142). `main` may have any visibility, and is usually written `fn main()` without `pub` (D-163).

1. `main` runs as the body of the root scope. Fibers spawned into scopes opened by `main` belong to it.
2. The program ends when `main` and all fibers in its scopes have finished.
3. Any detached fibers still running at that point are killed without unwinding (D-073).
4. The exit status is:
   - `0` if `main` returns `Nil` or `Ok(Nil)`,
   - `1` if `main` returns `Err(e)`, after printing `e` to standard error (D-073, D-182). `e` is printed with `Show` if its type satisfies `Show` ([§8.8](08-interfaces.md#88-standard-interfaces)), and with debug printing otherwise ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-182),
   - `101` if the root fiber panics, after printing the panic ([chapter 11](11-errors-and-panics.md)) (D-182).

`main` takes no parameters. A program reads its command-line arguments and environment variables through the standard-library module `std::env`, which any module may use (D-183).
