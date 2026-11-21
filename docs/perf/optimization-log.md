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

