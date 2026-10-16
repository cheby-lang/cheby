# cheby_fmt

The formatter behind `cheby fmt` (spec §13.7). Every Cheby source in the repository is already written in the form it will produce: two-space indentation, no semicolons, trailing commas in multi-line lists.

## Place in the pipeline

`cheby_fmt` depends on `cheby_syntax` and `cheby_diag`. It will format from the lossless concrete syntax tree that the compiler also parses (D-272, D-280), so comments and the exact text survive, and it never needs name resolution or types. `cheby_cli` and `cheby_lsp` depend on it.

## Status

Planned for M5. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
