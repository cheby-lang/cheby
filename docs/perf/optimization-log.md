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

