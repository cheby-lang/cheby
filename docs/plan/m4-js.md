# M4: JavaScript backend

Goal: every example runs on Node from `cheby build --target js`, with the same goldens as native (D-006, D-099, step 4; spec §1.1 principle 5).

Prerequisites: M3 done. JS parts of the tier-1 to tier-3 stdlib specified.

Detail level: workstreams with outcomes. This file is refined into tasks when M4 starts.

## Workstreams

### 4.1 Emitter

- MIR to ES modules, one per Cheby module, plus copied `.mjs` foreign files at the same relative paths (D-074, D-208).
- Value representations of §12.3.3: `Int` as a number checked to ±(2⁵³−1) (D-037), `I64` and `U64` as `BigInt` (D-106), `F32` with `Math.fround`, strings as JS strings (D-038).
- Self tail calls as loops, other tail calls through trampolines (D-015, ADR-0006).
- Source maps (D-074).
- `@external(js, …)` imports and `@async` externals as suspension points (D-188).

### 4.2 Suspension analysis

- Per-module summaries of direct suspension and call edges, computed with the MIR (D-258, ADR-0048).
- A link step that computes transitive suspension, conservative for calls through function values and `dyn` values (ADR-0005).
- JS code generated and cached per function, keyed on its IR hash and whether it suspends, so an edit regenerates only functions whose status changed.
- No entry yield checks on JS (D-185, ADR-0035).

### 4.3 JS runtime (`runtime-js/`)

- Collection and string kernels (D-274), equality, hashing and debug printing with the same type descriptors as native (D-237).
- Cooperative fiber scheduler over generators, scopes, cancellation at real suspension points, channels, selectors and timers (§12.3.1, D-232).
- Handle reference counting where Perceus would count, and the handle flag for values that may contain handles (D-100, D-172, ADR-0028).
- Callbacks from foreign code as new detached fibers (D-191).
- IO through host asynchronous APIs, on Node first, then Deno, Bun and browsers (D-074).

### 4.4 Cross-target conformance

- The harness runs every example on native and on Node and compares both against the same goldens.
- The differential fuzzer compares native and JS output (testing.md §8), with the documented JS-only panics of §12.4 as the only allowed difference.

## Exit criteria

- Every example passes on Node with the native goldens, except where the JS panics of §12.4 are the expected result and the example records it.
- Channel closing and handle drops happen at the same points on both targets, checked by dedicated UI run tests.
- An edit to one function body regenerates only the JS of functions whose suspension status or IR changed.
- Deno and Bun run the conformance suite in CI. Browser support is checked with a smoke test.
