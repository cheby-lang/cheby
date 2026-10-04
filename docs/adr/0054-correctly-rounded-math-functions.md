---
status: accepted
date: 2026-10-05
log: D-396
---

# Correctly rounded math functions

`float::pow`, `exp`, `ln`, `sin`, `cos`, `tan` and `atan2` return the correctly rounded result, the `Float` nearest to the exact value, on every target. They are computed by code shipped with the runtime, as the Unicode tables are (D-310), not by the host's math library. Host libraries differ between platforms and JS engines, so using them would give different results on different targets, which D-232 forbids. Correct rounding is the only rule that fixes one result without naming an algorithm, and correctly rounded implementations exist, such as CORE-MATH.

## Considered options

- The host's functions (`libm`, `Math.sin`): fast and small, but results differ between targets.
- No transcendental functions in tier 1: avoids the cost, but geometry, graphics and simulations need them.

## Consequences

- The functions are slower than the host's, and the JS runtime ships extra code for them.
- New functions such as `asin`, `log2` or the hyperbolic ones must also be correctly rounded when they are added.
