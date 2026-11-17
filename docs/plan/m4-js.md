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
- That design step is done: D-1347 to D-1426 settle what the server does and how it is built. Its slices, exit criterion and progress are in [m5-tooling.md](m5-tooling.md#status). It is built on its own branch, and `tests/milestone` stays `M4` while it lands (D-1352).

### 4.6 Context values (tier 4)

This workstream is built on native first, as one piece of work, before the JS backend exists. Its JS side follows with 4.3 and passes the same spec tests (D-1563). Its design is settled in D-1563 to D-1577, and the gaps found before the code in D-1578 to D-1602.

- `std::context`: a context per fiber, empty at `main`, each test and each REPL entry (D-1044, ADR-0068). A fiber's bindings are a persistent map in the runtime's HAMT, behind one pointer, so a spawn and a capture copy it in O(1) (D-1571).
- Keys: a `Key<T>` holds its constant's path, its type's text with each type's internal path, a 64-bit FNV-1a hash of both, and the default. The compiler passes the texts to the package-visible constructors `key_at` and `key_without_default_at` at the call that is a constant's whole initializer, and a lookup compares the hash, then the texts. At the REPL prompt the path is the bare name, and after `:reload` the unmarked path (D-1564, D-1572, D-1580, D-1587, D-1590). The std-only attribute `@std_initializer` marks `key` and `key_without_default`, names their constructors and drives the replacement, E0080, the rule that they never satisfy an interface, and completion (D-1593, D-1599, D-1601, D-1602).
- E0080, the compile error for every other mention of `context::key` or `context::key_without_default` in an expression, a call elsewhere, a function value or a pipe stage, with its explanation and its UI test. E0081, the evaluation error for a key whose default value holds a function, a `dyn` value or a handle, which E0068 no longer covers, with its explanation and its UI test (D-1045, D-1565, D-1578, D-1581, D-1589, D-1591, D-1600).
- `provide`, `with` and `random::with_generator` in Cheby over `@std_context` swap kernels. A scope records the fiber's bindings and stream when it opens and its close restores them, every entry starts from its own context, and the outer streams of `with_generator` and `with` live in the fiber. A test pins that every catch point after which Cheby code runs restores the context (D-1573).
- Captured contexts: the external type `Context` with `capture`, `with`, which replaces the fiber's context while its body runs, and `empty` (D-1566, D-1568, ADR-0080).
- `std::time`'s clocks in context, with timers left on the runtime's monotonic clock, and the readers `now_on` and `monotonic_on` (D-1047, D-1570). `now` and `monotonic` read the system clock through a kernel that looks at the context, with no lookup in Cheby unless a clock is provided (D-1597).
- `std::random`'s seeded stream per fiber, held in fiber fields beside the bindings, advanced by each host draw, and derived for a spawned fiber and for a capture from the fiber's state and spawn number without advancing the fiber's stream (D-1048, D-1567, D-1574).
- `std::log` ships on native, so the compiler's list of planned std modules is empty. E0038 stays tested on a made-up module, and `E0038.md` says that no current toolchain emits it (D-1633).
- The example `request_context`, under the next free number, whose `test.toml` names M3, so it binds at once, as the new UI tests and REPL transcripts do (D-1576, D-1582).
- The cost gate: the fiber group of `bench/runtime` and a loop of `time::now` calls in a new group `bench/runtime/time/`, timed before and after on one machine. Any one benchmark whose median run time of `main` reaches a ratio of 1.05, after a rerun, stops the work (D-1577, D-1586). The gate also reads `rc/map_updates` and the `collections` group, and the final measurement reports the new build under three provided values against an empty context, with no stop line (D-1592, D-1596).
- The tests of these parts in `context.md`, `time.md` and `random.md` leave `PENDING_SPEC_TESTS` when the native side passes them (D-741, D-1049, D-1563).
- On JS, with 4.3: the same module over `map.mjs`, and a Cheby function passed to foreign code takes the passing fiber's context with it for its callbacks' fibers (D-1569). Which stream state each call of one callback starts from is open.

### 4.7 Standard-library additions after M3

- The native kernels first, which need no JS work: `sys_raise` of `std::fiber`, a panic from a stored message and location in `crates/cheby_runtime/src/panic.rs` (D-1097), and `sys_send_now` and `sys_receive_now` of `std::channel` and `sys_select_now` of `std::selector`, over the runtime's existing checks of ready arms and claims of waiting fibers in `crates/cheby_runtime/src/fiber_ops/channel.rs` and the select code (D-1099). Then, in Cheby, `fiber::raise`, `fiber::map` and `fiber::try_map` over `scope`, `spawn`, a channel, S5's `fiber::cancel` and `raise` (D-1098), and `channel::send_now`, `channel::receive_now`, the type `channel::Refused` and `selector::select_now` over their kernels. A raised panic's backtrace starts with `  in fiber::raise`, with no location, and its report keeps the original location (D-1097, §11.6).
- `fiber::yield_now` with the JS scheduler: the private `@std_context` kernel `sys_yield_now` on both targets, a real suspension point on JS that the suspension analysis counts (4.2, D-014), and `("std::fiber", "yield_now")` in `crates/cheby_mir/src/waits.rs` (D-1102, D-1059).
- The `.mjs` side of these five kernels, bound as D-484 binds every kernel: `./fiber.mjs` `sys_raise` and `sys_yield_now`, `./channel.mjs` `sys_send_now` and `sys_receive_now`, `./selector.mjs` `sys_select_now`.
- `./list.mjs` `sort_with(xs, by)` (D-1448): the bottom-up merge sort of `list.md`, with runs of one element merged in adjacent pairs, front to back, a last run without a partner waiting for the next round, and the right element taken only when `by` returns `Greater`. It must call `by` on the same pairs in the same order as the native kernel, `cheby_list_sort_with` in `crates/cheby_runtime/src/list/kernels.rs`, and the std test "sort_with merges in the same order for any comparison" and the UI test `03-types/collections/sort_with_calls_run` pin both. A panic in `by` propagates.
- `./heap.mjs` `sys_push(items, item, max, by)`, `sys_pop(items, max, by)`, `sys_from_list(items, max, by)` and `sys_to_list(items, max, by)` (D-1612, D-1613): the binary heap of `heap.md` over the lists of `./list.mjs`, with `before(x, y)` meaning that `by(x, y)` is `Less`, or `Greater` when `max` is true. `sys_push` appends `item` and swaps it with its parent `(i - 1) / 2` while `before(item, parent)` holds; `sys_pop` drops element 0, which the caller has read, moves the last element there and sifts it down, taking the right child `2i + 2` over the left `2i + 1` only when `before(right, left)` holds and swapping while `before(child, element)` holds; `sys_from_list` sifts down each position from `n / 2 - 1` to `0`; `sys_to_list` pops a copy until it is empty. A kernel changes its list only where `./list.mjs` would update a list in place, and each must call `by` with the same arguments in the same order as the native kernels, `cheby_heap_sys_*` in `crates/cheby_runtime/src/list/heap/mod.rs`, which the UI test `03-types/collections/heap_calls_run` and the std test "a type's own compare is called, and ties pop in a fixed order" pin. The native kernels compare `Int`, the sized integers, `String` and tuples of those without calling `by`, which gives the same `Order` as calling it. A panic in `by` propagates.
- `./set.mjs` `sys_get(s, item)` (D-1447): the element `s` stores that is `==` to `item`, as `Some`, or `None`. It returns the stored element, not `item`, which shows for `-0.0` and `0.0` (D-588), and the std test "intersection keeps the elements of its first set" pins it.
- `./float.mjs` `to_fixed(x, digits)` (D-1085): clamp `digits` to 0 to 100; NaN gives `NaN` and the infinities `inf` and `-inf`; below 1e21 in size, `Math.abs(x).toFixed(digits)`, which is exact and rounds ties away from zero, with a `-` whenever the sign bit is set (`x < 0 || Object.is(x, -0)`); from 1e21 up, `BigInt(Math.abs(x)).toString()` with the sign, then `.` and `digits` zeros when `digits > 0`. The native kernel's tests, `to_fixed_*` in `crates/cheby_runtime/src/num/float/tests.rs`, are the table to check against.
- `./string.mjs` `words` (D-1092): split at runs of `White_Space` with the explicit table of 25 code points that `trim`'s JS kernels share (U+0009 to U+000D, U+0020, U+0085, U+00A0, U+1680, U+2000 to U+200A, U+2028, U+2029, U+202F, U+205F, U+3000), never `\s`. The std tests "words splits at runs of White_Space" and "words keeps text that is not White_Space together" pin U+0085 and U+FEFF, and the runtime test `white_space_is_the_table_of_25_code_points` pins the table.
- `./bytes.mjs` `index_of(data, part, from)` (D-1094): `from` clamped to 0 to the length, an empty `part` found at the clamped `from`, and O(n + m), so a linear search such as two-way or KMP, never a nested loop.
- `duration::scale` on JS (D-1101): where the seconds of the result leave ±(2⁵³−1), it panics with `integer overflow`, as native does beyond 64 bits, so the JS side checks the safe range of its `Int` steps and reports that message, and the std test "scale panics where the result does not fit" passes on Node.
- `random`'s `split`, `next_bool`, `choose` and `shuffle`, `channel`'s `fold`, `try_fold`, `to_list` and `send_all`, `std::base64`, `std::hex`, `to_base`, the wrapping conversions, `string::lines` and `from_bytes_lossy`, and `result`'s and `decode`'s additions are Cheby and owe M4 nothing beyond passing their tests on Node. `random`'s private `derived(generator, n)` is D-1048's derivation, for 4.6's seeded streams to reuse.
- The tests of these functions in `fiber.md`, `channel.md` and `selector.md` leave `PENDING_SPEC_TESTS` (D-741).

### 4.8 The warm restart, from M3

- A design step, in an entry of its own, for a partial second check: when a warm `cheby run` or `cheby test` finds hits whose code records miss, it checks again only those modules and what their lowering needs, instead of starting again with every module parsed after D-1256's three causes. It is designed together with the work on the warm start that M3's exit reads (D-1303, D-1304).

### 4.9 From M3's exit

- M4's first change removes `CHEBY_POSITIONS` and D-1193's positions byte, with a gate of its own, since it moves cache keys (D-1317). `bench-compile --restarts` then needs another setting for its `settings` cause.
- `xtask` gets the shipped allocator, so that D-1316's path reads what ships, and 20,000 is read again when it lands.
- A same-window A/B of M2's `xtask` against M3's reads the fall of 13% to 21% against M2's exit, which sits in the back end with no cause read. Each `xtask` is built for its own tree, since a binary bakes in its repository root. It runs in a quiet window, on AC power with the lid open, since `caffeinate -ims` does not hold off clamshell sleep.
- What a plain `fn main` does on native in a package with `targets = ["js"]`: nothing says yet, and it runs. `main_of` reads `native` only (D-1333), so it takes a target when `--target js` lands, and the rule is settled with it.
- Doc examples as tests, from `docs/plan/m5-tooling.md`, are generated bodies, so that slice looks at D-1328's rule.

## Status

The JS work, 4.1 to 4.4, has not started. Two workstreams run ahead of it. The LSP core, 4.5, has its own status in [m5-tooling.md](m5-tooling.md#status). The native side of 4.6 is implemented:

- `std::context` with keys, `get`, `provide`, `Context`, `capture`, `with` and `empty`, over seven `@std_context` kernels. A fiber's bindings are a map in the runtime's HAMT, and a captured context is runtime kind 48 (D-1566, D-1571, D-1583).
- The key rewrite through `@std_initializer`, E0080 and E0081 with their explanations and UI tests, and the key's identity in the REPL and after `:reload` (D-1580, D-1589, D-1599, D-1600).
- Scopes and entries restore the context, and a test pins the runtime's catch points (D-1573, D-1594).
- `std::time`'s clocks in context, with D-1597's kernels, and `std::random`'s seeded streams, where a spawn takes its number once the fiber exists and a capture once its object is allocated (D-1047, D-1048, D-1567, D-1585, D-1597).
- `std::log` ships on native, so no std module is planned, and E0038's probes use the made-up module `unshipped_probe` (D-1633).
- The tests of `context.md`, `time.md` and `random.md` run, so none of them is in `PENDING_SPEC_TESTS`. Example 033, `request_context`, passes as an M3 example (D-1582).

Still to come for 4.6: the final reading of the cost gate (D-1577, D-1586, D-1592, D-1596) and the full CI run, then, with 4.3, the JS side: `context.mjs` over `map.mjs`, the stream and clock kernels of `random.mjs` and `time.mjs`, and the context of a foreign callback's fiber, whose stream state per call is still open (D-1569).

## Exit criteria

- Every example passes on Node with the native goldens, except where the JS panics of §12.4 are the expected result and the example records it.
- Channel closing and handle drops happen at the same points on both targets, checked by dedicated UI run tests.
- An edit to one function body regenerates only the JS of functions whose suspension status or IR changed.
- Deno and Bun run the conformance suite in CI. Browser support is checked with a smoke test.
- `std::context`, clocks in context and seeded random streams pass their spec tests on native and on Node (D-1049).
- `std::log` passes its spec tests on native and on Node. It has no `.mjs` file of its own (D-1614, D-1635).
- `fiber::raise`, `map`, `try_map` and `yield_now`, `channel::send_now` and `receive_now`, and `selector::select_now` pass their spec tests on native and on Node, and the JS kernels of `float::to_fixed`, `string::words` and `bytes::index_of`, and `duration::scale`'s safe-range check, pass the std tests on Node (D-1085, D-1092, D-1094, D-1097 to D-1099, D-1101, D-1102).
