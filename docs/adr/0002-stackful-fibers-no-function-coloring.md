---
status: accepted
date: 2026-09-28
log: D-003
---

# Stackful fibers, no function coloring

Concurrency uses stackful fibers in the style of goroutines. There are no stackless coroutines and no async/await, so any function can block without changing its signature. The costs are per-fiber stacks (growable or segmented) in the native runtime, and a non-trivial compilation strategy on JS, which has no native stackful coroutines (see ADR-0005).
