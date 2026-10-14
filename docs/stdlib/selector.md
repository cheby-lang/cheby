# `std::selector`

Waiting on several channels and timers at once ([§10.6](../spec/10-concurrency.md#106-selectors), D-041). A selector is built from **arms**, each of which maps what it waits for to one common result type `E`, and [`select`](#select) waits until an arm is ready and returns that arm's result. In the form `cheby doc` prints:

```cheby
pub type Selector<E>
```

- **Immutable.** A selector is a value. `receive` and `after` return a new selector with one more arm and leave their argument usable. The same selector can be passed to `select` any number of times, and each call waits anew (D-673).
- **Handle.** `Selector` is a handle, since it holds receivers and functions: it prints as `<Selector>`, `==` and hashing panic when they reach one, and no constant may hold one (§3.13, §4.5, D-233, D-647). A selector keeps its receivers, and so their channels, alive while it exists.
- **Written in Cheby.** `new`, `receive` and `after` are written in Cheby over private kernels that add an arm, since only a kernel can make a value of an external type (D-590). `select` is written in Cheby over a private kernel that waits and one that takes the value. Their costs are informative (D-675, D-695).
- **On JS.** Timer arms become ready only when the running fiber suspends (D-185, D-232). A selector is a handle there too, so it is reference-counted and releases its receivers as deterministically as on native (D-100, D-647).

## Building

### `new`

```cheby
pub fn new<E>() -> Selector<E>
```

Returns a selector with no arms. `E` is usually inferred from the arms added next.

- **Cost:** O(1). Informative.

```cheby
test "selectors print as a placeholder" {
  let waiting = selector::new::<Int>()
  assert "{waiting:?}" == "<Selector>"
}
```

### `receive`

```cheby
pub fn receive<T, E>(
  selector: Selector<E>,
  receiver: channel::Receiver<T>,
  map: fn(Result<T, Nil>) -> E,
) -> Selector<E>
```

Returns `selector` with an arm that receives from `receiver` and passes the result to `map`, as [`channel::receive`](channel.md#receive) would return it (D-180). The arm is **ready** when the channel has a buffered value, when a sender is waiting on a capacity-0 channel, or when the channel is closed and empty, in which case `map` gets `Err(Nil)` (D-180, D-206). A constructor can be the mapping function, as in `selector::receive(_, requests, FromCounter)`.

- **Cost:** O(1). Informative.

```cheby
test "select takes a value from a ready arm" {
  let (tx, rx) = channel::new::<Int>(1)
  let assert Ok(Nil) = channel::send(tx, 7)
  let got =
    selector::new()
    |> selector::receive(_, rx, fn(r) { r })
    |> selector::select
  assert got == Ok(7)
}

test "a closed channel's arm is ready with Err(Nil)" {
  let (_, rx) = channel::new::<Int>(0)
  let got =
    selector::new()
    |> selector::receive(_, rx, fn(r) { r })
    |> selector::select
  assert got == Err(Nil)
}
```

### `after`

```cheby
pub fn after<E>(selector: Selector<E>, wait: duration::Duration, map: fn() -> E) -> Selector<E>
```

Returns `selector` with a timer arm, which is ready once `wait` has passed since `select` started waiting (D-103, D-673). The clock starts anew in each `select` call, not when `after` is called, so a selector reused in a loop gives each wait its own timer. A `wait` of zero or less is ready at once (D-652). A selector may have several timer arms. A selector with only a timer arm is a sleep.

- **Cost:** O(1). Informative.

```cheby
test "a timer arm is ready once its time has passed" {
  let (keep, never) = channel::new::<Int>(0)
  let got =
    selector::new()
    |> selector::receive(_, never, fn(_) { "message" })
    |> selector::after(_, duration::milliseconds(10), fn() { "timer" })
    |> selector::select
  assert got == "timer"
  // `select` released the selector, which held the only `Receiver`.
  assert channel::send(keep, 1) == Err(1)
}

test "a timer of zero is ready at once" {
  let got =
    selector::new()
    |> selector::after(_, duration::milliseconds(0), fn() { 1 })
    |> selector::select
  assert got == 1
}
```

## Waiting

### `select`

```cheby
pub fn select<E>(selector: Selector<E>) -> E
```

Waits until at least one arm of `selector` is ready, takes exactly one value from exactly one ready arm, and returns that arm's mapped result (§10.6, D-041, D-673). It has one parameter, so it is piped bare: `|> selector::select` (D-244).

- **Choice.** When several arms are ready, `select` chooses one of them uniformly at random, so it favors none (D-041, D-673). Timer arms with the same earliest deadline are ready together, and `select` chooses among them the same way (D-791).
- **Handoff.** While `select` waits, it counts as a receiver on the channel of every receive arm. A sender on a capacity-0 channel hands its value to the waiting selector instead of blocking, and its `send` returns `Ok(Nil)` once the selector has chosen that arm. Values of the arms that were not chosen stay in their channels, and their senders keep waiting (D-206).
- **Mapping.** `select` calls the chosen arm's mapping function exactly once, on the calling fiber, after the value has been taken, and returns its result. Before that call, `select` releases its reference to the selector, and so to every receiver it holds (D-205, D-670). A panic in the mapping function is a panic of the calling fiber at that point, and the value taken is released by unwinding.
- **No arms.** A selector without arms is never ready, so `select` waits until the fiber is cancelled, and never returns (D-673). If the program then deadlocks as a whole, it ends with status 103 (the Deadlock bullet of [`std::fiber`](fiber.md), §10.4, D-724).
- A `select` cancelled while it waits takes no value from any arm (D-670).
- **Cost:** O(k) in the number of arms, plus the wait. Informative.

```cheby
test "a waiting selector takes a handoff on a capacity-0 channel" {
  let (tx, rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() { channel::send(tx, 3) })
    selector::new()
    |> selector::receive(_, rx, fn(r) { r })
    |> selector::select
  }
  assert outcome == Ok(Ok(3))
}

test "select chooses among ready arms without favoring one" {
  let (_, left) = channel::new::<Nil>(0)
  let (_, right) = channel::new::<Nil>(0)
  let choose =
    selector::new()
    |> selector::receive(_, left, fn(_) { True })
    |> selector::receive(_, right, fn(_) { False })
  fn count(remaining, lefts) {
    case remaining {
      0 => lefts
      _ => case selector::select(choose) {
        True => count(remaining - 1, lefts + 1)
        False => count(remaining - 1, lefts)
      }
    }
  }
  let lefts = count(200, 0)
  assert lefts > 0 && lefts < 200
}

test "a selector without arms waits until it is cancelled" {
  let waited = fiber::timeout(duration::milliseconds(10), fn() { selector::select(selector::new::<Nil>()) })
  assert waited == Err(fiber::TimedOut)
}
```

Both arms of the choice test are always ready, because their channels are closed, so each `select` picks either one with probability one half, and the test fails with probability 2⁻¹⁹⁹.
