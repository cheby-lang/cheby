# The crates

The reference implementation of Cheby is one Cargo workspace of 21 `cheby_*` crates (D-270, D-284, D-928). This file says what each crate is for and how the crates depend on each other. Each crate has a `README.md` of its own with its layout. [`docs/plan/architecture.md`](../docs/plan/architecture.md) describes the pipeline, the intermediate representations and the runtime in depth, and is the source for anything this file leaves out.

Every crate is `publish = false`, shares the workspace's version, edition and lints, and is built, tested and linted by the commands in [`AGENTS.md`](../AGENTS.md).

## The pipeline

A build runs the phases below in order. The crate named on the right owns each phase.

```
cheby.toml, src/            cheby_package    manifest, module discovery, import graph
  │
  ▼
lex, parse, CST, AST views  cheby_syntax     lossless tree, grammar errors, validation
  │
  ▼
names                       cheby_resolve    imports, item collection, scopes, resolution
  │
  ▼
interfaces and types        cheby_types      signatures into a ModuleInterface, bodies into THIR
  │                         cheby_iface      the ModuleInterface model that every phase shares
  ▼
MIR                         cheby_mir        lowering, borrowing, dup/drop, reuse, unwind edges
  │
  ├──► Cranelift IR         cheby_codegen_clif
  │      ├──► JIT           cheby_jit        run, test, repl, constants (cheby_consteval)
  │      └──► object files  cheby_link       AOT (M3)
  └──► JavaScript           cheby_js         (M4)

running code                cheby_runtime    memory, kernels, fibers, channels, unwinding
                            cheby_rt_exe     the runtime archive linked into executables (M3)
```

`cheby_driver` runs the phases over a package and `cheby_cli` is the `cheby` binary that calls it. `cheby_diag` sits under everything and holds spans, the source map and diagnostics.

## Dependency graph

The crates depend on each other in pipeline order (D-284, D-465). The table lists each crate's internal dependencies as its `Cargo.toml` names them, dev-dependencies left out. A crate may use everything its dependencies export, and nothing of any crate it does not list.

