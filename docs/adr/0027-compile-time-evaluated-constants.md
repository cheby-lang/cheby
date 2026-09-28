---
status: accepted
date: 2026-09-28
log: D-090
---

# Compile-time evaluated constants, not full comptime

Module-level `const` initializers may call ordinary Cheby functions. The compiler runs them with the Cranelift JIT at build time and bakes the results into the output. This is deliberately narrower than Zig's comptime: types are not values and there are no comptime parameters, so generics and interfaces stay the only abstraction mechanism and D-057 (no metaprogramming in v1) still holds.

## Considered options

- Constants limited to literals and constructors (Gleam): simplest, but no lookup tables or precomputed data.
- Full Zig comptime: powerful, but a second generics system and a large tooling cost.

## Consequences

- Effects are untracked (D-016), so IO, fiber and channel operations are detected at run time during evaluation and become compile errors.
- For JS builds, values are computed natively. An `Int` outside ±2^53 in a constant is a compile error (D-037).
- The LSP evaluates constants too, so evaluation needs time and memory limits.
- Constants are immutable and shared, and never refcounted.
