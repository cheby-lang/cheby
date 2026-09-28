---
status: accepted
date: 2026-09-29
log: D-205
---

# Bindings release their value after the last use

Channels close when their last handle is dropped (D-054), so the moment a handle is released is observable: pipelines like the prime sieve shut down only because each fiber's handles go away as soon as they are no longer needed. We guarantee, on every target, that a binding stops holding its value right after its last use, or at the start of a branch that does not use it, rather than at the end of its scope. This is what Perceus does already on native, and ADR-0028 already makes handle counting deterministic on JS; this decision makes the timing itself normative.

## Considered options

- Release at the end of the enclosing scope (Rust's drop order): easy to explain, but a tail-recursive fiber would keep old handles alive, and the sieve would never shut down.
- Leave the timing unspecified: programs that work on one implementation would hang on another.

## Consequences

- Optimizations may not extend a handle's lifetime past its last use, even to avoid a reference-count operation.
- Keeping a handle alive on purpose needs a later use of it, which tests sometimes do explicitly.
