---
status: accepted
date: 2026-09-28
log: D-029
---

# Accept reference-cycle leaks through runtime objects in v1

Immutable data cannot form cycles, but mutable runtime objects such as channels and fibers can, and reference counting cannot collect them. v1 accepts these leaks, documents them, and reports them in debug mode. A cycle collector limited to runtime objects may be added later without language changes. This keeps the v1 runtime simple, at the cost of possible leaks in long-running programs with cyclic channel topologies.
