# cheby_lsp

The language server behind `cheby lsp` (spec §13.11). It speaks LSP 3.17 over stdio through `lsp-server` and `lsp-types` (D-1351), and publishes the diagnostics `cheby check` reports for each package (D-1348). It also answers the requests that need only a document's text: outline, folding, selection ranges and formatting.

Each run checks the package from its text, open documents and disk together, with no cache and no incremental reparsing: the server re-runs the whole front end after every edit (D-1361, D-1363). It neither reads nor writes the build cache.

## Place in the pipeline

`cheby_lsp` runs and reads `cheby_driver::analysis` (D-1350), and reaches no phase crate directly. It also names `cheby_syntax` and `cheby_fmt` in its `Cargo.toml` for the features of later waves. Only `cheby_cli` depends on it. Editor highlighting does not go through this crate: it comes from the sibling `tree-sitter-cheby` grammar, which `cargo xtask cross-check` keeps in step with the compiler parser (D-272).

## What the server does

- **Handshake and shutdown** are the server's own, not `lsp-server`'s helpers (D-1377). Before `initialize` a request is answered `ServerNotInitialized`. After `shutdown` every request is answered `InvalidRequest`. `exit`, or the end of input, ends the server with status 0 after `shutdown` and 1 before it. A frame whose body is not a JSON-RPC message ends it too, with status 1 and one line on stderr, since `lsp-server`'s reader stops there (D-1430).
- **Capabilities:** `positionEncoding`, `textDocumentSync`, the five text requests below, and workspace folders with change notifications. The server advertises a part of D-1379's surface only once it serves it, and a test checks that the advertised requests are exactly the routed ones. The encoding is UTF-8 if the client offers it, else UTF-32, else UTF-16 (D-1368).
- **Documents:** incremental sync. A document routes by its own path, never by `rootUri` or the workspace folders (A §6.4): a file under the materialized std directory is std's, and any other belongs to the package of its nearest `cheby.toml` when it is a module of that package or its manifest (D-1375). Each package root gets one `Session`. A workspace folder that is a package root starts its session at `initialized`. Any other session ends 120 s after its last document closes, and any session ends at once when its `cheby.toml` goes. Its files then get an empty publish, and the analysis thread drops its `Session`. Versions may restart at 0 on reopen, as Zed's do, since a text is named by its edit stamp, not its version (D-1445).
- **Single-file mode** (D-1371, A §6.4): a `.cheby` document with no `cheby.toml` above it, or inside a package without being one of its modules, gets a session of its own, keyed by its path. The driver checks it as the only module of a package with a synthetic manifest, as the REPL checks its entries, and never evaluates its constants. A file outside every package gets all of the run's diagnostics but W0005. A file inside a package gets its phase-A diagnostics only, since its imports of the package's modules cannot resolve. Its jobs and requests go as a package's do, and it is published as an open document of a package is. It ends when the document closes, with one empty publish. When a `cheby.toml` appears above it, the routing at the next open, close, save or watched manifest moves it into that package. Zed's window for a file outside its worktree, rooted at the file's directory with no workspace folders, is this case. Any other file that no package takes, such as a `cheby.toml` whose package is gone, gets no session and is never published.
- **Std documents** are never published, and formatting one answers `null` (A §6.3).
- **Disk changes:** a client that registers watchers dynamically gets one for `**/*.cheby` and `**/cheby.toml` at `initialized`, and a package outside every folder gets relative patterns of its own (A §8). A package with no watcher is swept every 2 s by the `cheby-sweep` thread, which only stats files. An open, close or save sweeps as well (D-1374). A new or deleted `cheby.toml` routes the open documents again, and so does every open, close and save, for a client with no watcher.
- **Runs:** an edit starts a debounce, `analysis.debounceMs`. One thread, `cheby-analysis`, with a 256 MiB stack, runs every package's jobs, one at a time (D-601). A job is the Q run on live text, then the D run when the Q run plans one and no newer edit supersedes it (D-1347). A panic in a run ends that run only (D-1362). A panic while the session forgets changed files drops the session, and the next job starts a fresh one (D-1431).
- **Requests** go by class (A §3.3, D-1367). A `Text` request reads the document's live text and its own parse, so it never waits. A `Doc` or `Package` request waits for a snapshot of the exact text it names, by edit stamp (D-1445), answers `ContentModified` when the text moved on first, and `RequestFailed` after `analysis.requestWaitMs`. It waits for edits only, never for the job of a save, a settings change, a sweep or a watched file (D-1443). No request of this wave is of those two classes yet. Handlers run on the `cheby-query-N` threads, `clamp(cores / 2, 2, 4)` of them. A panicking handler is answered `InternalError`, and its thread goes on (A §3.7). `$/cancelRequest` answers a waiting request `RequestCancelled` at once and stops a running one at its next check.
- **Text requests:** `documentSymbol`, hierarchical or flat, `foldingRange`, `selectionRange`, `formatting` and `rangeFormatting`. Formatting runs `cheby fmt`'s formatter and answers its line diff, in LF, so a CRLF file is converted as `cheby fmt` converts it. Range formatting keeps the hunks that touch the range (D-1399). It compares lines with a CR before the LF ignored, so a CRLF file's hunks stay local, and writes a kept hunk with CRLF where the lines it replaces have it (D-1444). A kept hunk is taken whole, so it can reach a neighbouring line, such as the blank lines above a reformatted function. A file with a grammar error formats to `null`.
- **Publishing** follows D-1373 and D-1442. Every file of a package and `cheby.toml` are published, open or closed. A publish is skipped when the path's last one had the same version and the same list, so a save, a settings change or a sweep that changes nothing sends nothing. `didOpen` and `didClose` forget the path's last publish, so an opened document still gets a publish with its version, even an empty one. A file that leaves the package gets one empty publish. Only the nearest package publishes a nested package's files. A request waits only for the text it names (D-1367, D-1443), so it is no barrier for a run that a save, a settings change or a sweep starts: a test that needs one sends an edit and waits for that version's publish, since a package's jobs run in order. `cheby/idle` (D-1415), the barrier D-1443 names for clients, is not sent yet. An empty range is widened to the character before it, so editors can show it.
- **Settings** come from `initializationOptions`, then `workspace/configuration` with the section `cheby`, then `didChangeConfiguration`'s `settings.cheby` (D-1376). A later layer wins per key. A value of the wrong type or an unknown key is skipped with a warning, and `null` means no opinion. A push that carries `settings.cheby` is used as it is. Only a push without that key makes the server ask with `workspace/configuration`, since Zed pushes to every server even when nothing changed.
- **Standard output** carries protocol frames only. The crate denies `clippy::print_stdout`.

