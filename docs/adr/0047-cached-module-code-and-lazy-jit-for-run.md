---
status: accepted
date: 2026-10-02
log: D-254
---

# Cached module code and lazy JIT for `cheby run`

JIT-compiling a whole program on every `cheby run` cannot restart a 1M-line project in under 500 ms (D-247). `cheby run` therefore loads machine code cached per module (D-249, D-255) for every unchanged module, and compiles changed modules lazily, each function on its first call, through the indirection table the REPL already has (D-043). Start-up cost no longer grows with project size, and an edit costs only the functions that actually run.

## Considered options

- Cached module code only: predictable, but a changed module is compiled whole even if little of it runs.
- Lazy JIT only: no loader needed, but every run recompiles everything it touches.

## Consequences

- Cranelift's JIT does not reload previously compiled code, so the toolchain needs its own loader for cached code, with relocations against the indirection table.
- The same cached code serves AOT builds, so `cheby run` and `cheby build` share work.
- Lazily compiled functions go through an indirection on their first call. This is the same mechanism as REPL redefinition, not a new one.
