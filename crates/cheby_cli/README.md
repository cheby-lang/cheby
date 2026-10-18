# cheby_cli

The `cheby` binary: every command of spec §13.1 (D-031). `check`, `run`, `test`, `repl` and `explain` work. `new`, `build`, `fmt`, `lsp`, `doc`, `add`, `update` and `fetch` report the milestone that brings them and exit with status 1. This crate parses the command line, sets up the worker pool and the color mode, calls the right library and turns its result into output and an exit status. It contains no compiler logic.

## Place in the pipeline

`cheby_cli` depends on `cheby_driver` for `check`, `run` and `test`, `cheby_repl` for `repl`, `cheby_diag` for rendering and `explain`, `cheby_runtime` for the program state and the leak check, and on the M5 skeletons `cheby_fmt`, `cheby_lsp`, `cheby_doc` and `cheby_pkg`. Nothing depends on it. It is the top of the graph.

- Diagnostics go to standard error in the layout of D-418. Errors that are not about the program, such as a missing `cheby.toml`, are `error:` lines without a code (D-442).
- Exit status is 0 on success and 1 on any error. `cheby run` exits with the program's status (§10.10): 0, 1 when `main` returns `Err`, 101 after a panic, 103 on a global deadlock, and in debug builds of the toolchain 102 when objects leak (D-458, D-724).
- `--deny-warnings` renders every warning as an error and runs nothing after one (D-549). `--target` checks the code as it runs on one target (D-550). `-j N` sizes the worker pool (D-551). `--color` and the environment decide coloring, which only adds escapes (`color.rs`). Panic reports of a running program stay plain (D-449).
- The command runs on a thread named `cheby` with 256 MiB of reserved stack, because parts of the compiler recurse over the source's structure (D-601).

## Layout

| Path           | What it holds                                                                                                                                                                                             |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/main.rs`  | The clap command tree and each command's body                                                                                                                                                             |
| `src/color.rs` | When output is colored: the `--color` flag, `NO_COLOR` and whether the stream is a terminal                                                                                                               |
| `tests/`       | End-to-end tests on the built binary: `run`, `explain`, color, closed streams, blocking IO under fibers, the REPL on the runtime, parallel `cheby test` and the naive-versus-Perceus differential (D-718) |

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
