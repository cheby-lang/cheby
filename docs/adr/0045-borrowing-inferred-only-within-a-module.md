---
status: accepted
date: 2026-10-02
log: D-250
---

# Borrowing is inferred only within a module

Perceus-style compilers infer which parameters a function borrows from its body (D-072). Across modules, that makes a body edit change how callers in other modules must call the function, which would bring back Rust-style rebuild cascades. We infer borrowing only for calls within one module. Calls across modules use a fixed owned calling convention, so the interface artifact (D-249) never depends on a function body.

## Considered options

- Infer everywhere and record the result in the interface artifact: fewer reference-count operations, but body edits force dependents to rebuild.
- A fixed convention only across packages: still cascades inside large packages.

## Consequences

- Calls across modules pay some extra increments and decrements in debug builds. Release builds can recover them through budgeted inlining and specialization (D-251).
- REPL redefinition (D-043) never changes a calling convention.
