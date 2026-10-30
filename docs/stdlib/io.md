# `std::io`

Standard input, standard output, standard error, reading and writing whole files, and the byte-stream interfaces [`Source`](#source) and [`Sink`](#sink) (D-1072). File handles, directories and `file::with_open` (D-101) are in [`std::file`](file.md) (D-328, D-992). So are the handles for the standard streams, [`file::stdin()`, `file::stdout()` and `file::stderr()`](file.md#standard-streams), which read and write bytes and follow the rules of this page's functions for the same streams. `std::io` cannot return a `file::File` itself, since `std::file` imports `std::io` (D-145, D-1071).

Every function that can fail returns `Result<T, Error>`, with the error type of this module (D-297):

```cheby
pub type Error
```

`Error` is opaque, because the kinds of IO failure follow the operating system and the host, and keep growing. Callers ask the predicate functions below, such as `io::is_not_found(e)`, and handle every other kind with a fallback arm. A new kind of error is a new predicate, which breaks no program (D-304, ADR-0051). `Error` satisfies `Show`, and its text is the same on every platform for the kinds this module knows (D-345).

Text is always UTF-8 outside the program. Text read from the operating system that is not valid UTF-8 gives an error for which `is_invalid_data` holds, and is never silently changed (D-311).

Output follows §10.10: standard output is line-buffered and flushed before the program reads standard input, standard error is not buffered, and all buffered output is flushed when the program ends, whatever its exit status (D-346). The functions that write without returning a `Result` end the program at once with exit status 1 and print nothing when they cannot write, for example because the reader of a pipe has gone (D-306). `write_stdout` and `write_stderr` return the failure instead. A stream that is already closed or missing when the program starts is not a failure: it behaves as a null device, so writes succeed and discard the text and reads see the end of input (D-474).

The examples of the functions that write output or read standard input are not `test` blocks, because their results depend on the environment. They are top-level functions marked _Example_. A function that needs a file is tested in a temporary directory from `file::with_temp_dir`, which is removed when the test ends (D-1066, D-1068). An _Example_ stays only where the environment still decides the result, such as permissions. The functions that fail have tests that check the failure, which is the same everywhere.

## Standard output and standard error

### `print`

```cheby
pub fn print(text: String)
```

Writes `text` to standard output, with no line ending. Because standard output is line-buffered, the text appears with the next line ending, before the program next reads standard input, or when the program ends (D-346). A prompt written with `print` therefore appears before the answer is read.

- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard output:

```cheby
fn ask_name() -> Result<Option<String>, io::Error> {
  io::print("Your name: ")
  io::read_line()
}
```

### `println`

```cheby
pub fn println(text: String)
```

Writes `text` and a line ending to standard output. The line ending is `\n` on every platform, including Windows.

- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard output:

```cheby
fn greet(name: String) {
  io::println("Hello, {name}!")
  list::each(["a", "b"], io::println)
}
```

### `eprint`

```cheby
pub fn eprint(text: String)
```

Writes `text` to standard error, with no line ending. Standard error is not buffered, so the text appears at once (D-346).

- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard error:

```cheby
fn show_progress(done: Int, total: Int) {
  io::eprint("{done} of {total}\r")
}
```

### `eprintln`

```cheby
pub fn eprintln(text: String)
```

Writes `text` and the line ending `\n` to standard error, at once.

- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard error:

```cheby
fn warn(message: String) {
  io::eprintln("warning: {message}")
}
```

### `write_stdout`

```cheby
pub fn write_stdout(text: String) -> Result<Nil, Error>
```

Writes `text` to standard output, with no line ending, and reports a failure instead of ending the program (D-306). It is for programs that stop quietly, or clean up, when their output is closed.

- **Fails:** `Err` when the text cannot be written, for example because the reader of a pipe has gone. Output written earlier with `print` or `println` and still in the buffer is reported by the same `Err`.
- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard output:

```cheby
fn print_all(lines: List<String>) -> Result<Nil, io::Error> {
  case lines {
    [] => Ok(Nil)
    [first, ..rest] => {
      use _ <- result::try(io::write_stdout(first + "\n"))
      print_all(rest)
    }
  }
}
```

`write_stdout` writes through the same buffer as `print` and then flushes it, so its `Result` covers everything written to standard output so far (D-365).

After an `Err`, the text that could not be written may stay in the buffer. A later flush that fails, at exit, before a panic report or before standard input is read, then does not end the program, because the `Err` has already reported that text. So a program that handles the `Err` and returns keeps its exit status. This ends when `print` or `println` adds text to standard output or a later `write_stdout` succeeds, and from then on a failing flush ends the program with status 1 again (§10.10, D-306, D-574).

### `write_stderr`

```cheby
pub fn write_stderr(text: String) -> Result<Nil, Error>
```

Writes `text` to standard error, with no line ending, and reports a failure instead of ending the program (D-306).

- **Fails:** `Err` when the text cannot be written, for example because standard error was closed while the program ran. A standard error closed before the program started discards the text without an error (D-474).
- **Cost:** O(n) in the length of `text`, plus the write. Informative.

_Example_, not a `test` because it writes to standard error:

```cheby
fn log(message: String) -> Result<Nil, io::Error> {
  io::write_stderr("log: " + message + "\n")
}
```

## Standard input

### `read_line`

```cheby
pub fn read_line() -> Result<Option<String>, Error>
```

Reads the next line from standard input. It returns `Ok(Some(line))` without the line ending, removing a final `\n` or `\r\n`, and `Ok(None)` at the end of input (D-305). The last line may have no ending. Calls after the end of input keep returning `Ok(None)`. Standard output is flushed first, so a prompt appears before the program waits (D-346). It reads from the runtime's process-wide reader of standard input, as `file::read_line(file::stdin())` does, so the two may be mixed in order (D-823, D-1071).

A cancellation that reaches a fiber waiting in `read_line` is delivered at once, and the fiber unwinds without waiting for the read. The refill of the reader keeps running on its pool thread, and the bytes it reads land in the reader's buffer, which the reader owns rather than the fiber. So the next `read_line` gets them, and no line is lost. The next read waits for a refill that is still running (D-983, D-1065). A `fiber::timeout` around `read_line` is therefore a real deadline.

- **Fails:** `Err` for which `is_invalid_data` holds when the line is not valid UTF-8 (D-311). The line is consumed, so the next call reads the line after it. `Err` when standard input cannot be read.
- **Cost:** O(n) in the length of the line, plus the read. Informative.

_Example_, not a `test` because it reads standard input:

```cheby
fn count_lines(total: Int) -> Result<Int, io::Error> {
  use line <- result::try(io::read_line())
  case line {
    None => Ok(total)
    Some(_) => count_lines(total + 1)
  }
}
```

## Files

A path is a `String`, and a relative path is resolved against the program's working directory. The text of an error does not include the path, so callers add it themselves, as the examples do with `"cannot read {path}: {reason}"`. A path that contains U+0000 gives an `Err` for which `is_invalid_input` holds, in every function of this section (D-981).

### `read_file`

```cheby
pub fn read_file(path: String) -> Result<String, Error>
```

Reads the whole file at `path` as UTF-8 text. The text is returned exactly as stored: line endings are not changed, and a byte-order mark at the start is kept as U+FEFF.

- **Fails:** `Err` for which `is_not_found` holds when there is no file at `path`, and `is_permission_denied` when the program may not read it. `Err` for which `is_invalid_data` holds when the contents are not valid UTF-8 (D-311). `Err` for every other failure of the operating system, such as `path` naming a directory.
- **Cost:** O(n) in the size of the file, plus the read. Informative.

```cheby
test "read_file reports a missing file" {
  let assert Err(reason) = io::read_file("no/such/directory/file.txt")
  assert io::is_not_found(reason)
}
```

```cheby
test "read_file returns the text as stored" {
  use dir <- file::with_temp_dir()
  let path = dir + "/notes.txt"
  let stored = bytes::from_list([0xEF, 0xBB, 0xBF, 0x61, 0x0D, 0x0A])
  use _ <- result::try(io::write_file_bytes(path, stored))
  assert io::read_file(path) == Ok("\u{FEFF}a\r\n")
  use _ <- result::try(io::write_file(path, "one two"))
  assert io::read_file(path) == Ok("one two")
  Ok(Nil)
}
```

A leading byte-order mark is kept, as in Rust, Go and Node.js, so `read_file` gives the same text as decoding `read_file_bytes` (D-366).

### `read_file_bytes`

```cheby
pub fn read_file_bytes(path: String) -> Result<bytes::Bytes, Error>
```

Reads the whole file at `path` as raw bytes, without decoding them (D-311).

- **Fails:** `Err` for which `is_not_found` holds when there is no file at `path`, and `is_permission_denied` when the program may not read it. `Err` for every other failure of the operating system.
- **Cost:** O(n) in the size of the file, plus the read. Informative.

```cheby
test "read_file_bytes reports a missing file" {
  let assert Err(reason) = io::read_file_bytes("no/such/directory/file.bin")
  assert io::is_not_found(reason)
}
```

### `write_file`

```cheby
pub fn write_file(path: String, text: String) -> Result<Nil, Error>
```

Writes `text` as UTF-8 to the file at `path`, creating the file if it does not exist and replacing its contents if it does (D-328). No byte-order mark is written, and line endings are written as they are in `text`. The directory that holds the file must already exist. If the write fails part way, what the file then holds is not specified.

- **Fails:** `Err` for which `is_not_found` holds when a directory on `path` does not exist, and `is_permission_denied` when the program may not write the file. `Err` for every other failure of the operating system, such as a full disk.
- **Cost:** O(n) in the length of `text`, plus the write. Informative.

```cheby
test "write_file needs the directory to exist" {
  let assert Err(reason) = io::write_file("no/such/directory/out.txt", "hello")
  assert io::is_not_found(reason)
}
```

### `write_file_bytes`

```cheby
pub fn write_file_bytes(path: String, data: bytes::Bytes) -> Result<Nil, Error>
```

Writes `data` to the file at `path` unchanged, creating the file if it does not exist and replacing its contents if it does (D-328). The directory that holds the file must already exist.

- **Fails:** the same cases as `write_file`.
- **Cost:** O(n) in the length of `data`, plus the write. Informative.

```cheby
test "write_file_bytes needs the directory to exist" {
  let assert Err(reason) = io::write_file_bytes("no/such/directory/out.bin", bytes::from_list([
    0x00,
  ]))
  assert io::is_not_found(reason)
}
```

What the file functions and `read_line` do on a host without a file system or standard input, such as a browser, is OQ-P14, which M4 settles together with the JS side of every module. The direction is that they fail there with a new kind of error rather than panic.

## Streams

Two interfaces stand for streams of bytes, so that one function can take a file, a socket or a standard stream, as Go's `io.Reader` and `io.Writer` do (D-046, D-1072, ADR-0070).

- **Satisfied for free.** `file::File`, `net::Connection` and the standard-stream handles of `std::file` satisfy both interfaces through their modules' `read_bytes` and `write_bytes`, with no code of their own, because satisfaction looks only at the type's own module ([§8.2](../spec/08-interfaces.md#82-satisfaction), D-1062, D-1071). `std::io` imports neither `std::file` nor `std::net` (D-145).
- **`dyn`.** Both interfaces are `dyn`-safe, so a `dyn io::Source` holds any source ([§8.6](../spec/08-interfaces.md#86-dyn-values)).
- **A program's own streams.** Values are immutable (D-017), so a type of the program satisfies `Source` only by wrapping something that has state, such as a handle or a channel. It reports a failure with an error built by [`other`](#other).
- **Written in Cheby.** [`copy`](#copy) and [`other`](#other) are Cheby with no kernels (D-1072).

### `Source`

```cheby
pub interface Source {
  fn read_bytes(Self, Int) -> Result<Option<bytes::Bytes>, Error>
}
```

A stream that bytes are read from. `read_bytes(source, limit)` returns the next bytes of the stream, at most `limit` of them, or `Ok(None)` at its end, as `file::read_bytes` and `net::read_bytes` do (D-1072).

A generic function names the interface with its module ([§8.5](../spec/08-interfaces.md#85-calling-interface-functions)):

```cheby
fn first_block<S: io::Source>(source: S) -> Result<Option<bytes::Bytes>, io::Error> {
  io::Source::read_bytes(source, 4_096)
}
```

### `Sink`

```cheby
pub interface Sink {
  fn write_bytes(Self, bytes::Bytes) -> Result<Nil, Error>
}
```

A stream that bytes are written to. `write_bytes(sink, data)` writes all of `data`, as `file::write_bytes` and `net::write_bytes` do (D-1072).

### `copy`

```cheby
pub fn copy<S: Source, K: Sink>(source: S, sink: K) -> Result<Int, Error>
```

Reads blocks of at most 64 KiB from `source` until its end, writes each one to `sink`, and returns the number of bytes copied (D-1072). It stops at the first `Err` of either side and returns it. What was written before stays written.

- **Fails:** the first `Err` of `source` or `sink`.
- **Tail calls.** `copy` calls the interfaces' functions, which counts as calling function values, so a tail call to it stays a tail call, and a deadlock report locates a fiber waiting in it at the nearest frame outside std that is still on the stack (D-1059).
- **Cost:** O(n) in the number of bytes copied, plus the reads and writes. Informative.

```cheby
test "copy moves every byte of one file to another" {
  use dir <- file::with_temp_dir()
  let data = bytes::from_string(string::repeat("block ", 20_000))
  use _ <- result::try(io::write_file_bytes(dir + "/from.bin", data))
  use copied <- result::try(file::with_open(dir + "/from.bin", fn(source) {
    file::with_create(dir + "/to.bin", fn(sink) { io::copy(source, sink) })
  }))
  assert copied == bytes::length(data)
  assert io::read_file_bytes(dir + "/to.bin") == Ok(data)
  Ok(Nil)
}
```

_Example_, not a `test` because it needs a peer:

```cheby
fn send_file(path: String, connection: net::Connection) -> Result<Int, io::Error> {
  use source <- file::with_open(path)
  io::copy(source, connection)
}
```

### `other`

```cheby
pub fn other(message: String) -> Error
```

Returns an `Error` whose text is `message`, so that a program's own `Source` or `Sink` can fail with the error type of the interfaces. It is of the kind that the operating system's other failures have, so no predicate holds for it, and two such errors are `==` when their messages are equal (D-342, D-981, D-1072).

- **Cost:** O(1). Informative.

```cheby
test "other builds an error with its own text" {
  let reason = io::other("upstream closed")
  assert "{reason}" == "upstream closed"
  assert !io::is_not_found(reason)
  assert reason == io::other("upstream closed")
}
```

## Errors

Each kind of failure that has a predicate below has a fixed text, the same on every platform. Tier 1 has three of them, `is_not_found`, `is_permission_denied` and `is_invalid_data` (D-367). Tier 3 adds one for each kind that its functions cause (D-981, D-1062, D-1063, D-1066):

| Predicate                | Text                  | Caused by                                                                           |
| ------------------------ | --------------------- | ----------------------------------------------------------------------------------- |
| `is_already_exists`      | `already exists`      | `file::create_dir`, `create_dir_all`, `create_new` and `with_create_new`            |
| `is_a_directory`         | `is a directory`      | `file::open`, `create`, `append` and `remove_file`                                  |
| `is_not_a_directory`     | `not a directory`     | `file::list_dir`, `remove_dir`, and a path through a file                           |
| `is_directory_not_empty` | `directory not empty` | `file::remove_dir`                                                                  |
| `is_connection_refused`  | `connection refused`  | `net::connect`, `http::get` and `http::send`                                        |
| `is_connection_reset`    | `connection reset`    | `net::read_bytes`, `write_bytes` and `shutdown_write`, `http::get` and `http::send` |
| `is_address_in_use`      | `address in use`      | `net::listen` and `http::serve`                                                     |
| `is_host_not_found`      | `host not found`      | `net::listen`, `net::connect`, `http::get` and `http::send`                         |
| `is_closed`              | `closed`              | all `std::file` functions on a handle, `std::net` ones on a socket but `local_port` |
| `is_invalid_input`       | `invalid input`       | `std::file`, `std::net`, `http::serve` and this module's file functions             |

`std::http` returns these through `http::io_error` (D-1016). Every other failure keeps the operating system's own message, or the text given to [`other`](#other) (D-1072).

### `is_not_found`

```cheby
pub fn is_not_found(error: Error) -> Bool
```

Tells whether `error` means that a file or a directory on its path does not exist.

- **Cost:** O(1). Informative.

```cheby
test "a missing file is not found" {
  let assert Err(reason) = io::read_file("no/such/directory/file.txt")
  assert io::is_not_found(reason)
  assert !io::is_permission_denied(reason)
  assert !io::is_invalid_data(reason)
}
```

These three are the predicates of tier 1 (D-367). The ten after [`is_invalid_data`](#is_invalid_data) came with the tier-3 functions that can cause them (D-981). Every other kind is reached through `show`.

### `is_permission_denied`

```cheby
pub fn is_permission_denied(error: Error) -> Bool
```

Tells whether `error` means that the operating system refused the program access to a file or a directory, or, for `net::listen`, to a port (D-1184).

- **Cost:** O(1). Informative.

_Example_, not a `test` because no file is unreadable on every platform:

```cheby
fn read_or_explain(path: String) -> Result<String, String> {
  case io::read_file(path) {
    Ok(text) => Ok(text)
    Err(reason) => case io::is_permission_denied(reason) {
      True => Err("{path} is not readable by this user")
      False => Err("cannot read {path}: {reason}")
    }
  }
}
```

### `is_invalid_data`

```cheby
pub fn is_invalid_data(error: Error) -> Bool
```

Tells whether `error` means that text read from the operating system was not valid UTF-8 (D-311).

- **Cost:** O(1). Informative.

```cheby
test "a file that is not UTF-8 is invalid data" {
  use dir <- file::with_temp_dir()
  let path = dir + "/latin1.txt"
  use _ <- result::try(io::write_file_bytes(path, bytes::from_list([0x66, 0xFF])))
  let assert Err(reason) = io::read_file(path)
  assert io::is_invalid_data(reason)
  assert "{reason}" == "invalid UTF-8"
  Ok(Nil)
}
```

### `is_already_exists`

```cheby
pub fn is_already_exists(error: Error) -> Bool
```

Tells whether `error` means that something already exists at a path where a directory, or a file opened with `file::create_new` or `file::with_create_new`, was to be created. Its text is `already exists` (D-981, D-1066).

- **Cost:** O(1). Informative.

```cheby
test "creating a directory twice is already exists" {
  fn ensure_dir(path: String) -> Result<Nil, io::Error> {
    case file::create_dir(path) {
      Ok(Nil) => Ok(Nil)
      Err(reason) => case io::is_already_exists(reason) {
        True => Ok(Nil)
        False => Err(reason)
      }
    }
  }
  use dir <- file::with_temp_dir()
  let path = dir + "/cache"
  use _ <- result::try(file::create_dir(path))
  let assert Err(reason) = file::create_dir(path)
  assert io::is_already_exists(reason)
  assert "{reason}" == "already exists"
  assert ensure_dir(path) == Ok(Nil)
  Ok(Nil)
}
```

### `is_a_directory`

```cheby
pub fn is_a_directory(error: Error) -> Bool
```

Tells whether `error` means that a path names a directory where a file was expected. Its text is `is a directory` (D-981).

- **Cost:** O(1). Informative.

```cheby
test "opening a directory as a file is a directory" {
  use dir <- file::with_temp_dir()
  let assert Err(reason) = file::open(dir)
  assert io::is_a_directory(reason)
  assert "{reason}" == "is a directory"
  Ok(Nil)
}
```

### `is_not_a_directory`

```cheby
pub fn is_not_a_directory(error: Error) -> Bool
```

Tells whether `error` means that a path names something other than a directory where a directory was expected, including a path that goes through a file. Its text is `not a directory` (D-981).

- **Cost:** O(1). Informative.

```cheby
test "listing a file is not a directory" {
  use dir <- file::with_temp_dir()
  let path = dir + "/plain.txt"
  use _ <- result::try(io::write_file(path, "plain"))
  let assert Err(reason) = file::list_dir(path)
  assert io::is_not_a_directory(reason)
  assert "{reason}" == "not a directory"
  Ok(Nil)
}
```

### `is_directory_not_empty`

```cheby
pub fn is_directory_not_empty(error: Error) -> Bool
```

Tells whether `error` means that a directory could not be removed because it has entries. Its text is `directory not empty` (D-981).

- **Cost:** O(1). Informative.

```cheby
test "removing a directory with entries is directory not empty" {
  use dir <- file::with_temp_dir()
  let full = dir + "/full"
  use _ <- result::try(file::create_dir(full))
  use _ <- result::try(io::write_file(full + "/entry.txt", "entry"))
  let assert Err(reason) = file::remove_dir(full)
  assert io::is_directory_not_empty(reason)
  assert "{reason}" == "directory not empty"
  Ok(Nil)
}
```

### `is_connection_refused`

```cheby
pub fn is_connection_refused(error: Error) -> Bool
```

Tells whether `error` means that nothing listens at the address a connection was made to. Its text is `connection refused` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because whether a port has a listener depends on the machine:

```cheby
fn server_is_up(port: Int) -> Result<Bool, io::Error> {
  case net::connect("127.0.0.1", port) {
    Ok(_) => Ok(True)
    Err(reason) => case io::is_connection_refused(reason) {
      True => Ok(False)
      False => Err(reason)
    }
  }
}
```

### `is_connection_reset`

```cheby
pub fn is_connection_reset(error: Error) -> Bool
```

Tells whether `error` means that the peer of a connection reset it, aborted it or went away while it was written to. Its text is `connection reset` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a peer that goes away:

```cheby
fn send_or_report(connection: net::Connection, data: bytes::Bytes) -> String {
  case net::write_bytes(connection, data) {
    Ok(Nil) => "sent"
    Err(reason) => case io::is_connection_reset(reason) {
      True => "the peer went away"
      False => "cannot send: {reason}"
    }
  }
}
```

### `is_address_in_use`

```cheby
pub fn is_address_in_use(error: Error) -> Bool
```

Tells whether `error` means that another socket already listens on an address and port. Its text is `address in use` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because which ports are taken depends on the machine:

```cheby
fn listen_or_explain(port: Int) -> Result<net::Listener, String> {
  case net::listen("0.0.0.0", port) {
    Ok(listener) => Ok(listener)
    Err(reason) => case io::is_address_in_use(reason) {
      True => Err("port {port} is taken")
      False => Err("cannot listen on {port}: {reason}")
    }
  }
}
```

### `is_host_not_found`

```cheby
pub fn is_host_not_found(error: Error) -> Bool
```

Tells whether `error` means that a host name could not be resolved. Every failure of name lookup is this one kind, including a temporary one, because resolvers' codes differ between platforms and their texts are not stable. Its text is `host not found` (D-981).

- **Cost:** O(1). Informative.

The _Example_ of [`net::connect`](net.md#connect) uses it.

### `is_closed`

```cheby
pub fn is_closed(error: Error) -> Bool
```

Tells whether `error` means that a handle was used after it was closed: a `file::File` after `file::close`, or after the `file::with_*` call that opened it returned (D-993), and a `net::Connection` or `net::Listener` after `net::close` or `net::close_listener`. A fiber parked in `net::accept`, `net::read_bytes` or `net::write_bytes` on a socket that another fiber closes wakes with this error too (D-1063). Its text is `closed` (D-981).

- **Cost:** O(1). Informative.

```cheby
test "closing a file twice is closed" {
  use dir <- file::with_temp_dir()
  let path = dir + "/notes.txt"
  use _ <- result::try(io::write_file(path, "notes"))
  use source <- result::try(file::open(path))
  use _ <- result::try(file::close(source))
  let assert Err(reason) = file::close(source)
  assert io::is_closed(reason)
  assert "{reason}" == "closed"
  Ok(Nil)
}
```

The tests of [`net::close`](net.md#close) and [`net::close_listener`](net.md#close_listener) show it for sockets.

### `is_invalid_input`

```cheby
pub fn is_invalid_input(error: Error) -> Bool
```

Tells whether `error` means that an argument could not be used: a path that contains U+0000, a port outside 0 to 65535, or a `file::File` used in a mode it was not opened for, such as writing to a file opened for reading. Its text is `invalid input` (D-981).

- **Cost:** O(1). Informative.

The tests of [`file::open`](file.md#open) and [`net::listen`](net.md#listen) use it.

### `show`

```cheby
pub fn show(error: Error) -> String
```

Makes `Error` satisfy `Show` (§8.8). It gives a fixed message for each kind that has a predicate, the same on every platform: `not found`, `permission denied` and `invalid UTF-8`, and the texts of tier 3's kinds in the table under [Errors](#errors) (D-981). For every other kind it gives the operating system's own message (D-345), or the message of an error built with [`other`](#other) (D-1072). Debug printing with `{e:?}` gives the same text and never the internal fields (§3.13). Neither includes the path.

- **Cost:** O(1). Informative.

```cheby
test "an io error shows a fixed message" {
  let assert Err(reason) = io::read_file("no/such/directory/file.txt")
  assert io::show(reason) == "not found"
  assert "{reason}" == "not found"
  assert "{reason:?}" == "not found"
}
```

Two errors are `==` when their kinds are the same and, for a kind without a predicate, their messages are the same, and they hash accordingly (D-342, D-368).
