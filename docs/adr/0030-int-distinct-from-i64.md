---
status: accepted
date: 2026-09-28
log: D-106
---

# Int is distinct from I64

`Int` and `I64` are separate types even though both are 64-bit on native. `Int` is the everyday integer and is a JS number on the JS target, with overflow checks at ±2^53 (ADR-0017). Sized types (`I8`…`U64`, `F32`) exist for exact FFI and binary work, so they behave identically on every target: `I64` and `U64` are `BigInt`s on JS. There is no `F64`, since `Float` already is one.

## Consequences

- Converting between `Int` and `I64` is explicit, and `I64 -> Int` returns `Result` (D-085), because it can fail on JS.
- `I64`/`U64` arithmetic is slow on JS, which is the price of exactness.
