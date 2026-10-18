---
status: accepted
date: 2026-10-11
log: D-665
---

# Scope bodies and timeouts run on the calling fiber

`scope` calls its body, and `timeout` calls its function, as ordinary calls on the calling fiber, with that fiber's stack. Only the `spawn` functions start fibers. A timeout's scope has no `Scope` value, so its only member is the function. Values that these functions capture or return stay on the calling fiber and are not marked shared.

§10.3 already says that a scope cancels "the scope's body" with its members. That only makes sense if the body's frames belong to the caller.

## Considered options

- A child fiber per `timeout`, as a Go goroutine would be: a deep recursion inside `timeout` in `main` would get a fiber's 8 MiB instead of the root's 64 MiB (D-660). Each call would cost a fiber and a marking traversal of everything the function captures and returns. Unwinding would stop at a fiber boundary for timeouts but mid-stack for scopes.
- A child fiber for the scope body as well: the same costs for every `scope`, which often wraps all of `main`.

## Consequences

- A panic or cancellation in a body unwinds the caller's own frames and stops at the `scope` or `timeout` call where the rules say so (D-667, D-675).
- A scope frame stays on the calling fiber's stack until every member has ended (D-666), so the body and its members are nested in one place.
- Like any scope, `timeout` returns only after its function has stopped. Work the runtime cannot interrupt, such as file IO, delays it (D-670, D-671).
- The body runs under JIT frames only, with no Rust frame between it and the caller. The fused scope sequence of ADR-0060 provides this.
