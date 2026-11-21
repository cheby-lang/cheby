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

