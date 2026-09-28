---
status: accepted
date: 2026-09-28
log: D-017
---

# No mutable escape hatch

Cheby has no mutable cells, mutable arrays or similar escape hatches. Long-lived state lives in fibers as recursive loops that carry state, and Perceus reuse gives in-place updates for uniquely owned values. This keeps the immutability guarantee absolute, but some algorithms (for example, dense array updates on shared data) may be slower. It can be revisited if benchmarks demand.
