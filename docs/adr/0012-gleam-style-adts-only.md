---
status: accepted
date: 2026-09-28
log: D-022
---

# Gleam-style ADTs as the only user-defined data type

Every user-defined type is an algebraic data type: an enum whose variants carry named record fields. A "struct" is simply a type with one variant. There is no separate `struct` keyword, despite the Rust-inspired syntax. This gives one data-definition construct, and pattern matching with exhaustiveness checking works the same way everywhere.
