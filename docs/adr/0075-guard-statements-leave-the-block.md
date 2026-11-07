---
status: accepted
date: 2026-11-07
log: D-1340
---

# Guard statements leave the block

`guard c else { … }` and `let guard p = e else { … }` run the rest of the block when the condition or pattern holds, and otherwise the `else` block, whose value is the value of the enclosing block. They are the counterparts of `assert` and `let assert`, which panic. Cheby still has no `return`.

```cheby
fn withdraw(id: AccountId, amount: Int) -> Result<Account, BankError> {
  guard amount > 0 else {
    Err(InvalidAmount)
  }
  let guard Some(account) = find(id) else {
    Err(NotFound)
  }
  Ok(debit(account, amount))
}
```

A guard is a `case` with the rest of the block in an arm, as a `use` is a call with the rest of the block in a callback. So a statement in a block exits early with no jump, and branching stays `case` (ADR-0001). A guard does not leave the function. It leaves its block, and only a function's body block makes that the function's result.

Early exit used to need `case`, which nests, or `use`, which costs a closure at every step. The measurement took 50M iterations on an Apple M3, with a release `cheby` of 2026-11-04. JIT and AOT agreed.

| Shape                                 | `case` | `use` | Slowdown |
| ------------------------------------- | ------ | ----- | -------- |
| Two `Bool` guards, a `guard` function | 85 ms  | 2.9 s | 34x      |
| One `result::try` step                | 0.49 s | 1.2 s | 2.5x     |

## Considered options

- `return`, local to the nearest function. The rest of a block after a `use` is a closure, and most error handling lives there. There `return` would mean "leave the callback", so inside `list::try_fold` it would go on folding. It reads as a function exit, and it would not be one.
- `return`, non-local. It would unwind through `fiber::scope` and `with_open`, and a stored or spawned closure outlives the frame it would return to. Kotlin allows this only for inline lambdas, and Cheby has none.
- Plain `let p = e else { … }`. It is shorter and needs no keyword, but the exit sits at the end of the line, so the line reads as an ordinary `let`, and a `Bool` needs a second form.
- `when c else { … }` and `when c => …`. They need no keyword, but the first reads poorly and the second states what fails where `let guard` states what holds, so adjacent lines would flip meaning.
- `assert c else { … }`. It needs no keyword, but `assert` means a bug.
- `guard let p = e` and `guard p = e`. The first reverses `let assert`'s order for no gain. The second needs lookahead, since `[a, ..]` is no expression, and it drops `let`.
- A case arm `_ => ..` that continues with the block. It batches guards and reaches the failed value, but its bindings would outlive their braces.
- `bool::guard` in std alone. It needs no language change, but it costs a closure per guard and builds its value even when the guard holds.

## Consequences

- A guard inside a `case` arm's block or a `use` body exits only that block or callback. People used to Swift's `guard` may expect more. A mismatch usually shows up as a type error, since the `else` block must have the block's type.
- `guard` and `else` are keywords, and `else` means nothing outside a guard.
- There are now four ways to exit early: `case`, `use`, `guard` and `let guard`. `use` stays the form for a failure that passes on the value it holds, as `result::try` does.
- The `else` block has no binder for the failed value. One can be added later without breaking code.
