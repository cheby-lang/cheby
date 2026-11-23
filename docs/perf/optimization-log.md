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

### Std's release exports in the cache (D-1778)

A release command no longer checks and optimizes std in every process: std's exported bodies and release masks are one record in the cache, and a body is decoded when the inliner asks for it. Instructions retired by `cheby build`, in units of 10⁹:

| Project         | Debug, cold | Debug, warm | Release, cold | Release, warm before | Release, warm after |
| --------------- | ----------: | ----------: | ------------: | -------------------: | ------------------: |
| `hello_world`   |        4.70 |       0.130 |         10.81 |                1.501 |               0.137 |
| `nbody`         |        4.83 |       0.169 |         11.02 |                1.588 |               0.251 |
| `compile_large` |       18.72 |        2.11 |         29.80 |                19.28 |               17.81 |

The cold builds include std's first use, which a toolchain pays once. A warm release build of `compile_large` still compiles its 200 modules anew, since release keeps no records of a package's own modules yet. Tried and not kept: decoding the whole record at once, which left `hello_world` at 0.157 G.

### Numeric kernels as instructions (D-1779)

82 std kernels that are one machine operation, the conversions that cannot fail, `float::sqrt` and the wrapping arithmetic, compile to it at a direct call. `spectral_norm` retires 4% fewer instructions, `mandelbrot` and `nbody` under 1%.

Looked at and left alone: `nbody`'s inner function `pull` lost its call of `sqrt` and kept its float spills, 22 stores and 27 reloads. It loads the fields of both bodies before the allocation and release calls of the record it rebuilds, and those calls clobber every float register. An allocation fast path written inline, with its slow path preserving registers as the entry check's does (D-1775), would remove them; that is the path D-1772 measured at 2% to 3.5% without counting the spills.

### Release devirtualization, clones and unboxing in clones (D-1780)

A call through a static dictionary, a known function value or a closure literal becomes a direct call, and a generic function called with one of them gets a clone in the caller's unit, which the inliner then works on. A clone's parameters and result are raw where every use allows it.

Instructions retired, in units of 10⁹, by programs of `bench/runtime` and of the comparison. "Spec off" and "Unbox off" are the same compiler with that lever off.

| Program                   | Debug | Release | Spec off | Unbox off |
| ------------------------- | ----: | ------: | -------: | --------: |
| `collections/sorted_map`  |  9.37 |    6.24 |     8.85 |      6.30 |
| `collections/sorted_set`  | 10.68 |    7.51 |     9.86 |      7.54 |
| `collections/ordered_map` |  2.93 |    2.71 |     2.73 |      2.71 |
| `rc/list_pipeline`        |  3.74 |    2.41 |     3.48 |      2.43 |
| `rc/record_updates`       |  5.25 |    3.57 |     4.89 |      3.57 |
| `rc/tree_inserts`         |  5.45 |    3.66 |     3.66 |      3.66 |
| `rc/map_updates`          |  4.12 |    4.11 |     4.11 |      4.11 |
| `rc/string_building`      |  8.86 |    8.62 |     8.62 |      8.62 |
| `nbody`                   | 42.03 |   38.52 |    40.81 |     38.52 |
| `word_freq`               | 26.03 |   24.97 |    25.90 |     24.99 |
| `lru_cache`               | 30.20 |   28.10 |    28.09 |     28.10 |

A fold of a million `Float`s: 1,032,270 allocations in debug and with `unbox` off, 32,269 in release.

Compile cost: a clean release build of `compile_large` takes 1.78 times the instructions of a clean debug build, and std's release first use 2.47 times its debug first use. A warm `cheby build --release` of `hello_world` retires 0.148 G instructions against 0.137 G in debug, and of `nbody` 0.287 G against 0.179 G.

Found on the way: release `rc/tree_inserts` retired 14.86 G instructions with the inliner of D-1776 against 7.05 G with inlining off. An inlined `int::compare` left an `Order` switch that was not threaded, and that stopped a reuse. Threading now checks only that the values live elsewhere do not change.

Left open:

- A callback called by a runtime kernel is out of reach of clones: `map::upsert`'s, which `ordered_map` and `word_freq` use for every update. `lru_cache` gains nothing from this step.
- A cycle of several functions is cloned one member at a time, so its calls between members stay generic.
- Helpers copied with a clone are copied again for each clone.