| Crate                | Depends on                                                                                                                                                                                                 |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cheby_diag`         | nothing internal                                                                                                                                                                                           |
| `cheby_runtime`      | nothing internal                                                                                                                                                                                           |
| `cheby_rt_exe`       | `cheby_runtime`, and the external `mimalloc` (D-928); nothing depends on it                                                                                                                                |
| `cheby_syntax`       | `cheby_diag`                                                                                                                                                                                               |
| `cheby_package`      | `cheby_diag`                                                                                                                                                                                               |
| `cheby_iface`        | `cheby_diag`, `cheby_package`                                                                                                                                                                              |
| `cheby_resolve`      | `cheby_diag`, `cheby_syntax`, `cheby_package`, `cheby_iface`                                                                                                                                               |
| `cheby_types`        | `cheby_diag`, `cheby_syntax`, `cheby_resolve`, `cheby_iface`                                                                                                                                               |
| `cheby_mir`          | `cheby_diag`, `cheby_types`, `cheby_iface`, `cheby_resolve`                                                                                                                                                |
| `cheby_codegen_clif` | `cheby_mir`, `cheby_iface`, `cheby_diag`, `cheby_runtime` (its `abi` module only)                                                                                                                          |
| `cheby_jit`          | `cheby_codegen_clif`, `cheby_runtime`, `cheby_mir`, `cheby_iface`                                                                                                                                          |
| `cheby_consteval`    | `cheby_diag`, `cheby_iface`, `cheby_jit`, `cheby_mir`, `cheby_package`, `cheby_runtime`                                                                                                                    |
| `cheby_link`         | `cheby_codegen_clif`                                                                                                                                                                                       |
| `cheby_js`           | `cheby_mir`                                                                                                                                                                                                |
| `cheby_driver`       | `cheby_diag`, `cheby_syntax`, `cheby_package`, `cheby_resolve`, `cheby_iface`, `cheby_types`, `cheby_mir`, `cheby_codegen_clif`, `cheby_jit`, `cheby_runtime`, `cheby_link`, `cheby_js`, `cheby_consteval` |
| `cheby_repl`         | `cheby_diag`, `cheby_driver`, `cheby_runtime`                                                                                                                                                              |
| `cheby_fmt`          | `cheby_diag`, `cheby_syntax`                                                                                                                                                                               |
| `cheby_lsp`          | `cheby_driver`, `cheby_syntax`, `cheby_fmt`                                                                                                                                                                |
| `cheby_doc`          | `cheby_driver`                                                                                                                                                                                             |
| `cheby_pkg`          | `cheby_package`                                                                                                                                                                                            |
| `cheby_cli`          | `cheby_diag`, `cheby_driver`, `cheby_repl`, `cheby_fmt`, `cheby_lsp`, `cheby_doc`, `cheby_pkg`, `cheby_runtime`                                                                                            |

Read bottom-up, the layers are: `cheby_diag` and `cheby_runtime` at the bottom, then `cheby_syntax` and `cheby_package`, then `cheby_iface`, the vocabulary of every later phase, then `cheby_resolve`, `cheby_types` and `cheby_mir` in order, then the backends, then `cheby_driver`, then the tools and the binary. `cheby_rt_exe` stands apart: it wraps `cheby_runtime` into the archive that `cheby build` links into executables, and no crate depends on it (D-928, D-929).

Three rules shape the graph:

- **Locality (D-246).** `cheby_types` checks a module from its own tree and the `ModuleInterface`s of its dependencies, and no crate offers another module's bodies. A feature that needs them does not compile.
- **One ABI, two sources (D-453, D-465).** `cheby_mir::abi` is the only source of machine signatures. `cheby_runtime::abi` is the only source of object headers, layout ids, context offsets and symbol names. `cheby_codegen_clif` reads that one module of the runtime and never calls the runtime.
- **The driver is the only orchestrator.** Only `cheby_driver` depends on every compiler crate. Tools build on the driver, never on the phases directly.

## The crates

| Crate                                                | Milestone | Purpose                                                                          |
| ---------------------------------------------------- | --------- | -------------------------------------------------------------------------------- |
| [`cheby_diag`](cheby_diag/README.md)                 | M1        | Spans, source map, diagnostic codes and their rendering                          |
| [`cheby_syntax`](cheby_syntax/README.md)             | M1        | Lexer, parser, lossless CST, typed AST views, validation                         |
| [`cheby_package`](cheby_package/README.md)           | M1        | `cheby.toml`, module discovery, import graph                                     |
| [`cheby_iface`](cheby_iface/README.md)               | M1        | Ids, names, types and the `ModuleInterface` every later phase shares             |
| [`cheby_resolve`](cheby_resolve/README.md)           | M1        | Imports, item collection, scopes, name resolution                                |
| [`cheby_types`](cheby_types/README.md)               | M1        | Type checker: interfaces, THIR, exhaustiveness, warnings                         |
| [`cheby_mir`](cheby_mir/README.md)                   | M1, M2    | MIR, lowering from THIR, borrowing, reference counting, reuse, unwind edges      |
| [`cheby_codegen_clif`](cheby_codegen_clif/README.md) | M1        | MIR to Cranelift IR, shared by the JIT, AOT and constant evaluation              |
| [`cheby_jit`](cheby_jit/README.md)                   | M1        | The Cranelift JIT, the indirection table, entries into generated code            |
| [`cheby_consteval`](cheby_consteval/README.md)       | M1        | Compile-time evaluation of constants in the JIT                                  |
| [`cheby_runtime`](cheby_runtime/README.md)           | M1, M2    | Native runtime: memory, kernels, generic operations, fibers, channels, unwinding |
| [`cheby_rt_exe`](cheby_rt_exe/README.md)             | M3        | The runtime archive of executables: the runtime with mimalloc, as a staticlib    |
| [`cheby_driver`](cheby_driver/README.md)             | M1        | Runs the phases over a package: `check`, `run`, `test`, REPL sessions, dumps     |
| [`cheby_repl`](cheby_repl/README.md)                 | M1        | The interactive session of `cheby repl`                                          |
| [`cheby_cli`](cheby_cli/README.md)                   | M1        | The `cheby` binary and the conformance, UI and REPL harnesses                    |
| [`cheby_link`](cheby_link/README.md)                 | M3        | AOT object output and linker invocation (skeleton)                               |
| [`cheby_js`](cheby_js/README.md)                     | M4        | JavaScript emitter and suspension analysis (skeleton)                            |
| [`cheby_fmt`](cheby_fmt/README.md)                   | M3, M5    | Formatter (`cheby fmt`); its later parts land beside M3 (D-1029)                 |
| [`cheby_lsp`](cheby_lsp/README.md)                   | M5        | Language server (skeleton)                                                       |
| [`cheby_doc`](cheby_doc/README.md)                   | M5        | Documentation generator (skeleton)                                               |
| [`cheby_pkg`](cheby_pkg/README.md)                   | M5        | Dependency resolution, lockfile and fetching (skeleton)                          |

The milestone column names the milestone that gives the crate its first real contents. Milestones are described in [`docs/plan/README.md`](../docs/plan/README.md). M1 and M2 are done and M3 is in progress. `cheby_rt_exe` and `cheby_fmt` have their first contents; the other crates of M3 to M5 are still three-line skeletons that exist so the workspace's shape is fixed from the start (D-284).

## Outside `crates/`

The workspace has five more members. Three of them use these crates:

| Member            | Uses                                                          | For                                                                      |
| ----------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `xtask`           | `cheby_driver`, `cheby_syntax`, `cheby_diag`, `tests/support` | Developer commands such as `bless`, `cross-check` and `bench-compile`    |
| `bench/gen`       | `cheby_driver`                                                | The synthetic project generator for compile-speed budgets (D-291)        |
| `fuzz`            | `cheby_driver`, `cheby_syntax`, `cheby_diag`                  | `cargo fuzz` targets, outside the workspace                              |
| `tests/support`   | none                                                          | Shared harness code: milestones, goldens, example and transcript metadata |
| `bench/baselines` | none                                                          | Rust baselines of the collections benchmarks in `bench/runtime` (D-664)  |

The standard library in [`std/`](../std/) is Cheby source, embedded in the `cheby` binary by `cheby_driver` and bound to its native kernels in `cheby_runtime` through `@external(native, …)` (D-484).
