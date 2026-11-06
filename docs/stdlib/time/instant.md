# `std::time::instant`

Readings of the monotonic clock, and the arithmetic on them (D-1057). The clock itself is read in [`std::time`](../time.md), with `time::monotonic`, and measured with `time::elapsed` and `time::until`. A timer waits for a reading with `time::sleep_until`. In the form `cheby doc` prints:

```cheby
pub type Instant
```

`Instant` is a reading of the monotonic clock, as a span since an origin that the runtime fixes once per process and does not reveal. Readings of the system clock never go backwards within one process, and the clock does not change when someone sets the wall clock. An `Instant` means nothing in another process or on another machine (D-1018, D-1021).

- **Representation.** `Instant` is opaque plain data: a record written in Cheby that holds a [`duration::Duration`](../duration.md) in its one field, `since_origin`, so it has the same nanosecond resolution and range. `==` and hashing are the built-in structural ones (D-650, D-1018, D-1181).
- **Comparing.** It satisfies `Compare` through [`compare`](#compare), so `<`, `<=`, `>` and `>=` work on instants ([§8.7](../../spec/08-interfaces.md#87-operator-interfaces), D-045). That is why the type has a module of its own: `time::compare` is `Timestamp`'s, and a module satisfies an interface for one type only (D-1057).
- **No `Show`.** Its origin is hidden, so an `Instant` has no text that means anything. Debug printing is the built-in one for records, such as `Instant { since_origin: Duration { seconds: 90, nanoseconds: 0 } }` for the reading 90 seconds after the origin (§3.13, D-1057, D-1181).
- **Written in Cheby.** The module imports only `std::duration`. None of its functions has a kernel or reads the context, so all of them run in constants and give the same result on every target (D-090, D-232, D-650, D-1057): `const origin: instant::Instant = instant::from_origin(duration::zero)` works. The functions that read the clock or wait are in `std::time`, which imports this module, because the clock in context, `time::clock`, is declared there and imports are acyclic (D-145, D-1047).
- **Importing it.** A program writes `import std::time::instant`, which binds the name `instant` (D-351). `std::time` alone does not bring it into scope.

## Building readings

### `from_origin`

```cheby
pub fn from_origin(span: duration::Duration) -> Instant
```

Returns the reading `span` after the monotonic clock's origin, so that a custom clock (`time::custom_clock`) can build readings, and `time::monotonic` builds the system clock's readings with it. The system clock's readings count from the same origin, which a program does not know, so a span between a built reading and one of the system clock is defined but means nothing (D-1047, D-1057). It replaces D-1047's `time::instant_at`.

- **Cost:** O(1). Informative.

```cheby
test "from_origin counts from the origin" {
  let first = instant::from_origin(duration::seconds(1))
  let second = instant::from_origin(duration::seconds(3))
  assert instant::between(first, second) == duration::seconds(2)
  assert first == instant::from_origin(duration::milliseconds(1_000))
}
```

### `add`

```cheby
pub fn add(at: Instant, span: duration::Duration) -> Instant
```

Returns the reading `span` after `at`, or before it when `span` is negative. A deadline is `instant::add(time::monotonic(), limit)`. It does not make `Instant` satisfy `Add`, whose two arguments have the same type ([§8.7](../../spec/08-interfaces.md#87-operator-interfaces)), so `at + span` does not compile.

- **Panics:** where `Duration` arithmetic overflows, as `time::add` does (D-025, D-650, D-1057).
- **Cost:** O(1). Informative.

```cheby
test "add moves a reading by a span" {
  let start = instant::from_origin(duration::seconds(10))
  assert instant::add(start, duration::seconds(5)) == instant::from_origin(duration::seconds(15))
  assert instant::add(start, duration::milliseconds(-500)) ==
    instant::from_origin(duration::milliseconds(9_500))
}
```

## Comparing and measuring

### `compare`

```cheby
pub fn compare(a: Instant, b: Instant) -> Order
```

Orders two readings by time, earlier first. It makes `Instant` satisfy `Compare` (§8.7), so `<`, `<=`, `>` and `>=` work (D-045, D-1057).

- **Cost:** O(1). Informative.

```cheby
test "instants compare with the operators" {
  let first = time::monotonic()
  let later = instant::add(first, duration::seconds(1))
  assert first < later
  assert instant::compare(later, later) == Equal
  assert instant::between(first, later) == duration::seconds(1)
}
```

### `between`

```cheby
pub fn between(earlier: Instant, later: Instant) -> duration::Duration
```

Returns the span from `earlier` to `later`, which is negative when `later` is the earlier reading (D-1018, D-1057). `time::elapsed(start)` is `between(start, time::monotonic())`, and `time::until(deadline)` is `between(time::monotonic(), deadline)`.

- **Cost:** O(1). Informative.

```cheby
test "between measures from the first reading to the second" {
  let first = instant::from_origin(duration::seconds(2))
  let second = instant::from_origin(duration::seconds(7))
  assert instant::between(first, second) == duration::seconds(5)
  assert instant::between(second, first) == -instant::between(first, second)
}
```