### Readings after these, all changes of the round together

Against the executables of `7a586d82`.

| Program          | Debug, instructions | Debug, cycles | Release, instructions | Release, cycles |
| ---------------- | ------------------: | ------------: | --------------------: | --------------: |
| `fannkuch_redux` |              −70.8% |        −71.3% |                −78.7% |          −80.2% |
| `lru_cache`      |              −66.5% |        −71.1% |                −68.8% |          −72.1% |
| `spectral_norm`  |              −59.1% |        −64.1% |                −61.7% |          −66.7% |
| `mandelbrot`     |              −49.7% |        −55.4% |                −53.2% |          −56.1% |
| `nbody`          |              −35.3% |        −46.2% |                −40.7% |          −48.8% |
| `json_roundtrip` |              −19.1% |        −21.4% |                −20.6% |          −21.9% |
| `word_freq`      |               −7.4% |        −34.5% |                −11.0% |          −35.2% |
| `binary_trees`   |               −1.7% |         −9.8% |                 −4.7% |          −10.7% |

### A list pattern step on a shared leaf, inline (D-1781)

The inline step of `[x, ..rest]` needed a leaf that nothing else holds, so a function that walks a list it shares called the runtime at every step. The step now dups the element from a shared leaf and writes nothing to it. `spectral_norm` at 1100: 9.96 G instructions before and 7.18 G after in release, 1.61 G cycles before and 0.97 G after. The list code of the runtime falls from 42% of its samples to 13%.

### Raw `Float`s in list leaves: measured, not built (D-1781)

This was the ninth row of the list that started the round. After the step above, a build that skipped the dup and the drop of an element's box on the shared path retired 9.7% fewer instructions in `spectral_norm` and took 11.9% fewer cycles, about 14 instructions per element. That, plus one load per element, is what raw leaves could save, in one program of the comparison. The price is a second leaf representation in every list operation, and a box on every read by generic code. Not built. A read of the head as a `Float` in one step, with a flag for who owns the box, would get the same saving for less, and is left open.

### Release reads scalar constants as their values (D-1782)

In release a read of a scalar constant of any module is its value: a literal's from its source, a computed one's from its evaluated image. A loop of 50 million iterations that reads two imported computed `Float` constants retires 1.17 G instructions with the lever and 4.17 G without. The comparison's programs do not move, since their hot constants were literals of their own module, folded since D-1764. Build cost is within noise.

### Final readings of the round

Against the executables of `7a586d82`, with the same compiler in both profiles (`e9b09ee1`). The machine was quieter for this reading than for the baseline's, so the instruction columns are the ones to trust.

| Program          | Debug, instructions | Debug, cycles | Release, instructions | Release, cycles |
| ---------------- | ------------------: | ------------: | --------------------: | --------------: |
| `fannkuch_redux` |              −70.8% |        −71.2% |                −78.7% |          −79.8% |
| `spectral_norm`  |              −70.2% |        −75.0% |                −72.4% |          −76.8% |
| `lru_cache`      |              −66.5% |        −76.1% |                −68.8% |          −76.8% |
| `mandelbrot`     |              −49.8% |        −55.7% |                −53.2% |          −56.3% |
| `nbody`          |              −35.0% |        −45.5% |                −40.5% |          −49.2% |
| `json_roundtrip` |              −19.3% |        −22.4% |                −20.8% |          −24.1% |
| `word_freq`      |               −7.3% |        −43.2% |                −10.9% |          −44.2% |
| `binary_trees`   |               −1.7% |        −11.1% |                 −4.7% |          −11.5% |

The three concurrency programs are still unread: their counts move too much from run to run on a loaded machine. The comparison's own runner, on a quiet machine, is the reading for them.

### What the round leaves open

- `map::upsert`'s callback: a kernel calls it through a trampoline, with a `Some` and a closure allocated per call. It is the largest cost left in `lru_cache` and `word_freq`, and neither clones nor inlining reach it. An update in two steps, find and then store, would.
- An allocation fast path written inline, with a slow path that preserves registers, for the float spills around allocation calls in `nbody`.
- Release records for a package's own modules, so that a release build is incremental.
- Threading `ctx` through the list module's own allocations, about 4% of `nbody`.
- `string::split`'s copies in `word_freq`, a `map` operation that removes a key and returns its value, a block cache that adapts, clones of a whole cycle.

