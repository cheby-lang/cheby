# Performance work log

This document records the performance work on the reference implementation that needs no
decision-log entry: what was measured, what was changed, and what was tried and rejected. Changes to
the language, to a required cost or to the ABI go to [the decision log](../decisions/LOG.md)
instead, and this document only points to them.

## How the numbers are taken

- Machine: an Apple M3, macOS, a release `cheby` (`cargo build --release -p cheby_cli`).
- `cargo xtask bench-run --runs 9 --cheby NEW --against BASE` runs the programs under
  `bench/runtime` with the two binaries interleaved and reports medians (D-718, D-895). One run of
  five of `collections/multimap` swings by 10% either way, so only medians of nine runs are quoted.
- Profiles come from the AOT executable of each benchmark (`cheby build`), sampled with macOS
  `sample` at a larger size, so the generated code's symbols (`cb$fn$…`) show beside the runtime's.
- The runtime's counters (`CHEBY_RT_STATS=1`) show copies that should have been in-place updates:
  `list push copied`, `list update copied`, `map/set update copied` and `reuse misses`.
- With a warm build cache, the time of `cheby run` outside `main` is under 25 ms for every
  benchmark and 8 ms for `001_hello_world`, so the work below is all about run time.

## Summary of the 2026-11-21 round

Five changes went in after the lists of D-1757, each described under [Done](#done). Medians of
nine runs of `bench-run`, the final commit against the commit the round started from:

| Benchmark                      | `c160f89e` | `a36c5bb2` | Change                         |
| ------------------------------ | ---------- | ---------- | ------------------------------ |
| `collections/multimap`         | 1.030 s    | 0.975 s    | −5.3%                          |
| `collections/ordered_map`      | 0.714 s    | 0.641 s    | −10.3%                         |
| `collections/ordered_multimap` | 1.049 s    | 0.925 s    | −11.9%                         |
| `collections/ordered_set`      | 0.532 s    | 0.482 s    | −9.4%                          |
| `collections/sorted_map`       | 0.776 s    | 0.761 s    | −2.0%                          |
| `collections/sorted_multimap`  | 0.603 s    | 0.585 s    | −3.0%                          |
| `collections/sorted_set`       | 0.684 s    | 0.681 s    | −0.4%                          |
| `fiber/buffered_4x4`           | 0.581 s    | 0.476 s    | −18.0% (second reading −16.8%) |
| `fiber/ping_pong`              | 0.115 s    | 0.102 s    | −11.5% (second reading −14.2%) |
| `fiber/scope_open_close`       | 0.825 s    | 0.821 s    | −0.4% (second reading −7.2%)   |
| `fiber/sieve`                  | 0.169 s    | 0.155 s    | −8.6% (second reading −10.7%)  |
| `fiber/spawn_join`             | 0.093 s    | 0.092 s    | −2.0% (second reading −2.1%)   |
| `json/roundtrip`               | 0.268 s    | 0.255 s    | −4.6%                          |
| `json/value`                   | 0.216 s    | 0.202 s    | −6.2%                          |
| `rc/list_pipeline`             | 0.169 s    | 0.155 s    | −8.0%                          |
| `rc/map_updates`               | 0.445 s    | 0.335 s    | −24.8%                         |
| `rc/record_updates`            | 0.244 s    | 0.235 s    | −3.7%                          |
| `rc/string_building`           | 0.614 s    | 0.402 s    | −34.6%                         |
| `rc/tree_inserts`              | 0.359 s    | 0.357 s    | −0.5%                          |
| `time/now_loop`                | 0.743 s    | 0.750 s    | +0.9%                          |

Geometric means: all 20 benchmarks −8.7%, the RC group −15.4%, collections −6.1%, JSON
−5.4%, fiber −8.3% and −10.4% in a second reading. The fiber benchmarks run on several threads and
swing between readings: `fiber/scope_open_close` read −12% when its change was measured alone.

On the final commit the whole suite passes (8,127 tests), as do std's own tests, every step of the
address sanitizer, and the other checks CI runs locally. Each change ran Miri on the groups it
touched (`hamt`, `generic`, `list`, `alloc`, `rc`, `string`, `text`, `bytes`, `values`), and the two
that touch what several threads read ran the thread sanitizer's runtime step. Linux and x86-64
were not measured: the `bench.yml` reading of D-895 and D-1255 is still to be dispatched.

None of the five changes what a program can observe, a required cost or a logged layout, so none
has a decision-log entry.

## 2026-11-21: the survey after D-1757

Baseline: commit `c160f89e`, the lists of D-1757 to D-1760. Where the time goes, by benchmark:

| Benchmark                                       | Largest shares of the samples                                                      |
| ----------------------------------------------- | ---------------------------------------------------------------------------------- |
| `collections/sorted_map`, `sorted_set`          | `std::int::compare` about half, then `sorted_map::insert` and `balanced`           |
| `rc/tree_inserts`                               | its own `insert`, then `std::int::compare`                                         |
| `collections/ordered_map`, `ordered_set`        | hashing a key (`generic::hash::step`), the HAMT lookup, `sorted_map::insert`       |
| `collections/ordered_multimap`                  | freeing and allocating list nodes: 977,204 list pushes copied their list           |
| `collections/multimap`, `rc/map_updates`        | the HAMT's `Path::walk`, `make_writable` and `Bitmap::writable`, the allocator     |
| `rc/list_pipeline`, `rc/record_updates`         | `list::fold`'s loop, `list_push`, and `pop_front_rec` (about 10%)                  |
| `rc/string_building`                            | `memmove`, `str_append`, the allocator, `to_string` of an `Int`, `split`           |
| `json/roundtrip`, `json/value`                  | the benchmark's own FNV loop: `bytes::fold`'s closure and `bytes::get` per byte    |
| `fiber/sieve`, `fiber/buffered_4x4`             | the context switch, `channel::send` and `receive`, a contended lock                |
| `time/now_loop`, `fiber/scope_open_close`       | two and three allocations per call, the clock read                                 |

## Done

### `ordered_map::insert` and `upsert` update in place

`std::ordered_map`'s `upsert` read the old value with `map::get`, which left the index holding it,
and then called `update`. The value's count was 2 there, so an `update` that pushed onto a list, as
`ordered_multimap::insert` does, copied the list on every call. `insert` and `upsert` now take the
record apart and change the index with one `map::upsert`, whose callback gets the stored value
unshared when the index is held only by the call. The index grows only for a new key, so
`map::size` before and after tells whether the key also goes into `order`. That is one hashed
lookup in place of two. A run test, `ordered_multimap_inserts_write_in_place`, pins
`list push copied: 0`.

| `collections/ordered_multimap` | Before    | After   |
| ------------------------------ | --------- | ------- |
| median of nine runs            | 1.000 s   | 0.940 s |
| list pushes that copied        | 977,204   | 0       |
| reuse misses                   | 977,206   | 2       |
| allocations                    | 8.75 M    | 7.45 M  |

`collections/ordered_map` reads −3.3% and its reuse misses go from 199,829 to 2. Each `insert` now
allocates a closure and a `Some`, so allocations rise on `ordered_map` (3.29 M to 3.77 M) and
`ordered_set` (2.21 M to 2.52 M) while their time is level or better.

One thing differs on a path no result depends on: when `update` panics on a present key of an
index held only by the call, `map::upsert` has already moved the value out, so the values are
released in another order during the unwinding. `std::multimap` has always behaved this way.

Tried and not kept: a private helper shared by `insert` and `upsert` for the size check and the
rebuilt record. It added 1.17 M allocations on `ordered_multimap`, because the new record can reuse
`m`'s block only when it is built in the function that took `m` apart.

The audit of the other Cheby collections found no second case: `multimap`, `sorted_multimap`,
`sorted_map`, `sorted_set` and `heap` show no copies from this pattern.

### Map and set keys hash and compare without the generic walk

A key was hashed by `generic::hash_with_seed`, which built a full `Hasher` (a SipHash state and
three scratch buffers) and ran the generic walk's first step, even for a tagged `Int` or a
`String`. `hash_with_seed` now hashes a tagged integer, a `String` or a `Bytes` with a bare
SipHash-1-3 state, writing the same bytes under the same key, and `equal` compares two tagged
integers, or two texts of one kind, directly. Every other value takes the walk as before. The
hashes are the same bit for bit, so iteration order, `==` on maps and `list::unique` do not change.
A property test, `leaf_paths_agree_with_the_walk`, compares the two paths on integers across the
tagged range, boxed numbers, floats with NaN and ±0, strings, bytes, tuples, constructors and
closures.

The HAMT's update path lost its per-call overhead too:

- `Path` made two `Vec` allocations per update. It now holds two fixed stacks of 14 entries, the
  most a path needs, and is built in the caller's frame, since returning it copied 280 bytes.
- `make_writable` remembers the room it made, so its second call in `upsert`'s new-key branch
  returns at once.
- The common case of `Bitmap::writable`, a unique node with room, is inline.
- `cheby_map_upsert` looks the running fiber up once for the `Some` it allocates and the closure
  it calls.

Medians of nine runs against `5dc18965`:

| Benchmark                      | Before  | After   | Change |
| ------------------------------ | ------- | ------- | ------ |
| `rc/map_updates`               | 0.442 s | 0.334 s | −24.4% |
| `collections/ordered_set`      | 0.543 s | 0.474 s | −12.8% |
| `collections/multimap`         | 0.684 s | 0.608 s | −11.1% |
| `collections/ordered_map`      | 0.577 s | 0.526 s | −8.9%  |
| `collections/ordered_multimap` | 0.683 s | 0.629 s | −7.8%  |

The collections group's geometric mean is −6.1% and the RC group's −6.1%. Checksums and allocation
counts are unchanged. Miri on the `hamt` and `generic` groups reports nothing.

Tried and not kept:

- Leaving `Path`'s construction to the inliner. The 280-byte copy stayed even under
  `#[inline(always)]`; only building it in the caller's frame removed it.
- More work on `find_hashed` and the path walk. What is left there is memory latency in an
  already tight loop.
- A hand-written SipHash for the 9-byte integer key. It might save part of the 4% that
  `hash_with_seed` still takes, for code that must match the library bit for bit for ever.

### A walk takes each leaf out of the tree without moving its neighbours

When a walk from the front had used up its head leaf, the tree's first leaf came out through
`pop_front_leaf`, which moved the other 31 children and 31 sizes of the branch down a slot and
subtracted the leaf's length from every size on the path. Now the leaf's slot gets a hole, the word
0, the sizes stay, and the root's offset moves past the leaf, in one descent that records its path.
A branch whose last child was taken is released and leaves a hole in its parent. The step that
uses up the head leaf does this on its fast path, so a walk calls the runtime once per 32 elements
and never enters the slow path. Holes are always the first children of unique branches on the path
to the offset, and `settle` closes them before any other operation, a share or a freeze reads the
tree. The path with the hole runs only when the tree holds exactly the list's part and the whole
path is unique; any other tree is cut and popped as before, so what is released, and when, does not
change (D-893).

Medians of nine runs against `a0caaa70`: `rc/list_pipeline` 0.169 s to 0.155 s (−8.2%),
`rc/record_updates` 0.241 s to 0.232 s (−3.8%), the RC group −2.6%. Allocation counts and peak
bytes are unchanged. Miri on the list group reports nothing, and `ends_match_a_deque` passes
40,000 cases.

One cost: `view()` of a big root now walks the tree's left edge to look for holes, O(height), and
stops at the first node another reference holds.

Tried and not kept:

- Holes written by a second descent: −3.9% only. The cost was the descent, twice, not the moves.
- Prefetching the next leaf: at most 1% at eight times the size, nothing at the default size.
- Moving the slow paths of `cheby_rt_list_push`, `cheby_rt_list_prepend` and
  `cheby_rt_list_uncons_owned` out of line to spare the fast path its frame: LLVM kept the frame,
  the gain was about 1%, and `unwind::tests::guards_cover_entry_points` requires `guarded(` in
  each entry point's body (D-697).
- Freeing a used-up head leaf with its length set to 0, to skip the scan of 32 cleared slots: not
  safe, since a head leaf that was shared and then became unique can hold elements before its
  position.

This settles the first Open item of the decision log's 2026-11-21 session ("Leaving a hole in the
branch would avoid it"). The log itself is not edited here.

### Sends, spawns and scopes without work they did not need

- `rc::mark_shared`, which every `channel::send` and every spawn calls, built a worklist and pushed
  a step even for a tagged integer. It now returns at once for a tagged integer, a static object or
  an object that is already shared, and marks an object whose value fields are all such words
  without a worklist. Anything else takes the walk from its start, as before, so a big list root is
  still folded when it is entered.
- A scope's `nested_slot` and `interrupt` were each a `Mutex<Option<usize>>` that only the opening
  fiber touches. They are relaxed atomics now.
- The close takes the timer's lock only for a timed scope, borrows the chain of parents in
  `outermost_failing` and clones only the scope it returns, and releases its handle through the
  fiber's own control block, which spares the free a thread-local lookup.
- The swap of a fiber's context bindings is a load and a store: the field has one writer at a time.

Medians of nine runs against `265d9f20`, read twice:

| Benchmark                | First reading | Second reading |
| ------------------------ | ------------- | -------------- |
| `fiber/buffered_4x4`     | −20.3%        | −18.0%         |
| `fiber/ping_pong`        | −11.6%        | −16.2%         |
| `fiber/scope_open_close` | −12.1%        | −12.0%         |
| `fiber/sieve`            | −11.5%        | −10.3%         |
| `fiber/spawn_join`       | −4.5%         | −3.4%          |

The fiber group's geometric mean is −12.1%. The AOT executables, timed directly, agree within two
points. Miri on the `alloc` and `rc` groups and the thread sanitizer's runtime step report nothing,
and the `loom` models pass.

One thing changes that no program can reach: marking a value with no local field allocates no
scratch memory, so under an exhausted memory budget that step no longer raises the memory limit.
A budget exists only while a constant is evaluated, where a send or a spawn is already an error
(E0077). Two runtime tests that used the raise now mark values that still need the walk.

Tried and not kept:

- Allocating the `Ok` and `Err` of a receive and a send through the fiber's control block: 0%.
- Moving the fast path of `cheby_rt_alloc` out of its guard, with the cache miss out of line:
  ±0.1% on `time/now_loop` and `rc/string_building`; LLVM still saved three register pairs on
  entry.
- Allocating `time::now`'s tuple through the control block: +0.1%.
- Borrowing the scope's state from its handle in the close, in place of an `Arc` clone: −0.7%, for
  lifetime reasoning that was not worth it.
- The channel's send and receive themselves. In `fiber/buffered_4x4` about 40% of the samples are
  contention on the channel's lock, and in `fiber/ping_pong` the cost is the return after the stack
  switch, which is assembly. A cheaper `dup` of a shared handle would need `fetch_sub`, which is
  not safe where a count overflows to static (D-454).

### Numbers to text, appends and splits without the detours

- `show` and the debug text of an integer built a Rust `String` and copied it into a Cheby one.
  The digits are now written into a 21-byte stack buffer, two at a time from a table, and the Cheby
  string is made from that. The text is the same byte for byte: one test covers every power of ten
  and its neighbours and each type's ends, and a property test draws 10,000 values per type.
- `cheby_rt_str_append` checked the accumulator, then its growth path, then appended. It is one
  call now, whose in-place case is inline and whose growth is out of line, and it allocates and
  frees through the fiber's own control block. D-577's and D-849's capacities are unchanged.
- A copy of up to 32 bytes is two overlapping loads and stores in place of a `memcpy` call.
- `string::split` on a one-byte separator searches with `memchr` in place of the general
  searcher, and takes its first part before deciding whether the text has the separator at all, in
  place of a separate scan.
- The kernels that build a text piece by piece look the running fiber up once, and those that
  sized their result exactly append without the growth check.

Medians of nine runs against `36850571`, with the AOT executables timed directly beside them:

| Benchmark            | `bench-run`      | AOT executable |
| -------------------- | ---------------- | -------------- |
| `rc/string_building` | −27.4%, −32.0%   | −32.1%         |
| `json/roundtrip`     | −7.3%, −3.1%     | −3.5%          |
| `json/value`         | −6.3%, −3.7%     | −5.0%          |

The RC group's geometric mean is −6.1%. Miri on the `string`, `text`, `bytes` and `values` groups
and the thread sanitizer's runtime step report nothing.

`time/now_loop` reads +1.6% to +4.1%. Its hot functions disassemble the same in both binaries;
`cheby_rt_alloc` and `cheby_rt_free_unique` happened to start on 64-byte boundaries before and do
not now. Nothing was done about it.

Tried and not kept:

- A cheaper lookup of the running fiber. One `tls::current_fiber()` costs 1.9 ns on macOS: an
  out-of-line call and a call through the thread-local machinery, with no lazy check and no
  `RefCell`. Only inlining it into generated code could make it cheaper, which D-686 rules out, so
  the number of lookups was cut instead. The "10%" that a profile showed inside `cheby_map_upsert`
  was sampling skid: adding four lookups there made `collections/multimap` 3.5% slower, so one
  costs about 0.9%.
- `str::split(char)` for a one-character separator, before `memchr`: it still called `memcmp` for
  each match.
- More work on `cheby_bytes_get`. It was already a bounds test, a load and a static `Some`. The
  rest of a byte loop's cost is in generated code: the wrapper, `bytes::fold`'s closure and
  `int::from_u8`.
- The `show` of a float still builds Rust strings. No benchmark spends time there.

## Looked at and left alone

- **`std::int::compare`.** It is about half the samples of `collections/sorted_map`,
  `sorted_set` and `sorted_multimap`, and a quarter of `rc/tree_inserts`. The function is 25
  instructions; what it costs is the mispredicted branch on a random key, once per tree level. A
  branch-free `compare` would only move the misprediction to the `case` in the caller. Inlining
  `compare` into `sorted_map::insert` is the fix, and the inliner is M6's (ADR-0046).
- **The context switch and the channel's lock.** `fiber/sieve` spends 40% of its samples in the
  switch's assembly, and `fiber/buffered_4x4` 40% in contention on one channel's lock from four
  threads. Neither has a safe local fix.
- **Two and three allocations per `time::now` and per scope.** They are the boxed results of
  Cheby functions and a scope's handle. Removing them needs results returned unboxed or the calls
  inlined, which is the compiler's work.
- **The placement of hot functions.** `time/now_loop` moved by a few percent when
  `cheby_rt_alloc` and `cheby_rt_free_unique` stopped falling on 64-byte boundaries. Forcing an
  alignment on the runtime's entry points was not tried.

## Not done: each needs a decision first

These would be faster, but each changes something the decision log pins, so none was built or
committed. They are proposals.

- **`[item, ..xs]` as `list_prepend`.** Done on 2026-11-22 as D-1763. The literal lowered to a push and an append then, O(log n)
  where D-1757 makes a prepend amortized O(1). The result is the same list, but MIR and the lines
  of `--reuse-report` change (D-717, D-852). It is an Open item of the 2026-11-21 session.
- **An `Int` interpolated straight into the accumulator.** `"{n}"` calls `show`, which allocates
  a string, appends it and frees it. A helper that appends the digits to the accumulator would
  remove two of the four allocations per line of `rc/string_building`. It adds a runtime helper to
  D-520's fixed set and changes interpolation's lowering (D-1170).
- **Calls of std kernels without their wrapper.** Already an Open item of the 2026-10-19 session,
  which measured −8% on its yardstick. It is what a byte loop over `bytes::get` and every
  `list::push` still pay. It changes D-455: std's interfaces would carry the binding.
- **No stack check in a function that calls nothing.** `int::compare` and its like spend a good
  part of their few instructions on the entry check and the frame it needs. The check is also where a fiber is interrupted and
  cancelled (D-124, D-688), so leaving it out changes when those are delivered.
- **The running fiber read inline in generated code or kernels.** One lookup costs 1.9 ns on
  macOS. D-686 keeps every thread-local access behind an accessor that is never inlined, because
  an inlined one may keep another thread's address across a switch.

The first Open item of the decision log's 2026-11-21 session, the hole in place of the shifted
branch, is done by the list-walk change above and can be struck through there.

## 2026-11-22: the round after the language comparison

The comparison in `../cheby-compare` was run again on 2026-11-22, and its notes name the costs that are left. This round takes them in turn. The decisions are D-1761 onward, in the session of the same date in the decision log.

**How these numbers are taken.** The machine was shared with other builds during this round, so wall time was useless. Each reading is instructions retired and cycles from `/usr/bin/time -l` for one run of a comparison program built with `cheby build`, against the executable of the same program built at `7a586d82`. Instructions repeat within 1%. Cycles move by a few percent with the load, so a change under 5% in cycles alone is noise. The sizes are smaller than the comparison's where a run is long: `lru_cache 100000 4000000`, `nbody 2200000`, `spectral_norm 1100`, `fannkuch_redux 9`, `json_roundtrip` with 3 passes.

### A self tail call jumps inside its function (D-1761)

Every loop is a self tail call, and each iteration ran the epilogue and the prologue, spilled its parameters and jumped through the slot. The call is now a jump to a loop header before the entry check, so the check still runs in every iteration. REPL sessions and the inner bodies of large frames keep the call.

### `ordered_map` keeps its order as links through its index (D-1762)

The sorted tree from insertion number to key is gone. Each entry of the hashed index holds the keys before and after it. In `lru_cache` a hit removes a key and inserts it again, which was two tree updates with dictionary comparisons.

### A list literal prepends its leading elements onto its first spread (D-1763)

`[x, ..xs]` was a push onto a new list and an append of `xs`. It is `list_prepend` now, in place when `xs` is not used again. `spawn_many` makes 3.0 allocations per task where it made 7.9.

### A scalar literal constant is not evaluated, and its own module reads the literal (D-1764)

A module that held any constant was compiled a second time, on `plain` code, for the evaluation module. For the 200 modules of `compile_large`, each with one literal constant, that was most of a cold `cheby check`. A read of a constant through its slot was also a load, a fill check, a dup and a drop in every iteration of a loop.

### Readings for the four changes together

| Program          | Instructions | Cycles |
| ---------------- | -----------: | -----: |
| `lru_cache`      |       −61.9% | −44.0% |
| `spawn_many`     |       −31.0% | −17.7% |
| `mandelbrot`     |       −28.4% |  −6.5% |
| `nbody`          |       −18.5% | −17.9% |
| `spectral_norm`  |        −3.5% |  −0.7% |
| `json_roundtrip` |        −3.0% |  −4.5% |
| `fannkuch_redux` |        −1.4% |  −0.8% |
| `word_freq`      |        −1.1% | −10.6% |
| `binary_trees`   |        −0.1% |  −3.2% |

`mandelbrot` retires 28% fewer instructions and takes only 6.5% fewer cycles: its loop is bound by a chain of float operations and reloads of spilled parameters, not by the count of instructions.

A cold build of `compile_large`'s 200 modules, from a cache that holds std, before and after D-1764:

| Command       | Instructions before | After   |
| ------------- | ------------------: | ------: |
| `cheby check` |             17.87 G |  2.53 G |
| `cheby run`   |             33.66 G | 13.54 G |
| `cheby build` |             34.31 G | 14.16 G |

### A direct call of a plain std kernel is its foreign call (D-1765)

A call of a std function bound to a runtime kernel went through a Cheby wrapper, which boxed a scalar result. `float::from_int` allocated on every call. The call site now makes the foreign call itself, with each counted argument moved into an owned temporary, so the kernel sees the counts the wrapper gave it. `spectral_norm` at n = 550 makes 92,499 allocations where it made 12,192,500.

### The release profile, first part (D-1766 to D-1769)

`cheby build`, `cheby run` and `cheby test` take `--release`. So far release code calls Cheby functions directly and is compiled with Cranelift's `speed`. `CHEBY_RELEASE_OFF=direct` or `=clif` switches one of them off for a reading. On `mandelbrot`, `speed` is worth 9% of the instructions and direct calls under 1%: a loop no longer calls anything since D-1761.

### Readings after these two, all changes of the round together

The first two columns are the debug profile against `7a586d82`. The last is the release build against the debug build of the same compiler, in instructions.

| Program          | Instructions | Cycles | Release against debug |
| ---------------- | -----------: | -----: | --------------------: |
| `spectral_norm`  |       −57.3% | −57.4% |                 −4.1% |
| `lru_cache`      |       −63.1% | −67.6% |                 −0.8% |
| `mandelbrot`     |       −41.8% | −20.1% |                 −9.8% |
| `nbody`          |       −28.0% | −28.4% |                 −1.2% |
| `fannkuch_redux` |        −7.8% | −12.5% |                 −0.0% |
| `json_roundtrip` |        −9.6% |  −0.2% |                 −0.5% |
| `word_freq`      |        −2.7% | −20.6% |                 −0.0% |
| `binary_trees`   |        −0.2% |  −1.9% |                 −3.0% |

The three concurrency programs are left out. Their instruction counts move by a factor of two between runs of one executable while the machine is loaded, because idle workers spin, so they wait for a quiet machine.

Tried and not kept, or left open:

- A check for interrupts at each direct kernel call. The wrapper's entry check ran before every kernel. Now an interrupt that arrives before a kernel call is noticed at the next function entry or loop iteration. That is what the language promises, and a check per call site would give back part of the gain.
- A read-only kernel such as `list::get` still gets a dup and a drop around the call when its argument is used afterwards. A list of kernels that never write in place would remove the pair.

### The release profile inlines calls inside a module (D-1770, D-1771)

In release, a call of a small function of the same module is replaced by its body before reference counts are inserted, and a clean-up pass cancels boxes against unboxes and folds what the merge made constant. A helper that returns a `Float` no longer boxes it.

Instructions retired, in units of 10⁹, by executables of the compiler in its development build. The comparison's programs are written without helpers, because a helper used to box, so the inliner finds little in them. The last row is `spectral_norm` written the natural way, with its matrix entry in a helper function.

| Program                      |  Debug | Release | Release, `inline` off | Release, `clif` off |
| ---------------------------- | -----: | ------: | --------------------: | ------------------: |
| `mandelbrot` 4000            |  19.42 |   17.54 |                 17.59 |               19.44 |
| `nbody` 2200000              | 118.21 |  117.28 |                117.55 |              117.60 |
| `spectral_norm` 1100         |  12.90 |   12.45 |                 12.45 |               12.89 |
| `fannkuch_redux` 9           |  14.30 |   13.81 |                 14.30 |               13.80 |
| `binary_trees` 18            |  68.42 |   67.77 |                 67.72 |               67.76 |
| `json_roundtrip` 3 passes    |  62.95 |   62.56 |                 62.82 |               62.55 |
| `spectral_norm`, with helper |  47.10 |   12.20 |                 46.57 |                   – |

A clean release build of `compile_large`'s 200 modules retires 1.41 times the instructions of a clean debug build, against D-267's limit of 3.

Found on the way: removing a move that looked dead changed when a handle's drop function ran, and example 020 printed an empty store. The clean-up now leaves alone every definition and branch whose removal would move the release of a value that may hold a handle.

### Frees that kill no field, boxes through `ctx`, a sized `free_unique` (D-1772)

The general free path was inlined into the fast ones and set up their frames, so freeing an object with no counted field cost about 100 instructions. It costs about 50 now. The boxing helpers no longer look the fiber up in thread-local storage, and a specialized drop passes the size it knows.

Tried and not kept:

- No block cache at all, every block from mimalloc: +4.6% instructions in `binary_trees`, +13.9% in `nbody`, +5.8% in `json_roundtrip`, +8.7% in `fannkuch_redux`.
- A block cache of 65,536 blocks per class in place of 512: −8.4% in `binary_trees`, nothing elsewhere, and up to about 277 MB held per thread. Left as an Open item of the decision log.
- An allocation fast path written inline in generated code: estimated at 2% in `binary_trees` and 3.5% in `nbody`, for a block cache whose layout becomes part of the ABI. Not built.

### Lookups without the `Some`, reading kernels that borrow, one-leaf list indexes inline (D-1773, D-1774)

`list::get` and `map::get` whose result is matched at once build no `Some`. Kernels that only read their arguments get them without an owned temporary. A read or a write of a list of one leaf is inline. `fannkuch_redux` at 9, by a compiler in its development build: 14.30 G instructions before, 7.61 G with the first part, 7.37 G with the second, 4.24 G with all three, and 1.6 million allocations where it made 12.9 million.

### Readings after these, all changes of the round together

Against the executables of `7a586d82`, as before. The load on the machine was lower for this reading than for the baseline's, so the cycles flatter the changes; the instructions do not.

| Program          | Debug, instructions | Debug, cycles | Release, instructions | Release, cycles |
| ---------------- | ------------------: | ------------: | --------------------: | --------------: |
| `lru_cache`      |              −66.6% |        −77.1% |                −67.1% |          −77.7% |
| `fannkuch_redux` |              −70.5% |        −72.6% |                −76.4% |          −77.1% |
| `spectral_norm`  |              −57.4% |        −63.1% |                −59.1% |          −62.5% |
| `mandelbrot`     |              −42.0% |        −44.4% |                −47.7% |          −52.5% |
| `nbody`          |              −34.8% |        −43.6% |                −35.9% |          −46.9% |
| `json_roundtrip` |              −19.0% |        −22.6% |                −20.2% |          −22.8% |
| `word_freq`      |               −7.5% |        −38.6% |                 −9.4% |          −28.0% |
| `binary_trees`   |               −2.3% |        −12.6% |                 −5.3% |          −13.0% |

### The block cache stays at 512 blocks per class

Measured after D-1772, by instructions retired and peak memory, for 512, 2,048, 8,192, 16,384 and 65,536 blocks per size class. Only `binary_trees` gains, and the gain grows with the logarithm of the size: 2.5%, 5.0%, 5.9% and 8.7%. At 16,384 `json_roundtrip`'s peak memory grows by 10% to 12%, and a thread may hold 73 MB. No value keeps three quarters of the gain within 5% more peak memory and 64 MB per thread, so the size is unchanged. A cache that grows while a thread keeps overflowing it and shrinks when the thread parks is a possible later task; `spawn_many` may gain from it too, which a quiet machine has to show.

### The entry check's slow paths clobber no register (D-1775)

Since D-1761 the entry check sits inside every loop, and its cold call clobbered every float register, so a loop kept each `Float` it carried on the stack. The slow-path stubs now save every register themselves and are called with Cranelift's `preserve_all` convention. The loop of `mandelbrot` has no stack store or load left. Measured alone: `mandelbrot` −12.9% instructions and −38.3% cycles in debug, −10.1% and −24.8% in release; the other programs within 1.5%.

Looked at and left alone: the calls of drop, free and allocation are not cold in allocation-heavy code, so a stub that saves everything would cost more than the spills it removes. In `nbody`'s `pull`, most of the 21 spill stores and 29 reloads sit around the call of `float::sqrt`'s kernel.

### Release inlining across the package's modules and from std (D-1776, D-1777)

The inliner takes bodies from the other units of the build, and std's by name. Std's own release code is optimized the same way, and its release masks are inferred over the optimized code. Instructions retired, release against debug of the same compiler: `lru_cache` −7.7%, `fannkuch_redux` −6.9%, `sorted_set` −8.0%, `sorted_map` −5.1%, `spectral_norm` −4.6%, `nbody` −2.7%, `word_freq` and `json_roundtrip` under 1%. The larger gains wait for clones of generic functions: a call through a dictionary or a closure is not a direct call yet.

Found on the way:

- Inlining a helper with a join into `sorted_map::insert` parted the matched node from the node rebuilt after the call. Reuse hits fell from 15.9 million to 0.3 million and instructions rose by 11.6%. A body with several blocks is now not inlined at a call in a statement when a counted copy of the host pairs fewer drops with allocations.
- Two faults in the clean-up pass of D-1770: an unbox of a removed block parameter stayed, and a sized constant was compared with a case of another width, so `i16::checked_div(min_value, -1)` panicked in release. Std's own tests run as release code found both.
- A release command checks and optimizes all of std in every process: a warm `cheby build --release` of `hello_world` retires 1.49 G instructions against 0.14 G in debug. Std's release bodies and masks in the cache are the next step.

### Readings after these, all changes of the round together

Against the executables of `7a586d82`.

| Program          | Debug, instructions | Debug, cycles | Release, instructions | Release, cycles |
| ---------------- | ------------------: | ------------: | --------------------: | --------------: |
| `fannkuch_redux` |              −70.7% |        −68.2% |                −76.7% |          −75.8% |
| `lru_cache`      |              −66.3% |        −59.0% |                −68.9% |          −66.3% |
| `spectral_norm`  |              −57.1% |        −59.3% |                −59.6% |          −60.3% |
| `mandelbrot`     |              −49.3% |        −54.3% |                −52.9% |          −51.7% |
| `nbody`          |              −34.6% |        −39.1% |                −36.7% |          −43.4% |
| `json_roundtrip` |              −18.7% |        −22.1% |                −19.5% |          −21.9% |
| `word_freq`      |               −7.4% |        −39.5% |                 −7.9% |          −39.8% |
| `binary_trees`   |               −1.7% |        −11.0% |                 −4.7% |          −11.6% |

