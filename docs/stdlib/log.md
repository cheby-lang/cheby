# `std::log`

Leveled, structured logging. A program logs a message with fields, and the logger in context decides what happens to it (D-1046, D-1614). In the form `cheby doc` prints:

```cheby
pub exposed type Level {
  Debug,
  Info,
  Warn,
  Error,
}

pub exposed type Location {
  path: String,
  line: Int,
  column: Int,
}

pub exposed type Record {
  timestamp: time::Timestamp,
  level: Level,
  location: Location,
  message: String,
  fields: List<Field>,
}

pub type Field

pub type Logger
```

- **Records.** Every log call that its logger accepts makes one `Record`: the time of the call, its level, the place it was written, its message and its fields. A logger receives the record whole, so the message and the fields stay apart until a line is written (D-1620).
- **Levels.** `Debug`, `Info`, `Warn` and `Error`, in rising order. A logger has a level and drops every record below it (D-1615).
- **The logger in context.** A fiber has one logger, the value of the key [`logger`](#logger) in its context ([`std::context`](context.md)). A program provides one with `context::provide(log::logger, …)`, for the code it calls and every fiber spawned meanwhile. With none provided, the logger is `text_logger(Info)`, which writes lines to standard error. There is no way to provide two loggers at once: a program that wants two destinations writes a [`custom_logger`](#custom_logger) (D-1616).
- **Where a call was written.** A record's `location` is the place of the log call: its file's path, line and column, the three parts of a panic report's location (§11.6). The place is where the log function is named. That covers a call, a pipe stage such as `message |> log::info`, and a function named as a value, as in `list::each(lines, log::info)`, whose records all carry the place of that mention. A function of your own that wraps a log call reports the place inside the wrapper (D-1628, D-1629, ADR-0081).
- **Fields.** A `Field` is a name with a value: text, an `Int`, a `Float` or a `Bool`. It is made by [`field`](#field), [`int`](#int), [`float`](#float), [`bool`](#bool) or [`inspect`](#inspect). Values are flat: there are no nested fields. An interpolated message such as `"listening on {port}"` has no field `port`. Pass `log::int("port", port)` for that (D-1618).
- **Scoped fields.** [`with_fields`](#with_fields) adds fields to every record logged while its body runs, also in the fibers spawned meanwhile. They sit in a private key of this module, so they follow spawns as every context value does, not channel messages (D-1619).
- **One name, one field.** A record never has two fields of one name when a log call builds it. Scoped fields come first, outermost first, then the call's own. A field whose name is already there replaces that field's value in place, so the last one wins and the first position is kept (D-1619).
- **Built-in loggers.** [`text_logger`](#text_logger) writes [the text line](#the-text-line) and [`json_logger`](#json_logger) [the JSON line](#the-json-line), each with a line ending, to standard error. A record is one write, so lines of fibers on different threads do not mix. A failed write is ignored: the record is lost and the program goes on, unlike `io::eprintln`, which ends the program (D-306, D-1624).
- **Custom loggers.** [`custom_logger`](#custom_logger) calls a function with each record, on the fiber that logged. While that function runs, the logger in context is the silent one, so a log call inside it does nothing. A fiber that the function spawns inherits the silent logger and keeps it (D-1625).
- **What a call does.** A log call reads the logger in context. If the call's level is below the logger's, nothing else happens: a lazy call's function is not called and the clock is not read. Otherwise the call runs its function, if it is a lazy call, reads `time::now()`, builds the record and hands it to the logger. The time comes from the clock in context ([`time::clock`](time.md#clock)), so a fixed clock makes logged lines the same on every run. A custom clock whose functions log makes the calls nest until the stack overflows, as one that calls `time::now` does (D-1570, D-1620).
- **Tests.** [`capture`](#capture) runs a body and returns the records it logged, at every level, and writes nothing. Under `cheby test`, a test that logs to the default logger shows its lines with its output, also when it passes. A test that wants none provides [`silent_logger`](#silent_logger) or uses `capture` (D-1626, D-1634).
- **Equality and printing.** A `Field` prints as the text line writes it, as in `port=8080`, and two fields are `==` when their names, kinds and values are: `log::int("n", 1)` is not `log::field("n", 1)`. A `Logger` prints as `<Logger>`. Two built-in loggers are `==` when they have the same form and level, and `==` or hashing panics when it reaches a custom logger or the logger of a `capture`, as it does on a function value (§3.13, D-233). Both types supply these through `@std_hooks` (D-1627).
- **Interfaces.** `Level` satisfies `Show` and `Compare` through [`show`](#show) and [`compare`](#compare), so `"{level}"` and `level >= log::Warn` work. A module's function serves one type (§8.2.1), so `Location`, `Field` and `Record` satisfy neither. A location's text is `"{place.path}:{place.line}:{place.column}"` (D-1615).
- **Names.** Use the module qualified. Its functions have common names, such as `error`, `debug` and `int`, and an item import of one bans that name for locals in the importing module (§5.2.3, D-062). A module that imports `std::log` cannot have a local named `log`. `std/log.cheby` itself imports `std::int`, `std::float` and `std::bool` under the plural aliases `ints`, `floats` and `bools` (D-1053).
- **Constants.** The log functions, `with_fields` and `capture` are E0077 in a constant, because they read the context. Every other function of the module is plain Cheby, and the logger builders run in the initializer of [`logger`](#logger). A constant cannot hold a `Logger`, whose type holds a function (§4.5).
- **Written in Cheby.** The module has no kernels. It is written over `std::context`, `std::time`, `std::channel` and `io::write_stderr`, so it runs on JS once those do (D-1614). The sixteen log functions carry the standard-library-only attribute `@std_call_site`, through which the compiler passes the place of each call (§4.8, D-1628). A backtrace names their frames `std::log::info_at` and the like, which are the functions the compiler calls in their place.
- **Status.** This module is in tier 4. It is implemented on native, in `std/log.cheby`, and it needs no JS file of its own: its tests pass on Node once `std::context`, `std::time` and `std::channel` do. No std module is planned since it shipped (D-1633, D-1635).

The examples use this time and this record:

```cheby
/// 2026-11-17T10:00:00.5Z.
const opening: time::Timestamp = time::from_unix(duration::milliseconds(1_794_909_600_500))

/// A record of level `Info` with `message` and `fields`, logged at `opening`.
fn sample(message: String, fields: List<log::Field>) -> log::Record {
  log::Record {
    timestamp: opening,
    level: log::Info,
    location: log::Location { path: "src/main.cheby", line: 12, column: 3 },
    message,
    fields,
  }
}

/// The messages of `records`.
fn messages(records: List<log::Record>) -> List<String> {
  list::map(records, fn(record) { record.message })
}
```

## Levels

### `show`

```cheby
pub fn show(level: Level) -> String
```

Returns `debug`, `info`, `warn` or `error`. It makes `Level` satisfy `Show` (§8.8). The JSON line writes this text, and the text line writes it in upper case (D-1615).

```cheby
test "show names a level in lower case" {
  assert log::show(log::Warn) == "warn"
  let level = log::Error
  assert "{level}" == "error"
}
```

### `parse`

```cheby
pub fn parse(text: String) -> Result<Level, Nil>
```

Returns the level that `text` names. It reads the four texts of [`show`](#show) in any mix of ASCII upper and lower case, as `std::hex` reads either case, and nothing else: no spaces around the name and no other spelling (D-1615).

- **Fails:** `Err(Nil)` for every other text, such as `"warning"` or `" info"`.

```cheby
test "parse reads a level in any case" {
  assert log::parse("warn") == Ok(log::Warn)
  assert log::parse("WARN") == Ok(log::Warn)
  assert log::parse("Info") == Ok(log::Info)
  assert log::parse("warning") == Err(Nil)
  assert log::parse(" info") == Err(Nil)
}
```

### `compare`

```cheby
pub fn compare(a: Level, b: Level) -> Order
```

Orders levels as `Debug`, `Info`, `Warn`, `Error`. It makes `Level` satisfy `Compare`, so the comparison operators work on levels (§8.7).

```cheby
test "levels rise from Debug to Error" {
  assert log::compare(log::Debug, log::Info) == Less
  assert log::Error >= log::Warn
  assert list::sort([log::Error, log::Debug, log::Warn]) == [log::Debug, log::Warn, log::Error]
}
```

## Loggers

### `logger`

```cheby
pub const logger: context::Key<Logger> = context::key(text_logger(Info))
```

The key of the logger that the log functions hand their records to. Its default is `text_logger(Info)`. A program provides another with `context::provide`, for the code it calls and everything spawned meanwhile (D-1044, D-1616).

```cheby
test "the logger in context decides what a call does" {
  let (seen_tx, seen) = channel::new::<String>(4)
  let sink = log::custom_logger(log::Debug, fn(record) {
    let _ = channel::send(seen_tx, record.message)
  })
  context::provide(log::logger, sink, fn() { log::info("inside") })
  assert channel::receive(seen) == Ok("inside")
  assert channel::receive(seen) == Err(Nil)
}
```

_Example_, not a `test` because it writes to standard error. A service logs JSON lines, from `Debug` up, for everything `serve` does:

```cheby
fn main() {
  use <- context::provide(log::logger, log::json_logger(log::Debug))
  serve()
}
```

### `text_logger`

```cheby
pub fn text_logger(level: Level) -> Logger
```

Returns a logger that writes [the text line](#the-text-line) of each record at or above `level`, and a line ending, to standard error in one write. A failed write is ignored (D-1624).

_Example_, not a `test` because it writes to standard error. It writes a line such as `2026-11-17T10:00:00.5Z WARN src/main.cheby:3:3: disk almost full free_mb=120`, and nothing for the `info` call:

```cheby
fn report(free_mb: Int) {
  use <- context::provide(log::logger, log::text_logger(log::Warn))
  log::warn_with("disk almost full", [log::int("free_mb", free_mb)])
  log::info("checked the disk")
}
```

### `json_logger`

```cheby
pub fn json_logger(level: Level) -> Logger
```

Returns a logger that writes [the JSON line](#the-json-line) of each record at or above `level`, and a line ending, to standard error in one write. A failed write is ignored (D-1624).

The example under [`logger`](#logger) uses it.

### `custom_logger`

```cheby
pub fn custom_logger(level: Level, handle: fn(Record) -> Nil) -> Logger
```

Returns a logger that calls `handle` with each record at or above `level`. `handle` runs on the fiber that logged, before the log call returns, and a panic in it is that fiber's panic. While `handle` runs, the logger in context is [`silent_logger`](#silent_logger), so a log call inside it does nothing, and a fiber spawned inside it keeps the silent logger. Several fibers may call `handle` at the same time (D-1625).

```cheby
test "a logger drops the records below its level" {
  let (seen_tx, seen) = channel::new::<String>(4)
  let sink = log::custom_logger(log::Warn, fn(record) {
    let _ = channel::send(seen_tx, record.message)
  })
  context::provide(log::logger, sink, fn() {
    log::info("dropped")
    log::warn("kept")
  })
  assert channel::receive(seen) == Ok("kept")
  assert channel::receive(seen) == Err(Nil)
}

test "a log call inside a handler does nothing" {
  let (seen_tx, seen) = channel::new::<String>(4)
  let sink = log::custom_logger(log::Debug, fn(record) {
    log::error("from the handler")
    let _ = channel::send(seen_tx, record.message)
  })
  context::provide(log::logger, sink, fn() { log::info("once") })
  assert channel::receive(seen) == Ok("once")
  assert channel::receive(seen) == Err(Nil)
}
```

_Example_, not a `test` because it writes to standard error. A logger for two destinations writes the text line to standard error and the JSON line to a file. It reports its own failures with `std::io`, since its log calls would do nothing:

```cheby
fn tee(audit: file::File) -> log::Logger {
  log::custom_logger(log::Debug, fn(record) {
    io::eprintln(log::text_line(record))
    let _ = file::write_line(audit, log::json_line(record))
  })
}
```

### `silent_logger`

```cheby
pub fn silent_logger() -> Logger
```

Returns a logger that drops every record. Under it a log call reads the context and returns.

```cheby
test "the silent logger takes no record" {
  let (built_tx, built) = channel::new::<String>(4)
  context::provide(log::logger, log::silent_logger(), fn() {
    log::error_lazy(fn() {
      let _ = channel::send(built_tx, "built")
      "never logged"
    })
  })
  assert channel::receive(built) == Err(Nil)
}
```

## Logging

Each level has four functions. `info` is described here, and [the other levels](#the-other-levels) have the same four.

### `info`

```cheby
pub fn info(message: String)
```

Logs `message` at level `Info`, with the scoped fields.

- **Cost:** when the logger drops the level, one read of the context. Informative.

```cheby
test "a call logs its message at its level" {
  let (_, records) = log::capture(fn() {
    log::info("started")
    log::error("failed")
  })
  let assert [first, second] = records
  assert first.level == log::Info
  assert first.message == "started"
  assert first.fields == []
  assert second.level == log::Error
}

test "a record carries the place of its call" {
  let (_, records) = log::capture(fn() { log::info("here") })
  let assert [record] = records
  assert record.location.path == "std/log.cheby"
  assert record.location.line >= 1
  assert record.location.column >= 1
}

test "a log function named as a value reports where it is named" {
  let (_, records) = log::capture(fn() { list::each(["a", "b"], log::info) })
  let assert [first, second] = records
  assert messages(records) == ["a", "b"]
  assert first.location == second.location
}
```

The second test names the path of this module's source, where the test lives. In a package's module the path is that module's, as in `src/main.cheby` (§11.6).

### `info_with`

```cheby
pub fn info_with(message: String, fields: List<Field>)
```

Logs `message` at level `Info` with the scoped fields followed by `fields`.

```cheby
test "a call passes its fields" {
  let (_, records) = log::capture(fn() {
    log::info_with("listening", [log::field("host", "localhost"), log::int("port", 8080)])
  })
  let assert [record] = records
  assert record.fields == [log::field("host", "localhost"), log::int("port", 8080)]
}

test "the last field of one name wins in place" {
  let (_, records) = log::capture(fn() {
    log::info_with("retry", [log::int("try", 1), log::bool("ok", False), log::int("try", 2)])
  })
  let assert [record] = records
  assert record.fields == [log::int("try", 2), log::bool("ok", False)]
}
```

### `info_lazy`

```cheby
pub fn info_lazy(build: fn() -> String)
```

Logs the message that `build` returns, at level `Info`, with the scoped fields. `build` is called only when the logger in context accepts the level, once, on the calling fiber. It is for a message that is costly to make (D-1617).

```cheby
test "a lazy call builds only what a logger takes" {
  let (built_tx, built) = channel::new::<String>(4)
  let sink = log::custom_logger(log::Info, fn(_) { Nil })
  context::provide(log::logger, sink, fn() {
    log::debug_lazy(fn() {
      let _ = channel::send(built_tx, "debug")
      "never logged"
    })
    log::info_lazy(fn() {
      let _ = channel::send(built_tx, "info")
      "logged"
    })
  })
  assert channel::receive(built) == Ok("info")
  assert channel::receive(built) == Err(Nil)
}
```

### `info_with_lazy`

```cheby
pub fn info_with_lazy(build: fn() -> (String, List<Field>))
```

Logs the message and the fields that `build` returns, at level `Info`, after the scoped fields. `build` is called as [`info_lazy`](#info_lazy)'s is.

```cheby
test "a lazy call may build its fields too" {
  let (_, records) = log::capture(fn() {
    log::info_with_lazy(fn() { ("sized", [log::int("bytes", 512)]) })
  })
  let assert [record] = records
  assert record.message == "sized"
  assert record.fields == [log::int("bytes", 512)]
}
```

### The other levels

```cheby
pub fn debug(message: String)
pub fn debug_with(message: String, fields: List<Field>)
pub fn debug_lazy(build: fn() -> String)
pub fn debug_with_lazy(build: fn() -> (String, List<Field>))

pub fn warn(message: String)
pub fn warn_with(message: String, fields: List<Field>)
pub fn warn_lazy(build: fn() -> String)
pub fn warn_with_lazy(build: fn() -> (String, List<Field>))

pub fn error(message: String)
pub fn error_with(message: String, fields: List<Field>)
pub fn error_lazy(build: fn() -> String)
pub fn error_with_lazy(build: fn() -> (String, List<Field>))
```

Each logs at the level in its name, as the `info` function of the same form does.

```cheby
test "each level has its functions" {
  let (_, records) = log::capture(fn() {
    log::debug("a")
    log::debug_with("b", [])
    log::warn_lazy(fn() { "c" })
    log::error_with_lazy(fn() { ("d", []) })
  })
  assert list::map(records, fn(record) { record.level }) == [
    log::Debug,
    log::Debug,
    log::Warn,
    log::Error,
  ]
  assert messages(records) == ["a", "b", "c", "d"]
}
```

## Fields

### `field`

```cheby
pub fn field<T: Show>(name: String, value: T) -> Field
```

Returns a field whose value is the text `show` gives for `value`. A `String` is kept as it is. The JSON line writes it as a string, also when `value` is a number: [`int`](#int), [`float`](#float) and [`bool`](#bool) keep the kind (D-1618).

```cheby
test "field keeps the text of a value" {
  assert log::field_value(log::field("host", "localhost")) == "localhost"
  assert log::field_value(log::field("left", Some(3))) == "Some(3)"
  assert log::field("n", "1") == log::field("n", 1)
  assert log::field("n", 1) != log::int("n", 1)
}
```

### `int`

```cheby
pub fn int(name: String, value: Int) -> Field
```

Returns a field whose value is the integer `value`. The JSON line writes it as a number.

### `float`

```cheby
pub fn float(name: String, value: Float) -> Field
```

Returns a field whose value is the float `value`. The JSON line writes it as `std::json` writes a `Float`, so NaN and the infinities are `null`.

### `bool`

```cheby
pub fn bool(name: String, value: Bool) -> Field
```

Returns a field whose value is `value`. The text line writes `True` or `False`, and the JSON line `true` or `false`.

```cheby
test "typed fields keep their kind" {
  assert log::field_value(log::int("port", 8080)) == "8080"
  assert log::field_value(log::float("ratio", 0.5)) == "0.5"
  assert log::field_value(log::bool("ready", True)) == "True"
  assert log::int("port", 8080) == log::int("port", 8080)
  assert log::int("port", 8080) != log::int("port", 8081)
}
```

### `inspect`

```cheby
pub fn inspect<T>(name: String, value: T) -> Field
```

Returns a field whose value is the debug text of `value` (§3.13), for a value whose type has no `show`, such as a record or a `Map`. The JSON line writes it as a string (D-1618).

```cheby
test "inspect keeps the debug text of any value" {
  assert log::field_value(log::inspect("pair", (1, "a"))) == "(1, \"a\")"
  assert log::inspect("text", "a") == log::field("text", "\"a\"")
}
```

### `field_name`

```cheby
pub fn field_name(entry: Field) -> String
```

Returns the name of `entry`.

### `field_value`

```cheby
pub fn field_value(entry: Field) -> String
```

Returns the value of `entry` as text: the text of a [`field`](#field) or an [`inspect`](#inspect) field, and `show` of an `Int`, a `Float` or a `Bool`. It is the text that the text line writes, before any quoting. A handler that needs the kind too writes the field with [`json_line`](#json_line).

```cheby
test "a field is read by name and value" {
  let entry = log::int("port", 8080)
  assert log::field_name(entry) == "port"
  assert log::field_value(entry) == "8080"
}

test "a field prints as the text line writes it" {
  let port = log::int("port", 8080)
  let host = log::field("host", "my host")
  assert "{port:?}" == "port=8080"
  assert "{host:?}" == "host=\"my host\""
}
```

### `with_fields`

```cheby
pub fn with_fields<R>(fields: List<Field>, body: fn() -> R) -> R
```

Calls `body` on the calling fiber with `fields` added after the scoped fields it already has, and returns what `body` returns. Every record logged while `body` runs carries them before the call's own fields, also in the fibers spawned meanwhile. When `body` returns or unwinds, the fiber's scoped fields are again what they were. It is written with `use` (§5.10, D-1619).

```cheby
test "scoped fields come first, and a later field of one name wins in place" {
  let (_, records) = log::capture(fn() {
    use <- log::with_fields([log::field("request", "a17"), log::int("try", 1)])
    use <- log::with_fields([log::field("user", "ada")])
    log::info_with("done", [log::int("try", 2), log::bool("ok", True)])
  })
  let assert [record] = records
  assert record.fields == [
    log::field("request", "a17"),
    log::int("try", 2),
    log::field("user", "ada"),
    log::bool("ok", True),
  ]
}

test "a spawned fiber inherits the scoped fields" {
  let (_, records) = log::capture(fn() {
    use <- log::with_fields([log::field("request", "a17")])
    use owner <- fiber::scope()
    fiber::join(fiber::spawn(owner, fn() { log::info("in the fiber") }))
  })
  let assert [record] = records
  assert list::map(record.fields, log::field_name) == ["request"]
}

test "scoped fields end with their body" {
  let (_, records) = log::capture(fn() {
    log::with_fields([log::field("request", "a17")], fn() { log::info("inside") })
    log::info("outside")
  })
  let assert [_, outside] = records
  assert outside.fields == []
}
```

## Records

### `text_line`

```cheby
pub fn text_line(record: Record) -> String
```

Returns [the text line](#the-text-line) of `record`, without a line ending. It writes the record's fields as they are, also when a record built by hand repeats a name (D-1623).

```cheby
test "text_line writes a record for people" {
  let record = sample("listening", [log::int("port", 8080), log::field("host", "my host")])
  let line = "2026-11-17T10:00:00.5Z INFO src/main.cheby:12:3: listening port=8080 host=\"my host\""
  assert log::text_line(record) == line
}

test "text_line quotes a name or a value that a reader could not split" {
  let fields = [
    log::field("empty", ""),
    log::field("pair", "a=b"),
    log::field("said", "say \"hi\""),
    log::field("slash", "a\\b"),
    log::field("lines", "a\nb"),
    log::field("my name", "é"),
    log::field("braces", "\{x}"),
  ]
  let line = log::text_line(sample("m", fields))
  let tail =
    r#": m empty="" pair="a=b" said="say \"hi\"" slash="a\\b" lines="a\nb" "my name"=é braces={x}"#
  assert string::ends_with(line, tail)
}

test "text_line keeps a message on one line" {
  let line = log::text_line(sample("a\nb\tc \"q\" \\ \u{1b}", []))
  assert string::ends_with(line, r#": a\nb\tc "q" \ \u{1b}"#)
  assert list::length(string::lines(line)) == 1
}
```

### `json_line`

```cheby
pub fn json_line(record: Record) -> String
```

Returns [the JSON line](#the-json-line) of `record`, without a line ending. It writes the record's fields as they are, so a record built by hand that repeats a name gives an object that repeats a key (D-1623).

```cheby
test "json_line writes five keys, with the fields nested" {
  let record = sample("listening", [log::int("port", 8080), log::field("host", "my host")])
  let line =
    r#"{"time":"2026-11-17T10:00:00.5Z","level":"info","message":"listening","location":"src/main.cheby:12:3","fields":{"port":8080,"host":"my host"}}"#
  assert log::json_line(record) == line
  assert string::ends_with(log::json_line(sample("m", [])), r#""fields":{}}"#)
}

test "json_line writes each kind of field as JSON does" {
  let fields = [
    log::float("nan", float::nan),
    log::float("half", 0.5),
    log::bool("ok", False),
    log::field("text", "a\"b\n"),
    log::field("five", 5),
    log::inspect("pair", (1, "a")),
  ]
  let line = log::json_line(sample("say \"hi\"", fields))
  let tail =
    r#""fields":{"nan":null,"half":0.5,"ok":false,"text":"a\"b\n","five":"5","pair":"(1, \"a\")"}}"#
  assert string::ends_with(line, tail)
  assert string::contains(line, r#""message":"say \"hi\"""#)
}
```

### `capture`

```cheby
pub fn capture<R>(body: fn() -> R) -> (R, List<Record>)
```

Calls `body` on the calling fiber with a collecting logger in context, and returns what `body` returns with the records logged until then, at every level. Nothing is written. It is for tests (D-1626).

1. The records are those of `body` and of the fibers that inherited the logger, in the order they arrived. One fiber's records keep their order, and the order between fibers that log at the same time is not specified.
2. `capture` does not wait for fibers. A fiber that outlives `body` keeps the collecting logger, and what it logs after `body` returned goes nowhere.
3. A `capture` inside `body` takes the records logged inside it, and the outer one does not see them.
4. A panic of `body` passes through, and its records are lost.
5. A record carries its real `timestamp` and `location`, so a test compares a record's `level`, `message` and `fields`, or provides a fixed clock.

- **Cost:** O(n) in the number of records. Informative.

```cheby
test "capture returns the body's result and its records" {
  use <- context::provide(time::clock, time::fixed_clock(opening))
  let (outcome, records) = log::capture(fn() {
    log::debug("first")
    log::error("second")
    7
  })
  assert outcome == 7
  let assert [first, second] = records
  assert first.level == log::Debug
  assert first.timestamp == opening
  assert second.message == "second"
}

test "capture collects what a scoped fiber logs" {
  let (outcome, records) = log::capture(fn() {
    use owner <- fiber::scope()
    fiber::join(fiber::spawn(owner, fn() { log::info("from the fiber") }))
    log::info("after the join")
  })
  assert outcome == Ok(Nil)
  assert messages(records) == ["from the fiber", "after the join"]
}

test "captures nest, and the inner one takes its records" {
  let ((_, inner), outer) = log::capture(fn() {
    log::info("outer")
    log::capture(fn() { log::info("inner") })
  })
  assert messages(inner) == ["inner"]
  assert messages(outer) == ["outer"]
}

test "a panic of the body passes through capture" {
  let outcome = {
    use _ <- fiber::scope()
    log::capture(fn() {
      log::info("before")
      panic as "boom"
    })
  }
  let assert Err(failure) = outcome
  assert fiber::message(failure) == "boom"
}
```

## The text line

A record's text line has this form, on one line (D-1621):

```text
<time> <LEVEL> <path>:<line>:<column>: <message> <name>=<value> <name>=<value>
```

- `<time>` is `time::show` of the record's `timestamp`: RFC 3339 in UTC with `Z`, and a fraction of a second only when it is not zero ([`time::show`](time.md#show)).
- `<LEVEL>` is `DEBUG`, `INFO`, `WARN` or `ERROR`.
- `<path>:<line>:<column>` is the record's `location`, as a panic report writes one.
- `<message>` is the message with each control character replaced: a line feed by `\n`, a carriage return by `\r`, a tab by `\t`, and any other character of U+0000 to U+001F and U+007F to U+009F by `\u{…}` with its code point in lower-case hexadecimal digits without leading zeros. Nothing else changes, so a record is always one line and text from outside cannot start a second one. The line is for people: a message that holds ` name=value` reads like a field.
- Each field follows as one space, its name, `=` and its value, in the record's order. A name or a value is written bare when it is not empty and holds no space, `=`, `"`, `\` or control character. Otherwise it is written between `"`, with `"` as `\"`, `\` as `\\` and each control character as in the message. A field's value is its [`field_value`](#field_value).
- A record without fields ends after the message.

## The JSON line

A record's JSON line is one JSON object on one line, with no spaces between its parts and always these five keys in this order (D-1622):

```json
{"time":"2026-11-17T10:00:00.5Z","level":"info","message":"listening","location":"src/main.cheby:12:3","fields":{"port":8080,"host":"my host"}}
```

- `time` is `time::show` of the record's `timestamp`, `level` is [`show`](#show) of its level, and `location` is `<path>:<line>:<column>`.
- `fields` is an object with one member per field in the record's order, and `{}` when there are none. Fields are nested so that no field can take the name of one of the five keys.
- A [`field`](#field) and an [`inspect`](#inspect) field are strings. An [`int`](#int) field is a number with all its digits. A [`float`](#float) field is what `json::to_string` writes for a `Float`, with `null` for NaN and the infinities. A [`bool`](#bool) field is `true` or `false`.
- Strings are escaped as `json::to_string` escapes them ([`std::json`](json.md)).
