# `std::time`

Reading the clocks, and waiting. The wall clock says what time it is, and the monotonic clock measures how long something took. Spans of time are [`duration::Duration`](duration.md) values, and readings of the monotonic clock are [`instant::Instant`](time/instant.md) values, from the submodule `std::time::instant` (D-1057). In the form `cheby doc` prints:

```cheby
pub type Timestamp

pub type Clock
```

- **`Timestamp`.** A point in time on the wall clock, as a span since the Unix epoch, 1970-01-01T00:00:00Z, in UTC, ignoring leap seconds as Unix time does. It is opaque: a record written in Cheby that holds a `Duration` in its one field, `since_epoch`, so it has the same nanosecond resolution and range, and points before 1970 are negative spans (D-650, D-1018, D-1181). It satisfies `Compare` and `Show` through [`compare`](#compare) and [`show`](#show), so `a < b` and `"{stamp}"` work. `==` and hashing are the built-in structural ones, which compare points because the representation is unique.
- **`instant::Instant`.** A reading of the monotonic clock. The type and its pure functions, `add`, `compare`, `between` and `from_origin`, live in the submodule [`std::time::instant`](time/instant.md), so that `Instant` can satisfy `Compare` while `time::compare` stays `Timestamp`'s: a module satisfies an interface for one type only (D-045, D-1057). The functions that read the clock or wait stay here, [`monotonic`](#monotonic), [`elapsed`](#elapsed), [`until`](#until) and [`sleep_until`](#sleep_until), because the key [`clock`](#clock) lives here and imports are acyclic (D-145). `std::time` imports `std::time::instant`, and a program that names the type writes `import std::time::instant` too.
- **`Clock`.** Where [`now`](#now), [`monotonic`](#monotonic), [`elapsed`](#elapsed) and [`until`](#until) read the time: the system's clocks, a fixed point, or two functions. They read the clock that the key [`clock`](#clock) holds in the calling fiber's context, which is the system clock unless a program provides another, for example in a test ([`std::context`](context.md), D-1046, D-1047, D-1057). Timers never read it, so [`sleep`](#sleep), [`sleep_until`](#sleep_until), `fiber::timeout`, a selector's timers and the deadlock check use the runtime's monotonic clock whatever clock is provided. `Clock` is opaque. The system clock and fixed clocks compare structurally, and `==` panics on a custom clock, as on any function value (§3.13, D-233). Clocks in context belong to tier 4 and are implemented in M4, with `std::context` (D-1049).
- **Written in Cheby.** Only [`now`](#now), [`monotonic`](#monotonic), [`sleep`](#sleep) and [`sleep_until`](#sleep_until) reach kernels, and `now` and `monotonic` do so only through the system clock. Every other function is written in Cheby over `std::duration` and `std::time::instant`, so it runs in constants and gives the same result on every target (D-090, D-232, D-650, D-991): `const launch: time::Timestamp = time::from_unix(duration::seconds(1_700_000_000))` works. Reaching `now`, `monotonic`, `elapsed`, `until`, `sleep` or `sleep_until` while computing a constant is E0077 (D-537), because they read the context or a kernel (D-1045, D-1057). The clocks of [Clocks in context](#clocks-in-context) are Cheby without kernels, so they can be built in constants (D-1047).
- **Kernels.** `now` and `monotonic` are ordinary kernels that return seconds and nanoseconds, bound as `@external(native, "cheby_time_sys_now")` and `@external(native, "cheby_time_sys_monotonic")`, and `monotonic` builds its reading with `instant::from_origin` (D-1057). `sleep` and `sleep_until` are Cheby over the `@std_context` kernel `sys_sleep`, which arms a timer on the runtime's timer heap and suspends the fiber (D-680, D-692, D-983, D-985). Each kernel declares `@external(js, "./time.mjs", "sys_<name>")`, and the submodule has no kernel of its own (D-661, D-986, D-1057).
- **Clock sources.** The wall clock is the system's real-time clock. The monotonic clock is the clock the runtime's timers use, read from one origin per process that is taken when the runtime first needs it. It is not a runtime's own timer epoch, so instants read in different runtimes of one process, such as the REPL's and a test's, are comparable (D-690, D-1021).
- **Timing.** The value of a clock reading depends on when it is taken, so it is outside D-232's promise of equal results. Everything computed from readings is inside it.
- **On JS.** OQ-P14, with M4. The wall clock comes from `Date.now()`, with millisecond resolution, and the monotonic clock from `performance.now()`, which browsers coarsen. Programs must not expect a resolution finer than a millisecond on JS (D-232).

## The wall clock

### `now`

```cheby
pub fn now() -> Timestamp
```

Returns the current time on the wall clock of the clock in context, which is the system's real-time clock by default (D-1047). The system's clock can jump forwards or backwards when someone sets it, so differences between two `now` readings are not durations: measure those with [`monotonic`](#monotonic).

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

The span between two points is `time::to_unix(later) - time::to_unix(earlier)`. There is no `time::between`: `to_unix` says the same, and `between` is `instant::between` (D-1057).

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
  assert time::parse("2026-10-21t12:30:00.25z") ==
    Ok(time::from_unix(duration::milliseconds(1_792_585_800_250)))
  assert time::parse("2026-10-21") == Err(Nil)
  assert time::parse("2026-10-21 12:30:00Z") == Err(Nil)
  assert time::parse("2026-02-30T00:00:00Z") == Err(Nil)
}
```

## The monotonic clock

### `monotonic`

```cheby
pub fn monotonic() -> instant::Instant
```

Returns the current reading of the monotonic clock of the clock in context (D-1047, D-1057). By default it is the clock the runtime's timers use, so a timer of one second and the time `elapsed` measures across it agree (D-682, D-1021). Readings are compared with `<` and the other comparison operators, and measured with `instant::between` ([`std::time::instant`](time/instant.md), D-1057).

- **Cost:** O(1), plus the system call. Informative.

The test of [`sleep`](#sleep) uses it.

### `elapsed`

```cheby
pub fn elapsed(start: instant::Instant) -> duration::Duration
```

Returns the span from `start` to now on the monotonic clock of the clock in context, as `instant::between(start, monotonic())` (D-1047, D-1057). On the system clock it is never negative for an instant read earlier in the same process.

- **Cost:** O(1), plus the system call. Informative.

```cheby
test "elapsed is never negative" {
  let start = time::monotonic()
  assert time::elapsed(start) >= duration::zero
}
```

### `until`

```cheby
pub fn until(deadline: instant::Instant) -> duration::Duration
```

Returns the span from now on the monotonic clock of the clock in context to `deadline`, as `instant::between(monotonic(), deadline)`. It is negative once `deadline` has passed (D-1057). A deadline computed once with `instant::add` and checked with `<`, `until` and [`sleep_until`](#sleep_until) bounds a loop and keeps its partial results, where a `fiber::timeout` would release them by unwinding.

- **Cost:** O(1), plus the system call. Informative.

```cheby
test "until counts down to a deadline" {
  let start = time::monotonic()
  let deadline = instant::add(start, duration::hours(1))
  assert time::until(deadline) <= duration::hours(1)
  assert time::until(deadline) > duration::minutes(59)
  assert time::until(start) <= duration::zero
}
```

_Example_, not a `test` because how much it finds depends on the machine. It keeps every item it finished before the deadline:

```cheby
fn solve_until(deadline: instant::Instant, next: Int, found: List<Int>) -> List<Int> {
  case time::monotonic() < deadline {
    True => solve_until(deadline, next + 1, [solve(next), ..found])
    False => found
  }
}
```

## Waiting

### `sleep`

```cheby
pub fn sleep(span: duration::Duration)
```

Suspends the calling fiber for at least `span` on the runtime's timer heap, while other fibers run. The clock in context does not change it (D-1047). A span of zero or less returns at once (D-652). A sleeping fiber can be cancelled, for example by a `fiber::timeout` around it, and then unwinds at once.

- **Endless sleeps.** A span longer than 2⁶³ − 1 nanoseconds never ends, as for every waiting function of the standard library (D-652, D-891). It arms no timer, so it is not pending work: a program whose fibers all wait, one of them in such a sleep, is deadlocked, and the report lists that fiber as `blocked in time::sleep` (§10.4, D-858, D-1058).
- **Other sleeps.** A sleep up to that cap arms a timer and counts as pending work, so a program whose only fiber sleeps is not deadlocked, and it never appears in a deadlock report (D-724, D-984, D-1020, D-1058).
- **Tail calls.** A tail call to `sleep` keeps its caller's frame, so a report locates the fiber at the caller and not at `<unknown location>` (D-861, D-1059).
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

_Example_, not a `test` because it ends the program with a deadlock report. 3,000,000 hours is past the cap, so nothing can ever wake the root fiber:

```cheby
fn main() {
  time::sleep(duration::hours(3_000_000))
}
```

It exits with status 103, and the report's second line is `  blocked in time::sleep at <location>`, the location of the call in `main` (D-1058, D-1059).

### `sleep_until`

```cheby
pub fn sleep_until(deadline: instant::Instant)
```

Suspends the calling fiber until the runtime's monotonic clock reaches `deadline`, while other fibers run. A deadline that has passed returns at once (D-652, D-1057). As a timer, `sleep_until` never reads the clock in context, so a deadline built from the readings of a provided clock means nothing to it (D-1047, D-1057). Otherwise it follows [`sleep`](#sleep): it can be cancelled, a deadline more than 2⁶³ − 1 nanoseconds away is an endless sleep that a deadlock report lists as `blocked in time::sleep` (D-1058), and a tail call to it keeps its caller's frame (D-1059).

- **Cost:** O(log t) in the number of pending timers, plus the wait. Informative.

```cheby
test "sleep_until waits until its deadline" {
  let deadline = instant::add(time::monotonic(), duration::milliseconds(10))
  time::sleep_until(deadline)
  assert time::until(deadline) <= duration::zero
}

test "sleep_until returns at once for a deadline that has passed" {
  let outcome = fiber::timeout(duration::seconds(10), fn() { time::sleep_until(time::monotonic()) })
  assert outcome == Ok(Nil)
}
```

_Example_, not a `test` because it writes to standard output. Each deadline is the one before plus the period, so the ticks do not drift however long each line takes to write:

```cheby
fn tick(next: instant::Instant, period: duration::Duration, left: Int) {
  case left > 0 {
    True => {
      time::sleep_until(next)
      io::println("tick")
      tick(instant::add(next, period), period, left - 1)
    }
    False => Nil
  }
}
```

## Clocks in context

A program replaces the clock for the code it calls by providing one for the key [`clock`](#clock), with [`context::provide`](context.md#provide). Everything spawned meanwhile inherits it ([§10.11](../spec/10-concurrency.md#1011-context-values), D-1044, D-1047).

### `clock`

```cheby
pub const clock: context::Key<Clock> = context::key(system_clock())
```

The key of the clock that [`now`](#now), [`monotonic`](#monotonic), [`elapsed`](#elapsed) and [`until`](#until) read. Its default is the system clock.

```cheby
test "a fixed clock fixes now" {
  let launch = time::from_unix(duration::seconds(1_700_000_000))
  use <- context::provide(time::clock, time::fixed_clock(launch))
  assert time::now() == launch
  assert time::elapsed(time::monotonic()) == duration::zero
}

test "a clock in context does not change sleep" {
  use <- context::provide(time::clock, time::fixed_clock(time::from_unix(duration::zero)))
  let outcome = fiber::timeout(duration::milliseconds(10), fn() { time::sleep(duration::hours(1)) })
  assert outcome == Err(fiber::TimedOut)
}
```

### `system_clock`

```cheby
pub fn system_clock() -> Clock
```

Returns the clock that reads the system's real-time clock for `now` and the runtime's monotonic clock for `monotonic`, the sources described at the top of this page (D-1021).

### `fixed_clock`

```cheby
pub fn fixed_clock(at: Timestamp) -> Clock
```

Returns a clock whose `now` is always `at`, and whose `monotonic` is always `instant::from_origin(duration::zero)`. So `elapsed` of any reading taken under it is zero (D-1047, D-1057).

The test of [`clock`](#clock) uses it.

### `custom_clock`

```cheby
pub fn custom_clock(read_now: fn() -> Timestamp, read_monotonic: fn() -> instant::Instant) -> Clock
```

Returns a clock that calls `read_now` for the wall clock and `read_monotonic` for the monotonic clock. The parameters are not named `now` and `monotonic`, because a parameter must not share a name with a function of its own module (D-062, D-1057). It builds its readings with [`instant::from_origin`](time/instant.md#from_origin), which replaces D-1047's `time::instant_at` (D-1057). A test can move time forward with functions that ask a fiber holding the current time over a channel (D-1047).

```cheby
test "a custom clock calls its functions" {
  let start = instant::from_origin(duration::seconds(5))
  let fake = time::custom_clock(fn() { time::from_unix(duration::zero) }, fn() {
    instant::from_origin(duration::seconds(8))
  })
  use <- context::provide(time::clock, fake)
  assert time::now() == time::from_unix(duration::zero)
  assert time::elapsed(start) == duration::seconds(3)
  assert time::until(instant::from_origin(duration::seconds(10))) == duration::seconds(2)
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-1018, D-1019):

- calendar fields (year, month, day, weekday), and building a point from them;
- local time, time zones and their database;
- formats other than RFC 3339, and parsing HTTP dates;
- the CPU time a program or a fiber used.
