---
status: accepted
date: 2026-09-28
log: D-061
---

# No methods: module functions and the pipe operator

Cheby has no methods and no `impl` blocks, despite its Rust-inspired syntax. All operations are module functions, called with a Rust-style path (`list::len(xs)`) or chained with `|>` (`xs |> list::len`). Structural interfaces (ADR-0010) are satisfied by same-named functions in the type's own module. This restates ADR-0015 with `::` module access (D-060) instead of `.`.

## Consequences

- There is no `x.method()` call syntax. `.` is only for record fields, and `::` is only for module paths.
- A type from another package cannot be made to satisfy a new interface after the fact, because only its own module counts. Wrapper types are the workaround, as in Go.
