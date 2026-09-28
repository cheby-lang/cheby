---
status: accepted
date: 2026-09-28
log: D-014
---

# JS fibers via whole-program suspension analysis

JS has no stackful coroutines. On the JS target, a whole-program analysis finds every function that can transitively reach a suspension point (channel ops, fiber yield, blocking IO) and compiles only those to generators. Pure code stays plain JS. The coloring still exists but only in the generated code, not for the user.

## Considered options

- Compile everything to generators/CPS: simple, but slows down all code.
- Wasm + JSPI: a different target, and it depends on newer runtime support.
- No fibers on JS: breaks portability of concurrent code.

## Consequences

JS builds need whole-program compilation, and indirect calls (closures, trait objects) make the analysis conservative.
