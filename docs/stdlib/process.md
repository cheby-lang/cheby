# `std::process`

The running program as a process, and the programs it starts. Tier 3 has [`exit`](#exit), which ends the program with a chosen status, [`until_interrupted`](#until_interrupted), which turns Ctrl-C or a request to stop into a cancellation of the code it runs (D-1073), and functions that start other programs, wait for them, and read and write their streams (D-1907). Command-line arguments and environment variables stay in [`std::env`](env.md) (D-183, D-988). In the form `cheby doc` prints:

```cheby
pub exposed type Interrupt {
  Interrupted,
  Panicked(fiber::Panic),
}

pub type Command

pub type Stream

pub type Child

pub type Reader

pub type Writer

pub exposed type Status {
  Exited(Int),
  Signaled(Int),
}

pub exposed type Output {
  status: Status,
  stdout: bytes::Bytes,
  stderr: bytes::Bytes,
}
```

- **`Interrupt`.** The error of [`until_interrupted`](#until_interrupted), with the shape of `fiber::Timeout`. It is `exposed`, so other packages match `process::Interrupted` and `process::Panicked(p)` in patterns and compare with `==` (D-181, D-1073).
- **`Command` and `Stream`.** A `Command` describes a program to start: its arguments, its working directory, its environment and where its streams go ([Describing a command](#describing-a-command)). A `Stream` is one place for a stream ([Streams](#streams)). Both are opaque data, built only through this module's functions, so parts can be added later. They hold a handle only through [`to_file`](#to_file) (D-1078, D-1909, D-1919).
- **`Status` and `Output`.** `Status` is how a child ended: `Exited(code)` with an exit status from 0 to 255, or `Signaled(number)` with the number of the signal that ended it. `Output` is what [`output`](#output) returns. Both are `exposed`, because an outside specification fixes their cases, so they match in `case` and compare with `==` (ADR-0051, D-1910).
- **Handles.** `Child`, `Reader` and `Writer` are handles ([§9.5](../spec/09-memory-model.md#95-handles-and-drop-functions), D-101), external types of this module whose drop functions the runtime supplies (D-703, D-982, D-1919). They print as `<Child>`, `<Reader>` and `<Writer>`, `==` and hashing panic when they reach one, W0011 warns about a comparison that can reach one, and no constant may hold one (D-233). A child lives inside [`with_child`](#with_child), which stops it when its body ends (D-1912, ADR-0086). Every later operation on the child or its pipes fails with `io::is_closed`. A `Child`'s drop function sends SIGKILL to a child still running and hands it to a process-wide reaper, and a pipe's drop function closes its descriptor. They never wait (D-703, D-1919). The reaper never waits on one child: it checks every child handed to it without waiting, so one that does not end holds up no other (D-1928).
- **Errors.** Every function that can fail returns `io::Error`. The kinds this module causes have predicates in [`std::io`](io.md), each with a fixed text: `io::is_not_found`, `io::is_permission_denied`, `io::is_invalid_input`, `io::is_invalid_data`, `io::is_closed` and `io::is_broken_pipe` (D-981, D-1915). A program that ran is `Ok` whatever its status, as an HTTP response of any status is `Ok`, and `Err` means that it could not be started, waited for, read or written (D-1008, D-1910).
- **Where work runs.** Starting a child runs on the blocking pool, and a cancellation that arrives meanwhile waits for the start, then kills and reaps the child (D-670, D-816, D-1916). Waiting for a child's exit and every read and write of its pipes wait on the runtime's poller, so a cancellation interrupts them at once ([§10.9](../spec/10-concurrency.md#109-blocking-io), D-983, D-1914). The runtime learns of an exit through `pidfd_open` on Linux, which needs Linux 5.3, and through a kqueue with `EVFILT_PROC` on macOS. A fiber that waits for a child counts as pending IO, so a program whose fibers wait only on children is never reported as deadlocked (D-984, D-1916).
- **No timeouts of its own.** No function here gives up after a time. A caller that needs a deadline wraps the call in `fiber::timeout` ([§10.8](../spec/10-concurrency.md#108-timeouts), D-103). A cancelled [`wait`](#wait) takes nothing, so the child runs on (D-1913).
- **Several fibers.** Any fiber may use a `Child`, `Reader` or `Writer` it holds while the child's `with_child` runs. Reads on one `Reader` are served one at a time, and so are writes on one `Writer`, in the order they reach it. Each write is atomic with respect to other writes on the same `Writer` (D-1001, D-1914). A child that answers while it reads needs one fiber that writes its standard input and another that reads its standard output, because one fiber that writes everything first can fill both pipes and wait for ever.
- **Streams.** `Reader` satisfies `io::Source` through [`read_bytes`](#read_bytes), and `Writer` satisfies `io::Sink` through [`write_bytes`](#write_bytes), with no code of their own, so [`io::copy`](io.md#copy) and other code generic over a stream take a pipe as they take a `file::File` (§8.2, D-1072, D-1914).
- **Written in Cheby.** `exit` and every function that starts a program are written in Cheby over private `@std_context` kernels named `sys_<name>`, bound as `@external(native, "cheby_process_sys_<name>")` and `@external(js, "./process.mjs", "sys_<name>")`, with `@async` where a JS host returns a `Promise` (D-484, D-661, D-692, D-985, D-986, D-1919). Each kernel that waits submits its own job to the blocking pool or registers its own wait with the poller, and no kernel calls a Cheby closure, so [`with_child`](#with_child) calls `body` in Cheby (D-484, D-983, D-1916). `until_interrupted` is written in Cheby over package-visible runtime operations of `std::fiber` that open and close a scope with a signal trigger, as `fiber::timeout` opens one with a timer, so no kernel calls `body` (D-675, D-790, D-1073). No function that starts, waits for, signals or talks to a child runs in constants: reaching one while computing a constant is E0077 (D-537, D-1919). `command`, the builders, the streams and `is_success` are plain Cheby and run there, but no constant may hold a `Command` or a `Stream`, since either can hold a `file::File` (D-233).
- **On JS.** OQ-P14, with M4. On Node, Deno and Bun the kernel of `exit` must drain standard output before it calls the host's exit, because Node's `process.exit()` can drop output still queued to a pipe, which would break D-346's flush. A browser has no process, and the direction is that `exit` stops every fiber and reports the status to the page's host, as a termination does. JS hosts that have signals get their listeners for `until_interrupted` in M4, and a browser never interrupts its scope (D-1073). On Node, Deno and Bun the kernels that start programs bind the host's child processes. In a browser every runner fails with a new `io::is_unsupported` kind (D-1919).
- **On Windows.** Until M6, every kernel that starts, waits for, signals or talks to a child ends the program with a fatal runtime error that names its operation, as `std::net`'s kernels do (D-714, D-983, D-1919). `exit` and `until_interrupted` work there as this page describes.
- **The program's end.** At every end of the program that the runtime controls, every child still open is killed with SIGKILL: `exit`, `main` returning or failing while another fiber holds a child, a panic that ends `main`, a deadlock, a fatal runtime error, and the end of a REPL session. Killing runs no user code and does not wait for a child, so `exit` still ends the program at once ([§10.10](../spec/10-concurrency.md#1010-program-entry-and-exit), D-1022, D-1917). A start that a detached fiber makes after that end is refused, and a child whose start was under way is killed too, so none outlives the program (D-1917). The end of a REPL session kills that session's children only. A program ended by a signal's default action or by SIGKILL leaves its children running, since no code runs then. A program that must stop its children on Ctrl-C or SIGTERM runs inside [`until_interrupted`](#until_interrupted), so the signal unwinds every `with_child` and each stops its child (D-1917).
- **Under `cheby test`.** A test's children end with its fibers, which unwind (D-1294, D-1917). An inherited standard output or standard error of a child goes to a temporary file, whose bytes join the test's captured output when the child is reaped, so they stay with their test (D-723). The order between the child's output and the test's own writes while the child runs is not kept, nor the order between the child's two streams. When an unwinding body kills such a child and leaves it to the reaper, the test's capture keeps the file and joins its bytes when the test ends, after the test's other output (D-1922). An inherited standard input is the process's, as `io::read_line` has it there. In the REPL and under `cheby run`, inherited streams are the real ones ([§13.6](../spec/13-tooling.md#136-tests), D-1918).
- **When to use `exit`.** A program usually ends by returning from `main`: `Nil` or `Ok(Nil)` gives status 0, and `Err(e)` prints `e` and gives status 1 ([§10.10](../spec/10-concurrency.md#1010-program-entry-and-exit), D-182). `exit` is for the statuses those two cannot give, such as 2 for a usage error, which tools like `grep` and `diff` use, and for ending at once from deep inside a program.

The tests on this page start programs that every Unix system has: `sh`, `printf`, `cat`, `true`, `false` and `sleep`. Each test reads what it checks through a pipe or a file, or discards it, so it prints nothing and runs the same on Linux and macOS. They do not run on Windows until M6 (D-1919, D-1920).

_Note_: [`with_child`](#with_child) inherits standard error unless the command sets it, and [`run`](#run) inherits all three streams. The standard library's own tests run without capturing their output (D-837), so no test on this page lets a child write to an inherited stream: a child that writes to standard error has it piped, discarded or merged into a captured standard output.

## Exiting

### `exit`

```cheby
pub fn exit(status: Int) -> Never
```

Ends the program with exit status `status`, as a termination does (D-709, D-1022). It never returns, so it fits anywhere a value is expected ([§3.10](../spec/03-types.md#310-never)).

1. **Other fibers.** The calling fiber stops where it is. Every other fiber, including the root fiber and detached fibers, stops without unwinding. No drop function and no user code runs, so the handles they hold are not closed by the program. The operating system closes files and sockets when the process ends, and since file writes are not buffered ([`file.md`](file.md), D-994), nothing written with `std::file` is lost.
2. **Output.** Standard output is flushed, so everything that any fiber wrote with `io::print` or `io::println` before `exit` was called appears. Output that other fibers write while `exit` runs may or may not appear. If the flush fails, the status is 1, as for any failed flush (D-306), unless an `io::write_stdout` has already reported that failure (D-574).
3. **The status.** A `status` from 0 to 255 is the exit status. Any other value, negative or larger, gives 255, so a failure never wraps to success (D-1022).
4. **Hosts.** Under `cheby run` and in an executable, the process ends with the status. Under `cheby test`, the test that called `exit` fails with the report `process::exit called with status N`, its fibers are cancelled and unwind as when a test ends, so unlike item 1 their cleanup runs, and the other tests run on (D-723, D-724, D-1023, D-1294). In the REPL, the session ends with the status, as after any termination (D-828, D-1023).
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

- If `body` returns before a cancellation reaches it, `until_interrupted` returns `Ok` with its value. When a signal cancelled the scope and every scope it cancelled returned so, the signal reached nothing: the last of them to close sends it again before `until_interrupted` returns, and the process takes the default action, with buffered output lost, as when no scope is open (item 1, D-1299).
- If `body` panics first, it returns `Err(Panicked(p))`.
- Otherwise it returns `Err(Interrupted)`, once every frame of `body` has unwound and every fiber in the scopes it opened has ended.

The signal's effect:

1. **Every scope.** A signal cancels every `until_interrupted` scope open in the program when it arrives. A signal while none is open takes the default action, as before, and so does one that interrupts no scope, because each closed first or its body returned before the cancellation reached it (D-1073, D-1299).
2. **A second signal.** After the first signal the default action is restored for the rest of the process, so a second Ctrl-C ends the process at once while a shutdown is stuck, even after another `until_interrupted` scope opens (D-1073, D-1299).
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

## Describing a command

A `Command` names a program and its arguments as a list, never as a shell line, so no argument is ever split or expanded (D-1078, D-1908). Each builder returns a copy of the command with one part replaced, and the original stays as it was (D-1051).

- **Finding the program.** A program without `/` is searched in the `PATH` that the child gets, after the edits of [`with_env`](#with_env) and the others. A relative program with `/`, such as `./build.sh`, is resolved against the program's own working directory, also when [`with_dir`](#with_dir) is set (D-1908). A child that gets no `PATH` is searched in `/usr/bin:/bin`, as `execvp` does (D-1925). An executable file that the system refuses to run, such as a script without a `#!` line, runs through `/bin/sh` with its path and arguments, as POSIX has `execvp` do, on every platform (D-1926).
- **The environment.** The child inherits the program's variables, with the edits of `with_env`, `without_env` and `with_empty_env` applied in the order they were made (D-1908).
- **Checked before the start.** Every runner fails with `Err` for which `io::is_invalid_input` holds, before anything starts, when the program, an argument, the directory, a variable's name or a value holds U+0000, when the program is empty, and when a name is empty or holds `=` (D-1908).
- **Printing.** The debug text of a `Command` shows the program and the arguments. It keeps each value given to `with_env` as a `secret::Secret<String>`, so the text shows `<redacted>` in its place, since tokens are passed in the environment far more often than as arguments ([`std::secret`](secret.md), D-1901, D-1908).

### `command`

```cheby
pub fn command(program: String, args: List<String>) -> Command
```

Describes a run of `program` with the arguments `args`, with the program's working directory and environment and the streams of the runner it is given to.

- **Cost:** O(n) in the number of arguments. Informative.

```cheby
test "command passes each argument as it is" {
  let spec = process::command("printf", ["%s|%s", "two words", "*"])
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("two words|*")
  Ok(Nil)
}

test "a command that cannot be started is invalid input" {
  let assert Err(empty) = process::run(process::command("", []))
  assert io::is_invalid_input(empty)
  let assert Err(zero) = process::run(process::command("printf", ["a\u{0}b"]))
  assert io::is_invalid_input(zero)
}

test "a program that does not exist is not found" {
  let assert Err(reason) = process::run(process::command("cheby-no-such-program", []))
  assert io::is_not_found(reason)
  assert "{reason}" == "not found"
  let assert Err(relative) = process::run(process::command("./no/such/program", []))
  assert io::is_not_found(relative)
}
```

### `with_dir`

```cheby
pub fn with_dir(spec: Command, path: String) -> Command
```

Returns `spec` with the child's working directory set to `path`. A relative `path` is resolved against the program's working directory. It does not change where a relative program with `/` is found (D-1908).

- **Cost:** O(1). Informative.

```cheby
test "with_dir runs the child in a directory" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(filepath::join(dir, "notes.txt"), "inside"))
  let spec = process::command("cat", ["notes.txt"])
    |> process::with_dir(_, dir)
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("inside")
  Ok(Nil)
}

test "a directory that does not exist is not found" {
  let spec = process::command("true", [])
    |> process::with_dir(_, "no/such/directory")
  let assert Err(reason) = process::run(spec)
  assert io::is_not_found(reason)
}
```

### `with_env`

```cheby
pub fn with_env(spec: Command, name: String, value: String) -> Command
```

Returns `spec` with the child's variable `name` set to `value`. The value is kept as a `secret::Secret<String>`, so the debug text of the command never shows it (D-1908).

- **Cost:** O(1). Informative.

```cheby
test "with_env sets a variable of the child" {
  let spec = process::command("sh", ["-c", "printf %s \"$CHEBY_GREETING\""])
    |> process::with_env(_, "CHEBY_GREETING", "hello")
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("hello")
  Ok(Nil)
}

test "the debug text of a command hides the values of variables" {
  let spec = process::command("curl", ["--silent", "https://example.com"])
    |> process::with_env(_, "TOKEN", "hunter2")
  let text = "{spec:?}"
  assert string::contains(text, "curl")
  assert string::contains(text, "--silent")
  assert string::contains(text, "<redacted>")
  assert !string::contains(text, "hunter2")
}

test "a variable name with = is invalid input" {
  let spec = process::command("true", [])
    |> process::with_env(_, "A=B", "c")
  let assert Err(reason) = process::run(spec)
  assert io::is_invalid_input(reason)
  let unnamed = process::command("true", [])
    |> process::with_env(_, "", "c")
  let assert Err(empty) = process::run(unnamed)
  assert io::is_invalid_input(empty)
}
```

### `without_env`

```cheby
pub fn without_env(spec: Command, name: String) -> Command
```

Returns `spec` with the child's variable `name` removed, whether the program has it or an earlier [`with_env`](#with_env) set it.

- **Cost:** O(1). Informative.

```cheby
test "without_env removes a variable" {
  let spec = process::command("sh", ["-c", "printf %s \"$CHEBY_GREETING\""])
    |> process::with_env(_, "CHEBY_GREETING", "hello")
    |> process::without_env(_, "CHEBY_GREETING")
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::new()
  Ok(Nil)
}
```

### `with_empty_env`

```cheby
pub fn with_empty_env(spec: Command) -> Command
```

Returns `spec` so that the child starts with no variables but those that a later [`with_env`](#with_env) sets. The earlier edits no longer count. Without `PATH`, a program without `/` is searched in `/usr/bin:/bin` (D-1925).

- **Cost:** O(1). Informative.

```cheby
test "with_empty_env keeps only the variables set after it" {
  let spec = process::command("/bin/sh", ["-c", "printf %s \"$CHEBY_KEPT|$CHEBY_GONE\""])
    |> process::with_env(_, "CHEBY_GONE", "before")
    |> process::with_empty_env
    |> process::with_env(_, "CHEBY_KEPT", "after")
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("after|")
  Ok(Nil)
}
```

### `with_input`

```cheby
pub fn with_input(spec: Command, data: bytes::Bytes) -> Command
```

Returns `spec` so that the child reads `data` on a pipe as its standard input, which is closed after the last byte. Of `with_input` and [`with_stdin`](#with_stdin), the later call wins. A child that stops reading before the end of `data` is not an error: [`run`](#run) and [`output`](#output) stop writing and return its status (D-1911). Given to [`with_child`](#with_child), it makes that runner fail with `Err` for which `io::is_invalid_input` holds, because the body writes standard input itself (D-1912).

- **Cost:** O(1). Writing `data` is part of the run. Informative.

```cheby
test "with_input reaches the child's standard input" {
  let spec = process::command("cat", [])
    |> process::with_input(_, bytes::from_string("one\ntwo\n"))
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("one\ntwo\n")
  Ok(Nil)
}

test "the later of with_input and with_stdin wins" {
  let dropped = process::command("cat", [])
    |> process::with_input(_, bytes::from_string("lost"))
    |> process::with_stdin(_, process::discarded())
  use first <- result::try(process::output(dropped))
  assert first.stdout == bytes::new()
  let kept = process::command("cat", [])
    |> process::with_stdin(_, process::discarded())
    |> process::with_input(_, bytes::from_string("kept"))
  use second <- result::try(process::output(kept))
  assert second.stdout == bytes::from_string("kept")
  Ok(Nil)
}

test "a child that stops reading its input is not an error" {
  let data = bytes::from_string(string::repeat("x", 1_000_000))
  let spec = process::command("true", [])
    |> process::with_input(_, data)
  assert process::run(spec) == Ok(process::Exited(0))
}
```

### `with_stdin`

```cheby
pub fn with_stdin(spec: Command, stream: Stream) -> Command
```

Returns `spec` with the child's standard input taken from `stream` ([Streams](#streams)). Of [`with_input`](#with_input) and `with_stdin`, the later call wins.

- **Cost:** O(1). Informative.

```cheby
test "a file as standard input starts after what the program has read" {
  use dir <- file::with_temp_dir()
  let path = filepath::join(dir, "table.txt")
  use _ <- result::try(io::write_file(path, "header\nrow 1\nrow 2\n"))
  use source <- file::with_open(path)
  assert file::read_line(source) == Ok(Some("header"))
  let spec = process::command("cat", [])
    |> process::with_stdin(_, process::to_file(source))
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("row 1\nrow 2\n")
  Ok(Nil)
}
```

### `with_stdout`

```cheby
pub fn with_stdout(spec: Command, stream: Stream) -> Command
```

Returns `spec` with the child's standard output sent to `stream` ([Streams](#streams)). After [`with_stderr_to_stdout`](#with_stderr_to_stdout), standard error goes there too.

- **Cost:** O(1). Informative.

The tests of [`discarded`](#discarded) and [`to_file`](#to_file) use it.

### `with_stderr`

```cheby
pub fn with_stderr(spec: Command, stream: Stream) -> Command
```

Returns `spec` with the child's standard error sent to `stream` ([Streams](#streams)). It undoes an earlier [`with_stderr_to_stdout`](#with_stderr_to_stdout), and a later one undoes it.

- **Cost:** O(1). Informative.

```cheby
test "with_stderr sends standard error elsewhere" {
  let spec = process::command("sh", ["-c", "printf oops >&2"])
    |> process::with_stderr(_, process::discarded())
  use captured <- result::try(process::output(spec))
  assert captured.status == process::Exited(0)
  assert captured.stderr == bytes::new()
  Ok(Nil)
}
```

### `with_stderr_to_stdout`

```cheby
pub fn with_stderr_to_stdout(spec: Command) -> Command
```

Returns `spec` so that the child's standard error goes wherever its standard output goes, as `2>&1` does in a shell. [`output`](#output) then returns both in `stdout`, with an empty `stderr` (D-1909).

- **Cost:** O(1). Informative.

```cheby
test "with_stderr_to_stdout merges the two streams" {
  let spec = process::command("sh", ["-c", "printf a; printf b >&2; printf c"])
    |> process::with_stderr_to_stdout
  use captured <- result::try(process::output(spec))
  assert captured.stdout == bytes::from_string("abc")
  assert captured.stderr == bytes::new()
  Ok(Nil)
}
```

## Streams

A `Stream` says where one of a child's streams goes. There are four places: the program's own stream, the null device, a pipe that the program reads or writes, and an open file (D-1909). `Stream` is opaque, so more places can come later.

Each runner has defaults for the streams that a command does not set (D-1909):

| Runner                      | Standard input | Standard output | Standard error |
| --------------------------- | -------------- | --------------- | -------------- |
| [`run`](#run)               | inherited      | inherited       | inherited      |
| [`output`](#output)         | empty          | captured        | captured       |
| [`with_child`](#with_child) | piped          | piped           | inherited      |

`with_child` inherits standard error because a piped standard error that nobody reads fills up and stops the child. Before a child starts, standard output is flushed, so the program's earlier output comes before the child's (D-1916). The child gets descriptors 0 to 2 and no others. On macOS and on Linux 5.11 and later every descriptor from 3 up stays out of the child, also one that foreign code opened without close-on-exec. On Linux before 5.11 only the runtime's own descriptors stay out, since it opens all of them close-on-exec (D-1916, D-1923). The child gets the default action of SIGPIPE, which the program itself ignores (D-925), and an empty signal mask. No other signal's action is reset (D-1916, D-1925). When the program inherited SIGCHLD's action as "ignore", the runtime sets it back to the default before its first child starts, for the rest of the process, so the system keeps each child until the runtime reaps it, and children get the default action too. A program that starts no child keeps the action it inherited (D-1927).

_Note_: a signal that the program ignores, such as SIGINT under `nohup` or in a shell's background job, stays ignored in the child, as POSIX has it. Only SIGPIPE gets its default action back (D-1925).

### `inherited`

```cheby
pub fn inherited() -> Stream
```

The program's own stream: the child reads the program's standard input, or writes to its standard output or standard error. Under `cheby test` a child's inherited output goes to the test's capture ([§13.6](../spec/13-tooling.md#136-tests), D-1918).

As standard input, `inherited()` first gives the child the bytes that the program's reader of standard input has read ahead and not taken (D-823). The start moves the descriptor back over them where the descriptor can seek, as for a file, so a program run with `< file` that reads a line and then runs a child gives the child the rest. Where it cannot seek, as for a pipe or a terminal, those bytes stay with the program's reader and the child never sees them. That reader reads up to 64 KiB at a time, so after a read of standard input from a pipe, a child that inherits it can miss up to 64 KiB that the program has not taken (D-1921).

- **Cost:** O(1). Informative.

```cheby
test "inherited gives the child the program's own stream" {
  let spec = process::command("true", [])
    |> process::with_stdin(_, process::inherited())
  assert process::run(spec) == Ok(process::Exited(0))
}
```

### `discarded`

```cheby
pub fn discarded() -> Stream
```

The null device: as standard input the child reads its end at once, and as an output what the child writes is dropped.

- **Cost:** O(1). Informative.

```cheby
test "discarded drops what the child writes" {
  let spec = process::command("printf", ["dropped"])
    |> process::with_stdout(_, process::discarded())
  use captured <- result::try(process::output(spec))
  assert captured.status == process::Exited(0)
  assert captured.stdout == bytes::new()
  Ok(Nil)
}
```

### `piped`

```cheby
pub fn piped() -> Stream
```

A pipe between the program and the child, which the body of [`with_child`](#with_child) reads through [`stdout`](#stdout) and [`stderr`](#stderr) and writes through [`stdin`](#stdin). Given to [`run`](#run) for any stream, it makes `run` fail with `Err` for which `io::is_invalid_input` holds, since no code could read or write the pipe (D-1911). In [`output`](#output), `piped()` for standard output or standard error is the capture that `output` makes anyway, and for standard input it fails with `io::is_invalid_input`, as in `run` (D-1924).

- **Cost:** O(1). Informative.

```cheby
test "piped is invalid input for run" {
  let spec = process::command("true", [])
    |> process::with_stdout(_, process::piped())
  let assert Err(reason) = process::run(spec)
  assert io::is_invalid_input(reason)
}

test "piped is the capture of output, and invalid input as its standard input" {
  let capturing = process::command("printf", ["kept"])
    |> process::with_stdout(_, process::piped())
  use ran <- result::try(process::output(capturing))
  assert ran.stdout == bytes::from_string("kept")
  let fed = process::command("cat", [])
    |> process::with_stdin(_, process::piped())
  let assert Err(reason) = process::output(fed)
  assert io::is_invalid_input(reason)
  Ok(Nil)
}
```

### `to_file`

```cheby
pub fn to_file(handle: file::File) -> Stream
```

The open file `handle`. After the start the program and the child share the file's position, as in a shell. The handles of the standard streams act as [`inherited`](#inherited): `to_file(file::stdout())` and `to_file(file::stderr())` for that stream, so `cheby test` captures them, and `to_file(file::stdin())` for standard input, with the read-ahead bytes given back as there (D-1071, D-1921, D-1924).

As standard input, any other file opened for reading gives the child the bytes that the program has not taken yet: the start moves the descriptor back over what the handle read ahead (D-995, D-1909). The runner fails with `Err` for which `io::is_closed` holds when `handle` is closed, and `io::is_invalid_input` when `handle`, given as standard input, holds bytes it read ahead and cannot seek back over them, as a pipe cannot (D-1909). A file opened only for reading given as standard output or standard error, and one opened only for writing given as standard input, fail with `io::is_invalid_input` too, as `file::write` on a reading handle does (D-981, D-1924).

- **Cost:** O(1). Informative.

```cheby
test "to_file sends a stream to an open file" {
  use dir <- file::with_temp_dir()
  let path = filepath::join(dir, "saved.txt")
  use sink <- file::with_create(path)
  let spec = process::command("printf", ["saved"])
    |> process::with_stdout(_, process::to_file(sink))
  assert process::run(spec) == Ok(process::Exited(0))
  use text <- result::try(io::read_file(path))
  assert text == "saved"
  Ok(Nil)
}

test "a closed file is no stream" {
  use dir <- file::with_temp_dir()
  use sink <- result::try(file::create(filepath::join(dir, "closed.txt")))
  use _ <- result::try(file::close(sink))
  let spec = process::command("true", [])
    |> process::with_stdout(_, process::to_file(sink))
  let assert Err(reason) = process::run(spec)
  assert io::is_closed(reason)
  Ok(Nil)
}

test "a file in the wrong mode is invalid input" {
  use dir <- file::with_temp_dir()
  let path = filepath::join(dir, "input.txt")
  use _ <- result::try(io::write_file(path, "text"))
  use source <- file::with_open(path)
  let written = process::command("printf", ["lost"])
    |> process::with_stdout(_, process::to_file(source))
  let assert Err(reason) = process::run(written)
  assert io::is_invalid_input(reason)
  use sink <- file::with_create(filepath::join(dir, "output.txt"))
  let reading = process::command("cat", [])
    |> process::with_stdin(_, process::to_file(sink))
  let assert Err(backwards) = process::run(reading)
  assert io::is_invalid_input(backwards)
  Ok(Nil)
}
```

The test of [`with_stdin`](#with_stdin) gives a child the rest of a file.

## Running

Three functions run a command. [`run`](#run) waits for the child and returns its status, [`output`](#output) also captures what it writes, and [`with_child`](#with_child) gives a body the child and its pipes. There is no function that returns a child to keep (D-1911, D-1912, ADR-0086).

The three fail the same way before the child starts:

- **Fails:** `Err` for which `io::is_invalid_input` holds for a command that [Describing a command](#describing-a-command) rejects, and for the cases of [`piped`](#piped), [`to_file`](#to_file) and [`with_input`](#with_input). `io::is_not_found` when the program or the working directory does not exist, `io::is_permission_denied` when the program may not be run, and `io::is_closed` when a file given to `to_file` is closed. `Err` for every other failure of the operating system, and on Linux before 5.3, which has no `pidfd_open` (D-1916).

### `run`

```cheby
pub fn run(spec: Command) -> Result<Status, io::Error>
```

Starts `spec` and waits for the child to end, then returns its status. By default the child inherits all three streams. A cancellation of `run` kills the child and reaps it (D-1078, D-1911).

- **Fails:** the cases above, and `Err` when the child cannot be waited for.
- **Cost:** O(1), plus starting the program and the wait. Informative.

```cheby
test "run reports an exit status" {
  assert process::run(process::command("true", [])) == Ok(process::Exited(0))
  assert process::run(process::command("false", [])) == Ok(process::Exited(1))
  assert process::run(process::command("sh", ["-c", "exit 3"])) == Ok(process::Exited(3))
}

test "run reports the signal that ended a child" {
  let spec = process::command("sh", ["-c", "kill -9 $$"])
  assert process::run(spec) == Ok(process::Signaled(9))
}
```

### `output`

```cheby
pub fn output(spec: Command) -> Result<Output, io::Error>
```

Starts `spec`, reads its standard output and standard error to their ends, and waits for the child to end. It returns once both ends and the exit have come, so a grandchild that keeps a pipe open keeps `output` waiting, and no output is lost. By default the child's standard input is empty. A stream that the command sends elsewhere is empty in the result. [`piped`](#piped) for standard output or standard error is the capture that `output` makes anyway, and for standard input it fails with `io::is_invalid_input` (D-1924). A cancellation of `output` kills the child and reaps it (D-1078, D-1911).

- **Fails:** the cases above, and `Err` when a stream cannot be read or the child cannot be waited for.
- **Cost:** O(n) in the bytes captured, plus starting the program and the wait. Informative.

```cheby
test "output captures standard output and standard error" {
  let spec = process::command("sh", ["-c", "printf out; printf err >&2; exit 4"])
  use captured <- result::try(process::output(spec))
  assert captured.status == process::Exited(4)
  assert captured.stdout == bytes::from_string("out")
  assert captured.stderr == bytes::from_string("err")
  assert !process::is_success(captured.status)
  Ok(Nil)
}
```

### `with_child`

```cheby
pub fn with_child<T>(spec: Command, body: fn(Child) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Starts `spec`, calls `body` with the child on the calling fiber, and returns what `body` returns, or the `Err` of the start. By default the child's standard input and output are piped and its standard error is inherited ([Streams](#streams)). When `body` returns, a child that is still running is killed, waited for and reaped before `with_child` returns. When the system refuses that SIGKILL, as for a child that changed its user, `with_child` does not wait: it hands the child to the reaper and returns what `body` returned, so the child may outlive `with_child` (D-1928). When `body` unwinds, the child is killed at once, and the runtime reaps it soon after, since an unwinding fiber cannot wait. A `Child` that `body` let escape, for example into a channel or a closure that outlives it, is then killed when its last reference is released or the program ends, as an escaped `File` of `file::with_open` closes (D-993). Every later operation on the `Child` or its pipes fails with `io::is_closed` (D-1912, ADR-0086).

- **Fails:** the cases above, and whatever `body` returns. `Err` for which `io::is_invalid_input` holds when `spec` has [`with_input`](#with_input) (D-1912).
- **Cost:** O(1), plus starting the program, the cost of `body`, and the wait for a child it kills. Informative.

```cheby
test "with_child returns what its body returns" {
  let spec = process::command("printf", ["first\nsecond\n"])
  let found = process::with_child(spec, fn(child) { process::read_line(process::stdout(child)) })
  assert found == Ok(Some("first"))
}

test "a child and its pipes print as placeholders" {
  use child <- process::with_child(process::command("cat", []))
  let reader = process::stdout(child)
  let writer = process::stdin(child)
  assert "{child:?}" == "<Child>"
  assert "{reader:?}" == "<Reader>"
  assert "{writer:?}" == "<Writer>"
  process::close(writer)
}

test "a child is closed once with_child returns" {
  let spec = process::command("cat", [])
  use kept <- result::try(process::with_child(spec, fn(child) { Ok(child) }))
  let assert Err(reason) = process::wait(kept)
  assert io::is_closed(reason)
  let assert Err(late) = process::read_line(process::stdout(kept))
  assert io::is_closed(late)
  Ok(Nil)
}

test "with_input is invalid input for with_child" {
  let spec = process::command("cat", [])
    |> process::with_input(_, bytes::from_string("x"))
  let assert Err(reason) = process::with_child(spec, fn(_) { Ok(Nil) })
  assert io::is_invalid_input(reason)
}
```

### `is_success`

```cheby
pub fn is_success(status: Status) -> Bool
```

Tells whether `status` is `Exited(0)`, the only status that Unix tools use for success (D-1910).

- **Cost:** O(1). Informative.

```cheby
test "is_success holds for an exit with status 0 only" {
  assert process::is_success(process::Exited(0))
  assert !process::is_success(process::Exited(1))
  assert !process::is_success(process::Signaled(9))
}
```

## A child

These functions take the `Child` that [`with_child`](#with_child) gives its body. After `with_child` returns, each of them that returns a `Result` fails with `io::is_closed` (D-1912).

### `wait`

```cheby
pub fn wait(child: Child) -> Result<Status, io::Error>
```

Waits on the runtime's poller until `child` ends, then reaps it and returns its status. A later `wait` returns the same status at once. `wait` never closes the child's standard input, so a child that reads it to its end waits for [`close`](#close). A cancelled `wait` takes nothing: the child runs on, and the end of `with_child` kills it (D-1913).

- **Fails:** `Err` for which `io::is_closed` holds after `with_child` returned. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the wait. Informative.

```cheby
test "wait returns the same status again" {
  use child <- process::with_child(process::command("sh", ["-c", "exit 7"]))
  assert process::wait(child) == Ok(process::Exited(7))
  assert process::wait(child) == Ok(process::Exited(7))
  Ok(Nil)
}
```

The test of [`kill`](#kill) cancels a `wait`.

### `interrupt`

```cheby
pub fn interrupt(child: Child) -> Result<Nil, io::Error>
```

Sends SIGINT to `child`, the signal that Ctrl-C sends, which many tools take as a request to stop cleanly. It returns `Ok(Nil)` also when the child has already ended. Only the child receives the signal, never a process group of its own: the child stays in the program's group, so Ctrl-C in a terminal reaches it too (D-1913).

- **Fails:** `Err` for which `io::is_closed` holds after `with_child` returned.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "a signal to a child that has ended is Ok" {
  use child <- process::with_child(process::command("true", []))
  assert process::wait(child) == Ok(process::Exited(0))
  assert process::interrupt(child) == Ok(Nil)
  assert process::terminate(child) == Ok(Nil)
  assert process::kill(child) == Ok(Nil)
  assert process::wait(child) == Ok(process::Exited(0))
  Ok(Nil)
}
```

### `terminate`

```cheby
pub fn terminate(child: Child) -> Result<Nil, io::Error>
```

Sends SIGTERM to `child`, the polite request to stop, and returns `Ok(Nil)`, also when the child has already ended. As for [`interrupt`](#interrupt), only the child receives it (D-1913).

- **Fails:** `Err` for which `io::is_closed` holds after `with_child` returned.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "terminate ends a child with SIGTERM" {
  use child <- process::with_child(process::command("sleep", ["10"]))
  use _ <- result::try(process::terminate(child))
  assert process::wait(child) == Ok(process::Signaled(15))
  Ok(Nil)
}
```

### `kill`

```cheby
pub fn kill(child: Child) -> Result<Nil, io::Error>
```

Sends SIGKILL to `child`, which the child cannot catch, and returns `Ok(Nil)`, also when the child has already ended. As for [`interrupt`](#interrupt), only the child receives it (D-1913).

- **Fails:** `Err` for which `io::is_closed` holds after `with_child` returned.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "a cancelled wait takes nothing, and kill ends the child" {
  use child <- process::with_child(process::command("sleep", ["10"]))
  let waited = fiber::timeout(duration::milliseconds(20), fn() { process::wait(child) })
  assert waited == Err(fiber::TimedOut)
  use _ <- result::try(process::kill(child))
  assert process::wait(child) == Ok(process::Signaled(9))
  Ok(Nil)
}
```

_Example_, not a `test` because how long a server takes to stop depends on the server. `terminate`, then `fiber::timeout` around `wait`, then `kill` stops a child politely (D-1913):

```cheby
fn stop(child: process::Child) -> Result<process::Status, io::Error> {
  use _ <- result::try(process::terminate(child))
  case fiber::timeout(duration::seconds(5), fn() { process::wait(child) }) {
    Ok(waited) => waited
    Err(_) => {
      use _ <- result::try(process::kill(child))
      process::wait(child)
    }
  }
}
```

### `stdin`

```cheby
pub fn stdin(child: Child) -> Writer
```

Returns the `Writer` of `child`'s standard input, so the program writes what the child reads. Every call returns a handle to the same pipe. Writing to it fails with `io::is_invalid_input` when the child's standard input is not [`piped`](#piped) (D-1914).

- **Cost:** O(1). Informative.

The test of [`write_line`](#write_line) uses it.

### `stdout`

```cheby
pub fn stdout(child: Child) -> Reader
```

Returns the `Reader` of `child`'s standard output, so the program reads what the child writes. Every call returns a handle to the same pipe and its read-ahead buffer. Reading from it fails with `io::is_invalid_input` when the child's standard output is not [`piped`](#piped) (D-1914).

- **Cost:** O(1). Informative.

```cheby
test "a stream that is not piped cannot be read" {
  let spec = process::command("true", [])
    |> process::with_stdout(_, process::discarded())
  use child <- process::with_child(spec)
  let assert Err(reason) = process::read_line(process::stdout(child))
  assert io::is_invalid_input(reason)
  Ok(Nil)
}
```

### `stderr`

```cheby
pub fn stderr(child: Child) -> Reader
```

Returns the `Reader` of `child`'s standard error, as [`stdout`](#stdout) does for standard output. `with_child` inherits standard error unless the command sets it, so reading it needs [`with_stderr`](#with_stderr) with [`piped`](#piped). Reading from it fails with `io::is_invalid_input` when the child's standard error is not piped (D-1914).

- **Cost:** O(1). Informative.

```cheby
test "a piped standard error is read through stderr" {
  let spec = process::command("sh", ["-c", "printf oops >&2"])
    |> process::with_stderr(_, process::piped())
  use child <- process::with_child(spec)
  let errors = process::stderr(child)
  assert process::read_line(errors) == Ok(Some("oops"))
  assert process::read_line(errors) == Ok(None)
  Ok(Nil)
}
```

## Pipes

A `Reader` reads a child's standard output or standard error, and a `Writer` writes its standard input. The functions have the names and shapes of `std::file`'s, so code generic over a stream takes a pipe (D-1914).

- **Reading.** A `Reader` has one read-ahead buffer, which [`read_bytes`](#read_bytes) and [`read_line`](#read_line) share, as a `file::File` has (D-995). Both report the end of the stream, once the child has closed its side and everything before it has been read, as `Ok(None)`, and keep returning it.
- **Writing.** A write returns once all of its data is in the pipe, waiting while the pipe is full. Each write is atomic with respect to other writes on the `Writer`. A write to a pipe whose reading end is closed, because the child closed its standard input or ended, fails with `io::is_broken_pipe`, and never ends the program by a signal (D-925, D-1915).
- **Cancellation.** Pipes wait on the poller, so every wait can be cancelled. A cancelled read has taken nothing, and a cancelled write may have written a prefix of its data, as on a socket (D-1000, D-1914).
- **Closed.** After [`close`](#close), and for every pipe once `with_child` has returned, every function here fails with `io::is_closed` (D-1912).

### `read_bytes`

```cheby
pub fn read_bytes(reader: Reader, limit: Int) -> Result<Option<bytes::Bytes>, io::Error>
```

Waits until `reader` has data, then returns at least one and at most `limit` bytes of it, without decoding them. `limit` caps the result and never sizes an allocation: a read takes at most one block of 64 KiB (D-1062, D-1914).

- **Fails:** `Err` for which `io::is_invalid_input` holds when the stream is not piped, and `io::is_closed` after `with_child` returned. `Err` for every other failure of the operating system.
- **Clamps:** `limit` below 1 counts as 1 (D-298).
- **Cost:** O(n) in the number of bytes returned, plus the wait. Informative.

```cheby
test "read_bytes returns at most its limit" {
  use child <- process::with_child(process::command("printf", ["abc"]))
  let reader = process::stdout(child)
  assert process::read_bytes(reader, 0) == Ok(Some(bytes::from_string("a")))
  fn collect(received) {
    case process::read_bytes(reader, 2) {
      Ok(Some(chunk)) => {
        assert bytes::length(chunk) <= 2
        collect(bytes::append(received, chunk))
      }
      Ok(None) => Ok(received)
      Err(reason) => Err(reason)
    }
  }
  use rest <- result::try(collect(bytes::new()))
  assert rest == bytes::from_string("bc")
  Ok(Nil)
}

test "a child's standard output is an io::Source" {
  use dir <- file::with_temp_dir()
  let path = filepath::join(dir, "copied.txt")
  use sink <- file::with_create(path)
  use child <- process::with_child(process::command("printf", ["copied"]))
  use count <- result::try(io::copy(process::stdout(child), sink))
  assert count == 6
  use text <- result::try(io::read_file(path))
  assert text == "copied"
  Ok(Nil)
}
```

### `read_line`

```cheby
pub fn read_line(reader: Reader) -> Result<Option<String>, io::Error>
```

Reads the next line from `reader`, as `file::read_line` reads a file: `Ok(Some(line))` without its line ending, removing a final `\n` or `\r\n`, and `Ok(None)` at the end of the stream (D-305, D-1914). The last line may have no ending.

- **Fails:** `Err` for which `io::is_invalid_data` holds when the line is not valid UTF-8. The line is consumed, so the next call reads the line after it (D-311). `io::is_invalid_input` when the stream is not piped, `io::is_closed` after `with_child` returned, and `Err` for every other failure of the operating system.
- **Cost:** O(n) in the length of the line, plus the wait. Informative.

```cheby
test "read_line reads a child's output line by line" {
  use child <- process::with_child(process::command("printf", ["one\r\ntwo\nthree"]))
  let reader = process::stdout(child)
  assert process::read_line(reader) == Ok(Some("one"))
  assert process::read_line(reader) == Ok(Some("two"))
  assert process::read_line(reader) == Ok(Some("three"))
  assert process::read_line(reader) == Ok(None)
  assert process::read_line(reader) == Ok(None)
  assert process::wait(child) == Ok(process::Exited(0))
  Ok(Nil)
}
```

### `write_bytes`

```cheby
pub fn write_bytes(writer: Writer, data: bytes::Bytes) -> Result<Nil, io::Error>
```

Writes all of `data` to `writer`, waiting while the pipe is full. A cancelled `write_bytes` may have written a prefix of `data` (D-1000, D-1914).

- **Fails:** `Err` for which `io::is_broken_pipe` holds when the child closed its standard input or ended (D-1915), `io::is_invalid_input` when the stream is not piped, and `io::is_closed` after [`close`](#close) or after `with_child` returned. `Err` for every other failure of the operating system.
- **Cost:** O(n) in the length of `data`, plus the wait. Informative.

```cheby
test "writing to a child that has ended is a broken pipe" {
  use child <- process::with_child(process::command("true", []))
  assert process::wait(child) == Ok(process::Exited(0))
  let assert Err(reason) = process::write_bytes(process::stdin(child), bytes::from_string("late"))
  assert io::is_broken_pipe(reason)
  assert "{reason}" == "broken pipe"
  Ok(Nil)
}
```

### `write`

```cheby
pub fn write(writer: Writer, text: String) -> Result<Nil, io::Error>
```

Writes `text` to `writer` in UTF-8, as [`write_bytes`](#write_bytes) writes bytes.

- **Fails:** the cases of [`write_bytes`](#write_bytes).
- **Cost:** O(n) in the length of `text`, plus the wait. Informative.

```cheby
test "write sends text without a line ending" {
  use child <- process::with_child(process::command("cat", []))
  let writer = process::stdin(child)
  use _ <- result::try(process::write(writer, "to"))
  use _ <- result::try(process::write(writer, "gether"))
  use _ <- result::try(process::close(writer))
  assert process::read_line(process::stdout(child)) == Ok(Some("together"))
  Ok(Nil)
}
```

### `write_line`

```cheby
pub fn write_line(writer: Writer, text: String) -> Result<Nil, io::Error>
```

Writes `text` and then `\n` to `writer`, as one write.

- **Fails:** the cases of [`write_bytes`](#write_bytes).
- **Cost:** O(n) in the length of `text`, plus the wait. Informative.

```cheby
test "cat echoes the lines it was given once its input is closed" {
  use child <- process::with_child(process::command("cat", []))
  let writer = process::stdin(child)
  let reader = process::stdout(child)
  use _ <- result::try(process::write_line(writer, "hello"))
  use _ <- result::try(process::write_line(writer, "world"))
  use _ <- result::try(process::close(writer))
  assert process::read_line(reader) == Ok(Some("hello"))
  assert process::read_line(reader) == Ok(Some("world"))
  assert process::read_line(reader) == Ok(None)
  assert process::wait(child) == Ok(process::Exited(0))
  Ok(Nil)
}
```

### `close`

```cheby
pub fn close(writer: Writer) -> Result<Nil, io::Error>
```

Closes the child's standard input now, so the child reads its end. A fiber parked in a write on `writer` wakes with `Err` for which `io::is_closed` holds, and so does every later write and close (D-1914).

- **Fails:** `Err` for which `io::is_closed` holds when `writer` was already closed or `with_child` returned, and `io::is_invalid_input` when the stream is not piped.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "closing standard input twice is closed" {
  use child <- process::with_child(process::command("cat", []))
  let writer = process::stdin(child)
  use _ <- result::try(process::close(writer))
  let assert Err(reason) = process::close(writer)
  assert io::is_closed(reason)
  let assert Err(late) = process::write(writer, "late")
  assert io::is_closed(late)
  Ok(Nil)
}
```

## Not in tier 3

Each of these can be added later without breaking a program:

- a child that outlives the code that started it, such as a `spawn` that returns a `Child` to keep, or a `spawn_detached` (D-1912, ADR-0086);
- more places for a stream, such as a pipe from one child to another, which `Stream` is opaque to allow (D-1909);
- the process id of the program, of its parent and of a child. The working directory is read with [`env::current_dir`](env.md#current_dir) (D-1440);
- signals to a child other than SIGINT, SIGTERM and SIGKILL, a process group of a child's own (D-1913), signals other than `SIGINT` and `SIGTERM` to the program, and handling a signal in any way other than [`until_interrupted`](#until_interrupted) (D-1073);
- changing environment variables of the program (D-988).

_Note_: the design follows D-1078's constraints. `Command` is an opaque builder whose builders that replace a part are named `with_` (D-1051), the program and its arguments are a list, never a shell string, and the pipes satisfy `io::Source` and `io::Sink` ([`io.md`](io.md#streams), D-1072). The one change is that a cancelled [`wait`](#wait) takes nothing, because the end of [`with_child`](#with_child) stops the child, while a cancelled [`run`](#run) or [`output`](#output) still kills and reaps it (D-1911, D-1913).
