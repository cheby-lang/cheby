# M5: Tooling

Goal: `cheby fmt`, `cheby lsp`, `cheby doc` and the package manager, all part of v1 (D-031). This work grows alongside M2 to M4 (D-099, step 5). M5 is the point where it is complete, not where it starts.

Detail level: workstreams with the milestone each slice lands next to.

The formatter core, the LSP core and `cheby new` were placed with M2 and did not land. The formatter core and `cheby new` run beside M3 in a lane of their own, as packages T1, T1b and T2 of [m3-aot-and-cache.md](m3-aot-and-cache.md). After M3's second checkpoint, where the user chooses the line width, one change formats `docs/examples/**/src` and `std/`, blesses the goldens whose locations moved after a review by eye, and adds `cheby fmt --check` to CI. Files written later are formatted by their authors. The LSP core sits on the driver's front-end APIs, which M3's cache rewrites, so it lands with M4, starting no earlier than M3's third checkpoint. LSP navigation and constant evaluation move behind it, and the protocol's open questions are settled in its own design step then (D-1029).

## Slices

| Slice                   | Lands with                  | Contents                                                                                                                                                                                                                                                      |
| ----------------------- | --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Formatter core          | M3                          | `cheby fmt` and `--check` over the lossless CST, zero options (D-079), comments and blank lines kept; a pretty printer driven by one line width, with imports sorted within their block (D-1030); the command's arguments and `-` for standard input (D-1031) |
| LSP core                | M4, after M3's checkpoint 3 | diagnostics, hover with types, go to definition, document symbols; in-memory state, queries allowed inside the server (D-249, D-1029)                                                                                                                         |
| `cheby new`             | M3                          | package skeleton: `cheby.toml`, a hello-world `src/main.cheby` that is a `cheby fmt` fixed point, and a `.gitignore` (D-097, D-1032)                                                                                                                          |
| LSP navigation          | M4, after the LSP core      | find references, rename, completion, formatting through `cheby fmt` (§13.11)                                                                                                                                                                                  |
| LSP constant evaluation | M4, after the LSP core      | constants evaluated under the limits of §13.9, never running `main` or tests                                                                                                                                                                                  |
| `cheby doc`             | M4                          | HTML from `pub` items and doc comments, interfaces satisfied structurally, Cheby highlighting (D-078, §13.10)                                                                                                                                                 |
| Doc examples as tests   | M4                          | code blocks in doc comments run by `cheby test` (D-197, D-265)                                                                                                                                                                                                |
| JSON code action        | M4                          | `to_json` and decoder generation in the LSP (D-210, D-215, D-224, D-225)                                                                                                                                                                                      |
| Editor integration      | M4                          | `zed-cheby` points at `cheby lsp`; tree-sitter stays for highlighting                                                                                                                                                                                         |
| Package manager         | M5                          | `cheby add`, `update`, `fetch`, git dependencies, lockfile with checksums, package identity by URL and major (D-044, D-097, D-238, D-239), minimal version selection (D-192), paths of dependency files in locations (D-468)                                  |

## Exit criteria

- Every example and every `std` source is a fixed point of `cheby fmt`, and the formatter passes the idempotence and meaning-preservation tests ([testing.md](testing.md#3-unit-and-property-tests)).
- The language server answers every request of §13.11 on all examples, and its diagnostics match `cheby build`.
- A package with git dependencies on two majors of one repository builds from its lockfile, and a checksum mismatch fails the build.
- 030 passes, with its git dependency served from a local fixture repository and its HTTP fixture started by the harness (D-446). The fixture repository has a stable URL that is not a `file://` path, so locations in the goldens contain no absolute path (D-468).
- `cheby doc` output for `std` matches `docs/stdlib/` module by module (D-220).
- All doc examples in `std` pass `cheby test`.
