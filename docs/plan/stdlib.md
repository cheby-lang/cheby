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

- Every kernel is bound as `@external(native, "cheby_<module>_<function>")` and `@external(js, "./<module>.mjs", "<function>")`. Each numeric module has its own kernels, one symbol per type. A kernel calls a Cheby closure only where the stdlib spec makes it a kernel, which is `map::upsert` alone, so every other function that takes one is written in Cheby over kernels. This includes the functions of `std::fiber` and `std::selector` that take functions: they are written in Cheby over private runtime operations, `@std_context` kernels and the `scope_run` intrinsic, none of which calls a closure (D-675, D-692, D-693). `std::io`'s kernels return errors as a kind code and the operating system's message, from which `std::io` builds its opaque `Error` (D-484). The kinds are 1 for not found, 2 for permission denied (`EACCES` and `EPERM`), 3 for invalid UTF-8 read from a file or standard input, and 0 for every other failure, never a raw operating-system code. The message is the operating system's text without Rust's ` (os error N)` suffix, Rust's own text for a failure without an operating-system code, and `invalid UTF-8` for kind 3. A kernel passes a message for every kind, and `std::io` drops it for kinds 1 to 3 (D-345, D-573).
- Kernels follow the native FFI ownership convention (D-187), and update in place when the argument's count is one, so they reuse memory even before Perceus exists (D-525). An argument that is the same object as another argument is never updated in place (D-563). [`string.md`](../stdlib/string.md) lists the `std::string` kernels that update in place and those that return their argument itself (D-577).
- Kernels never grow the stack with the input size, on either target (D-094).
- Once Perceus reuse (M2) and release-mode specialization (M6) are measured, each kernel is reviewed: those whose Cheby version is within an agreed factor of the Rust one move to Cheby. The factor is OQ-P13, settled with the M2 measurements. The review is a plan item of M6, and each move is a normal change, because the API does not change.
- `std` may use private runtime operations that user code cannot name. An `@external` function marked with the std-only attribute `@std_context` receives the running fiber's context as its first argument, and may suspend, raise, use fiber state or start fibers. The tier-2 runtime kernels and the io kernels that may suspend carry it (D-692). A std external whose native symbol is in the compiler's fixed table of `cheby_rt_` intrinsics gets a compiler-generated body instead of a foreign call. M2 has one, `scope_run`, which opens a scope, calls its body and closes the scope with no Cheby call in between (D-693, ADR-0060). Later intrinsics, such as raw array access for kernels written in Cheby, join the same table.

## 2. Tiers

The tiers are seeded from what the examples use. The function lists are the examples' current calls, which the spec must define or rename. A rename updates the examples in the same change.

### Tier 1: before M1

Drafted in [`docs/stdlib/`](../stdlib/README.md) (D-294 to D-352). The drafts marked their remaining gaps as `OQ-<module>-<n>` (D-352), settled by D-353 to D-409, except OQ-io-3 and OQ-env-1, which are deferred to tier 3 as OQ-P14.

| Module                                                                  | Spec file                                                                                                                                          |
| ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `std::io`, `std::env`                                                   | [`io.md`](../stdlib/io.md), [`env.md`](../stdlib/env.md)                                                                                           |
| `std::list`, `std::string`, `std::bytes`                                | [`list.md`](../stdlib/list.md), [`string.md`](../stdlib/string.md), [`bytes.md`](../stdlib/bytes.md)                                               |
| `std::map`, `std::set`, `std::multimap`                                 | [`map.md`](../stdlib/map.md), [`set.md`](../stdlib/set.md), [`multimap.md`](../stdlib/multimap.md)                                                 |
| `std::sorted_map`, `std::sorted_set`, `std::sorted_multimap` (D-337)    | [`sorted_map.md`](../stdlib/sorted_map.md), [`sorted_set.md`](../stdlib/sorted_set.md), [`sorted_multimap.md`](../stdlib/sorted_multimap.md)       |
| `std::ordered_map`, `std::ordered_set`, `std::ordered_multimap` (D-337) | [`ordered_map.md`](../stdlib/ordered_map.md), [`ordered_set.md`](../stdlib/ordered_set.md), [`ordered_multimap.md`](../stdlib/ordered_multimap.md) |
| `std::int`, `std::float`, `std::i8` ... `std::u64`, `std::f32`          | [`int.md`](../stdlib/int.md), [`float.md`](../stdlib/float.md), and one short file per sized type (D-350)                                          |
| `std::bool`, `std::nil`, `std::order`, `std::ops`                       | [`bool.md`](../stdlib/bool.md), [`nil.md`](../stdlib/nil.md), [`order.md`](../stdlib/order.md), [`ops.md`](../stdlib/ops.md)                       |
| `std::result`, `std::option`                                            | [`result.md`](../stdlib/result.md), [`option.md`](../stdlib/option.md)                                                                             |
| `std::json`, `std::json::decode`                                        | [`json.md`](../stdlib/json.md), [`json/decode.md`](../stdlib/json/decode.md)                                                                       |

