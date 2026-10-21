# `std::file`

Files through handles: opening a file, reading and writing it in pieces, closing it with a reported error, and directories. Whole-file reads and writes stay in [`std::io`](io.md) (D-328), which is still the short way to read or replace a whole file. In the form `cheby doc` prints:

```cheby
pub type File

pub type Info
```

- **Handle.** `File` is a handle ([§9.5](../spec/09-memory-model.md#95-handles-and-drop-functions), D-101). It prints as `<File>`, `==` and hashing panic when they reach one, W0011 warns about a comparison that can reach one, and no constant may hold one (D-233, D-647). It is an external type of this module whose drop function the runtime supplies, as for the tier-2 handles (D-703, D-982). The file closes right after the last use of the last reference to it (D-205), on the fiber that released it (D-171), also when that fiber unwinds. The drop function closes the descriptor and ignores any error. It never writes, because a `File` buffers nothing for writing (D-994).
- **`Info`.** What [`info`](#info) found about a path. It is opaque plain data with structural `==`, read through accessor functions, so fields can be added after 1.0 (D-996).
- **Errors.** Every function that can fail returns `io::Error`, the error type of [`std::io`](io.md). The kinds these functions cause have predicates there, such as `io::is_already_exists` and `io::is_closed`, each with a fixed text (D-981).
- **Paths.** As in `io.md` "Files": a path is a `String`, a relative path is resolved against the working directory, and the text of an error never includes the path. `/` separates components on every platform, including Windows. There are no path functions, and no way to read or change the working directory (D-996).
- **Blocking pool.** Every function that touches the operating system runs on the blocking pool while the calling fiber is suspended, so other fibers keep running (D-042, D-683, [§10.9](../spec/10-concurrency.md#109-blocking-io)). That work cannot be interrupted. A cancellation that arrives meanwhile is delivered when the work returns, and the result is released (D-670). A fiber that a cancellation has already reached never starts the work (D-816). So a cancelled `write` has written its text, and a cancelled `read_line` leaves what it read in the handle's buffer for the next read: no line is lost (D-983).
- **Several fibers, one file.** Any fiber may use a `File` it holds. Operations on one handle run one at a time, in the order they reach it. A `read_line` therefore never sees half of another fiber's `write`.
- **Written in Cheby.** Every public function is written in Cheby over private kernels named `sys_<name>`, bound as `@external(native, "cheby_file_sys_<name>")` with `@std_context`, as `std::io`'s suspending kernels are (D-484, D-692, D-755, D-985). Each kernel also declares `@external(js, "./file.mjs", "sys_<name>")`, so every function exists on both targets for the cross-target check (D-450, D-661, D-986). No function here runs in constants: reaching one while computing a constant is E0077 (D-090, D-537, D-991).
- **On JS.** Whether the kernels use `fs/promises` with `@async` or the synchronous API, and what a host without a file system does, is OQ-P14 and comes with M4. The direction is that every function exists there and fails with a new `io::is_unsupported` kind on such a host.

Every function that changes the file system, or needs a file to exist, has an _Example_ instead of a `test`, because its result depends on the environment. Where it can fail, a `test` checks a failure that is the same everywhere, as `io.md` does.

## Opening

There are three openers, named after what they do: [`open`](#open) reads, [`create`](#create) creates or truncates and writes, and [`append`](#append) creates if needed and appends. Each has a scoped form, [`with_open`](#with_open), [`with_create`](#with_create) and [`with_append`](#with_append). Reading is the default for the name `open`, and the six names cover every common case with no flags (D-992).

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
```

_Example_, not a `test` because it needs a file to exist:

```cheby
fn first_line(path: String) -> Result<Option<String>, io::Error> {
  use source <- result::try(file::open(path))
  file::read_line(source)
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
```

## Scoped opening

A file already closes at its last use (D-205). What the `with_*` functions add is a close whose error is reported, and a hard end to the handle's life, which is the explicit scoped closing of D-101. When `body` unwinds, the handle closes at its last reference like every handle, since Cheby has no `finally` (ADR-0029, D-993).

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
```

_Example_, not a `test` because it needs a file to exist:

```cheby
fn count_lines(path: String) -> Result<Int, io::Error> {
  use source <- file::with_open(path)
  fn step(total) {
    case file::read_line(source) {
      Ok(Some(_)) => step(total + 1)
      Ok(None) => Ok(total)
      Err(reason) => Err(reason)
    }
  }
  step(0)
}
```

### `with_create`

```cheby
pub fn with_create<T>(path: String, body: fn(File) -> Result<T, io::Error>) -> Result<T, io::Error>
```

Opens the file at `path` as [`create`](#create) does, calls `body` with it and closes it afterwards, with the rules of [`with_open`](#with_open).

- **Fails:** `Err` from opening, without calling `body`, in the cases of `create`. Otherwise as `with_open`.
- **Cost:** O(1), plus the two system calls and `body`. Informative.

```cheby
test "with_create does not call its body when the directory is missing" {
  let outcome = file::with_create("no/such/directory/out.txt", fn(_) { Ok(Nil) })
  let assert Err(reason) = outcome
  assert io::is_not_found(reason)
}
```

_Example_, not a `test` because it writes a file. It is the example of [§5.10](../spec/05-expressions.md#510-use) (D-992):

```cheby
fn with_log(path: String) -> Result<Nil, io::Error> {
  use handle <- file::with_create(path)
  file::write_line(handle, "started")
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

### `close`

```cheby
pub fn close(file: File) -> Result<Nil, io::Error>
```

Closes `file` now and reports what the operating system says about it. Every later operation on the handle, including another `close`, fails with `io::is_closed`. Releasing a closed handle does nothing more. A program needs `close` only when it wants the error: the file closes by itself at its last use (D-205, D-993).

- **Fails:** `Err` for which `io::is_closed` holds when `file` is already closed. `Err` when the operating system reports a failure on closing, which some network file systems use to report a write that did not reach the disk.
- **Cost:** O(1), plus the system call. Informative.

_Example_, not a `test` because it writes a file:

```cheby
fn save(path: String, text: String) -> Result<Nil, io::Error> {
  use target <- result::try(file::create(path))
  use _ <- result::try(file::write(target, text))
  file::close(target)
}
```

## Reading

A `File` opened for reading reads ahead in blocks of 64 KiB, and serves `read_line` and `read_bytes` from that buffer on the calling fiber's thread, as standard input's reader does. Only a read that empties the buffer is a blocking-pool job, so a run of `read_line` calls costs one job per buffer, not one per line. The block size is informative (D-548, D-823, D-995). `read_line` and `read_bytes` read from the same buffer and may be mixed.

### `read_line`

```cheby
pub fn read_line(file: File) -> Result<Option<String>, io::Error>
```

Reads the next line from `file`, as `io::read_line` reads standard input: `Ok(Some(line))` without its line ending, removing a final `\n` or `\r\n`, and `Ok(None)` at the end of the file (D-305). The last line may have no ending. Calls after the end keep returning `Ok(None)`.

- **Fails:** `Err` for which `io::is_invalid_data` holds when the line is not valid UTF-8 (D-311). The line is consumed, so the next call reads the line after it. `Err` for which `io::is_invalid_input` holds when `file` was not opened for reading, `io::is_closed` when it is closed, and `Err` for every other failure of the operating system.
- **Cost:** O(n) in the length of the line, plus a read when the buffer runs out. Informative.

_Example_, not a `test` because it needs a file to exist:

```cheby
fn header(path: String) -> Result<String, io::Error> {
  use source <- file::with_open(path)
  use found <- result::try(file::read_line(source))
  Ok(option::unwrap_or(found, ""))
}
```

### `read_bytes`

```cheby
pub fn read_bytes(file: File, limit: Int) -> Result<Option<bytes::Bytes>, io::Error>
```

Reads the next bytes of `file`, at least one and at most `limit`, without decoding them. It returns `Ok(None)` at the end of the file. It may return fewer than `limit` bytes before the end.

- **Fails:** `Err` for which `io::is_invalid_input` holds when `file` was not opened for reading, `io::is_closed` when it is closed, and `Err` for every other failure of the operating system.
- **Clamps:** `limit` below 1 counts as 1 (D-298).
- **Cost:** O(n) in the number of bytes returned, plus a read when the buffer runs out. Informative.

_Example_, not a `test` because it needs a file to exist:

```cheby
fn magic_number(path: String) -> Result<Option<bytes::Bytes>, io::Error> {
  use source <- file::with_open(path)
  file::read_bytes(source, 4)
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

_Example_, not a `test` because it writes a file:

```cheby
fn save_report(path: String, lines: List<String>) -> Result<Nil, io::Error> {
  use target <- file::with_create(path)
  file::write(target, string::join(lines, "\n") + "\n")
}
```

### `write_line`

```cheby
pub fn write_line(file: File, text: String) -> Result<Nil, io::Error>
```

Writes `text` and the line ending `\n` to `file`, in one write. The line ending is `\n` on every platform, including Windows.

- **Fails:** the same cases as [`write`](#write).
- **Cost:** O(n) in the length of `text`, plus one blocking-pool job. Informative.

_Example_, not a `test` because it writes a file:

```cheby
fn log_event(path: String, event: String) -> Result<Nil, io::Error> {
  use target <- file::with_append(path)
  file::write_line(target, event)
}
```

### `write_bytes`

```cheby
pub fn write_bytes(file: File, data: bytes::Bytes) -> Result<Nil, io::Error>
```

Writes `data` to `file` unchanged. The whole of `data` is written before the call returns.

- **Fails:** the same cases as [`write`](#write).
- **Cost:** O(n) in the length of `data`, plus one blocking-pool job. Informative.

_Example_, not a `test` because it writes a file:

```cheby
fn save_header(path: String) -> Result<Nil, io::Error> {
  use target <- file::with_create(path)
  file::write_bytes(target, bytes::from_list([0x89, 0x50, 0x4E, 0x47]))
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
```

### `create_dir_all`

```cheby
pub fn create_dir_all(path: String) -> Result<Nil, io::Error>
```

Creates the directory `path` and every missing directory above it. A directory that already exists at `path` is not an error.

- **Fails:** `Err` for which `io::is_already_exists` holds when something other than a directory exists at `path` or above it, and `io::is_permission_denied` when the program may not create one of them. `Err` for every other failure of the operating system. Directories created before the failure stay.
- **Cost:** O(d) in the number of directories created, plus the system calls. Informative.

_Example_, not a `test` because it changes the file system:

```cheby
fn prepare_output(root: String) -> Result<Nil, io::Error> {
  file::create_dir_all(root + "/reports/daily")
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
```

### `remove_dir`

```cheby
pub fn remove_dir(path: String) -> Result<Nil, io::Error>
```

Removes the directory at `path`, which must be empty. There is no recursive removal in tier 3.

- **Fails:** `Err` for which `io::is_not_found` holds when there is nothing at `path`, `io::is_not_a_directory` when `path` names something else, `io::is_directory_not_empty` when it has entries, and `io::is_permission_denied` when the program may not remove it. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "remove_dir reports a missing directory" {
  let assert Err(reason) = file::remove_dir("no/such/directory")
  assert io::is_not_found(reason)
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

_Example_, not a `test` because it needs a file to exist:

```cheby
fn is_empty_file(path: String) -> Result<Bool, io::Error> {
  use found <- result::try(file::info(path))
  Ok(file::is_file(found) && file::size(found) == 0)
}
```

### `is_file`

```cheby
pub fn is_file(entry: Info) -> Bool
```

Tells whether `entry` describes a regular file.

- **Cost:** O(1). Informative.

The _Example_ of [`size`](#size) uses it.

### `is_dir`

```cheby
pub fn is_dir(entry: Info) -> Bool
```

Tells whether `entry` describes a directory. An entry that is neither a file nor a directory, such as a device, gives `False` for both.

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a path to exist:

```cheby
fn is_directory(path: String) -> Bool {
  case file::info(path) {
    Ok(found) => file::is_dir(found)
    Err(_) => False
  }
}
```

### `modified`

```cheby
pub fn modified(entry: Info) -> time::Timestamp
```

Returns when the contents of the file or directory were last changed, as the operating system recorded it, as a [`time::Timestamp`](time.md).

- **Cost:** O(1). Informative.

_Example_, not a `test` because it needs a file to exist:

```cheby
fn changed_since(path: String, since: time::Timestamp) -> Result<Bool, io::Error> {
  use found <- result::try(file::info(path))
  Ok(file::modified(found) > since)
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-996):

- copying, permissions, ownership, symbolic links (reading one, creating one), hard links, file locking and watching;
- seeking, and reading or writing at an offset;
- a buffered writer type (D-994);
- recursive removal, temporary files and directories, and the working directory and changing it, which would be process-wide state that every fiber shares;
- path functions such as joining or taking an extension, in a later `std::path`;
- creating a file only when it does not exist yet (an `O_EXCL` opener).
