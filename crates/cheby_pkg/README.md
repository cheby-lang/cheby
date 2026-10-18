# cheby_pkg

The package manager: dependency resolution, the lockfile and fetching (spec §13.4), behind `cheby add`, `cheby update` and `cheby fetch`.

## Place in the pipeline

`cheby_pkg` depends on `cheby_package`, which already reads the `[dependencies]` of `cheby.toml`, with their versions and git references, and defines the names of dependency keys. This crate will resolve those dependencies to versions, write the lockfile and fetch sources, and `cheby_driver` will then load the fetched packages through `cheby_package` as it loads the local one. Only `cheby_cli` depends on it.

## Status

Planned for M5. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
