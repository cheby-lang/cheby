# cheby_iface

The shared vocabulary of every phase after syntax: dense ids, interned names, closed types, the `ModuleInterface` and the builtin module (spec §13.12.1, D-249). It holds data and the operations on that data, and runs no phase itself.

## Place in the pipeline

`cheby_iface` depends on `cheby_diag` and `cheby_package`. `cheby_resolve`, `cheby_types`, `cheby_mir`, `cheby_codegen_clif`, `cheby_jit`, `cheby_consteval` and `cheby_driver` all depend on it. It sits below name resolution because resolution already reads the interfaces of a module's dependencies (D-465).

- **The locality rule (D-246)** is built into the types here. A `ModuleInterface` holds exactly what §13.12.1 lists: function signatures, types, interfaces, aliases and constant types. `InterfaceSet` and `DepView` give a module the interfaces of its dependencies and nothing else, so a phase has no way to reach another module's bodies.
- **Interface hashing (D-460)** encodes the non-`priv`, non-test items of an interface canonically, sorted by namespace and name and referring to other definitions by path, never by index, and hashes the encoding with SipHash-1-3. Nothing caches on it yet. The REPL and the language server use it to skip re-checking dependents whose inputs did not change, and M3's build cache will store it as the interface artifact.
- **The builtin module** is the prelude's declarations, with no source. Each builtin type records its home module in `std` (D-447), and `Map`, `Set` and `Bytes` are items of the same module that are not in the prelude (D-481).

## Layout

| Module       | What it holds                                                                                            |
| ------------ | -------------------------------------------------------------------------------------------------------- |
| `index.rs`   | `IndexVec` and `newtype_index!`: dense ids whose missing entries are internal errors, not panics (D-423) |
| `name.rs`    | `Name`, `Interner` and `sym`: interned identifiers                                                       |
| `ids.rs`     | `PackageId`, `ModuleId`, `DefId` and the `ModuleTable`                                                   |
| `def.rs`     | `DefKind`, `Namespace` and `Vis`                                                                         |
| `ty.rs`      | `Ty`, `TyKind`, generics and the per-process `TyInterner`, with `display`                                |
| `iface.rs`   | `ModuleInterface` and the item signatures it holds                                                       |
| `hash.rs`    | The canonical encoding and `interface_hash`                                                              |
| `set.rs`     | `InterfaceSet`, `DepView` and `def_path`                                                                 |
| `builtin.rs` | The builtin module: the prelude's declarations                                                           |

## Read more

- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.12.1, the locality rule.
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.2 and §2.3.
- [`crates/README.md`](../README.md) for the whole crate graph.
