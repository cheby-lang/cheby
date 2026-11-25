# cheby_pkg

The package manager: dependency resolution, the lockfile and fetching (spec §13.4), behind `cheby add`, `cheby update` and `cheby fetch`.

## Place in the pipeline

`cheby_pkg` depends on `cheby_package`, which reads `cheby.toml` and checks git URLs, and on `cheby_diag` for the errors of manifests. `cheby_driver` reads the lockfile and the store through it and loads the packages, and `cheby_cli` runs its commands. Only those commands use the network (D-1881).

## Contents

- `lock`: `cheby.lock` (D-1880).
- `git`: the `git` found on `PATH`, over bare mirrors (D-1876).
- `store`: the machine's store of fetched sources, and their checksums (D-1877, D-1878).
- `resolve`: minimal version selection (D-192, D-238).
- `commands`: `add`, `update` and `fetch` (D-1882).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
