---
status: accepted
date: 2026-10-02
log: D-248, D-249
---

# Module interface artifacts as the recompilation boundary

The module, one file, is the unit of compilation, caching and parallelism. Each module produces an interface artifact holding its signatures, types, interfaces, constant types and small inlinable bodies, and a dependent module is rebuilt only when the hash of an interface it uses changes. This is Go's export data with early cutoff, at file granularity rather than package granularity, which acyclic module imports (D-145) make possible. Editing a function body leaves the interface hash unchanged, so only that module is rebuilt.

## Considered options

- Packages as the unit, as in Go: coarser, serializes large packages and rebuilds a whole package on any edit.
- Fine-grained query-based incremental compilation (rustc, salsa): precise, but complex, slow for clean batch builds and hard to make fast. The language server may still use queries internally.
- Rebuilding everything downstream of a change: simple but defeats the latency budget (D-247).

## Consequences

- Anything that affects how other modules are compiled must live in the interface artifact or be decided at link time. Inferred properties that depend on bodies must not leak into it (D-250).
