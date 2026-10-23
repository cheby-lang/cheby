# `std::file`

Files through handles: opening a file, reading and writing it in pieces, closing it with a reported error, the standard streams as handles, directories, and scratch directories for tests. Whole-file reads and writes stay in [`std::io`](io.md) (D-328), which is still the short way to read or replace a whole file. In the form `cheby doc` prints:

```cheby
pub type File

pub type Info
```

- **Handle.** `File` is a handle ([§9.5](../spec/09-memory-model.md#95-handles-and-drop-functions), D-101). It prints as `<File>`, `==` and hashing panic when they reach one, W0011 warns about a comparison that can reach one, and no constant may hold one (D-233, D-647). It is an external type of this module whose drop function the runtime supplies, as for the tier-2 handles (D-703, D-982). The file closes right after the last use of the last reference to it (D-205), on the fiber that released it (D-171), also when that fiber unwinds. The drop function closes the descriptor and ignores any error. It never writes, because a `File` buffers nothing for writing (D-994).
- **`Info`.** What [`info`](#info) found about a path. It is opaque plain data with structural `==`, read through accessor functions, so fields can be added after 1.0 (D-996).
- **Errors.** Every function that can fail returns `io::Error`, the error type of [`std::io`](io.md). The kinds these functions cause have predicates there, such as `io::is_already_exists` and `io::is_closed`, each with a fixed text (D-981).
- **Paths.** As in `io.md` "Files": a path is a `String`, a relative path is resolved against the working directory, and the text of an error never includes the path. `/` separates components on every platform, including Windows. There are no path functions, and no way to read or change the working directory (D-996).
- **Blocking pool.** Every function that touches the operating system runs on the blocking pool while the calling fiber is suspended, so other fibers keep running (D-042, D-683, [§10.9](../spec/10-concurrency.md#109-blocking-io)). That work cannot be interrupted. A fiber that a cancellation has already reached never starts the work (D-816). A cancellation that arrives during a write, an open, a close or a change to a directory is delivered when the work returns, and the result is released, because the work changes the outside world and the caller must know whether it happened (D-670). So a cancelled `write` has written its text. A refill of a handle's read-ahead buffer is different: a cancellation that reaches a fiber waiting for one is delivered at once, and the fiber unwinds without waiting. The refill runs to its end on its pool thread, holding its own reference to the handle, and its bytes land in the handle's buffer, which the handle owns rather than the fiber. The next read on the handle waits for a refill still running and then gets those bytes, so no line is lost (D-983, D-995, D-1065).
- **Several fibers, one file.** Any fiber may use a `File` it holds. Operations on one handle run one at a time, in the order they reach it. A `read_line` therefore never sees half of another fiber's `write`.
- **Streams.** `File` satisfies `io::Source` and `io::Sink` through [`read_bytes`](#read_bytes) and [`write_bytes`](#write_bytes), with no code of its own, so [`io::copy`](io.md#copy) and other code generic over a stream take a file, a standard stream or a `net::Connection` alike (§8.2, D-1062, D-1071, D-1072).
- **Written in Cheby.** Every public function is written in Cheby over private kernels named `sys_<name>`, bound as `@external(native, "cheby_file_sys_<name>")` with `@std_context`, as `std::io`'s suspending kernels are (D-484, D-692, D-755, D-985). Each kernel also declares `@external(js, "./file.mjs", "sys_<name>")`, so every function exists on both targets for the cross-target check (D-450, D-661, D-986). No function here runs in constants: reaching one while computing a constant is E0077 (D-090, D-537, D-991).
- **On JS.** Whether the kernels use `fs/promises` with `@async` or the synchronous API, and what a host without a file system does, is OQ-P14 and comes with M4. The direction is that every function exists there and fails with a new `io::is_unsupported` kind on such a host.

Every function that changes the file system, or needs a file to exist, is tested inside a fresh directory from [`with_temp_dir`](#with_temp_dir), which the test removes again, so the tests run the same on every machine (D-1068). A test body may have type `Result<Nil, E>`, so `use dir <- file::with_temp_dir()` starts one directly (§13.6). Under `cheby test` every such directory lies under one temporary root per run, which `cheby test` removes when the run ends, so a failed test leaves nothing behind (D-1067). An entry keeps an _Example_ only where the environment still decides the result, such as reading standard input. Where a function can fail, a `test` also checks a failure that is the same everywhere, as `io.md` does.

## Opening

There are four openers, named after what they do: [`open`](#open) reads, [`create`](#create) creates or truncates and writes, [`append`](#append) creates if needed and appends, and [`create_new`](#create_new) creates a file that must not exist yet and writes. Each has a scoped form, [`with_open`](#with_open), [`with_create`](#with_create), [`with_append`](#with_append) and [`with_create_new`](#with_create_new). Reading is the default for the name `open`, and the eight names cover every common case with no flags (D-992, D-1066).

### `open`

```cheby
pub fn open(path: String) -> Result<File, io::Error>
```

Opens the file at `path` for reading, from its start.

- **Fails:** `Err` for which `io::is_not_found` holds when there is no file at `path`, `io::is_permission_denied` when the program may not read it, `io::is_a_directory` when `path` names a directory, and `io::is_invalid_input` when `path` contains U+0000. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "open reports a missing file" {
  let assert Err(reason) = file::open("no/such/directory/file.txt")
  assert io::is_not_found(reason)
  assert "{reason}" == "not found"
}

test "a path with U+0000 is invalid input" {
  let assert Err(reason) = file::open("bad\u{0}name.txt")
  assert io::is_invalid_input(reason)
}

test "open reads a file from its start" {
  use dir <- file::with_temp_dir()
  let target = dir + "/notes.txt"
  use _ <- result::try(io::write_file(target, "first\nsecond\n"))
  use source <- result::try(file::open(target))
  use line <- result::try(file::read_line(source))
  assert line == Some("first")
  Ok(Nil)
}
```

### `create`

```cheby
pub fn create(path: String) -> Result<File, io::Error>
```

Opens the file at `path` for writing, creating it if it does not exist and truncating it to empty if it does. The directory that holds it must already exist.

- **Fails:** `Err` for which `io::is_not_found` holds when a directory on `path` does not exist, `io::is_permission_denied` when the program may not write there, `io::is_a_directory` when `path` names a directory, and `io::is_invalid_input` when `path` contains U+0000. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "create needs the directory to exist" {
  let assert Err(reason) = file::create("no/such/directory/out.txt")
  assert io::is_not_found(reason)
}

test "create truncates a file that exists" {
  use dir <- file::with_temp_dir()
  let target = dir + "/out.txt"
  use _ <- result::try(io::write_file(target, "old text"))
  use handle <- result::try(file::create(target))
  use _ <- result::try(file::write(handle, "new"))
  use text <- result::try(io::read_file(target))
  assert text == "new"
  Ok(Nil)
}
```

### `append`

```cheby
pub fn append(path: String) -> Result<File, io::Error>
```

Opens the file at `path` for writing at its end, creating it if it does not exist. Every write goes to the end of the file, also when another program writes to it meanwhile.

- **Fails:** the same cases as [`create`](#create).
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "append needs the directory to exist" {
  let assert Err(reason) = file::append("no/such/directory/log.txt")
  assert io::is_not_found(reason)
}

test "append writes after what the file holds" {
  use dir <- file::with_temp_dir()
  let target = dir + "/log.txt"
  use _ <- result::try(io::write_file(target, "a\n"))
  use log <- result::try(file::append(target))
  use _ <- result::try(file::write_line(log, "b"))
  use text <- result::try(io::read_file(target))
  assert text == "a\nb\n"
  Ok(Nil)
}
```

### `create_new`

```cheby
pub fn create_new(path: String) -> Result<File, io::Error>
```

Creates the file at `path` and opens it for writing, only if nothing exists at `path` yet. The check and the creation are one atomic step of the operating system (`O_CREAT | O_EXCL`), so of several programs or fibers that race to create the same path, only one succeeds, which makes `create_new` fit for lock files. The directory that holds the file must already exist (D-1066).

- **Fails:** `Err` for which `io::is_already_exists` holds when anything exists at `path`, a file, a directory or a symbolic link, and otherwise the cases of [`create`](#create).
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "create_new refuses a path that exists" {
  use dir <- file::with_temp_dir()
  let target = dir + "/lock"
  use _ <- result::try(file::create_new(target))
  let assert Err(reason) = file::create_new(target)
  assert io::is_already_exists(reason)
  Ok(Nil)
}
```

## Scoped opening

A file already closes at its last use (D-205). What the `with_*` functions add is a close whose error is reported, and a hard end to the handle's life, which is the explicit scoped closing of D-101. When `body` unwinds, the handle closes at its last reference like every handle, since Cheby has no `finally` (ADR-0029, D-993).

`body` returns `Result<T, io::Error>`, so `use source <- file::with_open(path)` works in a function whose error type is `io::Error`. A function with its own error type keeps the IO inside the `with_*` call and maps the error outside it, after the call returns (D-1075):

```cheby
type LoadError { ReadFailed(io::Error), Empty }

fn load(path: String) -> Result<String, LoadError> {
  let read = file::with_open(path, fn(source) { file::read_line(source) })
  use found <- result::try(result::map_error(read, ReadFailed))
  option::to_result(found, Empty)
}
```

The same holds for every `with_*` function of this module, [`with_create_new`](#with_create_new) and [`with_temp_dir`](#with_temp_dir) included. A scoped form that takes a function to wrap the error, such as `with_open_as(path, wrap, body)`, can be added later without breaking a program (D-993, D-1075).

### `with_open`

```cheby
pub fn with_open<T>(path: String, body: fn(File) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Opens the file at `path` for reading as [`open`](#open) does, calls `body` with it, and closes it afterwards. It is written with `use` ([§5.10](../spec/05-expressions.md#510-use)). When `body` returns `Ok`, a failure to close is returned as `Err`. When `body` returns `Err`, that error is returned and a failure to close is ignored. When `body` panics or is cancelled, the file closes like any handle when its last reference is released, which is during the unwinding unless `body` let it escape. If `body` closed the file itself, `with_open` does not close it again.

`body` is an ordinary call on the calling fiber (D-675). The handle is closed after `body` returns even when `body` let it escape, for example into a closure or a channel: every later operation on it fails with `io::is_closed` (D-993).

- **Fails:** `Err` from opening, without calling `body`, in the cases of `open`. Otherwise the `Err` of `body`, or of the close when `body` returned `Ok`.
- **Cost:** O(1), plus the two system calls and `body`. Informative.

```cheby
test "with_open does not call its body when the file cannot be opened" {
  let outcome = file::with_open("no/such/directory/file.txt", fn(_) { Ok(1) })
  let assert Err(reason) = outcome
  assert io::is_not_found(reason)
}

test "with_open reads a file line by line" {
  use dir <- file::with_temp_dir()
  let target = dir + "/lines.txt"
  use _ <- result::try(io::write_file(target, "a\nb\nc"))
  use total <- result::try({
    use source <- file::with_open(target)
    fn step(count) {
      case file::read_line(source) {
        Ok(Some(_)) => step(count + 1)
        Ok(None) => Ok(count)
        Err(reason) => Err(reason)
      }
    }
    step(0)
  })
  assert total == 3
  Ok(Nil)
}

test "with_open closes a handle that escapes its body" {
  use dir <- file::with_temp_dir()
  let target = dir + "/lines.txt"
  use _ <- result::try(io::write_file(target, "a\n"))
  use kept <- result::try(file::with_open(target, fn(source) { Ok(source) }))
  let assert Err(reason) = file::read_line(kept)
  assert io::is_closed(reason)
  Ok(Nil)
}
```

### `with_create`

```cheby
pub fn with_create<T>(path: String, body: fn(File) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Opens the file at `path` as [`create`](#create) does, calls `body` with it and closes it afterwards, with the rules of [`with_open`](#with_open).

- **Fails:** `Err` from opening, without calling `body`, in the cases of `create`. Otherwise as `with_open`.
- **Cost:** O(1), plus the two system calls and `body`. Informative.

The block inside the second test is the body of the example of [§5.10](../spec/05-expressions.md#510-use) (D-992):

```cheby
test "with_create does not call its body when the directory is missing" {
  let outcome = file::with_create("no/such/directory/out.txt", fn(_) { Ok(Nil) })
  let assert Err(reason) = outcome
  assert io::is_not_found(reason)
}

test "with_create writes a new file" {
  use dir <- file::with_temp_dir()
  let target = dir + "/log.txt"
  use _ <- result::try({
    use handle <- file::with_create(target)
    file::write_line(handle, "started")
  })
  use text <- result::try(io::read_file(target))
  assert text == "started\n"
  Ok(Nil)
}
```

### `with_append`

```cheby
pub fn with_append<T>(path: String, body: fn(File) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Opens the file at `path` as [`append`](#append) does, calls `body` with it and closes it afterwards, with the rules of [`with_open`](#with_open).

- **Fails:** `Err` from opening, without calling `body`, in the cases of `append`. Otherwise as `with_open`.
- **Cost:** O(1), plus the two system calls and `body`. Informative.

```cheby
test "with_append does not call its body when the directory is missing" {
  let outcome = file::with_append("no/such/directory/log.txt", fn(_) { Ok(Nil) })
  let assert Err(reason) = outcome
  assert io::is_not_found(reason)
}
```

### `with_create_new`

```cheby
pub fn with_create_new<T>(path: String, body: fn(File) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Creates and opens the file at `path` as [`create_new`](#create_new) does, calls `body` with it and closes it afterwards, with the rules of [`with_open`](#with_open) (D-1066).

- **Fails:** `Err` from creating, without calling `body`, in the cases of `create_new`. Otherwise as `with_open`.
- **Cost:** O(1), plus the two system calls and `body`. Informative.

```cheby
test "with_create_new does not call its body when the file exists" {
  use dir <- file::with_temp_dir()
  let target = dir + "/lock"
  use _ <- result::try(file::with_create_new(target, fn(handle) { file::write(handle, "1") }))
  let outcome = file::with_create_new(target, fn(_) { Ok(Nil) })
  let assert Err(reason) = outcome
  assert io::is_already_exists(reason)
  use text <- result::try(io::read_file(target))
  assert text == "1"
  Ok(Nil)
}
```

### `close`

```cheby
pub fn close(file: File) -> Result<Nil, io::Error>
```

Closes `file` now and reports what the operating system says about it. Every later operation on the handle, including another `close`, fails with `io::is_closed`. Releasing a closed handle does nothing more. A program needs `close` only when it wants the error: the file closes by itself at its last use (D-205, D-993).

- **Fails:** `Err` for which `io::is_closed` holds when `file` is already closed. `Err` when the operating system reports a failure on closing, which some network file systems use to report a write that did not reach the disk.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "close closes the handle now" {
  use dir <- file::with_temp_dir()
  let target = dir + "/saved.txt"
  use handle <- result::try(file::create(target))
  use _ <- result::try(file::write(handle, "saved"))
  use _ <- result::try(file::close(handle))
  let assert Err(again) = file::close(handle)
  assert io::is_closed(again)
  let assert Err(late) = file::write(handle, "late")
  assert io::is_closed(late)
  use text <- result::try(io::read_file(target))
  assert text == "saved"
  Ok(Nil)
}
```

## Reading

A `File` opened for reading reads ahead in blocks of 64 KiB, and serves `read_line` and `read_bytes` from that buffer on the calling fiber's thread, as standard input's reader does. Only a read that empties the buffer is a blocking-pool job, so a run of `read_line` calls costs one job per buffer, not one per line. The block size is informative (D-548, D-823, D-995). `read_line` and `read_bytes` read from the same buffer and may be mixed. A cancelled refill leaves its bytes in that buffer for the next read (D-1065).

### `read_line`

```cheby
pub fn read_line(file: File) -> Result<Option<String>, io::Error>
```

Reads the next line from `file`, as `io::read_line` reads standard input: `Ok(Some(line))` without its line ending, removing a final `\n` or `\r\n`, and `Ok(None)` at the end of the file (D-305). The last line may have no ending. Calls after the end keep returning `Ok(None)`.

- **Fails:** `Err` for which `io::is_invalid_data` holds when the line is not valid UTF-8 (D-311). The line is consumed, so the next call reads the line after it. `Err` for which `io::is_invalid_input` holds when `file` was not opened for reading, `io::is_closed` when it is closed, and `Err` for every other failure of the operating system.
- **Cost:** O(n) in the length of the line, plus a read when the buffer runs out. Informative.

```cheby
test "read_line removes line endings" {
  use dir <- file::with_temp_dir()
  let target = dir + "/lines.txt"
  use _ <- result::try(io::write_file(target, "one\r\ntwo\nthree"))
  use source <- file::with_open(target)
  assert file::read_line(source) == Ok(Some("one"))
  assert file::read_line(source) == Ok(Some("two"))
  assert file::read_line(source) == Ok(Some("three"))
  assert file::read_line(source) == Ok(None)
  assert file::read_line(source) == Ok(None)
  Ok(Nil)
}

test "read_line consumes a line that is not UTF-8" {
  use dir <- file::with_temp_dir()
  let target = dir + "/mixed.txt"
  use _ <- result::try(io::write_file_bytes(target, bytes::from_list([0xFF, 0x0A, 0x6F, 0x6B])))
  use source <- file::with_open(target)
  let assert Err(reason) = file::read_line(source)
  assert io::is_invalid_data(reason)
  assert file::read_line(source) == Ok(Some("ok"))
  Ok(Nil)
}
```

### `read_bytes`

```cheby
pub fn read_bytes(file: File, limit: Int) -> Result<Option<bytes::Bytes>, io::Error>
```

Reads the next bytes of `file`, at least one and at most `limit`, without decoding them. It returns `Ok(None)` at the end of the file. It may return fewer than `limit` bytes before the end. `limit` caps the result and never sizes an allocation: a read takes at most what the read-ahead buffer holds, so a large `limit` costs nothing extra (D-1062).

- **Fails:** `Err` for which `io::is_invalid_input` holds when `file` was not opened for reading, `io::is_closed` when it is closed, and `Err` for every other failure of the operating system.
- **Clamps:** `limit` below 1 counts as 1 (D-298).
- **Cost:** O(n) in the number of bytes returned, plus a read when the buffer runs out. Informative.

```cheby
test "read_bytes returns at most its limit" {
  use dir <- file::with_temp_dir()
  let target = dir + "/image.png"
  let data = bytes::from_list([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A])
  use _ <- result::try(io::write_file_bytes(target, data))
  use source <- file::with_open(target)
  assert file::read_bytes(source, 0) == Ok(Some(bytes::from_list([0x89])))
  fn collect(acc) {
    case file::read_bytes(source, 4) {
      Ok(Some(chunk)) => {
        assert bytes::length(chunk) <= 4
        collect(bytes::append(acc, chunk))
      }
      Ok(None) => Ok(acc)
      Err(reason) => Err(reason)
    }
  }
  use whole <- result::try(collect(bytes::from_list([0x89])))
  assert whole == data
  Ok(Nil)
}
```

## Writing

Writes are not buffered. Each call hands its data to the operating system before it returns, as one blocking-pool job, so a write that returned `Ok` is in the file and visible to every reader of it, and dropping a handle never has anything left to write. There is no `flush`. A program that writes many small pieces builds a `String` first, for example with `string::join` (D-994, ADR-0065).

### `write`

```cheby
pub fn write(file: File, text: String) -> Result<Nil, io::Error>
```

Writes `text` to `file` as UTF-8, with no line ending. The whole text is written before the call returns.

- **Fails:** `Err` for which `io::is_invalid_input` holds when `file` was opened only for reading, `io::is_closed` when it is closed, and `Err` for every other failure of the operating system, such as a full disk. If the write fails part way, how much of `text` reached the file is not specified.
- **Cost:** O(n) in the length of `text`, plus one blocking-pool job. Informative.

```cheby
test "write writes text as it is" {
  use dir <- file::with_temp_dir()
  let target = dir + "/report.txt"
  use _ <- result::try({
    use handle <- file::with_create(target)
    file::write(handle, string::join(["a", "b"], "\n") + "\n")
  })
  use text <- result::try(io::read_file(target))
  assert text == "a\nb\n"
  use source <- file::with_open(target)
  let assert Err(reason) = file::write(source, "more")
  assert io::is_invalid_input(reason)
  Ok(Nil)
}
```

### `write_line`

```cheby
pub fn write_line(file: File, text: String) -> Result<Nil, io::Error>
```

Writes `text` and the line ending `\n` to `file`, in one write. The line ending is `\n` on every platform, including Windows.

- **Fails:** the same cases as [`write`](#write).
- **Cost:** O(n) in the length of `text`, plus one blocking-pool job. Informative.

```cheby
test "write_line ends each line with a newline" {
  use dir <- file::with_temp_dir()
  let target = dir + "/events.log"
  fn log_event(event) {
    use handle <- file::with_append(target)
    file::write_line(handle, event)
  }
  use _ <- result::try(log_event("start"))
  use _ <- result::try(log_event("stop"))
  use text <- result::try(io::read_file(target))
  assert text == "start\nstop\n"
  Ok(Nil)
}
```

### `write_bytes`

```cheby
pub fn write_bytes(file: File, data: bytes::Bytes) -> Result<Nil, io::Error>
```

Writes `data` to `file` unchanged. The whole of `data` is written before the call returns.

- **Fails:** the same cases as [`write`](#write).
- **Cost:** O(n) in the length of `data`, plus one blocking-pool job. Informative.

```cheby
test "write_bytes writes bytes unchanged" {
  use dir <- file::with_temp_dir()
  let target = dir + "/header.bin"
  let data = bytes::from_list([0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF])
  use _ <- result::try({
    use handle <- file::with_create(target)
    file::write_bytes(handle, data)
  })
  use written <- result::try(io::read_file_bytes(target))
  assert written == data
  Ok(Nil)
}
```

## Standard streams

[`stdin`](#stdin), [`stdout`](#stdout) and [`stderr`](#stderr) return handles for the standard streams, so every function of this module, and every function generic over `io::Source` or `io::Sink`, works on them, and a program can read and write them as bytes (D-1071, D-1072). The handles follow the rules of §10.10 for the standard streams, as `std::io`'s functions do ([§10.10](../spec/10-concurrency.md#1010-program-entry-and-exit)):

- Each call returns a new handle. [`close`](#close) and the drop function mark only that handle closed, and never close the stream itself, so closing one handle does not affect another part of the program, or `std::io`.
- Writing to a `stdin()` handle, or reading from a `stdout()` or `stderr()` handle, fails with `io::is_invalid_input`, as for a file opened in the other mode.
- A stream that is closed or missing when the program starts stays a null device: writes succeed and discard the data, and reads see the end of input (D-474).
- The kernel is `sys_std_stream(which)`, and the handles are `File`s whose runtime kind carries a flag that routes them to the streams (D-1071).

### `stdin`

```cheby
pub fn stdin() -> File
```

Returns a new handle for standard input. Its reads come from the runtime's one reader of standard input, the one that `io::read_line` uses, not from a buffer of the handle, and standard output is flushed first, as for `io::read_line` (D-346, D-823). So `file::read_line(file::stdin())` behaves as `io::read_line()`, the two may be mixed and keep the order of the input, and `file::read_bytes(file::stdin(), limit)` reads standard input as bytes. A cancelled refill of the reader leaves its bytes for the next read, whichever function makes it (D-1065).

- **Cost:** O(1). Informative.

```cheby
test "a standard input handle does not write" {
  let assert Err(reason) = file::write(file::stdin(), "text")
  assert io::is_invalid_input(reason)
}
```

_Example_, not a `test` because it reads standard input:

```cheby
fn count_bytes(source: file::File, total: Int) -> Result<Int, io::Error> {
  case file::read_bytes(source, 65_536) {
    Ok(Some(chunk)) => count_bytes(source, total + bytes::length(chunk))
    Ok(None) => Ok(total)
    Err(reason) => Err(reason)
  }
}

fn main() -> Result<Nil, io::Error> {
  use total <- result::try(count_bytes(file::stdin(), 0))
  io::println("{total}")
  Ok(Nil)
}
```

### `stdout`

```cheby
pub fn stdout() -> File
```

Returns a new handle for standard output. A write to it goes through standard output's buffer and then flushes it, as `io::write_stdout` does, so output written through the handle and through `io::print` never changes order, and the write's `Result` covers everything written to standard output so far (D-365, D-574, D-1071).

- **Cost:** O(1). Informative.

```cheby
test "a standard output handle does not read" {
  let assert Err(reason) = file::read_line(file::stdout())
  assert io::is_invalid_input(reason)
}
```

### `stderr`

```cheby
pub fn stderr() -> File
```

Returns a new handle for standard error. A write to it is written at once, since standard error is not buffered (D-346, D-1071).

- **Cost:** O(1). Informative.

```cheby
test "closing a standard stream handle closes only that handle" {
  let first = file::stderr()
  use _ <- result::try(file::close(first))
  let assert Err(reason) = file::write(first, "")
  assert io::is_closed(reason)
  file::write(file::stderr(), "")
}
```

## Directories

A directory listing gives entry names, not paths, sorted by their UTF-8 bytes, without `.` and `..`, so it is the same on every platform and file system, as goldens need (D-232, D-997).

### `list_dir`

```cheby
pub fn list_dir(path: String) -> Result<List<String>, io::Error>
```

Returns the names of the entries of the directory at `path`, sorted by their UTF-8 bytes, without `.` and `..`.

- **Fails:** `Err` for which `io::is_not_found` holds when there is no directory at `path`, `io::is_not_a_directory` when `path` names something else, `io::is_permission_denied` when the program may not list it, and `io::is_invalid_data` when an entry's name is not valid UTF-8 (D-311, D-997). `Err` for every other failure of the operating system.
- **Cost:** O(n log n) in the number of entries, plus the reads. Informative.

```cheby
test "list_dir reports a missing directory" {
  let assert Err(reason) = file::list_dir("no/such/directory")
  assert io::is_not_found(reason)
}

test "list_dir sorts names by their bytes" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(dir + "/b.txt", ""))
  use _ <- result::try(io::write_file(dir + "/Zed.txt", ""))
  use _ <- result::try(file::create_dir(dir + "/a"))
  use names <- result::try(file::list_dir(dir))
  assert names == ["Zed.txt", "a", "b.txt"]
  Ok(Nil)
}
```

### `create_dir`

```cheby
pub fn create_dir(path: String) -> Result<Nil, io::Error>
```

Creates the directory `path`. Its parent must already exist.

- **Fails:** `Err` for which `io::is_already_exists` holds when something already exists at `path`, `io::is_not_found` when its parent does not exist, and `io::is_permission_denied` when the program may not create it. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "create_dir needs its parent" {
  let assert Err(reason) = file::create_dir("no/such/directory/child")
  assert io::is_not_found(reason)
}

test "create_dir refuses a path that exists" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(file::create_dir(dir + "/child"))
  let assert Err(reason) = file::create_dir(dir + "/child")
  assert io::is_already_exists(reason)
  Ok(Nil)
}
```

### `create_dir_all`

```cheby
pub fn create_dir_all(path: String) -> Result<Nil, io::Error>
```

Creates the directory `path` and every missing directory above it. A directory that already exists at `path` is not an error.

- **Fails:** `Err` for which `io::is_already_exists` holds when something other than a directory exists at `path` or above it, and `io::is_permission_denied` when the program may not create one of them. `Err` for every other failure of the operating system. Directories created before the failure stay.
- **Cost:** O(d) in the number of directories created, plus the system calls. Informative.

```cheby
test "create_dir_all creates every missing directory" {
  use dir <- file::with_temp_dir()
  let daily = dir + "/reports/daily"
  use _ <- result::try(file::create_dir_all(daily))
  use _ <- result::try(file::create_dir_all(daily))
  use found <- result::try(file::info(daily))
  assert file::is_dir(found)
  use _ <- result::try(io::write_file(dir + "/plain", ""))
  let assert Err(reason) = file::create_dir_all(dir + "/plain")
  assert io::is_already_exists(reason)
  Ok(Nil)
}
```

### `remove_file`

```cheby
pub fn remove_file(path: String) -> Result<Nil, io::Error>
```

Removes the file at `path`. Open handles to it keep working where the operating system allows it.

- **Fails:** `Err` for which `io::is_not_found` holds when there is nothing at `path`, `io::is_a_directory` when `path` names a directory, and `io::is_permission_denied` when the program may not remove it. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "remove_file reports a missing file" {
  let assert Err(reason) = file::remove_file("no/such/directory/file.txt")
  assert io::is_not_found(reason)
}

test "remove_file removes a file" {
  use dir <- file::with_temp_dir()
  let target = dir + "/old.txt"
  use _ <- result::try(io::write_file(target, "old"))
  use _ <- result::try(file::remove_file(target))
  let assert Err(reason) = file::info(target)
  assert io::is_not_found(reason)
  Ok(Nil)
}
```

### `remove_dir`

```cheby
pub fn remove_dir(path: String) -> Result<Nil, io::Error>
```

Removes the directory at `path`, which must be empty. [`remove_dir_all`](#remove_dir_all) removes a directory with everything in it.

- **Fails:** `Err` for which `io::is_not_found` holds when there is nothing at `path`, `io::is_not_a_directory` when `path` names something else, `io::is_directory_not_empty` when it has entries, and `io::is_permission_denied` when the program may not remove it. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "remove_dir reports a missing directory" {
  let assert Err(reason) = file::remove_dir("no/such/directory")
  assert io::is_not_found(reason)
}

test "remove_dir removes only an empty directory" {
  use dir <- file::with_temp_dir()
  let child = dir + "/child"
  use _ <- result::try(file::create_dir(child))
  use _ <- result::try(io::write_file(child + "/kept.txt", ""))
  let assert Err(reason) = file::remove_dir(child)
  assert io::is_directory_not_empty(reason)
  use _ <- result::try(file::remove_file(child + "/kept.txt"))
  use _ <- result::try(file::remove_dir(child))
  use names <- result::try(file::list_dir(dir))
  assert names == []
  Ok(Nil)
}
```

### `remove_dir_all`

```cheby
pub fn remove_dir_all(path: String) -> Result<Nil, io::Error>
```

Removes the directory at `path` and everything in it. It never follows a symbolic link: a link inside the tree is removed, not what it points to, and when `path` itself is a symbolic link, the link is removed and its target is left alone, as Rust's `remove_dir_all` and Go's `RemoveAll` do. So it cannot remove anything outside the tree, which a removal written over [`list_dir`](#list_dir) and [`info`](#info), which follows links, could (D-1066).

When nothing exists at `path`, or `path` names a file, it fails as [`remove_dir`](#remove_dir) does, as Rust's `remove_dir_all` does, and unlike Go's `RemoveAll` (D-1066).

- **Fails:** `Err` for which `io::is_not_found` holds when there is nothing at `path`, `io::is_not_a_directory` when `path` names something that is neither a directory nor a symbolic link, and `io::is_permission_denied` when the program may not remove an entry. `Err` for every other failure of the operating system. Entries removed before the failure stay removed.
- **Cost:** O(n) in the number of entries in the tree, plus a system call for each. Informative.

```cheby
test "remove_dir_all removes a whole tree" {
  use dir <- file::with_temp_dir()
  let tree = dir + "/tree"
  use _ <- result::try(file::create_dir_all(tree + "/a/b"))
  use _ <- result::try(io::write_file(tree + "/a/b/leaf.txt", "leaf"))
  use _ <- result::try(file::remove_dir_all(tree))
  let assert Err(reason) = file::info(tree)
  assert io::is_not_found(reason)
  Ok(Nil)
}
```

### `rename`

```cheby
pub fn rename(old_path: String, new_path: String) -> Result<Nil, io::Error>
```

Moves the file or directory at `old_path` to `new_path`, replacing a file already at `new_path`. Both paths must be on the same file system.

- **Fails:** `Err` for which `io::is_not_found` holds when nothing exists at `old_path` or the directory of `new_path` does not exist, and `io::is_permission_denied` when the program may not move it. `Err` for every other failure of the operating system, such as paths on different file systems.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "rename reports a missing source" {
  let assert Err(reason) = file::rename("no/such/directory/a.txt", "no/such/directory/b.txt")
  assert io::is_not_found(reason)
}

test "rename replaces a file at the new path" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(dir + "/a.txt", "new"))
  use _ <- result::try(io::write_file(dir + "/b.txt", "old"))
  use _ <- result::try(file::rename(dir + "/a.txt", dir + "/b.txt"))
  use names <- result::try(file::list_dir(dir))
  assert names == ["b.txt"]
  use text <- result::try(io::read_file(dir + "/b.txt"))
  assert text == "new"
  Ok(Nil)
}
```

## Scratch directories

### `with_temp_dir`

```cheby
pub fn with_temp_dir<T>(body: fn(String) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Creates a new, empty directory with a random name under the host's temporary directory, calls `body` with its path, and removes the directory and everything in it, as [`remove_dir_all`](#remove_dir_all) does, when `body` returns. The path is in the host's own form, such as `/tmp/…` or `C:\Users\…\Temp\…`, and a program builds paths inside it with `/`, which every platform accepts (D-996). Under `cheby test`, the directory lies under the run's temporary root. Its path differs from run to run, so it never belongs in a golden (D-1066, D-1067).

As for [`with_open`](#with_open), a failure to remove the directory is returned only when `body` returned `Ok` (D-993). When `body` panics or is cancelled, the directory stays, since Cheby has no `finally` (ADR-0029) and a drop function must not wait (D-703). Under `cheby test`, it is removed with the run's temporary root when the run ends (D-1067).

- **Fails:** `Err` from creating the directory, without calling `body`, such as `io::is_permission_denied` when the program may not write to the host's temporary directory. Otherwise the `Err` of `body`, or of the removal when `body` returned `Ok`.
- **Cost:** O(1), plus the system calls, `body`, and the removal of what `body` left. Informative.

```cheby
test "with_temp_dir gives a new, empty directory" {
  use dir <- file::with_temp_dir()
  use names <- result::try(file::list_dir(dir))
  assert names == []
  use other <- file::with_temp_dir()
  assert other != dir
  Ok(Nil)
}

test "with_temp_dir removes its directory afterwards" {
  let assert Ok(kept) = file::with_temp_dir(fn(dir) {
    use _ <- result::try(io::write_file(dir + "/left.txt", "left"))
    Ok(dir)
  })
  let assert Err(reason) = file::info(kept)
  assert io::is_not_found(reason)
}
```

## Information about a path

### `info`

```cheby
pub fn info(path: String) -> Result<Info, io::Error>
```

Returns what the operating system knows about the file or directory at `path`, following symbolic links. There is no `exists`: a missing path is `Err` for which `io::is_not_found` holds, which also keeps a permission failure from looking like a missing file (D-996).

- **Fails:** `Err` for which `io::is_not_found` holds when there is nothing at `path`, and `io::is_permission_denied` when the program may not reach it. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "info reports a missing path" {
  let assert Err(reason) = file::info("no/such/directory/file.txt")
  assert io::is_not_found(reason)
}
```

The accessors below take their `Info` as `entry`, because `info` is a function of this module (D-051, D-996).

### `size`

```cheby
pub fn size(entry: Info) -> Int
```

Returns the size in bytes of the file that `entry` describes. For a directory the value is whatever the operating system reports, and is not specified.

- **Cost:** O(1). Informative.

```cheby
test "size counts bytes" {
  use dir <- file::with_temp_dir()
  let target = dir + "/word.txt"
  use _ <- result::try(io::write_file(target, "héllo"))
  use found <- result::try(file::info(target))
  assert file::size(found) == 6
  assert file::is_file(found)
  Ok(Nil)
}
```

### `is_file`

```cheby
pub fn is_file(entry: Info) -> Bool
```

Tells whether `entry` describes a regular file.

- **Cost:** O(1). Informative.

The test of [`size`](#size) uses it.

### `is_dir`

```cheby
pub fn is_dir(entry: Info) -> Bool
```

Tells whether `entry` describes a directory. An entry that is neither a file nor a directory, such as a device, gives `False` for both.

- **Cost:** O(1). Informative.

```cheby
test "is_dir tells a directory from a file" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(dir + "/plain.txt", ""))
  use folder <- result::try(file::info(dir))
  assert file::is_dir(folder)
  assert !file::is_file(folder)
  use plain <- result::try(file::info(dir + "/plain.txt"))
  assert !file::is_dir(plain)
  Ok(Nil)
}
```

### `modified`

```cheby
pub fn modified(entry: Info) -> time::Timestamp
```

Returns when the contents of the file or directory were last changed, as the operating system recorded it, as a [`time::Timestamp`](time.md).

- **Cost:** O(1). Informative.

The exact time depends on the host's clock, so the test only checks that a new file was changed after 2020 began.

```cheby
test "a new file was modified after 2020 began" {
  use dir <- file::with_temp_dir()
  let target = dir + "/new.txt"
  use _ <- result::try(io::write_file(target, ""))
  use found <- result::try(file::info(target))
  assert file::modified(found) > time::from_unix(duration::seconds(1_577_836_800))
  Ok(Nil)
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-996):

- copying, permissions, ownership, creating a symbolic link or reading its target, hard links, file locking and watching;
- reading a path without following a final symbolic link, as `symlink_info(path: String) -> Result<Info, io::Error>` with `is_symlink(entry: Info) -> Bool`, and a directory walk, `walk(root)`, which returns sorted paths relative to `root` and never follows links. Until then, [`remove_dir_all`](#remove_dir_all) is the one recursive operation, and it never follows links (D-1076);
- seeking, and reading or writing at an offset;
- a buffered writer type (D-994);
- an unscoped `create_temp_dir`, which would leave the removal to its caller, and temporary files, which a directory from [`with_temp_dir`](#with_temp_dir) holds any number of (D-1066);
- scoped openers that take a function to wrap the error, such as `with_open_as(path, wrap, body)` (D-1075);
- the working directory and changing it, which would be process-wide state that every fiber shares;
- path functions such as joining or taking an extension, in a later module whose name that tier decides, since `path` is the parameter of most functions here (D-1053). It works on `String`s, with no `Path` type, and its `join` keeps the result under its base: `join("a", "/b")` gives `a/b`, as Go's `path.Join` does, not `/b` as in Rust and Python, so that `join(base, input)` cannot leave `base` (D-1077).
