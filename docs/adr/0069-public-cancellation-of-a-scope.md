---
status: accepted
date: 2026-10-23
log: D-1060
---

# Public cancellation of a scope

`fiber::cancel(owner)` cancels every fiber spawned into `owner` so far, and every scope nested in them, with `owner` as the target. It does not cancel the scope's body and does not mark the scope as cancelled: fibers spawned afterwards run normally, and the scope returns `Ok` with the body's value once every fiber has ended. Any fiber that holds the `Scope` may call it, and a caller that is one of the fibers it reaches unwinds at once. `fiber::race` is written in Cheby on top of it.

```cheby
test "race returns the first value and cancels the rest" {
  let (keep, never) = channel::new::<Int>(1)
  let slow = fn() {
    let _ = channel::receive(never)
    2
  }
  assert fiber::race([fn() { 1 }, slow]) == Ok(Some(1))
  // The slow contender was cancelled and released `never`.
  assert channel::send(keep, 1) == Err(1)
}
```

Until now, a scope's fibers stopped only when a member panicked or a timeout expired (D-040, D-667, ADR-0019). Cancellation was a consequence of failure. "First answer wins", "stop at the first `Err`" and "stop the workers now" could only be written with a fake panic, whose `Err(Panic)` then meant success. D-1012 had already defined the walk, as a package-visible `fiber::cancel_members` for `http::serve_on`. This decision makes it public, so that programs can write their own policies.

Cancellation itself does not change. It is still delivered at the next suspension point and still cannot be caught (D-102, ADR-0029). A panic of a fiber after the cancel has reached it is not recorded, as for a timeout.

## Considered options

- Keep the walk package-visible, and ship only `race` and a parallel `try_map` over it. The public surface stays smallest, but users cannot write policies such as "the first two of three".
- A `cancel` that also refuses later spawns, as Trio's cancel scope does. It breaks worker pools that cancel one round and start the next.
- A `cancel` that also cancels the body. `scope`'s `Result<T, Panic>` would need a new `Err` case.
- A second join that returns `None` for a failed fiber. It is safe after a cancel, but it is a second way to join. It can still be added later.

## Consequences

- Joining a fiber that `cancel` reached follows D-668: a joiner that is not itself being cancelled, such as the body, panics with `joined fiber failed`. D-668's remark that a member "is always" being cancelled no longer holds, and §10.3 says so. `race` never joins.
- §10.7 lists `fiber::cancel` as a third cause of cancellation, beside a failing scope and a timeout.
- `http::serve_on` calls `fiber::cancel` where D-1012 called `cancel_members`, and a closed listener ends it with `Ok(Nil)` (D-1064). S5 builds the public kernel instead of the private one.
- The kernel needs a JS binding in M4.
