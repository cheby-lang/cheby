# Standard-library workstream

The standard library is specified in `docs/stdlib/`, one file per module (D-204, D-220), and implemented in `std/`. Both run as a workstream parallel to the compiler milestones, in tiers. A tier's spec is written and reviewed before the milestone that needs it starts, and implemented during that milestone (D-276).

## 1. How modules are implemented

Data-structure and text kernels live in the runtime: Rust in `cheby_runtime` for native, hand-written ES modules in `std/` for JS. They are exposed to Cheby as `@external` functions. Everything that can be written over those kernels, such as `map`, `fold`, `filter`, `join` and the `Result` and `Option` helpers, is written in Cheby (D-274):

```cheby
// std/list.cheby
@external(native, "cheby_list_push")
@external(js, "./list.mjs", "push")
pub fn push<T>(xs: List<T>, x: T) -> List<T>

pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U> {
  fold(xs, new(), fn(acc, x) { push(acc, f(x)) })
}
```

- Kernels follow the native FFI ownership convention (D-187), and update in place when the argument's count is one, so they reuse memory even before Perceus exists.
- Kernels never grow the stack with the input size, on either target (D-094).
- Once Perceus reuse (M2) and release-mode specialization (M6) are measured, each kernel is reviewed: those whose Cheby version is within an agreed factor of the Rust one move to Cheby. The review is a plan item of M6, and each move is a normal change, because the API does not change.
- `std` may use private runtime intrinsics that user code cannot name, such as raw array access for kernels written in Cheby later. Their names and the mechanism that hides them are decided when the first one is needed.

## 2. Tiers

The tiers are seeded from what the examples use. The function lists are the examples' current calls, which the spec must define or rename. A rename updates the examples in the same change.

### Tier 1: before M1

| Module                                      | Seeded from the examples                                                                                                                            |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `std::io`                                   | `println`, `eprintln`, `read_line`, `read_file`                                                                                                     |
| `std::env`                                  | `args`, `get` (D-183)                                                                                                                               |
| `std::list`                                 | `all`, `any`, `contains`, `each`, `filter`, `fold`, `get`, `is_empty`, `length`, `map`, `reverse`, `sort`, `take`, plus the kernels (`new`, `push`, ...) |
| `std::map`                                  | `new`, `from_list`, `get`, `insert`, `remove`, `size`, `to_list`, `to_sorted_list`, `upsert`, `values`                                              |
| `std::set`                                  | `from_list`, `contains`, `insert`, `remove`, `size`, `fold`, `filter` (no API exists yet, D-204)                                                    |
| `std::string`                               | `compare`, `join`, `length`, `lowercase`, `uppercase`, `pad_start`, `pad_end`, `split`, `starts_with`, `to_code_points`, `trim`, `trim_end`        |
| `std::bytes`                                | `from_list`, `from_string`, `to_list`, `fold`, the builder of D-095                                                                                 |
| `std::int`                                  | `parse`, `compare`, `checked_add/sub/mul/div`, `from_u32` and other widenings, `wrapping_*` (D-025)                                                  |
| `std::float`                                | `parse`, `truncate`, `round`, `from_u32` and other conversions (D-085), text of floats (§3.3.6)                                                     |
| `std::i8` ... `std::u64`, `std::f32`        | `from_int` and the narrowing conversions (D-085)                                                                                                    |
| `std::result`, `std::option`                | `try`, `map`, `map_error`, `unwrap_or`, ...                                                                                                         |
| `std::ops`                                  | the operator interfaces (D-169, §8.7)                                                                                                               |
| `std::json`                                 | `parse`, `to_string`, `decode` and the decoder and encoder API of example 011 (D-209, D-210)                                                        |

`std::json` is the largest module and the only one written almost entirely in Cheby. If its spec is late, example 011 moves to the M2 exit list rather than delaying M1.

### Tier 2: before M2

| Module           | Seeded from the examples                                                                                 |
| ---------------- | -------------------------------------------------------------------------------------------------------- |
| `std::fiber`     | `scope`, `spawn`, `spawn_detached`, `join`, `timeout`, `message`, the `Scope`, `Fiber`, `Panic` and `Timeout` types (D-174, D-181, D-199) |
| `std::channel`   | `new`, `send`, `receive`, `Sender`, `Receiver` (D-054, D-179)                                            |
| `std::selector`  | `new`, `receive`, `after`, `select` (D-041, D-180)                                                        |
| `std::duration`  | `Duration`, `seconds`, `milliseconds`                                                                    |
| `std::random`    | `int_between`, `between`, seeding                                                                        |
| `std::runtime`   | setting the number of scheduler threads (D-175)                                                          |

The tier-2 spec fixes the provisional names of D-174 and D-199. If it renames anything, spec chapter 10 and the examples are updated in the same change.

### Tier 3: before M3 and M4

| Module                   | Purpose                                                                                     |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| `std::file`              | file handles and `use f <- file::with_open(path)` (D-101)                                   |
| `std::net`               | TCP sockets on the poller (D-042)                                                           |
| `std::http`              | `get`, `serve`, `header`, `with_header`, `method_name`, `json`, used by examples 030 and 031 |
| `std::time`              | wall clock and monotonic time                                                               |
| `std::process`           | exit with a status, spawning processes if v1 needs it                                       |

Tier 3 also covers the JS side of every earlier module: the `.mjs` kernels, and IO through the host's asynchronous APIs with `@async` (D-188).

## 3. Done criteria for a module

A module is done when:

1. its spec file in `docs/stdlib/` is reviewed and its decisions are in the log,
2. it is implemented on every target whose milestone has started,
3. every function has a doc comment with at least one example, which `cheby test` runs from M5 on (D-197),
4. it has unit tests in `test` blocks, and kernel property tests where it has kernels ([testing.md](testing.md#3-unit-and-property-tests)),
5. `cheby doc` output matches the spec file module by module (D-220), checked from M5 on.
