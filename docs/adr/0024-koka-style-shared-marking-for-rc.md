---
status: accepted
date: 2026-09-28
log: D-072
---

# Koka-style shared marking for thread-safe RC

With M:N parallel fibers (ADR-0004), reference counts on values reachable from several threads must be updated atomically. Cheby marks a value (and what it reaches) as shared when it is sent on a channel, captured by a spawned fiber or stored in a global. Only shared values pay for atomic RC, and fiber-local values use plain increments. Borrowing is inferred by the compiler, with no user annotations.

## Consequences

- Marking a large structure as shared costs a traversal, once per structure.
- Perceus in-place reuse only applies to unshared, uniquely owned values.
