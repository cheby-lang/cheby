# cheby_js

The JavaScript emitter and the link-time suspension analysis ([ADR-0048](../../docs/adr/0048-js-suspension-analysis-at-link-time.md)). In M4 it will turn MIR into ES modules that run on the JS runtime in `runtime-js/`, and decide which functions may suspend so that fibers work on a target without stackful coroutines.

## Place in the pipeline

`cheby_js` depends on `cheby_mir`, the single mid-level IR that feeds every backend (D-019). The MIR already carries what this backend needs: which calls are tail calls, which operations suspend directly and which values may contain handles (D-172). Only `cheby_driver` depends on it. The JS kernels of the standard library live next to the Cheby sources in `std/` as `.mjs` files (D-274, D-484).

## Status

Planned for M4. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m4-js.md`](../../docs/plan/m4-js.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
