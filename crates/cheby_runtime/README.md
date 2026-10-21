# cheby_runtime

The native runtime: memory, the kernels behind the standard library, generic equality, hashing and debug text, panics, and from M2 fibers, channels, scopes, unwinding and IO. It is Rust, built as an rlib for the JIT and as a staticlib for AOT linking, and it exposes a C ABI to generated code.

## Place in the pipeline

`cheby_runtime` depends on no other `cheby_*` crate. It depends on `mio`, `parking_lot`, `crossbeam-deque`, `crossbeam-utils`, `getrandom`, `libc`, `ryu`, `siphasher` and `unicode-segmentation` (D-730, D-656), and it uses no `wasmtime-internal-*` crate.

- **`abi` is the contract with code generation.** It has no state and no dependencies, and is the single source of header bits, tagged-integer encoding, layout ids, string layout, context offsets, exit statuses, panic kinds and symbol names. `cheby_codegen_clif` reads only this module (D-465). `cheby_mir::abi` holds the other half of the contract, the machine signatures.
- **`cheby_jit`** resolves generated code's imports against `symbols`, the registry every kernel module fills, registers user layouts in `layout` and runs entries through `entry` and `sched`.
- **`cheby_consteval`** uses `consteval`, the constant table and the forcing hook. **`cheby_repl`** reads its prompt through `io::read_input_line`, the one buffered reader of standard input (D-575). **`cheby_driver`** and **`cheby_cli`** create the `Runtime` of a run, a test run or a session (D-690) and read the leak check.
- **`std/`** binds every kernel as `@external(native, "cheby_<module>_<function>")` (D-484). A kernel never calls a Cheby closure, except `map::upsert` through `callback`, and a `@std_context` kernel gets the running fiber's `ctx` first and may suspend or raise (D-692).

Rules that hold across the crate:

- Every C-ABI entry point is `extern "C"`, so an internal Rust panic aborts instead of unwinding through JIT frames. A Cheby unwinding inside Rust is a Rust unwinding with a zero-sized marker, caught by a guard at the entry point (D-697).
- `tls.rs` is the only module that declares thread-locals, and every access goes through an `#[inline(never)]` accessor, because a fiber may migrate between threads across a switch (D-686).
- Objects and bytes are counted per accounting domain, and debug builds fail a run with status 102 when objects leak (D-445, D-458, D-691). `CHEBY_RT_POISON=1` poisons freed objects, and `CHEBY_RT_STATS=1` turns on the counters of in-place updates (D-635, D-768).
- A runtime kind that declares a `CustomKind::shape` must describe exactly what its `size` and `children` compute, and a kind with a finalizer or a handle kind declares none. Debug builds check the shape on every free (D-907).
- A fiber suspends only through `park`, which releases the lock it holds after the switch, so no lock, borrow or thread-local pointer lives across a switch (D-687).

## Layout

| Area          | Modules                                                                                                                                                                                                                                                                                                                                                                            |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Contract      | `abi.rs`, `symbols.rs`                                                                                                                                                                                                                                                                                                                                                             |
| Memory        | `alloc.rs` with `alloc/scratch.rs` (allocation, iterative free, domains, poisoning, scratch buffers), `rc.rs` (`dup`/`drop`, uniqueness, shared marking), `reuse.rs`, `object.rs`, `layout.rs` (the global layout table and the free-shape table)                                                                                                                                  |
| Values        | `values.rs`, `string.rs`, `desc.rs` (type descriptors), `handles.rs` (runtime kinds 40 to 44), `generic/` (equality, hashing, debug text, `@std_hooks`)                                                                                                                                                                                                                            |
| Kernels       | `text/` (`std::string`), `bytes/`, `list/` (RRB trees, D-563), `hamt/` (`std::map` and `std::set`), `num/` (numeric modules, with the correctly rounded functions of D-396), `io.rs`, `env.rs`, `random.rs`, `runtime_info.rs`                                                                                                                                                     |
| Running code  | `ctx.rs` (the fiber control block, D-685), `entry.rs`, `panic.rs`, `callback.rs`, `consteval.rs`, `stats.rs`, `capture.rs` (captured output of parallel tests, D-723)                                                                                                                                                                                                              |
| Fibers        | `fiber/` (stack slots and the `naked_asm!` context switch for x86-64 SysV and AAPCS64, D-676, D-677), `sched/` (one `Runtime` per host run: workers, Chase-Lev deques, idle protocol, monitor, poller, timers, parking, lifecycle, D-689), `fiber_ops/` (spawn, scopes, cancellation, join, channels, selectors, detached fibers), `pool/` (the process-wide blocking pool, D-683) |
| Unwinding     | `unwind/` (the return flag, raise, the unwind record, guards and the entry check's releasing slow paths, D-733)                                                                                                                                                                                                                                                                    |
| Thread-locals | `tls.rs`                                                                                                                                                                                                                                                                                                                                                                           |

## Tests

- Unit tests sit next to the code, and `proptest-regressions/` keeps the failing cases of the property tests of `generic`, `num` and `text`.
- The `models.rs`, `select_models.rs`, `park_models.rs` and `retire_models.rs` files are `loom` models of the protocols, not of the runtime's code. They build with plain `cargo test -p cheby_runtime`.
- The `dev` profile builds this crate with `opt-level = 2`, so the debug toolchain runs Cheby code on an optimized runtime while the leak check and poisoning stay on.
- Until M6 the crate builds with stubs on Windows, and entries run on dedicated threads there (D-714).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.7, value representation, and §3, the runtime, with §3.1 to §3.8 for M2.
- [`docs/spec/09-memory-model.md`](../../docs/spec/09-memory-model.md), [`docs/spec/10-concurrency.md`](../../docs/spec/10-concurrency.md) and [`docs/spec/11-errors-and-panics.md`](../../docs/spec/11-errors-and-panics.md).
- [ADR-0024](../../docs/adr/0024-koka-style-shared-marking-for-rc.md), [ADR-0058](../../docs/adr/0058-own-fiber-context-switch.md), [ADR-0059](../../docs/adr/0059-integrated-io-poller-on-mio.md), [ADR-0060](../../docs/adr/0060-std-intrinsics-and-the-fused-scope-sequence.md), [ADR-0061](../../docs/adr/0061-return-flag-unwinding.md) and [ADR-0062](../../docs/adr/0062-parallel-tests-with-ordered-output.md).
- [`crates/README.md`](../README.md) for the whole crate graph.
