# `std::io`

Standard input, standard output, standard error, and reading and writing whole files. File handles, directories and `file::with_open` (D-101) are in [`std::file`](file.md) (D-328, D-992).

Every function that can fail returns `Result<T, Error>`, with the error type of this module (D-297):

```cheby
pub type Error
```

`Error` is opaque, because the kinds of IO failure follow the operating system and the host, and keep growing. Callers ask the predicate functions below, such as `io::is_not_found(e)`, and handle every other kind with a fallback arm. A new kind of error is a new predicate, which breaks no program (D-304, ADR-0051). `Error` satisfies `Show`, and its text is the same on every platform for the kinds this module knows (D-345).

Text is always UTF-8 outside the program. Text read from the operating system that is not valid UTF-8 gives an error for which `is_invalid_data` holds, and is never silently changed (D-311).

Output follows §10.10: standard output is line-buffered and flushed before the program reads standard input, standard error is not buffered, and all buffered output is flushed when the program ends, whatever its exit status (D-346). The functions that write without returning a `Result` end the program at once with exit status 1 and print nothing when they cannot write, for example because the reader of a pipe has gone (D-306). `write_stdout` and `write_stderr` return the failure instead. A stream that is already closed or missing when the program starts is not a failure: it behaves as a null device, so writes succeed and discard the text and reads see the end of input (D-474).

The examples of the functions that write output, read standard input or need a file to exist are not `test` blocks, because their results depend on the environment. They are top-level functions marked _Example_. The functions that fail have tests that check only the failure, which is the same everywhere.

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

Reads the next line from standard input. It returns `Ok(Some(line))` without the line ending, removing a final `\n` or `\r\n`, and `Ok(None)` at the end of input (D-305). The last line may have no ending. Calls after the end of input keep returning `Ok(None)`. Standard output is flushed first, so a prompt appears before the program waits (D-346).

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

_Example_, not a `test` because it needs a file to exist:

```cheby
fn word_total(path: String) -> Result<Int, io::Error> {
  use text <- result::try(io::read_file(path))
  Ok(text |> string::split(_, " ") |> list::length)
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
  let assert Err(reason) = io::write_file_bytes("no/such/directory/out.bin", bytes::from_list([0x00]))
  assert io::is_not_found(reason)
}
```

What the file functions and `read_line` do on a host without a file system or standard input, such as a browser, is OQ-P14, which M4 settles together with the JS side of every module. The direction is that they fail there with a new kind of error rather than panic.

## Errors

Each kind of failure that has a predicate below has a fixed text, the same on every platform. Tier 1 has three of them, `is_not_found`, `is_permission_denied` and `is_invalid_data` (D-367). Tier 3 adds one for each kind that its functions cause (D-981):

| Predicate                | Text                  | Caused by                                                               |
| ------------------------ | --------------------- | ----------------------------------------------------------------------- |
| `is_already_exists`      | `already exists`      | `file::create_dir`, `file::create_dir_all`                              |
| `is_a_directory`         | `is a directory`      | `file::open`, `create`, `append` and `remove_file`                      |
| `is_not_a_directory`     | `not a directory`     | `file::list_dir`, `remove_dir`, and a path through a file               |
| `is_directory_not_empty` | `directory not empty` | `file::remove_dir`                                                      |
| `is_connection_refused`  | `connection refused`  | `net::connect`, `http::get` and `http::send`                            |
| `is_connection_reset`    | `connection reset`    | `net::read`, `write` and `shutdown_write`, `http::get` and `http::send` |
| `is_address_in_use`      | `address in use`      | `net::listen` and `http::serve`                                         |
| `is_host_not_found`      | `host not found`      | `net::listen`, `net::connect`, `http::get` and `http::send`             |
| `is_closed`              | `closed`              | every `std::file` function on a handle                                  |
| `is_invalid_input`       | `invalid input`       | `std::file`, `std::net`, `http::serve` and this module's file functions |

`std::http` returns these through `http::io_error` (D-1016). Every other failure keeps the operating system's own message.

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

Tells whether `error` means that the operating system refused the program access to a file or a directory.

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

_Example_, not a `test` because it writes a file:

```cheby
fn check_invalid_text(path: String) -> Result<Nil, io::Error> {
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

Tells whether `error` means that something already exists at a path where a directory was to be created. Its text is `already exists` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it changes the file system:

```cheby
fn ensure_dir(path: String) -> Result<Nil, io::Error> {
  case file::create_dir(path) {
    Ok(Nil) => Ok(Nil)
    Err(reason) => case io::is_already_exists(reason) {
      True => Ok(Nil)
      False => Err(reason)
    }
  }
}
```

### `is_a_directory`

```cheby
pub fn is_a_directory(error: Error) -> Bool
```

Tells whether `error` means that a path names a directory where a file was expected. Its text is `is a directory` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a directory to exist:

```cheby
fn describe_open_failure(path: String) -> String {
  case file::open(path) {
    Ok(_) => "{path} can be read"
    Err(reason) => case io::is_a_directory(reason) {
      True => "{path} is a directory, not a file"
      False => "cannot open {path}: {reason}"
    }
  }
}
```

### `is_not_a_directory`

```cheby
pub fn is_not_a_directory(error: Error) -> Bool
```

Tells whether `error` means that a path names something other than a directory where a directory was expected, including a path that goes through a file. Its text is `not a directory` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a file to exist:

```cheby
fn entries_or_none(path: String) -> Result<List<String>, io::Error> {
  case file::list_dir(path) {
    Ok(names) => Ok(names)
    Err(reason) => case io::is_not_a_directory(reason) {
      True => Ok([])
      False => Err(reason)
    }
  }
}
```

### `is_directory_not_empty`

```cheby
pub fn is_directory_not_empty(error: Error) -> Bool
```

Tells whether `error` means that a directory could not be removed because it has entries. Its text is `directory not empty` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it changes the file system:

```cheby
fn remove_if_empty(path: String) -> Result<Bool, io::Error> {
  case file::remove_dir(path) {
    Ok(Nil) => Ok(True)
    Err(reason) => case io::is_directory_not_empty(reason) {
      True => Ok(False)
      False => Err(reason)
    }
  }
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
  case net::write(connection, data) {
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

Tells whether `error` means that a `file::File` was used after `file::close`, or after the `file::with_*` call that opened it returned (D-993). Its text is `closed` (D-981).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a file to exist:

```cheby
fn check_close_twice(path: String) -> Result<Nil, io::Error> {
  use source <- result::try(file::open(path))
  use _ <- result::try(file::close(source))
  let assert Err(reason) = file::close(source)
  assert io::is_closed(reason)
  Ok(Nil)
}
```

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

Makes `Error` satisfy `Show` (§8.8). It gives a fixed message for each kind that has a predicate, the same on every platform: `not found`, `permission denied` and `invalid UTF-8`, and the texts of tier 3's kinds in the table under [Errors](#errors) (D-981). For every other kind it gives the operating system's own message (D-345). Debug printing with `{e:?}` gives the same text and never the internal fields (§3.13). Neither includes the path.

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