## 2026-11-23: the comparison measured again

The Cheby rows of `../cheby-compare` were measured again at `e6c538a4` with its own runner, on a quiet machine: medians of 10 timed runs, or 3 for a run over 5 seconds. The executable row is now `cheby build --release`. The `cheby run` row stays the debug profile, by the user's choice, so the report shows both. "Before" is the reading of 2026-11-22 at `7a586d82`, when both rows were debug builds.

| Benchmark        | Executable before | Executable now | Against Rust | `cheby run` now | Rank of the executable |
| ---------------- | ----------------: | -------------: | -----------: | --------------: | ---------------------: |
| `binary_trees`   |            1.06 s |         1.03 s |        1.04× |          1.06 s |               15 of 21 |
| `nbody`          |           27.24 s |        15.70 s |        30.5× |         17.22 s |               15 of 21 |
| `spectral_norm`  |           21.48 s |         5.62 s |        7.00× |          6.04 s |               14 of 21 |
| `mandelbrot`     |            1.67 s |         946 ms |        1.32× |          967 ms |               13 of 21 |
| `fannkuch_redux` |            3.74 s |         722 ms |        4.65× |          1.07 s |               13 of 21 |
| `word_freq`      |            2.07 s |         1.88 s |        5.36× |          1.93 s |               12 of 21 |
| `json_roundtrip` |            3.76 s |         3.04 s |        7.85× |          3.13 s |               12 of 21 |
| `lru_cache`      |           25.75 s |         9.51 s |        14.4× |          9.90 s |               15 of 21 |
| `ping_pong`      |             57 ms |          56 ms |      0.0827× |           64 ms |                2 of 23 |
| `spawn_many`     |             76 ms |          52 ms |      0.0649× |           61 ms |                3 of 23 |
| `prime_sieve`    |             19 ms |          21 ms |      0.0398× |           29 ms |                1 of 23 |

Geometric means against Rust, before and now: the executables 9.68× and 4.24× on the CPU group, 13.1× and 8.47× on the data group, 10.8× and 5.49× on both together. `cheby run`: 9.62× and 4.78×, 13.5× and 8.75×, 10.9× and 6.00×. On CPU and data together the executables now stand between Clojure (5.13×) and Koka (5.70×), where they stood with the BEAM languages (11.0× to 14.7×).

Compile time did not pay for it. `cheby build --release` of `hello_world` takes 54 ms, as the debug build did. For `compile_large`'s 200 modules a cold `cheby run` takes 302 ms where it took 571 ms, a cold `cheby build --release` 539 ms where the debug build took 716 ms, and a cold `cheby check` 80 ms where it took 304 ms, all three because of D-1764.

Checks behind the round: the whole suite passes on the final code (8,265 tests), with std's own tests in both profiles, the determinism check in both profiles, and every lint of CI. Miri, the address sanitizer and the thread sanitizer passed on `68d73eee`, which holds every runtime change of the round except the shared-leaf step of D-1781; that one passed Miri's list group and the address sanitizer's runtime step on its own.

## 2026-11-23: the two rows that were measured and not built

The round's first report left two rows of the original list measured and not built. Both are built now.

### A list element read as a `Float` in one step (D-1783)

The user chose the narrow form of the ninth row over raw `Float`s in list leaves. When an element is only unboxed to a `Float`, the read gives the number: from a shared leaf, and for an indexed read, with no count update at all. `spectral_norm` at 1100 retires 6.17 G instructions in release where it retired 6.79 G, 9.1% fewer, and 8.4% fewer in debug. `nbody` and `fannkuch_redux` do not move: their loops read no floats out of lists. From a unique leaf the gain is small, since the box is still freed there.

### Allocation and sized frees inline, with slow paths that save every register (D-1784)

The block cache got a fixed layout that generated code reads through the control block. An allocation pops a block and a sized free pushes one, with no call, and the calls that are left on those paths save every register, so a function that allocates keeps its `Float`s in registers. `nbody`'s inner function had 22 float spill stores and 27 reloads in release and has none.

Instructions retired, in units of 10⁹, release executables, minimum of three runs:

