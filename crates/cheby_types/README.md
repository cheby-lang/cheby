# cheby_types

The type checker. `build_interface` lowers every item's signature into the module's `ModuleInterface`, which is all that dependents ever see, and `check_bodies` checks the bodies against that interface and the interfaces of the module's dependencies, producing typed bodies (THIR). A body with an error is not returned, so MIR never sees errors.

## Place in the pipeline

`cheby_types` depends on `cheby_diag`, `cheby_syntax`, `cheby_resolve` and `cheby_iface`, and on `rayon` to check the functions of a module in parallel (D-263). `cheby_mir` and `cheby_driver` depend on it.

- **Fed by** `cheby_resolve`: the checker of module M reads M's tree, M's resolution and the `ModuleInterface`s of M's dependencies through a `DepView`. It never reads another module's tree, resolution or bodies (D-246). The driver builds interfaces in import order and then checks bodies, independent modules in parallel.
- **Feeds** `cheby_mir` with THIR, which lowers without inference or interface search. `use`, pipes and captures are desugared during checking (D-244, D-245), so later stages see only calls and closures. A `dyn I` type is closed during inference, and every dictionary carries its type's descriptor (D-499).
- Inference is local to one top-level body (D-011, D-234) and bidirectional: expected types flow down and drive implicit `dyn` conversion (D-168, D-235). Deferred constraints, such as operators, interface calls and `dyn` conversions, are solved at the end of the body after numeric literals are defaulted.
- Interface satisfaction looks up same-named functions in the type's own module, one type per interface per module (D-045), with conditional conformance (D-064) and embedding.
- Exhaustiveness uses a usefulness algorithm with a fixed step budget and an explicit heap stack, so a list pattern of thousands of elements never deepens the Rust stack (D-508, D-633).
- The module-local warnings of §13.8 run after checking, and `package_warnings` is the whole-package pass for unused package-visible items that the driver runs once per `cheby check` (D-266, D-516).

## Layout

| Module        | What it holds                                                                                                                              |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `sig.rs`      | `build_interface`: every item's `ItemSig` and the module's interface                                                                       |
| `lower_ty.rs` | Written types to closed `Ty`s                                                                                                              |
| `decl/`       | Declaration checks: visibility of types in signatures, `@std_hooks` functions                                                              |
| `ifaces/`     | Interfaces: declarations, the satisfaction engine, dictionaries, operators and E0063/E0065                                                 |
| `infer.rs`    | Body-local inference types: type variables and union-find                                                                                  |
| `check/`      | Bodies into THIR: statements, expressions, calls, `case`, closures, records, interpolation, coercions, deferred constraints and desugaring |
| `thir.rs`     | The typed bodies, the checker's output and MIR's input                                                                                     |
| `exhaust/`    | Exhaustiveness, reachability and irrefutability                                                                                            |
| `targets.rs`  | The cross-target check of `@target`, run on every build (D-056, D-190)                                                                     |
| `consts.rs`   | Constant restrictions and cycles (§4.5)                                                                                                    |
| `warnings/`   | W0002 to W0012: unused locals, imports and items, deprecated uses, and the package-wide pass                                               |
| `repl.rs`     | `ReplSession`: checking REPL entries against their session, redefinitions and stale types (§13.5)                                          |

## Read more

- [`docs/spec/03-types.md`](../../docs/spec/03-types.md) §3.12, inference, [`docs/spec/06-patterns.md`](../../docs/spec/06-patterns.md) §6.9, exhaustiveness, and [`docs/spec/08-interfaces.md`](../../docs/spec/08-interfaces.md).
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.4.
- [`crates/README.md`](../README.md) for the whole crate graph.
