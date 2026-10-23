# 10. Concurrency

Cheby programs are concurrent through **fibers**: lightweight, stackful threads of execution managed by the runtime (D-003, ADR-0002). Fibers run in parallel across OS threads (D-012, ADR-0004), communicate through typed channels (D-013), and are organized by structured concurrency (D-040, ADR-0019).

There is no `async`, no `await` and no function coloring. Any function may block, for example on a channel or on IO, without its type changing (D-003).

_Note:_ this chapter defines the semantics of fibers, scopes, channels and selectors. The names and signatures used here, such as `fiber::scope`, `fiber::spawn`, `fiber::join`, `channel::new`, `channel::send`, `channel::receive` and `selector::select`, are those of the standard-library modules [`std::fiber`](../stdlib/fiber.md), [`std::channel`](../stdlib/channel.md) and [`std::selector`](../stdlib/selector.md) (D-646). Durations come from [`std::duration`](../stdlib/duration.md) (D-650), and the scheduler's settings from [`std::runtime`](../stdlib/runtime.md) (D-657).

## 10.1 Fibers

A fiber runs a function to completion on its own stack. Every Cheby program starts with one fiber running `main` ([§10.10](#1010-program-entry-and-exit)).

- Fibers share no mutable state, because there is none (D-004). Values passed to a fiber, captured by it or sent to it are immutable, and become shared for reference-counting purposes ([§9.4](09-memory-model.md#94-sharing-across-threads)) (D-072). So does a fiber's result, which another fiber reads with `join` (D-674).
- A fiber ends when its function returns, when it panics ([§11.7](11-errors-and-panics.md#117-panics-and-fibers)), or when it is cancelled ([§10.7](#107-cancellation)).
- A fiber's result is observed only through its scope or by joining it ([§10.3](#103-scopes)).
- Each fiber has a context of values provided through `std::context`, which a spawn copies to the new fiber ([§10.11](#1011-context-values)) (D-1044).

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

- The environment variable `CHEBY_THREADS`, like Go's `GOMAXPROCS`. The runtime reads it once, before `main` starts, in `cheby run`, `cheby test`, the REPL and executables alike (D-927). Unset or empty, it gives the default, capped at 1024. A decimal whole number of at least 1 sets the count, clamped to 1024: ASCII digits only, leading zeros allowed, with no sign and no surrounding whitespace, and a number too large for the machine word clamps too (D-740). Any other value stops the program before `main` runs, with `error: CHEBY_THREADS must be a whole number of at least 1, not "<value>"` on standard error and exit status `1`. It is independent of `-j`, which sizes the compiler's own workers ([§13.1](13-tooling.md#131-the-cheby-command)) (D-622, D-658).
- The function `runtime::set_thread_count`, which clamps its argument to 1 to 1024 and applies to the whole program at once. Added threads start taking fibers at once, and a removed thread stops after its fiber reaches a suspension point (D-298, D-657).

On JS, fibers share the one JS thread, so `runtime::thread_count()` is 1, `set_thread_count` has no effect and `CHEBY_THREADS` is not read (D-657).

## 10.3 Scopes

Every fiber belongs to a **scope** (D-040). A scope is opened with `use` ([§5.10](05-expressions.md#510-use)):

```cheby
type Fetched = Result<http::Response, http::Error>

fn fetch_both(a: String, b: String) -> Result<(Fetched, Fetched), fiber::Panic> {
  use scope <- fiber::scope()
  let first = fiber::spawn(scope, fn() { http::get(a) })
  let second = fiber::spawn(scope, fn() { http::get(b) })
  (fiber::join(first), fiber::join(second))
}
```

`http::get` returns a `Result` of its own, so each fiber's value is a `Fetched`, and a failed request is an `Err` inside the scope's `Ok`, not a panic (D-1010).

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

- A joiner that is being cancelled unwinds instead ([§10.7](#107-cancellation)). This is the case for a member of `f`'s scope when that scope has failed or has been cancelled.
- Any other joiner, such as a fiber of an unrelated scope that received `f` over a channel, panics with the message `joined fiber failed`.

_Note:_ `fiber::cancel` cancels the fibers of a scope but not its body, and fibers spawned into the scope afterwards run normally ([§10.7](#107-cancellation)). So a member of `f`'s scope is not always being cancelled when `f` was: after `fiber::cancel`, the body, or a fiber spawned after the cancel, that joins a fiber the cancel reached panics with `joined fiber failed`, and the scope fails (D-668, D-1060).

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
- is killed when the program exits ([§10.10](#1010-program-entry-and-exit)) (D-073), or, when a test or a fiber the test started spawned it, when that test ends ([§13.6](13-tooling.md#136-tests)). A test is a root like `main`, so its detached fibers end with it, including those that its scoped and detached fibers spawned. They are cancelled and unwind, and no user code runs while they do (D-040, D-073, D-723, D-799). On JS, a detached fiber that never suspends keeps the program from finishing, because `main` never gets the thread back (D-232).

_Note:_ a fiber that the cancellation has not reached yet can still spawn a detached fiber while the detached fibers of its program or test are being stopped. The new fiber starts cancelled: its function is never called, and it ends at once, so the stopping never waits for a fiber its cancellation missed and no user code runs in it (D-800).

Detached fibers are the escape hatch for long-lived background work. Leaked detached fibers, and fibers blocked forever, are not collected (D-029). Erlang-style links, monitors and supervision are provided by a library on top of scopes and detached fibers (D-040).

**Deadlock.** A program is **deadlocked** when no fiber is running or runnable, no timer is pending, no IO operation or `@blocking` foreign call is in progress ([§10.9](#109-blocking-io)), and the root fiber has not finished (D-724). An IO operation is in progress while a fiber waits for it: a socket that no fiber waits on, such as an idle `net::Listener`, cannot wake anyone and does not count (D-984). An open `process::until_interrupted` scope is not pending work either, so a program whose fibers all block inside one is deadlocked, although a signal could still cancel it (D-1073). A `time::sleep` or `time::sleep_until` that never ends ([§10.8](#108-timeouts)) arms no timer, so a fiber in it counts as blocked (D-1058). No fiber can then make progress. `cheby test` applies this rule to each test's own fibers, and the REPL to the running entry and the detached fibers of the session (D-857). Every build detects a deadlock and treats it as fatal:

- `cheby run` fixes exit status `103`, flushes standard output, writes `fatal error: all fibers are blocked` to standard error, then one line per blocked fiber that names the operation it waits in and the location of that call, in a deterministic order, and ends the program with status `103` ([§10.10](#1010-program-entry-and-exit)) (D-859). A failed write after that leaves the status as it is (D-785).
- A deadlock is not a panic. It cannot be caught by a scope or a timeout. In `cheby run`, nothing unwinds before the exit (D-859).
- `cheby test` cancels the deadlocked test's root scope, so the test's fibers unwind. It fails that test with the report as its failure block and goes on with the other tests, which a deadlock of another test does not affect ([§13.6](13-tooling.md#136-tests)) (D-857, D-859).
- The REPL cancels only the entry's root scope, so the entry's fibers unwind. The report also lists the session's blocked detached fibers, which keep waiting, and the session continues ([§13.5.4](13-tooling.md#1354-fibers-in-the-repl)) (D-859). Fibers that a `:reload` could not stop are left out of detection and of the report, so a later entry that deadlocks is still reported (D-857).

**The report.** Its first line is `fatal error: all fibers are blocked`. Each further line is `  blocked in <operation> at <location>`, one per blocked fiber (D-858):

- The operation is `channel::send`, `channel::receive`, `selector::select`, `fiber::join`, `fiber::scope` for a fiber waiting at the end of a scope, `fiber::timeout` for a fiber waiting at the end of a timed scope, or `time::sleep` for a fiber in a sleep that never ends (D-858, D-1058). For a function of `std` written in Cheby, the operation is the one its kernel waits in, such as `channel::receive` for a fiber waiting in `fiber::race` (D-1059).
- The location is that of the innermost call on the fiber's stack whose function is not defined in package `std`. A function of `std` counts as `std` also where user code instantiates it. The location is written `<path>:<line>:<column>`, as in a panic report ([§11.6](11-errors-and-panics.md#116-panic-and-todo)), and is the start of the call: the callee's path for `f(x)`, the start of the chain for a pipe, and the `use` of a `use` statement. The search passes through `std` functions and the compiler's Cheby-convention shims, such as the one for a function used as a value. When it reaches the runtime or foreign code first, such as the runtime starting a spawned fiber's function or a foreign call, the location is `<unknown location>` (D-856).
- Lines are sorted by path, compared as bytes, then by line and column as numbers, then by operation, and `<unknown location>` lines come last. Equal lines are all kept, and the report has no limit on its length. The order depends only on what the fibers wait in, never on fiber identities, workers or timing (D-858).

_Note:_ a tail call replaces its caller's frame ([§5.15.3](05-expressions.md#5153-guaranteed-tail-calls)), so a wait reached through tail calls is reported at the innermost call still on the stack. A tail call from code outside `std` to a public function of `std` that can wait on a channel, a fiber, a scope or a timer and calls no function value on the calling fiber keeps its caller's frame. Today these are `channel::send`, `channel::receive`, `fiber::join`, `time::sleep` and `time::sleep_until` (D-861, D-1059). So a function such as `fn() { channel::receive(rx) }` passed to `fiber::spawn` is reported at its `channel::receive`. A waiting function that calls a function value keeps its tail call, as `selector::select`, `fiber::race`, `process::until_interrupted` and `io::copy` do, and a fiber waiting in one is located at the nearest frame outside `std` that is still on the stack (D-861, D-1059). Socket waits and work on the blocking pool never appear in a report (D-857, D-984), so the rule leaves them out. Panics raised by standard-library operations still report `<unknown location>` (D-732).

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

_Note:_ the reference implementation finds a location by mapping the return address of the fiber's innermost frame outside `std` to its call through a table the JIT records per call site (D-856). Its monitor thread confirms a deadlock by surveying the domain's fibers twice, and finds a deadlocked test among running ones within about 200 ms (D-860).

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

There is no `close` function. A channel is closed exactly when its last `Sender` or its last `Receiver` is dropped (D-054, D-087). "Dropped" means the last reference to any handle of that kind is released, which the runtime detects precisely through reference counting ([§9.5](09-memory-model.md#95-handles-and-drop-functions)), on every target (D-100, ADR-0028). A binding releases its handle right after its last use ([§9.2](09-memory-model.md#92-reference-counting)) (D-205, ADR-0036), so a channel closes as soon as no code can use its last handle of that kind again, not when the enclosing function returns. The one exception is a handle cut from a list by a slice, which a slice may keep until it dies ([§9.2](09-memory-model.md#92-reference-counting)) (D-893, D-894).

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

A fiber is **cancelled** when its scope fails ([§10.3](#103-scopes)), when an enclosing scope is cancelled, when a timeout expires ([§10.8](#108-timeouts)), when `fiber::cancel` reaches it (D-1060, ADR-0069), or when the process receives SIGINT or SIGTERM while it runs inside a `process::until_interrupted` scope ([§10.10](#1010-program-entry-and-exit)) (D-1073, ADR-0071). Cancellation (D-102, ADR-0029):

- is delivered at the fiber's next suspension point or function-entry yield check ([§10.2](#102-scheduling)); on JS, which has no entry yield checks, only at the next real suspension point (D-185, D-232, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)),
- makes the fiber unwind exactly like a panic: its stack is released and handle drop functions run ([§11.3](11-errors-and-panics.md#113-unwinding)),
- cannot be caught, ignored or delayed by the fiber,
- means that a blocking operation that is cancelled never returns to its caller.

Every cancellation has a **target**, an open scope: a failing scope targets itself, an expired timeout targets its own scope ([§10.8](#108-timeouts)), `fiber::cancel(owner)` targets `owner`, and a signal targets each open `process::until_interrupted` scope (D-1060, D-1073). Cancelling a scope cancels its fibers and its body. `fiber::cancel` is the exception: it cancels every fiber spawned into `owner` so far, and every scope nested in them, but never `owner`'s body, and it does not mark the scope as cancelled, so fibers spawned afterwards run normally and the scope returns `Ok` with the body's value once every fiber has ended. A fiber that calls `fiber::cancel` on its own scope unwinds at once, and on a scope that has ended, failed or been cancelled the call does nothing. A panic of a fiber after `fiber::cancel` has reached it is not recorded (D-1060). The target decides which frames unwind (D-667):

- A cancelled body unwinds the calling fiber down to the target's `scope` or `timeout` call, which then returns `Err`. A cancelled fiber unwinds completely and ends.
- Scopes that a cancelled fiber or body has open are cancelled too, with the same target. The unwinding passes through their frames without returning from them, and at each one it first waits until every fiber of that scope has ended. So an inner scope never returns `Err` to code that a cancellation has already reached (D-102).
- A fiber with several cancellations pending unwinds to the outermost target among them, and a cancellation that arrives while the fiber unwinds for an inner target extends the unwinding to the new target.
- Detached fibers are never reached ([§10.4](#104-detached-fibers)).

A cancellation therefore stops only at the boundary of the scope it targets, and a scope cancelled from outside never returns.

A cancelled operation takes and delivers nothing (D-670). A cancelled `send` has not enqueued its value, which unwinding releases, and a cancelled `receive`, `select` or `join` takes nothing, so no value is lost. A cancelled `net::accept` or `net::read_bytes` has taken nothing, and a cancelled `net::connect` leaves no connection. A cancelled `net::write_bytes` is the one exception: it may have sent a prefix of its data, and the connection stays usable (D-1000, D-1062).

A cancellation that reaches a fiber waiting on the lookup of a host name, or on a refill of standard input's reader or of a file's read-ahead buffer ([§10.9](#109-blocking-io)), is delivered at once. The fiber unwinds without waiting for the job, which keeps running on its pool thread. A lookup's result is dropped when it returns. A refill's bytes land in the reader's buffer, which the reader owns rather than the fiber, so the next read gets them and no line is lost, and the next read on that reader waits for a refill that is still running (D-1065). Other work that the runtime cannot interrupt, which is the rest of file IO, such as writes, opens, closes and directory changes, and foreign calls marked `@blocking` ([§10.9](#109-blocking-io)), finishes first, because a caller must know whether it happened. The cancellation is delivered when it returns, and its result is released, so a timeout waits for that work (D-670, D-683, D-1065). A fiber that a cancellation has already reached never starts such work: the call is cancelled before the work starts, so no line of standard input is consumed and no file is written (D-816).

A cancelled fiber is not reported as having panicked. Its scope reports the original cause.

Cleanup therefore belongs in handle drop functions and in scope boundaries (`use` functions such as `file::with_open`), not in user code that runs "finally" (ADR-0029).

## 10.8 Timeouts

The standard library provides timeouts built on cancellation (D-103):

```cheby
case fiber::timeout(duration::seconds(2), fn() { http::get(url) }) {
  Ok(Ok(page)) => render(page)
  Ok(Err(_)) => render_failure()
  Err(_) => render_timeout()
}
```

The outer `Result` is the timeout's, and the inner one is `http::get`'s (D-1010).

`fiber::timeout(d, f)` calls `f` in a new scope and returns `Result<T, fiber::Timeout>`, where `fiber::Timeout` is the exposed ADT `Timeout { TimedOut, Panicked(Panic) }` of `std::fiber`, with structural `==` (D-181, D-649). Like a scope's body, `f` is an ordinary call on the calling fiber, with that fiber's stack, so a deep recursion that works without `timeout` works inside it too (D-665, ADR-0057). The scope has no `Scope` value, so its only member is `f`. Fibers that `f` spawns into scopes it opens are inside the timeout, because those scopes are open in `f`'s frames ([§10.7](#107-cancellation)).

- If `f` returns before a cancellation reaches it, `timeout` returns `Ok` with its value, even when `d` has passed by then (D-671).
- If `f` panics before a cancellation reaches it, `timeout` returns `Err(Panicked(p))` with the panic's `fiber::Panic` ([§10.3](#103-scopes)), so callers can tell a timeout from a crash (D-181). The panic is not printed (D-666).
- Otherwise, once `d` has passed, `f` is cancelled, with the timeout's scope as the target ([§10.7](#107-cancellation)). `timeout` returns `Err(TimedOut)` once everything inside has unwound, and never earlier than `d` after the call. A panic during that unwinding does not change the result (D-103, D-671).
- A `d` of zero or less has already passed: `timeout` returns `Err(TimedOut)` without calling `f`, on every target (D-652, D-671).
- Fibers that `f` spawns into a scope opened outside `f`, whose `Scope` it captured, and detached fibers are not cancelled (D-671).
- `timeout` releases `f` when the call returns, before it waits or returns (D-670).

A function of the standard library that waits for a duration, such as `fiber::timeout` and `selector::after`, treats a duration of zero or less as already elapsed, and one longer than 2⁶³ − 1 nanoseconds, about 292 years, as one that never elapses, on every platform and target (D-652, D-891). A `time::sleep` that never elapses, or a `time::sleep_until` with a deadline that far away, arms no timer, so a program whose fibers all wait, one of them in such a sleep, is deadlocked ([§10.4](#104-detached-fibers)) (D-1058).

On JS, a timeout cannot interrupt `f` between real suspension points, so a CPU-bound `f` may run past `d`, finish and return `Ok`. To bound CPU work on JS, the work must suspend, for example on a channel operation or a timer (D-185, D-232, [§12.4](12-targets-and-ffi.md#124-semantic-differences-between-targets)).

Selectors have a timer arm for deadlines ([§10.6](#106-selectors)). Timers use the runtime's poller ([§10.9](#109-blocking-io)).

## 10.9 Blocking IO

On native targets, the runtime performs IO so that a blocking operation suspends only the calling fiber, not its OS thread (D-042):

- Network IO and timers use an OS event poller (kqueue, epoll or IOCP) (D-680, ADR-0059). A fiber waiting on a socket is suspended until the poller reports it ready. The waits of `std::net` and `std::http`, such as accepting a connection, connecting, reading and writing, and `time::sleep`, are such waits, and a cancellation interrupts them at once ([§10.7](#107-cancellation)) (D-983, D-1000).
- File IO, a read of standard input that must wait for input, the lookup of a host name when `std::net` or `std::http` connects or listens, and foreign functions marked `@blocking` ([§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls)), run on a separate pool of blocking threads while the calling fiber is suspended (D-683, D-823, D-983). No Cheby code runs on those threads (D-191).
- Work on the blocking pool cannot be interrupted: it runs to its end. A cancellation that reaches its fiber meanwhile is delivered when the work returns, and the work's result is released ([§10.7](#107-cancellation)) (D-670, D-683). The lookup of a host name and a refill of standard input's reader or of a file's read-ahead buffer are the exceptions: their fiber unwinds at once and abandons the job, which still runs to its end on its own (D-983, D-1065).
- A `@blocking` foreign function of a package may build Cheby values with `cheby.h` while it runs on the pool, and they belong to the calling fiber ([§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls)) (D-950).
- Foreign functions without `@blocking` are assumed to return quickly, and are called directly on the fiber's thread (D-042).

_Note:_ in the reference implementation, standard input has one process-wide buffered reader, which reads 64 KiB at a time and serves both `io::read_line` and the REPL's prompt ([§13.5.5](13-tooling.md#1355-prompt-output-and-standard-streams)). Reads take their lines in the order they ask for them. A fiber takes a line that the reader already holds on its own thread, without the blocking pool, unless a cancellation of the fiber is pending, another read is waiting for the reader, or another thread holds it. Only a read that must wait goes to the pool (D-548, D-823). A file opened for reading reads ahead the same way, in blocks of 64 KiB, so only a read that empties the handle's buffer goes to the pool. Writes to a file are not buffered: each write is one job on the pool, and it returns once the operating system has the data (D-994, D-995). When the system refuses the pool a thread and no pool thread runs, the work runs on the calling fiber's own thread instead, and blocks that thread (D-817). An abandoned lookup or refill holds its pool thread until it returns, so a program that retries abandoned lookups in a loop can fill the pool's 512 threads (D-683, D-1065).

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
   - `103` if the program deadlocks, after printing the blocked fibers ([§10.4](#104-detached-fibers)) (D-724),
   - the status given to `process::exit`, when a fiber calls it: from `0` to `255` as given, and `255` for any other value. The program ends at once, as when a write fails below: the calling fiber stops where it is, the other fibers stop without unwinding, no drop function or other user code runs, and standard output is flushed (D-709, D-1022). Under `cheby test`, `process::exit` fails the calling test instead and the run goes on, and in the REPL it ends the session with that status ([§13.6](13-tooling.md#136-tests), [§13.5.4](13-tooling.md#1354-fibers-in-the-repl)) (D-1023).

An executable that `cheby build` produces exits with the same statuses as `cheby run` for the same program (D-926).

_Note:_ an implementation may instead cancel the detached fibers of item 3 and let them unwind, as debug toolchains of the reference implementation do to check for leaks. No user code runs while they unwind, the drop functions of user external types are not called, and standard output has been flushed and the exit status fixed before, so a program cannot observe the difference (D-073, D-710). For the same reason, a detached fiber that fails to write after that point, while it is being stopped, leaves the exit status as it is and prints nothing more (D-785).

_Note:_ a toolchain built with debug assertions, and the executables it builds, also check, when `main` returns with status `0` or `1` and when the root fiber panics, that no objects are still live. If some are, the program prints `error: leak check failed: N objects are still live at exit` to standard error, after the panic report if there is one, and exits with status `102` (D-458, D-710, D-926). The check is skipped when a fiber cannot be stopped because it waits on work on the blocking pool ([§10.9](#109-blocking-io)), or is blocked in such work on its own thread, as when the system refused the pool a thread, or waits only for such a fiber, as when it closes a scope whose child is blocked so (D-710, D-814, D-817). Such a fiber is left as it is, and the program still ends with the status already fixed (D-786, D-804). Work on the pool that has not returned does not hold the program back: once two polls 5 ms apart find a fiber still waiting on it, nothing else running and no work completed between them, the runtime gives up on the pool, and the work's result is dropped when it returns (D-815). A `@blocking` foreign call that the runtime gave up on, or that is still running when the runtime stops, is abandoned: it keeps running on its pool thread, its later `cheby.h` calls build nothing, release nothing and write nothing, its result is never released, and the arguments it borrowed stay readable until the process exits (D-950). The check is also skipped when a request for memory that the system refused ended in `out of memory` ([§11.8.2](11-errors-and-panics.md#1182-out-of-memory)), because the refusal can leave a value that was being updated in place unreachable, so a leak there never turns the status into `102` (D-771, D-907).

`main` takes no parameters and no type parameters ([§4.2.1](04-declarations.md#421-main)) (D-491). A program reads its command-line arguments and environment variables through the standard-library module `std::env`, which any module may use (D-183). `env::args()` does not include the program name, which `env::program_name()` returns (D-347).

Standard output and standard error behave the same way on every target:

- Standard output is line-buffered. It is also flushed before the program reads standard input, so a prompt written without a newline appears in time (D-346).
- Standard error is not buffered (D-346).
- One call that writes to a standard stream, such as `io::println`, writes its whole text at once, so the output of fibers that write at the same time never interleaves inside one call (D-684).
- Foreign code writes to the standard streams in order with `io::println` only through `cheby_ffi_write_stdout` and `cheby_ffi_write_stderr` ([§12.5.1](12-targets-and-ffi.md#1251-native-abi)). Each call is one write, as above. It returns `-1` when the write fails, and never ends the program, as `io::write_stdout` returns `Err`. Output that foreign code writes through C's own `stdio` has no ordering guarantee with the program's other output (D-951).
- All buffered output is flushed when the program ends, whatever its exit status (D-346).
- When a write or flush that does not return a `Result`, such as `io::println`, cannot complete, for example because the reader of a pipe has gone, the program ends at once with exit status `1` and prints nothing (D-306).
- After `io::write_stdout` returns `Err`, the text it could not write may stay unwritten, and a later flush of standard output that fails does not end the program, whether it is the flush at exit, before a panic report or before reading standard input. The `Err` has already reported that text, so a program that handles it keeps its exit status. This lasts until `io::print` or `io::println` adds text to standard output or a later `io::write_stdout` succeeds, after which the previous rule applies again (D-574).
- A standard stream that is closed or missing when the program starts behaves as a null device: writes and flushes succeed and discard the text, and reads see the end of input. The previous rule applies only to streams that are open at start (D-474).
- The handles `file::stdin()`, `file::stdout()` and `file::stderr()` follow these rules. Reads from `stdin()` come from the same reader as `io::read_line`, after standard output is flushed, a write to `stdout()` goes through standard output's buffer and then flushes it, and a write to `stderr()` is written at once, so output through a handle and through `io::print` never reorders. Closing such a handle closes only that handle, never the stream (D-1071).

These rules hold for executables as for `cheby run`. An executable ignores the `SIGPIPE` signal, so a write to a pipe or socket whose reader has gone fails as described above, or with an `Err`, and never kills the process (D-925).

SIGINT, which Ctrl-C sends, and SIGTERM take their default action and end the process at once, unless a `process::until_interrupted` scope is open (D-1073, ADR-0071). The first of them that the process receives while one is open cancels every open `until_interrupted` scope, each as the target ([§10.7](#107-cancellation)), so handles close, `with_*` scopes end and the program's own error path runs. The default action is then restored, so a second signal ends the process at once while a shutdown is stuck. A `main` that returns after `Err(process::Interrupted)` exits with the status of its result, as in item 4, and a program that wants the shell's convention for an interrupt calls `process::exit(130)`. Under `cheby test` and in the REPL no signal reaches such a scope ([§13.6](13-tooling.md#136-tests), [§13.5.4](13-tooling.md#1354-fibers-in-the-repl)) (D-1073).

_Note:_ the reference implementation turns a signal into an event on its poller through a self-pipe, and the thread that holds the poll runs the cancellation, as for an expired timer. On Windows no signal reaches an `until_interrupted` scope until M6, and on JS none does until M4, while a browser never interrupts the scope (D-1073).

## 10.11 Context values

Each fiber has a **context**: for each context key, the value that the program has provided to the fiber, if any. A program reads and provides values through the standard-library module `std::context`, and a key is a constant with a default (D-1044, D-1045).

- `context::provide(key, value, body)` binds `key` to `value` in the calling fiber while `body` runs. When `body` returns or unwinds, the fiber's context is again what it was before the call. So a `fiber::scope` or `fiber::timeout` that catches a panic continues with the context it had when it was called ([§10.3](#103-scopes), [§10.8](#108-timeouts)).
- Every spawn, scoped or detached, gives the new fiber the spawning fiber's context at the moment of the spawn, and the new fiber keeps it when `provide` returns in the spawning fiber. The spawning fiber is the one that calls `spawn`, which may differ from the scope's owner (D-666).
- The body of a scope, the function of a timeout and a selector's mapping function run on the calling fiber, so they see its context (D-665, D-675).
- Context follows spawns, not messages. A fiber that receives a value over a channel keeps its own context.
- The root fiber of a program, each test and each REPL entry start with an empty context, in which every key has its default.
- A context holds immutable values. A value that another fiber inherits becomes shared, as a captured value does ([§9.4](09-memory-model.md#94-sharing-across-threads)) (D-072). The one exception is `std::random`'s seeded stream, which each draw advances in the drawing fiber only, and from which a spawn derives the new fiber's stream without advancing it (D-1048).

Std reads only three dependencies from context: the clock of `std::time`, the stream of `std::random`'s host functions, and the logger of `std::log` once it is specified (D-1046). Context values are implemented in M4 (D-1049).
