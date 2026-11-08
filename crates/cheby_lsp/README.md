# cheby_lsp

The language server behind `cheby lsp` (spec §13.11). It speaks LSP 3.17 over stdio through `lsp-server` and `lsp-types` (D-1351), and publishes the diagnostics `cheby check` reports for each package (D-1348). It answers no other request yet.

Each run checks the package from its text, open documents and disk together, with no cache and no incremental reparsing: the server re-runs the whole front end after every edit (D-1361, D-1363). It neither reads nor writes the build cache.

## Place in the pipeline

`cheby_lsp` runs and reads `cheby_driver::analysis` (D-1350), and reaches no phase crate directly. It also names `cheby_syntax` and `cheby_fmt` in its `Cargo.toml` for the features of later waves. Only `cheby_cli` depends on it. Editor highlighting does not go through this crate: it comes from the sibling `tree-sitter-cheby` grammar, which `cargo xtask cross-check` keeps in step with the compiler parser (D-272).

## What the server does

- **Handshake and shutdown** are the server's own, not `lsp-server`'s helpers (D-1377). Before `initialize` a request is answered `ServerNotInitialized`. After `shutdown` every request is answered `InvalidRequest`. `exit`, or the end of input, ends the server with status 0 after `shutdown` and 1 before it.
- **Capabilities:** `positionEncoding` and `textDocumentSync` only. The server advertises a part of D-1379's surface only once it serves it. The encoding is UTF-8 if the client offers it, else UTF-32, else UTF-16 (D-1368).
- **Documents:** incremental sync. Each open document belongs to the package of its nearest `cheby.toml` (D-1375), and each package root gets one `Session`. There is no file watcher yet, so an open, close or save also sweeps the package's files on disk.
- **Runs:** an edit starts a debounce, `analysis.debounceMs`. One thread, `cheby-analysis`, with a 256 MiB stack, runs every package's jobs, one at a time (D-601). A job is the Q run on live text, then the D run when the Q run plans one and no newer edit supersedes it (D-1347). A panic ends that run only.
- **Publishing** follows D-1373. Every file of a package and `cheby.toml` are published, open or closed. An open document gets a publish with its version after each run, even an empty one. A closed file is published only when its list changed. A file that leaves the package gets one empty publish. An empty range is widened to the character before it, so editors can show it.
- **Settings** come from `initializationOptions`, then `workspace/configuration` with the section `cheby`, then `didChangeConfiguration`'s `settings.cheby` (D-1376). A later layer wins per key. A value of the wrong type or an unknown key is skipped with a warning, and `null` means no opinion.
- **Standard output** carries protocol frames only. The crate denies `clippy::print_stdout`.

## Handle confinement

The driver's generation handles, such as `Ty` and `DefId`, are valid for one run only, and a stale one gives a silent wrong answer (D-1366). `clippy.toml` in this crate bans their types everywhere, and the future `features` module will be the one place that may name them, under a single `#[expect]`. The lint sees written type names only, so a value held through inference is review's job. The driver's types have no `Serialize`, so a handle cannot become JSON either, which a `compile_fail` doctest in `lib.rs` shows. A crate-local `clippy.toml` replaces the root one, so it repeats the root's four settings.

## Layout

| Path                     | What it holds                                                                              |
| ------------------------ | ------------------------------------------------------------------------------------------ |
| `src/lib.rs`             | `ServerOptions { build, child }`, `run` over stdio, `run_on` over any connection for tests |
| `src/analysis_thread.rs` | The analysis thread, its `Session`s, jobs and events                                       |
| `src/server/mod.rs`      | The main loop, the handshake and shutdown                                                  |
| `src/server/state.rs`    | Open documents and packages, as plain data                                                 |
| `src/server/docs.rs`     | `didOpen`, `didChange`, `didClose` and `didSave`                                           |
| `src/server/schedule.rs` | The debounce, and which package runs next                                                  |
| `src/server/publish.rs`  | The publishing rules, and diagnostics in protocol form                                     |
| `src/server/caps.rs`     | The client's capabilities as read, and the server's                                        |
| `src/server/settings.rs` | The settings and their layers                                                              |
| `src/server/routes.rs`   | The dispatch table of methods                                                              |
| `src/server/effects.rs`  | Messages a step sends, applied in order by the main loop                                   |
| `src/server/tests.rs`    | Tests in process, over `run_on`                                                            |
| `src/text/lines.rs`      | The line index: byte offsets to positions in each encoding, BOM and line-end rules         |
| `src/text/uri.rs`        | `file:` URIs to paths and back, with the Windows rules testable on every platform          |
| `clippy.toml`            | The handle lint of D-1366                                                                  |

`ServerOptions.child` is the `cheby` executable for child jobs, `Some(current_exe())` from `cheby lsp` and `None` in tests in process, so a test binary never runs as the child. No child job exists yet.

## Tests

- `cargo test -p cheby_lsp` runs the tests in process: capabilities, settings, publishing and shutdown, through `run_on`. The text module has property tests with fixed seeds: offsets to positions and back against a reference model, edits applied through positions, and paths through URIs and back.
- `cargo test -p cheby_cli --test lsp` runs the built `cheby lsp` over stdio, with diagnostics parity against `cheby check` and the UI goldens. [`tests/lsp/README.md`](../../tests/lsp/README.md) describes it.

## Status

The protocol skeleton and diagnostics are in. Requests, file watching, single-file mode and child jobs come in later waves, one commit each on the `lsp` branch (D-1354, [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md)).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §4.2, the analysis API, and §4.3, the server.
- [`docs/plan/testing.md`](../../docs/plan/testing.md) §10, the tests.
- [`crates/README.md`](../README.md) for the whole crate graph.
