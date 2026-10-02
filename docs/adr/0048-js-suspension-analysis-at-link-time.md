---
status: accepted
date: 2026-10-02
log: D-258
---

# JS suspension analysis at link time

The JS backend compiles only functions that can reach a suspension point to generators (ADR-0005), and that property depends on the bodies of everything a function calls. Putting it in interface artifacts would let a body edit change an interface and rebuild dependents, breaking D-249. Instead, each module exports a small summary of which functions suspend directly and its call edges, a link-time step computes the transitive result, and JS code is generated and cached per function, keyed on the function's IR hash and whether it suspends.

## Considered options

- Suspension status in the interface artifact: propagates through ordinary rebuilds, but a body edit in a leaf can rebuild every transitive caller across packages.
- Whole-program analysis and JS generation on every build: simple, but scales with program size on every edit.

## Consequences

- An edit regenerates only the functions whose suspension status actually changed, plus the edited function.
- The link step runs over the whole program's call graph, which is cheap compared with code generation.
