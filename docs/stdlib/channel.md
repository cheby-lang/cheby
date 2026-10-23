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
- **Not waiting.** Every `send` and `receive` may wait. Operations that never wait are planned as `send_now` and `receive_now` (D-1050). A selector with a timer arm of zero is not a substitute, because it takes a ready value only at random ([`selector::after`](selector.md#after), D-1079).
- **Kernels.** `new`, `send` and `receive` are kernels that receive the running fiber's context (`@std_context`), and their costs are required (D-300, D-692).
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
