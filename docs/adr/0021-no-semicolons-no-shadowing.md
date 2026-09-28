---
status: accepted
date: 2026-09-28
log: D-051
---

# No semicolons and no shadowing

Statements are newline-terminated with no semicolons, and a `let` may never shadow an earlier binding with the same name. Rust and Gleam both allow shadowing, and it is the usual substitute for mutation (`let x = x + 1`). Cheby deliberately forbids it, so within its scope a name always refers to one value. The cost is that step-by-step transformations need distinct names (`raw`, `parsed`, `validated`), and common Gleam patterns such as `use x <- result.try(parse(x))` must pick a new name.
