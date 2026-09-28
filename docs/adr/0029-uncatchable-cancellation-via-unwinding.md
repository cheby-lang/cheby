---
status: accepted
date: 2026-09-28
log: D-102
---

# Uncatchable cancellation via unwinding

When a scope cancels its fibers (a sibling panicked or the scope exited), each fiber is cancelled at its next suspension point or function-entry yield check (D-028). It then unwinds exactly like a panic (ADR-0026), releasing references and running handle drops (ADR-0028). Fibers cannot catch or ignore cancellation, and a cancelled blocking operation never returns to its caller.

## Considered options

- Cancellation as a value returned from `send`/`receive`: explicit, but fibers could ignore it and every blocking call site would need to handle it.

## Consequences

- Cleanup logic can only live in handle drop functions and scope boundaries, not in user "finally" code.
- Cancellation latency is bounded by the gap between yield checks, which is at most one function body without calls.
