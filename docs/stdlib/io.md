# `std::io`

Standard input, standard output, standard error, and reading and writing whole files. File handles, directories and `file::with_open` (D-101) come in tier 3 (D-328).

Every function that can fail returns `Result<T, Error>`, with the error type of this module (D-297):

```cheby
pub type Error
```

`Error` is opaque, because the kinds of IO failure follow the operating system and the host, and keep growing. Callers ask the predicate functions below, such as `io::is_not_found(e)`, and handle every other kind with a fallback arm. A new kind of error is a new predicate, which breaks no program (D-304, ADR-0051). `Error` satisfies `Show`, and its text is the same on every platform for the kinds this module knows (D-345).

Text is always UTF-8 outside the program. Text read from the operating system that is not valid UTF-8 gives an error for which `is_invalid_data` holds, and is never silently changed (D-311).

Output follows §10.10: standard output is line-buffered and flushed before the program reads standard input, standard error is not buffered, and all buffered output is flushed when the program ends, whatever its exit status (D-346). The functions that write without returning a `Result` end the program at once with exit status 1 and print nothing when they cannot write, for example because the reader of a pipe has gone (D-306). `write_stdout` and `write_stderr` return the failure instead.

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

### `write_stderr`

```cheby
pub fn write_stderr(text: String) -> Result<Nil, Error>
```

Writes `text` to standard error, with no line ending, and reports a failure instead of ending the program (D-306).

- **Fails:** `Err` when the text cannot be written, for example because standard error has been closed.
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

A path is a `String`, and a relative path is resolved against the program's working directory. The text of an error does not include the path, so callers add it themselves, as the examples do with `"cannot read {path}: {reason}"`.

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

What the file functions and `read_line` do on a host without a file system or standard input, such as a browser, is specified in tier 3 together with the JS side of every module.

## Errors

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

These three are the predicates of tier 1 (D-367). Every other kind is reached through `show`, and its predicate comes with the tier-3 functions that can cause it.

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

### `show`

```cheby
pub fn show(error: Error) -> String
```

Makes `Error` satisfy `Show` (§8.8). It gives a fixed message for each kind that has a predicate, the same on every platform: `not found`, `permission denied` and `invalid UTF-8`. For every other kind it gives the operating system's own message (D-345). Debug printing with `{e:?}` gives the same text and never the internal fields (§3.13). Neither includes the path.

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
