---
status: accepted
date: 2026-10-11
log: D-655
---

# The seeded random generator is specified bit for bit

`std::random`'s seeded `Generator` is SplitMix64 over one 64-bit state. The specification fixes the seeding, the step, the mapping of outputs to integer ranges and the mapping to fractions. A seed therefore gives the same numbers on native and JS, and from 1.0 on with every toolchain.

Replaying a simulation or a failing test is the main use of a seed, and a replay must work across toolchain versions. D-232 already requires native and JS to agree. SplitMix64 needs only `U64` addition, multiplication, xor and right shifts, which are exact on JS too (ADR-0030). It has one word of state, passes BigCrush, and is what Java's `SplittableRandom` uses.

## Considered options

- An unspecified algorithm, as Rust's `rand::StdRng`: free to improve later, but seeded output changes silently between versions, and D-232 still forces native and JS to agree within one version.
- A specified algorithm, as Go's named generators in `math/rand/v2` and Java's `SplittableRandom`: chosen, because replays and test seeds keep working.
- PCG64 instead of SplitMix64: better statistics on paper, but it needs a 128-bit multiplication.
- `xoshiro256**` instead of SplitMix64: four words of state, and it needs rotations. In Cheby, `<<` panics on overflow (§5.4.6), so a rotation needs masking.
- Lemire's multiply-and-shift mapping to ranges: fast, but it needs a 128-bit product.

## Consequences

- `Generator` can never change. A better algorithm later is a new type or a new function.
- Ranges use the rejection method: an output below `(2⁶⁴ − span) % span` is drawn again. A draw may take more than one step, with probability below one half per step.
- The host functions apply the same mapping to fresh host bits (D-656), so the mapping is written once in Cheby for both targets.
- The seeded functions are plain Cheby, so they run in constants.
- The module's spec tests pin known outputs for fixed seeds, which any implementation must reproduce.
