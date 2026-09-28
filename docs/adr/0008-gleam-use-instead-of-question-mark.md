---
status: accepted
date: 2026-09-28
log: D-018
---

# Gleam-style use instead of Rust's ? operator

Errors are values of `Result`/`Option` types. Early return on errors is written with Gleam-style `use` syntax, not Rust's `?` operator, which the user dislikes. `panic` is for unrecoverable errors and kills only the current fiber. This is a deliberate departure from the otherwise Rust-inspired syntax, so nobody should "fix" it by adding `?`.
