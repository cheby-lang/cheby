# cheby_package

The package model: reading `cheby.toml` (spec §13.3), mapping the files under `src/` to module paths (§7.2) and ordering modules by their imports, rejecting cycles (D-145). It knows the shape of a package but nothing about the code inside it.

## Place in the pipeline

`cheby_package` depends only on `cheby_diag` and the `toml` crate. It is the first thing `cheby_driver` runs, and its output tells the driver which files to parse and in which order to check them.

- **Used by** `cheby_iface`, which keys modules by the package and module ids defined here, `cheby_resolve`, which builds the import graph through `graph`, `cheby_consteval`, `cheby_driver` and `cheby_pkg`, the M5 package manager that will extend the manifest with dependency resolution.
- Problems with the package's contents are diagnostics: `E0036` for the manifest, with a snippet of it, and `E0037` for invalid file and directory names. Failures that are not about the contents, such as an unreadable directory, are `PackageError`s, which the CLI prints as plain `error:` lines without a code (D-442).

## Layout

| Module        | What it holds                                                                          |
| ------------- | -------------------------------------------------------------------------------------- |
| `manifest.rs` | `Manifest::load`: `name`, `version`, targets, dependencies, `[runtime]`, `[native]`    |
| `names.rs`    | Names of packages, modules and dependency keys, with `suggest_name` for diagnostics    |
| `discover.rs` | `discover`: `src/` files to `ModulePath`s, with the root file and the source directory |
| `graph.rs`    | Import-graph algorithms over dense node indices: build order and cycle detection       |

## Read more

- [`docs/spec/07-modules-and-packages.md`](../../docs/spec/07-modules-and-packages.md) §7.2 and [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.3.
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.2.
- [`crates/README.md`](../README.md) for the whole crate graph.
