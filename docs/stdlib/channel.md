# `std::channel`

Typed, bounded channels between fibers ([§10.5](../spec/10-concurrency.md#105-channels), D-013, D-054). A channel is a FIFO queue with a capacity, reached through two kinds of handle. In the form `cheby doc` prints:

```cheby
pub type Sender<T>

pub type Receiver<T>
```

- **Handles.** `Sender<T>` sends values of type `T` and `Receiver<T>` receives them. Both are handles (§9.5): a `Sender` prints as `<Sender>` and a `Receiver` as `<Receiver>`, `==` and hashing panic when they reach one, and no constant may hold one (§3.13, §4.5, D-233, D-647). They are ordinary immutable values otherwise: they go into records, variants and lists, and through other channels. Every copy of a handle is the same handle.
- **Closing.** There is no `close`. A channel closes for its receivers when its last `Sender` is dropped, and for its senders when its last `Receiver` is dropped. Handles are released right after their last use (§10.5.3, D-054, D-087, D-205).
- **Release of buffered values.** When the last `Receiver` is dropped, the values still in the buffer are released at once, on the fiber that dropped it, although senders may still hold the channel (D-670).
- **Drop functions.** The drop functions of `Sender` and `Receiver` close the channel and wake the fibers waiting on it. They never block and never suspend (D-171, D-672).
- **Order.** Values are received in the order they were enqueued, or handed off for capacity 0. So the values that one fiber sends arrive in the order it sent them. The order of sends from different fibers that overlap in time is unspecified, and so is which of several waiting receivers gets a value (§10.5, D-672).
- **Sharing.** A sent value becomes shared, with everything it reaches (§9.4, D-072).
- **Waiting forever.** A `send` or `receive` that no other fiber will ever complete waits until its fiber is cancelled. When the program deadlocks as a whole, it ends with status 103, which is not a panic (the Deadlock bullet of [`std::fiber`](fiber.md), §10.4, D-724).
- **Not waiting.** `send` and `receive` may wait. [`send_now`](#send_now) and [`receive_now`](#receive_now) never wait, and neither is a suspension point, so a loop that only polls keeps its thread, and on JS it starves the other fibers (§10.5, D-185, D-1099). A selector with a timer arm of zero is not a substitute, because it takes a ready value only at random ([`selector::after`](selector.md#after), D-1079).
- **Kernels.** `new`, `send` and `receive` are kernels that receive the running fiber's context (`@std_context`), and their costs are required (D-300, D-692). `fold`, `try_fold`, `to_list` and `send_all` are written in Cheby over `receive` and `send` (D-1100), and `send_now` and `receive_now` over private kernels (D-1099).
- **On JS.** Channels close as deterministically as on native, through handle reference counting, which covers every tier-2 handle and every value that holds one (D-100, ADR-0028, D-647).

## Building

### `new`

```cheby
pub fn new<T>(capacity: Int) -> (Sender<T>, Receiver<T>)
```

Returns the two ends of a new channel that buffers up to `capacity` values. Capacity 0 is a synchronous handoff: a send completes only when a receiver takes the value (D-054). The element type is usually written with a turbofish, as in `channel::new::<Job>(16)`.

- **Clamps:** `capacity`: a negative capacity counts as 0 (D-298, D-672).
- **Cost:** O(1), required. The buffer's memory grows with the values it holds, not with `capacity`. Informative.

```cheby
test "a channel delivers values in the order they were sent" {
  let (tx, rx) = channel::new::<Int>(2)
  let assert Ok(Nil) = channel::send(tx, 1)
  let assert Ok(Nil) = channel::send(tx, 2)
  assert channel::receive(rx) == Ok(1)
  assert channel::receive(rx) == Ok(2)
  assert channel::receive(rx) == Err(Nil)
}

test "a negative capacity counts as 0" {
  let capacity = -1
  let (tx, rx) = channel::new::<Int>(capacity)
  // Nobody receives, so a send on a capacity-0 channel waits.
  let attempt = fiber::timeout(duration::milliseconds(10), fn() { channel::send(tx, 1) })
  assert attempt == Err(fiber::TimedOut)
  // The cancelled send delivered nothing, and the only `Sender` is gone.
  assert channel::receive(rx) == Err(Nil)
}

test "handles print as placeholders" {
  let (tx, rx) = channel::new::<Int>(0)
  assert "{tx:?} {rx:?}" == "<Sender> <Receiver>"
}
```

## Sending and receiving

### `send`

```cheby
pub fn send<T>(sender: Sender<T>, value: T) -> Result<Nil, T>
```

Puts `value` into the channel and returns `Ok(Nil)` once it is enqueued, or, for capacity 0, once a receiver or a waiting selector has taken it (§10.5.1, §10.6, D-087, D-179, D-206). While the buffer is full, or for capacity 0 while nobody takes the value, the calling fiber waits.

- **Fails:** `Err(value)`, giving the value back, when every `Receiver` of the channel has been dropped, whether before the call or while the fiber waited.
- A `send` cancelled while it waits has not enqueued `value`. Unwinding releases it, and no receiver ever gets it (D-102, D-670).
- **Cost:** O(1) when it does not wait, plus marking `value` as shared (§9.4). Required.

```cheby
test "a capacity-0 send completes when a receiver takes the value" {
  let (tx, rx) = channel::new::<String>(0)
  let outcome = {
    use owner <- fiber::scope()
    let sender = fiber::spawn(owner, fn() { channel::send(tx, "hello") })
    let received = channel::receive(rx)
    (received, fiber::join(sender))
  }
  assert outcome == Ok((Ok("hello"), Ok(Nil)))
}

test "send gives the value back once every Receiver is gone" {
  let (tx, _) = channel::new::<String>(4)
  assert channel::send(tx, "lost") == Err("lost")
}

test "a blocked send fails when the last Receiver goes" {
  let (tx, rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    let sender = fiber::spawn(owner, fn() { channel::send(tx, 9) })
    // Give the sender time to block, then drop the only `Receiver`.
    selector::new()
    |> selector::after(_, duration::milliseconds(10), fn() { Nil })
    |> selector::select
    let _ = rx
    fiber::join(sender)
  }
  assert outcome == Ok(Err(9))
}
```

### `receive`

```cheby
pub fn receive<T>(receiver: Receiver<T>) -> Result<T, Nil>
```

Takes the next value from the channel and returns it in `Ok` (§10.5.2, D-054, D-179). While the channel is empty and some `Sender` still exists, the calling fiber waits. Values buffered before the last `Sender` was dropped are still received.

- **Fails:** `Err(Nil)` once the channel is empty and every `Sender` has been dropped.
- A `receive` cancelled while it waits takes nothing, so no value is lost (D-670).
- **Cost:** O(1) when it does not wait. Required.

```cheby
test "receive drains the buffer before it reports the closing" {
  let (tx, rx) = channel::new::<String>(2)
  let assert Ok(Nil) = channel::send(tx, "last words")
  // `tx` is not used again, so the channel is closed now.
  assert channel::receive(rx) == Ok("last words")
  assert channel::receive(rx) == Err(Nil)
}

test "values still buffered are released with the last Receiver" {
  let (inner_tx, inner_rx) = channel::new::<Int>(0)
  let (spare_tx, _) = channel::new::<Int>(0)
  let (tx, rx) = channel::new::<channel::Sender<Int>>(1)
  let assert Ok(Nil) = channel::send(tx, inner_tx)
  // Dropping the only `Receiver` releases the buffered `inner_tx`,
  // although `tx` is still alive.
  let _ = rx
  assert channel::receive(inner_rx) == Err(Nil)
  let assert Err(_) = channel::send(tx, spare_tx)
}
```

A channel whose buffer holds one of its own `Sender`s is a reference cycle, and leaks once nothing else reaches it (§9.6, D-029). Releasing the buffer with the last `Receiver` breaks the cycle in the common case where the receivers go first.

## Draining and filling

These functions are written in Cheby over `receive` and `send`, and wait as those do (D-1100). A cancellation while one of them waits unwinds as usual, and what it has received is lost. `to_list` and `send_all` call no function value, so a tail call to them keeps its caller's frame for a deadlock report, and `fold` and `try_fold` call `f` and keep their tail calls (the Deadlock bullet of [`std::fiber`](fiber.md), D-1059).

### `fold`

```cheby
pub fn fold<T, A>(receiver: Receiver<T>, initial: A, f: fn(A, T) -> A) -> A
```

Receives until the channel is closed and empty, and folds each value into the accumulator in the order received, as [`list::fold`](list.md#fold) does: `f` gets the accumulator and the value, and returns the next accumulator. It returns the last accumulator, which is `initial` when the channel is already closed and empty. `f` is called on the calling fiber.

- **Cost:** O(n) in the values received, plus the calls of `f` and the waits. Informative.

```cheby
test "fold receives until the channel is closed" {
  let (tx, rx) = channel::new::<Int>(3)
  let assert Ok(Nil) = channel::send(tx, 1)
  let assert Ok(Nil) = channel::send(tx, 2)
  let assert Ok(Nil) = channel::send(tx, 3)
  // `tx` is not used again, so the channel is closed now.
  assert channel::fold(rx, [], fn(seen, n) { [n, ..seen] }) == [3, 2, 1]
}
```

### `try_fold`

```cheby
pub fn try_fold<T, A, E>(
  receiver: Receiver<T>,
  initial: A,
  f: fn(A, T) -> Result<A, E>,
) -> Result<A, E>
```

Receives and folds as [`fold`](#fold) does, but `f` returns a `Result` (D-1080). The first `Err` stops it, and `try_fold` returns that `Err`. The values still in the channel stay there, for a later `receive`. When the channel is closed and empty, it returns `Ok` of the last accumulator, which is `Ok(initial)` when nothing was received.

- **Cost:** as `fold`. Informative.

```cheby
test "try_fold stops at the first Err and leaves the rest" {
  let (tx, rx) = channel::new::<Int>(4)
  let assert Ok(Nil) = channel::send_all(tx, [1, 2, -3, 4])
  let total = channel::try_fold(rx, 0, fn(sum, n) {
    case n < 0 {
      True => Err(n)
      False => Ok(sum + n)
    }
  })
  assert total == Err(-3)
  assert channel::receive(rx) == Ok(4)
  assert channel::receive(rx) == Err(Nil)
}

test "try_fold of a closed channel is Ok of the last accumulator" {
  let (tx, rx) = channel::new::<Int>(2)
  let assert Ok(Nil) = channel::send_all(tx, [5, 6])
  let total: Result<Int, Nil> = channel::try_fold(rx, 0, fn(sum, n) { Ok(sum + n) })
  assert total == Ok(11)
}
```

### `to_list`

```cheby
pub fn to_list<T>(receiver: Receiver<T>) -> List<T>
```

Returns every value received until the channel is closed and empty, in the order received. It is `fold` with [`list::push`](list.md#push), and it waits until every `Sender` is gone, so the senders must drop theirs for it to return.

- **Cost:** O(n) in the values received, amortized, plus the waits. Informative.

```cheby
test "to_list drains a closed channel" {
  let (tx, rx) = channel::new::<String>(2)
  let assert Ok(Nil) = channel::send_all(tx, ["a", "b"])
  assert channel::to_list(rx) == ["a", "b"]
}

test "send_all and to_list pass a list between fibers" {
  let (tx, rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    fiber::spawn(owner, fn() { channel::send_all(tx, [1, 2, 3, 4]) })
    channel::to_list(rx)
  }
  assert outcome == Ok([1, 2, 3, 4])
}
```

### `send_all`

```cheby
pub fn send_all<T>(sender: Sender<T>, xs: List<T>) -> Result<Nil, List<T>>
```

Sends the elements of `xs` in order, each with [`send`](#send), and returns `Ok(Nil)` once the last one is enqueued or taken.

- **Fails:** once every `Receiver` is gone, `Err` with the elements it could not send, starting with the refused one, so no value is lost (D-179).
- **Cost:** O(n) in the length of `xs`, plus the waits. Informative.

```cheby
test "send_all gives back what it could not send" {
  let (tx, _) = channel::new::<Int>(4)
  assert channel::send_all(tx, [1, 2, 3]) == Err([1, 2, 3])
  assert channel::send_all(tx, []) == Ok(Nil)
}

test "send_all stops when the last Receiver goes" {
  let (tx, rx) = channel::new::<Int>(0)
  let outcome = {
    use owner <- fiber::scope()
    let sender = fiber::spawn(owner, fn() { channel::send_all(tx, [1, 2, 3]) })
    let first = channel::receive(rx)
    // `rx` is not used again, so the sender's next send fails.
    (first, fiber::join(sender))
  }
  assert outcome == Ok((Ok(1), Err([2, 3])))
}
```

## Not waiting

`send_now` and `receive_now` do what `send` and `receive` do when those would not wait, and otherwise return at once (D-1099). Neither waits or is a suspension point, so a fiber that polls in a loop keeps its thread, and on JS it starves the other fibers until it suspends elsewhere ([§10.5](../spec/10-concurrency.md#105-channels), D-185). Across several channels, [`selector::select_now`](selector.md#select_now) takes from whichever is ready. The names follow D-1050's suffix `_now`. Both are written in Cheby over private kernels, implemented after M3, and their tests are pending for M4 (D-741).

### `Refused`

```cheby
pub exposed type Refused<T> {
  Full(T),
  Closed(T),
}
```

Why [`send_now`](#send_now) did not send, with the value given back either way (D-179, D-1099). `Full(value)`: the buffer had no room, or, for capacity 0, no receiver or selector was waiting. `Closed(value)`: every `Receiver` is gone. It is `exposed`, so other packages match `channel::Full(v)` and `channel::Closed(v)` and compare with `==`.

### `send_now`

```cheby
pub fn send_now<T>(sender: Sender<T>, value: T) -> Result<Nil, Refused<T>>
```

Enqueues `value` and returns `Ok(Nil)` when the buffer has room, or hands it to a receiver or a selector that is waiting now. It never waits (D-1099).

- On capacity 0 it succeeds only when it wins a waiting receiver or selector, as `send` claims one (D-206), and it never waits for a selector's choice.
- **Fails:** `Err(Closed(value))` when every `Receiver` is gone, which is checked first, and otherwise `Err(Full(value))` when the value could not be enqueued or handed off.
- **Cost:** O(1), plus marking `value` as shared when it is sent (§9.4). Informative.

```cheby
test "send_now reports a full buffer and gives the value back" {
  let (tx, rx) = channel::new::<Int>(1)
  assert channel::send_now(tx, 1) == Ok(Nil)
  assert channel::send_now(tx, 2) == Err(channel::Full(2))
  assert channel::receive_now(rx) == Some(Ok(1))
  assert channel::send_now(tx, 3) == Ok(Nil)
}

test "send_now without receivers is Closed" {
  let (tx, _) = channel::new::<Int>(0)
  assert channel::send_now(tx, 3) == Err(channel::Closed(3))
}

test "send_now on capacity 0 needs a waiting receiver" {
  let (tx, rx) = channel::new::<Int>(0)
  assert channel::send_now(tx, 1) == Err(channel::Full(1))
  let outcome = {
    use owner <- fiber::scope()
    let receiver = fiber::spawn(owner, fn() { channel::receive(rx) })
    // Offer the value until the receiver waits, pausing between offers.
    fn offer(value) {
      case channel::send_now(tx, value) {
        Ok(Nil) => Nil
        Err(_) => {
          selector::new()
          |> selector::after(_, duration::milliseconds(1), fn() { Nil })
          |> selector::select
          offer(value)
        }
      }
    }
    offer(2)
    fiber::join(receiver)
  }
  assert outcome == Ok(Ok(2))
}
```

### `receive_now`

```cheby
pub fn receive_now<T>(receiver: Receiver<T>) -> Option<Result<T, Nil>>
```

Returns `Some` of what [`receive`](#receive) would return when `receive` would not wait, and `None` otherwise (D-1099). So `Some(Ok(value))` takes the next value, from the buffer or from a sender waiting on a capacity-0 channel, `Some(Err(Nil))` means that the channel is closed and empty, and `None` that it is empty and some `Sender` still exists. It never waits.

- **Cost:** O(1). Informative.

```cheby
test "receive_now never waits" {
  let (tx, rx) = channel::new::<String>(1)
  assert channel::receive_now(rx) == None
  let assert Ok(Nil) = channel::send(tx, "ready")
  assert channel::receive_now(rx) == Some(Ok("ready"))
  // `tx` is not used again, so the channel is closed and empty now.
  assert channel::receive_now(rx) == Some(Err(Nil))
}
```