The sorted, insertion-ordered and multimap collections are written in Cheby, with no runtime kernels (D-336, D-338). They are not shipped at M1 exit, so importing them is E0038 until they are (D-480). M2 implements and ships all seven in its workstream 2.9, and benchmarks them against a Rust tree once with naive reference counting and again with Perceus reuse, to settle OQ-P13. M6 measures them again (D-336, D-664).

`std::json` is the largest module and the only one written almost entirely in Cheby. If its spec is late, example 011 moves to the M2 exit list rather than delaying M1.

### Tier 2: before M2

Drafted in [`docs/stdlib/`](../stdlib/README.md#tier-2) (D-645 to D-675). It fixes the provisional names of D-174 and D-199 (D-646).

| Module          | Spec file                              | Names the examples use                                                                        |
| --------------- | -------------------------------------- | --------------------------------------------------------------------------------------------- |
| `std::fiber`    | [`fiber.md`](../stdlib/fiber.md)       | `scope`, `spawn`, `spawn_detached`, `join`, `timeout`, `message`, `Scope`, `Panic`, `Timeout` |
| `std::channel`  | [`channel.md`](../stdlib/channel.md)   | `new`, `send`, `receive`, `Sender`, `Receiver`                                                |
| `std::selector` | [`selector.md`](../stdlib/selector.md) | `new`, `receive`, `after`, `select`                                                           |
| `std::duration` | [`duration.md`](../stdlib/duration.md) | `Duration`, `seconds`, `milliseconds`                                                         |
| `std::random`   | [`random.md`](../stdlib/random.md)     | `int_between`                                                                                 |
| `std::runtime`  | [`runtime.md`](../stdlib/runtime.md)   | none; the scheduler's thread count (D-175)                                                    |

`random::between` is not a tier-2 name: it is the function of example 010's own module `guess_number::random` (D-229). Seeding is `random::from_seed`, which threads a `Generator` through calls (D-653). If the tier-2 spec renames anything the examples use, spec chapter 10 and the examples are updated in the same change.

Each tier-2 kernel and intrinsic declares its JS binding now, and the `.mjs` files follow with tier 3 in M4, as for tier 1 (D-450, D-661).

### Tier 3: before M3 and M4

| Module         | Purpose                                                                                      |
| -------------- | -------------------------------------------------------------------------------------------- |
| `std::file`    | file handles and `use f <- file::with_open(path)` (D-101)                                    |
| `std::net`     | TCP sockets on the poller (D-042)                                                            |
| `std::http`    | `get`, `serve`, `header`, `with_header`, `method_name`, `json`, used by examples 030 and 031 |
| `std::time`    | wall clock and monotonic time                                                                |
| `std::process` | exit with a status, spawning processes if v1 needs it                                        |

Tier 3 also covers the JS side of every earlier module: the `.mjs` kernels, and IO through the host's asynchronous APIs with `@async` (D-188). What file IO, standard input and `std::env` do on JS hosts, including those without a process such as browsers, is OQ-P14 (OQ-io-3, OQ-env-1).

## 3. Done criteria for a module

A module is done when:

1. its spec file in `docs/stdlib/` is reviewed and its decisions are in the log,
2. it is implemented on every target whose milestone has started,
3. every function has a doc comment with at least one example, which `cheby test` runs from M5 on (D-197),
4. it has unit tests in `test` blocks next to its code in `std/`, which user builds skip (D-479) and `cargo xtask test-std` runs (D-542), among them every test of its spec file, in its own module or in one that imports it, with every helper item named by a written path (D-630, D-631), and kernel property tests where it has kernels ([testing.md](testing.md#3-unit-and-property-tests)),
5. `cheby doc` output matches the spec file module by module (D-220), checked from M5 on.
