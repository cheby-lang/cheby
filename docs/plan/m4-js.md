# M4: JavaScript backend

Goal: every example runs on Node from `cheby build --target js`, with the same goldens as native (D-006, D-099, step 4; spec §1.1 principle 5).

Prerequisites: M3 done. JS parts of the tier-1 to tier-3 stdlib specified. OQ-P14 (file IO, standard input and `std::env` on JS hosts, including browsers) settled.

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
- Measure `std::json` on Node. Its parser works on UTF-8 bytes (D-900), so on JS each parsed string or number costs a UTF-8 decode through the package-visible `string::from_byte_range` (D-905), which `string.mjs` must export with one shared `TextDecoder("utf-8", { ignoreBOM: true })` so that a leading U+FEFF is kept as on native (D-366), and the text costs one encode through `bytes::from_string`. If those conversions dominate, give the byte steps JS kernels that work on the string directly. The writer converts nothing: it calls the package-visible `string::add_json_quoted` (D-903), which `string.mjs` must export with the other kernels. The parser finds the plain runs of its strings with the package-visible `bytes::json_plain_end` (D-904), which `bytes.mjs` must export.
- Cooperative fiber scheduler over generators, scopes, cancellation at real suspension points, channels, selectors and timers (§12.3.1, D-232).
- Handle reference counting where Perceus would count, and the handle flag for values that may contain handles (D-100, D-172, ADR-0028).
- Callbacks from foreign code as new detached fibers (D-191).
- IO through host asynchronous APIs, on Node first, then Deno, Bun and browsers (D-074). A standard stream that is closed or missing at start is a null device, and only failures of streams that were open at start end the program with status 1 (D-306, D-474).

- The `.mjs` files of tier 3, whose bindings M3 declares, with `@async` where the host API returns a `Promise`. Every std `@async` kernel resolves with the `(kind, message)` error pair instead of rejecting (D-986). `std::http` binds `fetch` and the host's server API (D-1003).

### 4.4 Cross-target conformance

- The harness runs every example on native and on Node and compares both against the same goldens.
- The differential fuzzer compares native and JS output (testing.md §8), with the documented JS-only panics of §12.4 as the only allowed difference.

### 4.5 The LSP core (M5 lane)

- The LSP core lands with M4, starting no earlier than M3's third checkpoint, because it sits on the driver's front-end APIs that M3's cache rewrites. Its protocol questions are settled in its own design step ([m5-tooling.md](m5-tooling.md), D-1029).

## Exit criteria

- Every example passes on Node with the native goldens, except where the JS panics of §12.4 are the expected result and the example records it.
- Channel closing and handle drops happen at the same points on both targets, checked by dedicated UI run tests.
- An edit to one function body regenerates only the JS of functions whose suspension status or IR changed.
- Deno and Bun run the conformance suite in CI. Browser support is checked with a smoke test.
