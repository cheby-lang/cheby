# cheby_resolve

Imports, item collection, scopes and name resolution (spec §7.5 to §7.7). Given a module's syntax tree and the interfaces of its dependencies, it says what every path means, and reports duplicate names, visibility violations, shadowing (D-051, D-062), case conventions (E0006) and reserved words (E0007, D-469).

## Place in the pipeline

`cheby_resolve` depends on `cheby_diag`, `cheby_syntax`, `cheby_package` and `cheby_iface`. It runs after parsing and before type checking.

- **Fed by** `cheby_driver`, which calls `collect_imports` and `resolve_import_targets` for every module, builds the import graph with `cheby_package::graph`, reports cycles with `cycle_diagnostic` (E0040) and then calls `resolve_module` in build order. Standard-library modules are loaded on first use through the driver's `ModuleLookup`.
- **Feeds** `cheby_types`, which checks bodies against the resolution produced here, and `cheby_mir`, which reads constructor and item resolutions when it lowers. Both read syntax only through `cheby_syntax::ast` views keyed by the `NodeId`s stored here.
- **Locality (D-246):** a module's scope is built from its own items, its imports and the prelude, and other modules are read only through their interfaces in a `DepView`.
- The only part of the support gate (D-451) left in the front end is E0038 for an import of a standard-library module that the spec plans but the toolchain does not ship yet. Every item and construct resolves.

## Layout

| Module       | What it holds                                                                                                    |
| ------------ | ---------------------------------------------------------------------------------------------------------------- |
| `imports.rs` | Import requests, their targets and import cycles (§7.5, D-145)                                                   |
| `items.rs`   | Item collection in `DefIndex` order, with duplicate names                                                        |
| `scope.rs`   | `ModuleScope`: the module's items, its imports, the prelude and its hiding warning (D-146)                       |
| `paths.rs`   | Resolving paths: unqualified names, `m::x` through an import, and visibility (`pub`, package, `priv`, `exposed`) |
| `body.rs`    | Signatures and bodies: every path, binding and local, and the shadowing ban                                      |
| `naming.rs`  | Case conventions and reserved words where names are bound or declared                                            |
| `repl.rs`    | `SessionScope`: what a REPL entry sees of its session (§13.5, D-547)                                             |

## Read more

- [`docs/spec/07-modules-and-packages.md`](../../docs/spec/07-modules-and-packages.md) §7.5 to §7.7.
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.2.
- [`crates/README.md`](../README.md) for the whole crate graph.
