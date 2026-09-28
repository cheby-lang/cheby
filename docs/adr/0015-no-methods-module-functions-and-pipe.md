---
status: superseded by ADR-0023
date: 2026-09-28
log: D-032
---

# No methods: module functions and the pipe operator

Cheby has no methods and no `impl` blocks, despite its Rust-inspired syntax. All operations are module functions, called qualified (`list.len(xs)`) or chained with `|>`. Structural interfaces (ADR-0010) are satisfied by same-named functions in the type's own module.

## Consequences

- There is no `x.method()` call syntax. Dot syntax is only for module access and record fields.
- A type from another package cannot be made to satisfy a new interface after the fact, because only its own module counts. Wrapper types are the workaround, as in Go.
