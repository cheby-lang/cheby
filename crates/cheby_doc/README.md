# cheby_doc

The documentation generator behind `cheby doc` (spec §13.10). It will render the `///` item comments and `//!` module comments of a package and of the standard library.

## Place in the pipeline

`cheby_doc` depends on `cheby_driver`, through which it gets a checked package with its interfaces and doc comments. Only `cheby_cli` depends on it.

## Status

Planned for M5. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
