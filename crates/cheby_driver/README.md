# cheby_driver

The build driver: it runs the compiler's phases over a package (spec §13.12). It is the only crate that depends on every compiler crate, and every tool builds on it rather than on the phases directly. `check`, `prepare_run` and `prepare_test` are what `cheby check`, `cheby run` and `cheby test` call, `session` is the driver side of the REPL, and `dump_mir`, `dump_clif` and `bench_build` serve the goldens, the determinism checks and `cargo xtask bench-compile`.

## Place in the pipeline

`cheby_driver` depends on `cheby_diag`, `cheby_syntax`, `cheby_package`, `cheby_resolve`, `cheby_iface`, `cheby_types`, `cheby_mir`, `cheby_codegen_clif`, `cheby_jit`, `cheby_runtime`, `cheby_consteval`, and on the M3 and M4 skeletons `cheby_link` and `cheby_js`. `cheby_cli`, `cheby_repl`, `cheby_lsp` and `cheby_doc` depend on it, as do `xtask`, `bench/gen` and the fuzz targets.

A build runs these steps, each in the crate named:

1. **Phase A:** `cheby_package` reads `cheby.toml` and discovers the modules, and `cheby_syntax` lexes, parses and validates every file. An error here stops the later phases (D-452).
2. **Front end:** `cheby_resolve` builds the import graph and resolves names, `cheby_types` builds interfaces in import order and checks bodies, independent modules in parallel on rayon's global pool (D-263), then the module-local and whole-package warnings run.
3. **Back end**, for `run` and `test`: `cheby_mir` lowers the items reachable from `main` or the tests across modules (D-510), the RC passes run over all units, `cheby_codegen_clif` and `cheby_jit` compile one batch, and `cheby_consteval` evaluates the constants the code reaches in a JIT of their own on `plain` code (D-978), writes each result as an image and installs the images into the program's JIT before anything runs (D-979); `cheby build` puts the images into the program object instead. Unresolved native symbols (E0053) and constant errors are reported like compile errors before anything runs.
4. **Running:** `main`, each test or each REPL entry runs as a root fiber of one `cheby_runtime::Runtime` per host run (D-690), with the exit statuses of §10.10 and the leak check of debug builds.

Every phase that reads other modules' interfaces, outside std, reads them through a recording view (D-955): `Checked::front_reads` holds what resolution and checking read beyond a module's imports, and the back end's `Lowered::reads` what lowering and borrow inference read beyond those, per code unit and tests unit. Debug toolchains decode every interface they build and compare it with the original (D-954).

`cheby check`, `cheby run` and `cheby test` go through the build cache (`Cache`, D-955 to D-965). Each command first reads its package index (D-962), one hint per package root and command family that holds every module's keys and front record inline. Each package module is looked up by its front key and its recorded reads, through the index first; a miss is checked from source and its front record written, with its reference and use summaries (D-965, D-967), unless it has an error (D-964). In `cheby check` a hit is neither parsed nor checked: its interface hash comes from its record, its interface is decoded only when a checked module needs it (D-954), and its diagnostics, externals and use summary are replayed; W0005 runs over every module's use summary. The modules constant evaluation needs, those holding constants and what they import, are checked from source on fresh interfaces. Until cached code exists, `cheby run` and `cheby test` check every module once, from source, and print a hit's replayed diagnostics, which debug toolchains compare with the fresh ones. No module is checked twice, except on `cheby check`'s error path. The output never depends on the cache (D-268).

Output is deterministic whatever the number of threads (D-264): results are merged in a fixed order, the first error in build order is reported, and diagnostics are sorted by `SourceMap::sort`. `configure_jobs` sizes the pool for `-j`.

The standard library's sources in `std/` are embedded in the binary by `std_lib.rs` (D-450). A build parses and checks only the std modules its imports reach and the shipped home modules (D-447), and `test_std` runs std's own `test` blocks for `cargo xtask test-std` (D-479, D-599). A std module listed in `STAGED_STD_MODULES` is embedded but not loadable until it moves to `STD_MODULES` (D-486).

## Layout

| Module               | What it holds                                                                                                                                                          |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `lib.rs`             | Command entry points, cacheless and through a `Cache` (`check_cached`, `prepare_run_cached`), `load_package`, `Program`, the dumps, `bench_build`, `configure_jobs`    |
| `build.rs`           | Std's source hashes, computed when `cheby` is built (D-963)                                                                                                            |
| `frontend.rs`        | The build graph and the front-end phases                                                                                                                               |
| `backend.rs`         | MIR lowering, code generation, constant evaluation and running                                                                                                         |
| `borrowing.rs`       | Borrowing inference over a build's units and its records: facts and masks records, code keys (D-968, D-969)                                                            |
| `code_cache.rs`      | K4c's code records: a JIT batch of every unit through its record, fresh or from the store, and the fresh records stored (D-914, D-1193)                                |
| `cache/`             | The build cache (D-955 to D-965, D-973): the store, keys, location, trimming, `CHEBY_CACHE_LOG`; `front.rs`, front records; `replay.rs`, stored diagnostics; `index.rs`, package indexes |
| `test.rs`            | `prepare_test`, `TestProgram` and `test_std`: `cheby test` (§13.6, D-541) and std's self-test                                                                          |
| `session.rs`         | REPL sessions: checking each entry as the module `<repl:N>`, redefinition, `:module` and `:reload` (§13.5, D-606, D-625)                                               |
| `std_lib.rs`         | The embedded standard library and its catalog                                                                                                                          |
| `reuse_report.rs`    | `--reuse-report`: where reuse in place happens or not, and why (D-717)                                                                                                 |
| `lifecycle_tests.rs` | Tests of runtime creation and shutdown per host                                                                                                                        |
| `reads_tests.rs`     | Tests of the recorded hidden reads (D-955)                                                                                                                             |
| `cache_tests.rs`     | Tests of front records, their keys and replay, and the cache-hit tests (D-973)                                                                                         |
| `borrowing_tests.rs` | Tests of per-unit facts, the facts and masks records and the code keys (D-969)                                                                                         |

## Tests

`tests/golden/` holds the MIR and CLIF dumps of `001_hello_world` and of the packages under `tests/packages/`, which `cargo xtask check-determinism` compares across thread counts and paths. Many tests of the compiler crates build whole packages through this crate, so a change to its public functions is visible across the workspace.

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2, the pipeline, §4, driver, cache and processes, and §5, where the standard library lives.
- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md).
- [`crates/README.md`](../README.md) for the whole crate graph.
