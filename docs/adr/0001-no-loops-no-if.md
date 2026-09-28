---
status: accepted
date: 2026-09-28
log: D-002
---

# No loops and no if

Cheby has no loop constructs and no `if` expression. Iteration is written as recursion, which relies on guaranteed tail calls (ADR-0006), and all branching uses `case`. This follows Gleam: one way to express each thing, and a natural fit for immutable data. The trade-off is that the language is less familiar to people coming from Rust, Go or C++, and some code is more verbose.
