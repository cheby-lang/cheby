---
status: accepted
date: 2026-09-28
log: D-039
---

# The built-in List is an RRB-tree vector

The core `List` type is a persistent RRB-tree (relaxed radix balanced) vector, not a singly linked list as in Gleam, Erlang or Haskell. It gives near-constant indexing, efficient concatenation and slicing, and structural sharing, and it is the only built-in sequence type.

## Consequences

- Recursing over a list by head/tail costs O(log n) per step instead of O(1), unless the implementation adds a head/tail view optimization.
- Perceus in-place reuse works per tree node rather than per cons cell.
