# cheby_jit

The JIT: a Cranelift `JITModule` that compiles batches of MIR units, the runtime's symbol registry, the indirection table of top-level functions (D-043) and the way into generated code. `cheby run`, `cheby test`, the REPL and constant evaluation all run on it. AOT output comes with `cheby_link` in M3.

## Place in the pipeline

`cheby_jit` depends on `cheby_codegen_clif`, `cheby_runtime`, `cheby_mir` and `cheby_iface`, and on `cranelift-jit`, `cranelift-codegen` and `cranelift-module`. `cheby_consteval` and `cheby_driver` depend on it.

- **Code model:** the JIT compiles for `cheby_codegen_clif::isa::host_isa`, the target's fixed feature set (D-921), never the host's detected features, so it runs the code the cache and executables hold (D-913, ADR-0064). Runtime and foreign symbols resolve into the session's import words `cb$imp$<symbol>`, data the code loads, shared by the session's later batches. No Cranelift libcall resolves.

- **Fed by** `cheby_codegen_clif`: `Jit::compile` is incremental. Each batch registers its layouts in the runtime's layout table, declares its functions, extending the persistent declaration table, defines them and finalizes. `cheby run` compiles one batch, and constant evaluation and the REPL compile one per step. Later batches call earlier code through the existing slots and reuse the statics earlier batches defined (D-523).
- **Binds to** `cheby_runtime`: native symbols are resolved against the runtime's registry and the process before any code runs, and every missing one is E0053 at its `@external` (D-455, D-532). Under `cheby check` a missing symbol is bound to a stub, and only a constant that calls it is an error.
- **Slots and redefinition:** every call of a top-level function loads its slot, a writable data object. The REPL repoints a slot when a redefinition keeps the signature and allocates a new one otherwise (D-456), reverts the repoints of a rejected entry, and publishes an entry's slots only once the constants its code reads are filled (D-832). Slots are written with release stores, because a session's detached fibers may run the code while a later entry compiles (D-712).
- **Entering code:** the host never calls `tail`-convention code. It calls C-ABI entry thunks through `Entry`, which borrows the `Jit` so safe code cannot run freed code. `Entry::run_main` applies the exit-status policy of §10.10, and `Entry::run_root` runs an entry as a root fiber of a `Runtime` (D-690). A panicking entry unwinds by the return flag (D-733).
- **Memory:** code and data live in one reservation of 1 GiB of address space per `Jit`, committed segment by segment, so every PC-relative relocation reaches on x86_64 and aarch64. A JIT's code memory belongs to a keep-alive token that the runtime holds until its fibers have stopped (D-708).

## Layout

| Module                | What it holds                                                                                                                                                                                     |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `lib.rs`              | `Jit`, `Entry`, the symbol registry, layout registration, slots and session names                                                                                                                 |
| `load.rs`             | Loading code records into the JIT, one batch per command: declarations, definitions, finalizing and registration (D-914, D-918, D-1169)                                                           |
| `memory.rs`           | The address-space reservation, its segments and their protection                                                                                                                                  |
| `native.rs`           | `NativeLibrary` (a package's shared library, `dlopen`ed and never closed) and D-941's resolver: the runtime, the package libraries in order, the process; library generations of a REPL `:reload` |
| `tests/`              | End-to-end tests that compile and run small programs: scalars, objects, control, closures, literals, reuse, borrowing, unwinding, large frames, `@blocking`, `@std_context` and REPL redefinition |
| `start_root_tests.rs` | Running entries as root fibers                                                                                                                                                                    |

Tests that build or run JIT code carry `#[cfg_attr(windows, ignore = …)]` until M6 (D-545, D-641).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.6, native code generation, and §3.8, lifecycle and hosts.
- [`crates/README.md`](../README.md) for the whole crate graph.
