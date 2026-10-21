# `std::process`

The running program as a process. Tier 3 has one function, [`exit`](#exit), which ends the program with a chosen status. Command-line arguments and environment variables stay in [`std::env`](env.md) (D-183, D-988). Tier 3 does not start other processes: nothing needs it yet, browsers cannot provide it, and adding it later breaks nothing (D-1024).

- **When to use it.** A program usually ends by returning from `main`: `Nil` or `Ok(Nil)` gives status 0, and `Err(e)` prints `e` and gives status 1 ([§10.10](../spec/10-concurrency.md#1010-program-entry-and-exit), D-182). `exit` is for the statuses those two cannot give, such as 2 for a usage error, which tools like `grep` and `diff` use, and for ending at once from deep inside a program.
- **Kernel.** `exit` is written in Cheby over the private `@std_context` kernel `sys_exit`, bound as `@external(native, "cheby_process_sys_exit")` and `@external(js, "./process.mjs", "sys_exit")` (D-484, D-661, D-692, D-985, D-986). Reaching it while computing a constant is E0077 (D-537).
- **On JS.** OQ-P14, with M4. On Node, Deno and Bun the kernel must drain standard output before it calls the host's exit, because Node's `process.exit()` can drop output still queued to a pipe, which would break D-346's flush. A browser has no process, and the direction is that `exit` stops every fiber and reports the status to the page's host, as a termination does.

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

## Not in tier 3

Each of these can be added later without breaking a program (D-1024):

- starting other processes, reading their output and waiting for them;
- the process id, the parent's id and the working directory;
- signals, including handling `SIGINT` and `SIGTERM` for a graceful shutdown;
- changing environment variables (D-988).
