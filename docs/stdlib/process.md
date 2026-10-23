# `std::process`

The running program as a process. Tier 3 has two functions: [`exit`](#exit), which ends the program with a chosen status, and [`until_interrupted`](#until_interrupted), which turns Ctrl-C or a request to stop into a cancellation of the code it runs (D-1073). Command-line arguments and environment variables stay in [`std::env`](env.md) (D-183, D-988). Tier 3 does not start other processes: nothing needs it yet, browsers cannot provide it, and adding it later breaks nothing (D-1024). In the form `cheby doc` prints:

```cheby
pub exposed type Interrupt { Interrupted, Panicked(fiber::Panic) }
```

- **`Interrupt`.** The error of [`until_interrupted`](#until_interrupted), with the shape of `fiber::Timeout`. It is `exposed`, so other packages match `process::Interrupted` and `process::Panicked(p)` in patterns and compare with `==` (D-181, D-1073).
- **When to use `exit`.** A program usually ends by returning from `main`: `Nil` or `Ok(Nil)` gives status 0, and `Err(e)` prints `e` and gives status 1 ([§10.10](../spec/10-concurrency.md#1010-program-entry-and-exit), D-182). `exit` is for the statuses those two cannot give, such as 2 for a usage error, which tools like `grep` and `diff` use, and for ending at once from deep inside a program.
- **Kernel.** `exit` is written in Cheby over the private `@std_context` kernel `sys_exit`, bound as `@external(native, "cheby_process_sys_exit")` and `@external(js, "./process.mjs", "sys_exit")` (D-484, D-661, D-692, D-985, D-986). `until_interrupted` is written in Cheby over package-visible runtime operations of `std::fiber` that open and close a scope with a signal trigger, as `fiber::timeout` opens one with a timer, so no kernel calls `body` (D-675, D-790, D-1073). Reaching either while computing a constant is E0077 (D-537).
- **On JS.** OQ-P14, with M4. On Node, Deno and Bun the kernel must drain standard output before it calls the host's exit, because Node's `process.exit()` can drop output still queued to a pipe, which would break D-346's flush. A browser has no process, and the direction is that `exit` stops every fiber and reports the status to the page's host, as a termination does. JS hosts that have signals get their listeners for `until_interrupted` in M4, and a browser never interrupts its scope (D-1073).

## Exiting

### `exit`

```cheby
pub fn exit(status: Int) -> Never
```

Ends the program with exit status `status`, as a termination does (D-709, D-1022). It never returns, so it fits anywhere a value is expected ([§3.10](../spec/03-types.md#310-never)).

1. **Other fibers.** The calling fiber stops where it is. Every other fiber, including the root fiber and detached fibers, stops without unwinding. No drop function and no user code runs, so the handles they hold are not closed by the program. The operating system closes files and sockets when the process ends, and since file writes are not buffered ([`file.md`](file.md), D-994), nothing written with `std::file` is lost.
2. **Output.** Standard output is flushed, so everything that any fiber wrote with `io::print` or `io::println` before `exit` was called appears. Output that other fibers write while `exit` runs may or may not appear. If the flush fails, the status is 1, as for any failed flush (D-306), unless an `io::write_stdout` has already reported that failure (D-574).
3. **The status.** A `status` from 0 to 255 is the exit status. Any other value, negative or larger, gives 255, so a failure never wraps to success (D-1022).
4. **Hosts.** Under `cheby run` and in an executable, the process ends with the status. Under `cheby test`, the test that called `exit` fails with the report `process::exit called with status N`, its fibers are stopped as when a test ends, and the other tests run on (D-723, D-1023). In the REPL, the session ends with the status, as after any termination (D-828, D-1023).
5. **Checks.** A toolchain built with debug assertions does not run the leak check after `exit`, because fibers were stopped holding their values (§10.10, D-710).

The statuses 101, 102 and 103, which the runtime uses for a panic, a failed leak check and a deadlock, are not reserved: a program may exit with them, and then they mean what the program says.

- **Cost:** O(1), plus the flush. Informative.

_Example_, not a `test` because it ends the program:

```cheby
fn main() {
  case env::args() {
    [path] => run(path)
    _ => {
      io::eprintln("usage: wc FILE")
      process::exit(2)
    }
  }
}
```

Here the `case` arm that calls `exit` has type `Never`, which converts to the `Nil` of the other arm (§3.10).

## Interrupts

### `until_interrupted`

```cheby
pub fn until_interrupted<T>(body: fn() -> T) -> Result<T, Interrupt>
```

Calls `body` on the calling fiber in a new scope, as `fiber::timeout` does, and the first `SIGINT` or `SIGTERM` that the process receives while the scope is open cancels it, with the scope as the target ([§10.7](../spec/10-concurrency.md#107-cancellation), D-103, D-1073). Ctrl-C in a terminal sends `SIGINT`, and a container's stop sends `SIGTERM`. Everything inside unwinds, so handles close, `with_*` scopes end, and the program's own error path runs, where a signal would otherwise kill the process with buffered output lost.

- If `body` returns before a cancellation reaches it, `until_interrupted` returns `Ok` with its value.
- If `body` panics first, it returns `Err(Panicked(p))`.
- Otherwise it returns `Err(Interrupted)`, once every frame of `body` has unwound and every fiber in the scopes it opened has ended.

The signal's effect:

1. **Every scope.** A signal cancels every `until_interrupted` scope open in the program. A signal while none is open takes the default action, as before (D-1073).
2. **A second signal.** After the first signal the default action is restored, so a second Ctrl-C ends the process at once while a shutdown is stuck (D-1073).
3. **Deadlocks.** An open scope is not pending work, so a program whose fibers all block on channels inside one is still reported as deadlocked, as with an idle `net::Listener` (§10.4, D-984, D-1073).
4. **Hosts.** Under `cheby test` and in the REPL, no signal reaches the scope: Ctrl-C keeps the host's behavior, and `body` runs as in any scope (§13.5, §13.6, D-1073). On Windows no signal reaches the scope until M6, which brings `SetConsoleCtrlHandler`.
5. **The exit status.** `main` that returns after `Err(Interrupted)` exits with the status of its result (§10.10). A program that wants the shell's convention calls `exit(130)` (D-1073).
6. **HTTP.** Requests in flight in `http::serve` are cancelled with the rest, not drained (D-1064, D-1073).

A tail call to `until_interrupted` stays a tail call, because it calls a function value, so a deadlock report locates a fiber waiting in it at the nearest frame outside std that is still on the stack (D-1059).

- **Fails:** `Err(Interrupted)` when a signal arrived first, and `Err(Panicked(p))` when `body` panicked first.
- **Cost:** O(1), plus the cost of `body`. Informative.

```cheby
test "until_interrupted returns the value of a body that finishes" {
  assert process::until_interrupted(fn() { 6 * 7 }) == Ok(42)
}

test "until_interrupted reports a panic as Panicked" {
  let outcome = process::until_interrupted(fn() { panic as "crash" })
  let assert Err(process::Panicked(failure)) = outcome
  assert fiber::message(failure) == "crash"
}
```

_Example_, not a `test` because it needs a signal. A server that stops on Ctrl-C:

```cheby
fn main() -> Result<Nil, http::Error> {
  let served = process::until_interrupted(fn() {
    http::serve(8080, fn(_) { http::text(200, "home") })
  })
  case served {
    Ok(outcome) => outcome
    Err(process::Interrupted) => Ok(Nil)
    Err(process::Panicked(failure)) => panic as fiber::message(failure)
  }
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-1024):

- starting other processes, reading their output and waiting for them;
- the process id, the parent's id and the working directory;
- signals other than `SIGINT` and `SIGTERM`, and handling a signal in any way other than [`until_interrupted`](#until_interrupted) (D-1073);
- changing environment variables (D-988).

_Note_: the tier that starts processes follows these constraints (D-1078). No signature is fixed yet.

- An opaque `Command` builder, never an exposed record (D-1009's reason), whose builders that replace a part are named `with_`, such as `with_env` (D-1051).
- The program and its arguments are a list, never a shell string.
- A cancelled wait for a child kills and reaps it.
- The child's pipes satisfy `io::Source` and `io::Sink` ([`io.md`](io.md#streams), D-1072).
