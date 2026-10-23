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

### 4.6 Context values (tier 4)

- `std::context` on both targets: a context per fiber, held as a persistent map so that a spawn copies it in O(1), restored when `provide`'s body returns or unwinds, and empty at `main`, each test and each REPL entry (D-1044, ADR-0068).
- The compile error for a call of `context::key` or `context::key_without_default` that is not a constant's whole initializer, with its code, its explanation and its UI test (D-1045).
- `std::time`'s clocks in context, with timers left on the runtime's monotonic clock (D-1047).
- `std::random`'s seeded stream per fiber, advanced by each host draw, and derived for a spawned fiber from its parent's state and spawn number without advancing the parent (D-1048).
- The tests of these parts in `context.md`, `time.md` and `random.md` leave `PENDING_SPEC_TESTS` (D-741, D-1049).

### 4.7 Standard-library additions after M3

- The native kernels first, which need no JS work: `sys_raise` of `std::fiber`, a panic from a stored message and location in `crates/cheby_runtime/src/panic.rs` (D-P18), and `sys_send_now` and `sys_receive_now` of `std::channel` and `sys_select_now` of `std::selector`, over the runtime's existing checks of ready arms and claims of waiting fibers in `crates/cheby_runtime/src/fiber_ops/channel.rs` and the select code (D-P20). Then, in Cheby, `fiber::raise`, `fiber::map` and `fiber::try_map` over `scope`, `spawn`, a channel, S5's `fiber::cancel` and `raise` (D-P19), and `channel::send_now`, `channel::receive_now`, the type `channel::Refused` and `selector::select_now` over their kernels. A raised panic's backtrace starts with `  in fiber::raise`, with no location, and its report keeps the original location (D-P18, §11.6).
- `fiber::yield_now` with the JS scheduler: the private `@std_context` kernel `sys_yield_now` on both targets, a real suspension point on JS that the suspension analysis counts (4.2, D-014), and `("std::fiber", "yield_now")` in `crates/cheby_mir/src/waits.rs` (D-P23, D-1059).
- The `.mjs` side of these five kernels, bound as D-484 binds every kernel: `./fiber.mjs` `sys_raise` and `sys_yield_now`, `./channel.mjs` `sys_send_now` and `sys_receive_now`, `./selector.mjs` `sys_select_now`.
- `./float.mjs` `to_fixed(x, digits)` (D-P06): clamp `digits` to 0 to 100; NaN gives `NaN` and the infinities `inf` and `-inf`; below 1e21 in size, `Math.abs(x).toFixed(digits)`, which is exact and rounds ties away from zero, with a `-` whenever the sign bit is set (`x < 0 || Object.is(x, -0)`); from 1e21 up, `BigInt(Math.abs(x)).toString()` with the sign, then `.` and `digits` zeros when `digits > 0`. The native kernel's tests, `to_fixed_*` in `crates/cheby_runtime/src/num/float/tests.rs`, are the table to check against.
- `./string.mjs` `words` (D-P13): split at runs of `White_Space` with the explicit table of 25 code points that `trim`'s JS kernels share (U+0009 to U+000D, U+0020, U+0085, U+00A0, U+1680, U+2000 to U+200A, U+2028, U+2029, U+202F, U+205F, U+3000), never `\s`. The std tests "words splits at runs of White_Space" and "words keeps text that is not White_Space together" pin U+0085 and U+FEFF, and the runtime test `white_space_is_the_table_of_25_code_points` pins the table.
- `./bytes.mjs` `index_of(data, part, from)` (D-P15): `from` clamped to 0 to the length, an empty `part` found at the clamped `from`, and O(n + m), so a linear search such as two-way or KMP, never a nested loop.
- `duration::scale` on JS (D-P22): where the seconds of the result leave ±(2⁵³−1), it panics with `integer overflow`, as native does beyond 64 bits, so the JS side checks the safe range of its `Int` steps and reports that message, and the std test "scale panics where the result does not fit" passes on Node.
- `random`'s `split`, `next_bool`, `choose` and `shuffle`, `channel`'s `fold`, `try_fold`, `to_list` and `send_all`, `std::base64`, `std::hex`, `to_base`, the wrapping conversions, `string::lines` and `from_bytes_lossy`, and `result`'s and `decode`'s additions are Cheby and owe M4 nothing beyond passing their tests on Node. `random`'s private `derived(generator, n)` is D-1048's derivation, for 4.6's seeded streams to reuse.
- The tests of these functions in `fiber.md`, `channel.md` and `selector.md` leave `PENDING_SPEC_TESTS` (D-741).

## Exit criteria

- Every example passes on Node with the native goldens, except where the JS panics of §12.4 are the expected result and the example records it.
- Channel closing and handle drops happen at the same points on both targets, checked by dedicated UI run tests.
- An edit to one function body regenerates only the JS of functions whose suspension status or IR changed.
- Deno and Bun run the conformance suite in CI. Browser support is checked with a smoke test.
- `std::context`, clocks in context and seeded random streams pass their spec tests on native and on Node (D-1049).
- `fiber::raise`, `map`, `try_map` and `yield_now`, `channel::send_now` and `receive_now`, and `selector::select_now` pass their spec tests on native and on Node, and the JS kernels of `float::to_fixed`, `string::words` and `bytes::index_of`, and `duration::scale`'s safe-range check, pass the std tests on Node (D-P06, D-P13, D-P15, D-P18 to D-P20, D-P22, D-P23).