| Program          | Before | After | Change |
| ---------------- | -----: | ----: | -----: |
| `nbody`          |  38.68 | 32.44 | −16.1% |
| `binary_trees`   |  21.46 | 18.92 | −11.8% |
| `lru_cache`      |  28.09 | 25.54 |  −9.1% |
| `json_roundtrip` |  23.21 | 21.73 |  −6.4% |
| `fannkuch_redux` |  1.611 | 1.528 |  −5.2% |
| `word_freq`      |  24.98 | 23.96 |  −4.1% |

`spectral_norm`, `spawn_many` and `ping_pong` are within 3%. Peak memory is unchanged, but for 0.5 MB more in `spawn_many`.

Tried and not kept:

- A refill of one block per miss: `binary_trees`, whose trees are built from an empty cache, retired 24.81 G instructions against 21.46 G before the change, because every allocation then went through the register-saving stub. Batches of 16, 32 and 64 gave 19.10 G, 18.93 G and 19.01 G.
- Sized frees through the stub in place of the inline push: `nbody` 35.70 G against 32.44 G, and `word_freq` slower than before the change.

D-1772 had measured this path at 2% to 3.5% and left it. The estimate counted the instructions of the call and not the spills around it.

Open: a toolchain built with debug assertions sends every sized free through the stub, to keep D-1772's size check, and so pays more there than before. The x86-64 stubs are first built in Linux CI.

### The comparison measured once more, with both rows built

The Cheby rows of `../cheby-compare` at `e9b6af0b`, on a quiet machine, with the comparison's own runner. This reading replaces the one above in its results file.

| Benchmark        | Executable on 11-22 | Executable now | Against Rust | `cheby run` now | Rank of the executable |
| ---------------- | ------------------: | -------------: | -----------: | --------------: | ---------------------: |
| `binary_trees`   |              1.06 s |         900 ms |       0.908× |          898 ms |               13 of 21 |
| `nbody`          |             27.24 s |        13.16 s |        25.6× |         13.63 s |               15 of 21 |
| `spectral_norm`  |             21.48 s |         5.16 s |        6.43× |          5.58 s |               14 of 21 |
| `mandelbrot`     |              1.67 s |         945 ms |        1.32× |          966 ms |               13 of 21 |
| `fannkuch_redux` |              3.74 s |         679 ms |        4.37× |          999 ms |               13 of 21 |
| `word_freq`      |              2.07 s |         1.86 s |        5.31× |          1.90 s |               12 of 21 |
| `json_roundtrip` |              3.76 s |         2.88 s |        7.45× |          2.86 s |               13 of 21 |
| `lru_cache`      |             25.75 s |         8.93 s |        13.5× |          9.21 s |               15 of 21 |
| `ping_pong`      |               57 ms |          55 ms |      0.0807× |           62 ms |                2 of 23 |
| `spawn_many`     |               76 ms |          51 ms |      0.0637× |           61 ms |                3 of 23 |
| `prime_sieve`    |               19 ms |          21 ms |      0.0388× |           27 ms |                1 of 23 |

Geometric means against Rust: the executables 3.86× on the CPU group, 8.12× on the data group and 5.11× on both together, where they stood at 9.68×, 13.1× and 10.8× on 2026-11-22. `cheby run`, the debug profile: 4.29×, 8.25× and 5.48×. On CPU and data together the executables are level with Clojure (5.13×) and ahead of Koka (5.70×). `binary_trees` now runs faster than the Rust program.

Compile time: `cheby build --release` of `hello_world` takes 56 ms, and of `compile_large`'s 200 modules 536 ms.

The whole suite passes on the final code (8,280 tests), with std's own tests and the determinism check in both profiles and every lint of CI.

Miri, the address sanitizer and the thread sanitizer passed every step on `e9b6af0b`, which holds every change of the round, with no report.

## 2026-11-23: `map::upsert` without the kernel calling back (D-1785)

The round left one cost named as the largest: the callback of `map::upsert`, which a kernel called through a trampoline, with a `Some` and usually a closure allocated per call. The user asked for it to be built too. A direct call is now a block of steps in generated code: a helper finds or makes the entry's slot, the callback is called like any function, and the result is stored in the slot. In release the callback is inlined and the `Some` goes.

Instructions retired, in units of 10⁹, and allocations in millions, release executables, minimum of three runs:

