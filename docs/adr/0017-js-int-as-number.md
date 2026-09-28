---
status: accepted
date: 2026-09-28
log: D-037
---

# Int on JS is a JS number

On the JS target, `Int` is a plain JS number. Overflow checks trigger outside the safe-integer range (±2^53), while native checks at 64 bits. Both targets panic instead of producing wrong results, but the same program can panic on JS where it succeeds natively.

## Considered options

- BigInt masked to 64 bits: identical semantics, but roughly 10-50× slower.
- A 32-bit pair representation: identical semantics, also slow.
