# cheby_cli

The `cheby` binary: every command of spec §13.1 (D-031). `check`, `build`, `run`, `test`, `repl`, `fmt` and `explain` work. `new`, `lsp`, `doc`, `add`, `update` and `fetch` report the milestone that brings them and exit with status 1, and so does `build --target js` (M4, D-282). This crate parses the command line, sets up the worker pool and the color mode, calls the right library and turns its result into output and an exit status. It contains no compiler logic.

## Place in the pipeline

`cheby_cli` depends on `cheby_driver` for `check`, `build`, `run` and `test`, `cheby_repl` for `repl`, `cheby_diag` for rendering and `explain`, `cheby_runtime` for the program state and the leak check, `cheby_fmt` for `fmt`, and on the M5 skeletons `cheby_lsp`, `cheby_doc` and `cheby_pkg`. Nothing depends on it. It is the top of the graph.

- Diagnostics go to standard error in the layout of D-418. Errors that are not about the program, such as a missing `cheby.toml`, are `error:` lines without a code (D-442).
- Exit status is 0 on success and 1 on any error. `cheby run` exits with the program's status (§10.10): 0, 1 when `main` returns `Err`, 101 after a panic, 103 on a global deadlock, and in debug builds of the toolchain 102 when objects leak (D-458, D-724).
- `--deny-warnings` renders every warning as an error and runs nothing after one (D-549). `--target` checks the code as it runs on one target (D-550). `-j N` sizes the worker pool (D-551). `--color` and the environment decide coloring, which only adds escapes (`color.rs`). Panic reports of a running program stay plain (D-449).
- `cheby fmt` formats every `.cheby` file under the package's `src/`, or the files it is given, and `-` reads standard input and writes standard output (D-1031). A file with a syntax error is reported and left alone. A file is rewritten only when it changes, through a temporary file renamed over it. `--check` writes nothing, lists the files that would change, one per line, and exits with 1 if any would.
- `cheby build [module_path] [--target native|js] [-o PATH] [--deny-warnings]` builds a module's `main` into an executable for the host, `build/native/<package>` (or `build/native/<module path with :: replaced by ->`) unless `-o` says otherwise (D-937, D-936). It links with `cc` from `PATH` against the runtime archive `libcheby_rt_exe.a` next to `cheby`, which `cargo build -p cheby_rt_exe` builds (`cargo test` does not); a missing or stale archive is a tool error (D-929, D-930). User types and their `@std_hooks` reach executables through the module records, which the executable's start registers (D-919, D-920). The constants a program reaches reach its executable as images, installed before `main` (D-979, D-1187 to D-1189). With a cache it lowers through `cheby run`'s code records and takes constants from their entries, so a warm build compiles and evaluates only what misses (D-1285, D-1286). Windows executables are M6's (D-935). `tests/build.rs` tests it end to end, comparing executables with `cheby run`; `cargo xtask check-determinism` builds every runnable example twice from two paths and compares the executables (D-934, D-1164).
- `check`, `build`, `run` and `test` open the build cache (D-959): `CHEBY_CACHE`, or the OS default, with the build ID. When it cannot be created or written, the command uses a temporary cache, deleted when it ends, and prints one note on standard error before anything else. A relative `CHEBY_CACHE`, an unsafe root and a temporary cache that cannot be created are tool errors. `CHEBY_CACHE_LOG` names a file that gets every lookup, sorted (D-973). The cache is closed, and a temporary one deleted, before a program or the tests start. `check`, `build` and `test` trim it at most once a day (D-961). `tests/cache.rs` tests this end to end.
- The command runs on a thread named `cheby` with 256 MiB of reserved stack, because parts of the compiler recurse over the source's structure (D-601).

## Layout

| Path              | What it holds                                                                                                                                                                                                    |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `build.rs`        | The build ID (D-957): SipHash-1-3 128 of every file under `crates/` and `std/`, `Cargo.lock`, the toolchain file, the profile, the target, the cfgs and the runtime ID (D-930)                                   |
| `src/main.rs`     | The clap command tree and each command's body                                                                                                                                                                    |
| `src/build_id.rs` | `build_id()`, the build ID the cache keys and file headers carry; its tests run `build.rs`'s hashing                                                                                                             |
| `src/color.rs`    | When output is colored: the `--color` flag, `NO_COLOR` and whether the stream is a terminal                                                                                                                      |
| `tests/`          | Tests of the built binary: `run`, `build`, `explain`, `fmt`, color, closed streams, blocking IO under fibers, the REPL on the runtime, parallel `cheby test` and the naive-versus-Perceus differential (D-718)   |

## The harnesses

The three harnesses of [`tests/`](../../tests/) are test targets of this crate, so `cargo test` builds `cheby` first:

| Target        | Source                | Inputs                                                            |
| ------------- | --------------------- | ----------------------------------------------------------------- |
| `conformance` | `tests/conformance/`  | The examples in `docs/examples/` and their goldens                |
| `ui`          | `tests/ui_harness/`   | One rule per test under [`tests/ui/`](../../tests/ui/README.md)   |
| `repl`        | `tests/repl_harness/` | The transcripts under [`tests/repl/`](../../tests/repl/README.md) |

Run one with `cargo test -p cheby_cli --test <name>`. They follow `tests/milestone` (D-288), share `tests/support`, and `cargo xtask bless` rewrites their goldens (D-292).

## Read more

- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.1, the commands.
- [`docs/plan/testing.md`](../../docs/plan/testing.md) for the harnesses.
- [`crates/README.md`](../README.md) for the whole crate graph.
