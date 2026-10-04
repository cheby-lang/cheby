# M5: Tooling

Goal: `cheby fmt`, `cheby lsp`, `cheby doc` and the package manager, all part of v1 (D-031). This work grows alongside M2 to M4 (D-099, step 5). M5 is the point where it is complete, not where it starts.

Detail level: workstreams with the milestone each slice lands next to.

## Slices

| Slice                      | Lands with | Contents                                                                                                                    |
| -------------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------- |
| Formatter core             | M2         | `cheby fmt` and `--check` over the lossless CST, zero options (D-079), comments and blank lines kept                        |
| LSP core                   | M2         | diagnostics, hover with types, go to definition, document symbols; in-memory state, queries allowed inside the server (D-249) |
| `cheby new`                | M2         | package skeleton (D-097)                                                                                                    |
| LSP navigation             | M3         | find references, rename, completion, formatting through `cheby fmt` (§13.11)                                                 |
| LSP constant evaluation    | M3         | constants evaluated under the limits of §13.9, never running `main` or tests                                                 |
| Package manager            | M3         | `cheby add`, `update`, `fetch`, git dependencies, lockfile with checksums, package identity by URL and major (D-044, D-097, D-238, D-239), minimal version selection (D-192) |
| `cheby doc`                | M4         | HTML from `pub` items and doc comments, interfaces satisfied structurally, Cheby highlighting (D-078, §13.10)                |
| Doc examples as tests      | M4         | code blocks in doc comments run by `cheby test` (D-197, D-265)                                                               |
| JSON code action           | M4         | `to_json` and decoder generation in the LSP (D-210, D-215, D-224, D-225)                                                     |
| Editor integration         | M4         | `zed-cheby` points at `cheby lsp`; tree-sitter stays for highlighting                                                       |

## Exit criteria

- Every example and every `std` source is a fixed point of `cheby fmt`, and the formatter passes the idempotence and meaning-preservation tests ([testing.md](testing.md#3-unit-and-property-tests)).
- The language server answers every request of §13.11 on all examples, and its diagnostics match `cheby build`.
- A package with git dependencies on two majors of one repository builds from its lockfile, and a checksum mismatch fails the build.
- `cheby doc` output for `std` matches `docs/stdlib/` module by module (D-220).
- All doc examples in `std` pass `cheby test`.
