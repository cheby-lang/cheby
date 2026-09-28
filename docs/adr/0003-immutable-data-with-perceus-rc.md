---
status: accepted
date: 2026-09-28
log: D-004, D-005
---

# Immutable data with Perceus reference counting

All values are immutable, and persistent data structures share structure. Memory is managed with Perceus-style precise reference counting (as in Koka), not a tracing GC. Perceus reuse turns updates of uniquely owned values into in-place writes ("functional but in-place").

## Consequences

- Reference counting cannot collect cycles. Immutable data cannot form cycles by construction, but mutable runtime objects such as channels and fibers can, so they need a rule of their own.
- With parallel fibers (ADR-0004), shared objects need atomic RC operations.
