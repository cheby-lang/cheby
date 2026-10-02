---
status: accepted
date: 2026-10-02
log: D-251
---

# Cross-module optimization only in release builds

ADR-0011 left monomorphization "to be added later" with no limit, and that is where Rust's compile cost lives. Debug builds therefore do no specialization and no inlining across modules, so they stay purely local. Release builds may specialize generic functions and inline across modules, but only within a size budget, as Go does, using the small bodies exported in interface artifacts (D-249). This refines ADR-0011 without superseding it: generic code is still compiled once by default.

## Considered options

- Never optimize across modules: slower code from the uniform representation forever.
- Whole-program, LTO-style optimization in release builds: best code, but release builds would scale like Rust's.
