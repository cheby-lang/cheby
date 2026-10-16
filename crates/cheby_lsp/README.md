# cheby_lsp

The language server behind `cheby lsp` (spec §13.11). It will keep its own in-memory state, reparse incrementally with the cursors and parent links of `cheby_syntax`, and use the interface hashes of `cheby_iface` to skip re-checking dependents whose inputs did not change (D-249, D-460).

## Place in the pipeline

`cheby_lsp` depends on `cheby_driver` for checking, `cheby_syntax` for the tree and `cheby_fmt` for formatting on save. Only `cheby_cli` depends on it. Error recovery in the parser exists from the start because this server parses incomplete code (architecture §2.1). Editor highlighting does not go through this crate: it comes from the sibling `tree-sitter-cheby` grammar, which `cargo xtask cross-check` keeps in step with the compiler parser (D-272).

## Status

Planned for M5. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