## Handle confinement

The driver's generation handles, such as `Ty` and `DefId`, are valid for one run only, and a stale one gives a silent wrong answer (D-1366). `clippy.toml` in this crate bans their types everywhere, and the `features` module is the one place that may name them, under a single `#[expect]` once a handler does. The lint sees written type names only, so a value held through inference is review's job. The driver's types have no `Serialize`, so a handle cannot become JSON either, which a `compile_fail` doctest in `lib.rs` shows. A crate-local `clippy.toml` replaces the root one, so it repeats the root's four settings.

## Layout

| Path                     | What it holds                                                                              |
| ------------------------ | ------------------------------------------------------------------------------------------ |
| `src/lib.rs`             | `ServerOptions { build, child }`, `run` over stdio, `run_on` over any connection for tests |
| `src/analysis_thread.rs` | The analysis thread, its `Session`s, jobs and events                                       |
| `src/server/mod.rs`      | The main loop, the handshake and shutdown                                                  |
| `src/server/state.rs`    | Open documents and packages, as plain data                                                 |
| `src/server/docs.rs`     | `didOpen`, `didChange`, `didClose` and `didSave`                                           |
| `src/server/sessions.rs` | Routing by path, std directories, sessions and their end                                   |
| `src/server/watch.rs`    | File watchers, `didChangeWatchedFiles` and the sweep thread                                |
| `src/server/schedule.rs` | The debounce, which package runs next, and requests waiting for a snapshot                 |
| `src/server/pool.rs`     | The query threads, and how a request reaches them and is answered                          |
| `src/server/publish.rs`  | The publishing rules, and diagnostics in protocol form                                     |
| `src/server/caps.rs`     | The client's capabilities as read, and the server's                                        |
| `src/server/settings.rs` | The settings and their layers                                                              |
| `src/server/routes.rs`   | The dispatch table of methods, with each query's class                                     |
| `src/server/effects.rs`  | Messages a step sends, applied in order by the main loop                                   |
| `src/server/*tests.rs`   | Tests in process, over `run_on`                                                            |
| `src/features/mod.rs`    | `Analyzed`, `RequestCx`, `DocText`, `HandlerOutput`, request classes and errors            |
| `src/features/handlers/` | One function per LSP method                                                                |
| `src/features/ide/`      | Feature logic free of protocol types: tokens at a cursor, outlines, line diffs             |
| `src/text/lines.rs`      | The line index: byte offsets to positions in each encoding, BOM and line-end rules         |
| `src/text/uri.rs`        | `file:` URIs to paths and back, with the Windows rules testable on every platform          |
| `clippy.toml`            | The handle lint of D-1366                                                                  |

`ServerOptions.child` is the `cheby` executable for child jobs, `Some(current_exe())` from `cheby lsp` and `None` in tests in process, so a test binary never runs as the child. No child job exists yet.

## Tests

- `cargo test -p cheby_lsp` runs the tests in process, through `run_on`: capabilities, settings, publishing and shutdown, requests and their waits (`request_tests.rs`), routing, sessions and watchers (`session_tests.rs`), and single-file mode (`single_file_tests.rs`). The handlers have unit tests, and every example formats to no edit. The text module has property tests with fixed seeds: offsets to positions and back against a reference model, edits applied through positions, and paths through URIs and back.
- `cargo test -p cheby_cli --test lsp` runs the built `cheby lsp` over stdio, with diagnostics parity against `cheby check` and the UI goldens. [`tests/lsp/README.md`](../../tests/lsp/README.md) describes it.

## Status

The protocol skeleton, diagnostics, single-file mode, request dispatch, file watching and the text requests are in. Navigation, completion, child jobs and the rest come in later waves, one commit each on the `lsp` branch (D-1354, [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md)).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §4.2, the analysis API, and §4.3, the server.
- [`docs/plan/testing.md`](../../docs/plan/testing.md) §10, the tests.
- [`crates/README.md`](../README.md) for the whole crate graph.
