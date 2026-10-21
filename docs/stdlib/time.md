# `std::time`

Reading the clocks, and waiting. The wall clock says what time it is, and the monotonic clock measures how long something took. Spans of time are [`duration::Duration`](duration.md) values. In the form `cheby doc` prints:

```cheby
pub type Timestamp

pub type Instant
```

- **`Timestamp`.** A point in time on the wall clock, as a span since the Unix epoch, 1970-01-01T00:00:00Z, in UTC, ignoring leap seconds as Unix time does. It is opaque: a record written in Cheby that holds a `Duration`, so it has the same nanosecond resolution and range, and points before 1970 are negative spans (D-650, D-1018). It satisfies `Compare` and `Show` through [`compare`](#compare) and [`show`](#show), so `a < b` and `"{stamp}"` work. `==` and hashing are the built-in structural ones, which compare points because the representation is unique.
- **`Instant`.** A reading of the monotonic clock, as a span since an origin that the runtime fixes once per process and does not reveal. Readings never go backwards within one process, and the clock does not change when someone sets the wall clock. An `Instant` means nothing in another process or on another machine. It is opaque plain data like `Timestamp`, with structural `==`, but satisfies neither `Compare` nor `Show`, since a module has one `compare` and one `show` (D-045). A program compares instants through [`between`](#between) and [`elapsed`](#elapsed) (D-1018).
- **Written in Cheby.** Only [`now`](#now), [`monotonic`](#monotonic) and [`sleep`](#sleep) reach kernels. Every other function is written in Cheby over `std::duration`, so it runs in constants and gives the same result on every target (D-090, D-232, D-650, D-991): `const launch: time::Timestamp = time::from_unix(duration::seconds(1_700_000_000))` works. Reaching `now`, `monotonic` or `sleep` while computing a constant is E0077 (D-537).
- **Kernels.** `now` and `monotonic` are ordinary kernels that return seconds and nanoseconds, bound as `@external(native, "cheby_time_sys_now")` and `@external(native, "cheby_time_sys_monotonic")`. `sleep` is Cheby over the `@std_context` kernel `sys_sleep`, which arms a timer on the runtime's timer heap and suspends the fiber (D-680, D-692, D-983, D-985). Each kernel declares `@external(js, "./time.mjs", "sys_<name>")` (D-661, D-986).
- **Clock sources.** The wall clock is the system's real-time clock. The monotonic clock is the clock the runtime's timers use, read from one origin per process that is taken when the runtime first needs it. It is not a runtime's own timer epoch, so `Instant`s read in different runtimes of one process, such as the REPL's and a test's, are comparable (D-690, D-1021).
- **Timing.** The value of a clock reading depends on when it is taken, so it is outside D-232's promise of equal results. Everything computed from readings is inside it.
- **On JS.** OQ-P14, with M4. The wall clock comes from `Date.now()`, with millisecond resolution, and the monotonic clock from `performance.now()`, which browsers coarsen. Programs must not expect a resolution finer than a millisecond on JS (D-232).

## The wall clock

### `now`

```cheby
pub fn now() -> Timestamp
```

Returns the current time on the wall clock. It can jump forwards or backwards when the system's clock is set, so differences between two `now` readings are not durations: measure those with [`monotonic`](#monotonic).

- **Cost:** O(1), plus the system call. Informative.

_Example_, not a `test` because its result depends on when it runs:

```cheby
fn log_line(message: String) -> String {
  let stamp = time::now()
  "{stamp} {message}"
}
```

### `from_unix`

```cheby
pub fn from_unix(since_epoch: duration::Duration) -> Timestamp
```

Returns the point `since_epoch` after the Unix epoch. A negative span gives a point before 1970.

- **Cost:** O(1). Informative.

```cheby
test "from_unix counts from the Unix epoch" {
  assert time::show(time::from_unix(duration::zero)) == "1970-01-01T00:00:00Z"
  assert time::show(time::from_unix(duration::seconds(946_684_800))) == "2000-01-01T00:00:00Z"
  assert time::show(time::from_unix(duration::seconds(-1))) == "1969-12-31T23:59:59Z"
}
```

### `to_unix`

```cheby
pub fn to_unix(stamp: Timestamp) -> duration::Duration
```

Returns the span from the Unix epoch to `stamp`, the inverse of [`from_unix`](#from_unix).

- **Cost:** O(1). Informative.

```cheby
test "to_unix is the inverse of from_unix" {
  let stamp = time::from_unix(duration::milliseconds(1_792_585_800_250))
  assert time::to_unix(stamp) == duration::milliseconds(1_792_585_800_250)
  assert duration::to_seconds(time::to_unix(stamp)) == 1_792_585_800
}
```

### `add`

```cheby
pub fn add(stamp: Timestamp, span: duration::Duration) -> Timestamp
```

Returns the point `span` after `stamp`, or before it when `span` is negative. It does not make `Timestamp` satisfy `Add`, whose two arguments have the same type ([§8.7](../spec/08-interfaces.md#87-operator-interfaces)), so `stamp + span` does not compile.

- **Panics:** where `Duration` arithmetic overflows (D-025, D-650).
- **Cost:** O(1). Informative.

```cheby
test "add moves a point by a span" {
  let start = time::from_unix(duration::seconds(946_684_800))
  assert time::show(time::add(start, duration::hours(36))) == "2000-01-02T12:00:00Z"
  assert time::show(time::add(start, duration::milliseconds(-500))) == "1999-12-31T23:59:59.5Z"
}
```

The span between two points is `time::to_unix(later) - time::to_unix(earlier)`.

### `compare`

```cheby
pub fn compare(a: Timestamp, b: Timestamp) -> Order
```

Orders two points by time, earlier first. It makes `Timestamp` satisfy `Compare` (§8.7), so `<`, `<=`, `>` and `>=` work.

- **Cost:** O(1). Informative.

```cheby
test "earlier points compare as less" {
  let first = time::from_unix(duration::seconds(10))
  let second = time::from_unix(duration::seconds(20))
  assert time::compare(first, second) == Less
  assert first < second
  assert time::compare(second, second) == Equal
}
```

## Text

A point is written in RFC 3339 form in UTC, and read in RFC 3339 form with any offset. It is one unambiguous text that sorts as it reads and that other systems parse, the same on every target. Tier 3 has no calendar fields, no other formats and no local time zones, since a zone database would differ between machines and targets (D-214, D-232, D-1019).

### `show`

```cheby
pub fn show(stamp: Timestamp) -> String
```

Makes `Timestamp` satisfy `Show` ([§8.8](../spec/08-interfaces.md#88-standard-interfaces)). It writes `stamp` in RFC 3339 form in UTC with `Z`, such as `2026-10-21T12:30:00Z`. A fraction of a second is written only when it is not zero, with up to nine digits and no trailing zeros, such as `2026-10-21T12:30:00.25Z`. Years from 0 to 9999 have four digits. Other years, which RFC 3339 cannot write, are written with a sign and at least five digits, as ISO 8601's expanded form does, such as `+10000-01-01T00:00:00Z`, and `parse` does not read them. Debug printing is the built-in one for records (§3.13), such as `Timestamp { since_epoch: Duration { seconds: 0, nanoseconds: 0 } }`.

- **Cost:** O(1). Informative.

```cheby
test "show writes RFC 3339 in UTC" {
  let stamp = time::from_unix(duration::milliseconds(1_792_585_800_250))
  assert time::show(stamp) == "2026-10-21T12:30:00.25Z"
  assert "{stamp}" == "2026-10-21T12:30:00.25Z"
}
```

### `parse`

```cheby
pub fn parse(text: String) -> Result<Timestamp, Nil>
```

Reads a point written in RFC 3339 form: `YYYY-MM-DDTHH:MM:SS`, an optional fraction of one to nine digits after a `.`, and either `Z` or an offset such as `+02:00`. `T` and `Z` may also be lower case. The offset is applied, so the result is the same point in UTC. Every text that `show` gives for a year from 0 to 9999 parses back to the same point.

- **Fails:** `Err(Nil)` for any other text, including a date without a time, a space instead of `T`, a fraction of more than nine digits, a date that does not exist such as `2026-02-30`, and a leap second `:60`, which Unix time cannot represent.
- **Cost:** O(n) in the length of `text`. Informative.

```cheby
test "parse reads RFC 3339 with any offset" {
  let assert Ok(stamp) = time::parse("2026-10-21T14:30:00+02:00")
  assert time::show(stamp) == "2026-10-21T12:30:00Z"
  assert time::parse("2026-10-21t12:30:00.25z") == Ok(time::from_unix(duration::milliseconds(1_792_585_800_250)))
  assert time::parse("2026-10-21") == Err(Nil)
  assert time::parse("2026-10-21 12:30:00Z") == Err(Nil)
  assert time::parse("2026-02-30T00:00:00Z") == Err(Nil)
}
```

## The monotonic clock

### `monotonic`

```cheby
pub fn monotonic() -> Instant
```

Returns the current reading of the monotonic clock. It is the clock the runtime's timers use, so a timer of one second and the time `elapsed` measures across it agree (D-682, D-1021).

- **Cost:** O(1), plus the system call. Informative.

The test of [`sleep`](#sleep) uses it.

### `elapsed`

```cheby
pub fn elapsed(start: Instant) -> duration::Duration
```

Returns the span from `start` to now on the monotonic clock. It is never negative for an `Instant` read earlier in the same process.

- **Cost:** O(1), plus the system call. Informative.

```cheby
test "elapsed is never negative" {
  let start = time::monotonic()
  assert time::elapsed(start) >= duration::zero
}
```

### `between`

```cheby
pub fn between(earlier: Instant, later: Instant) -> duration::Duration
```

Returns the span from `earlier` to `later`, which is negative when `later` was read first.

- **Cost:** O(1). Informative.

```cheby
test "between measures from the first reading to the second" {
  let first = time::monotonic()
  let second = time::monotonic()
  assert time::between(first, second) >= duration::zero
  assert time::between(second, first) == -time::between(first, second)
}
```

## Waiting

### `sleep`

```cheby
pub fn sleep(span: duration::Duration)
```

Suspends the calling fiber for at least `span` on the runtime's timer heap, while other fibers run. A span of zero or less returns at once, and one longer than 2⁶³ − 1 nanoseconds never ends, as for every waiting function of the standard library (D-652, D-891). A sleeping fiber can be cancelled, for example by a `fiber::timeout` around it, and then unwinds at once. It counts as pending work, so a program whose only fiber sleeps is not deadlocked, and it never appears in a deadlock report (D-724, D-984, D-1020).

- **Cost:** O(log t) in the number of pending timers, plus the wait. Informative.

```cheby
test "sleep waits at least its span" {
  let start = time::monotonic()
  time::sleep(duration::milliseconds(10))
  assert time::elapsed(start) >= duration::milliseconds(10)
}

test "a sleep is cancelled by a timeout" {
  let outcome = fiber::timeout(duration::milliseconds(10), fn() { time::sleep(duration::hours(1)) })
  assert outcome == Err(fiber::TimedOut)
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-1018, D-1019):

- calendar fields (year, month, day, weekday), and building a point from them;
- local time, time zones and their database;
- formats other than RFC 3339, and parsing HTTP dates;
- a monotonic deadline type, and comparing two `Instant`s directly;
- the CPU time a program or a fiber used.