| Program                        | Instructions before | After | Change | Allocations before | After |
| ------------------------------ | ------------------: | ----: | -----: | -----------------: | ----: |
| `lru_cache` 100000 4000000     |               25.55 | 19.14 | −25.1% |               40.8 |  18.8 |
| `word_freq`                    |               24.01 | 20.51 | −14.6% |               33.2 |  21.4 |
| `rc/map_updates`               |                3.95 |  2.98 | −24.6% |                3.1 |  0.33 |
| `collections/ordered_map`      |                2.57 |  1.99 | −22.4% |                3.7 |   1.6 |
| `collections/ordered_set`      |                2.61 |  2.12 | −18.6% |                2.9 |   1.1 |
| `collections/ordered_multimap` |                3.96 |  3.26 | −17.6% |                8.6 |   5.7 |
| `collections/multimap`         |                3.95 |  3.74 |  −5.4% |                5.3 |   5.3 |

In debug the same programs retire 5% to 15% fewer instructions: the trampoline is gone, the `Some` and the closure stay. `json_roundtrip` and `nbody` do not move.

Tried and not kept: the steps as three arms in MIR. A record taken apart before the upsert and rebuilt after it then lost its reuse, since a reuse token does not cross a join, and `multimap`'s reuse hits fell from 1.68 million to 0.08 million. The steps stay in one block, and the inliner's guard keeps a callback as a call where inlining it would lose such a reuse, as in `multimap::insert`.

Left open: the clean-up does not substitute a constant field of a constructor it takes apart, because that gave invalid MIR in `std::option`'s release tests, for a reason not yet found. The other kernels that call back, `list::sort_with`, `heap`'s comparator calls and the hooks of equality, hashing and debug text, are unchanged.

### The comparison with it

The Cheby rows of `../cheby-compare` at `4480f4ae`, on a quiet machine, with the comparison's own runner. This reading replaces the earlier ones of the day in its results file.

| Benchmark        | Executable on 11-22 | Executable now | Against Rust | `cheby run` now | Rank of the executable |
| ---------------- | ------------------: | -------------: | -----------: | --------------: | ---------------------: |
| `binary_trees`   |              1.06 s |         892 ms |       0.900× |          872 ms |               13 of 21 |
| `nbody`          |             27.24 s |        13.04 s |        25.4× |         13.72 s |               15 of 21 |
| `spectral_norm`  |             21.48 s |         5.18 s |        6.45× |          5.57 s |               14 of 21 |
| `mandelbrot`     |              1.67 s |         942 ms |        1.32× |          963 ms |               13 of 21 |
| `fannkuch_redux` |              3.74 s |         675 ms |        4.35× |          993 ms |               13 of 21 |
| `word_freq`      |              2.07 s |         1.56 s |        4.48× |          1.72 s |               11 of 21 |
| `json_roundtrip` |              3.76 s |         2.85 s |        7.37× |          2.91 s |               12 of 21 |
| `lru_cache`      |             25.75 s |         7.33 s |        11.1× |          8.77 s |               15 of 21 |
| `ping_pong`      |               57 ms |          54 ms |      0.0801× |           63 ms |                2 of 23 |
| `spawn_many`     |               76 ms |          50 ms |      0.0624× |           59 ms |                3 of 23 |
| `prime_sieve`    |               19 ms |          21 ms |      0.0391× |           26 ms |                1 of 23 |

Geometric means against Rust: the executables 3.85× on the CPU group, 7.15× on the data group and 4.85× on both together, where they stood at 9.68×, 13.1× and 10.8× on 2026-11-22. `cheby run`, the debug profile: 4.26×, 7.89× and 5.37×. On CPU and data together the executables are ahead of Clojure (5.13×) and Koka (5.70×).

### Checks behind the final code

- The whole suite, std's own tests in both profiles, the determinism check in both profiles and every lint of CI pass locally.
- Miri, the address sanitizer and the thread sanitizer passed every step on `4480f4ae`, which holds every runtime change, with no report.
- CI passed all 17 jobs on `b4cfc7aa` (run 45196654481): Linux and Windows on x86-64 and aarch64, and macOS. Its first run on this branch failed on three lints that only other targets see and on one test that asserted aarch64's frame shape; the stubs themselves needed no change. Before that run, the x86-64 stubs had passed the runtime's tests under Rosetta.
- One test was timing-dependent and failed once under load: it required an interrupt to arrive during a short loop. It now loops until one has.

