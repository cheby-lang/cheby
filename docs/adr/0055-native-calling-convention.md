---
status: accepted
date: 2026-10-07
log: D-453
---

# Native calling convention

Every Cheby function compiled to native code takes a hidden first parameter, `ctx`, a pointer to the execution context of the running entry, which becomes the running fiber's control block in M2. The function-entry check compares the stack pointer with the stack limit stored in the context. Setting that limit to the highest address forces the next function entry into the runtime, so the yield check (D-028), the deadline of constant evaluation (D-195) and stack-overflow detection (D-124) cost one compare together.

Every Cheby function returns one uniform word, whatever its declared type, while its parameters keep the representation of their declared types. A tail call between any two Cheby functions is then always a `return_call` (D-015), including a call from a function returning `Int` to a generic function returning `T`.

Every call whose callee is not known statically uses one convention: `ctx`, the owned closure object `self`, then uniform words. Closures, local functions, function values of top-level functions, dictionary entries and `dyn` calls all go through it, so indirect and `dyn` tail calls need no adapters. A local function refers to itself through `self`, not through a capture, and a function value of a top-level function calls it through its indirection slot (D-043).

## Considered options

- The context in thread-local storage: `cranelift-jit` does not support it, and switching fibers would have to rewrite it.
- One global stack limit: it cannot differ per fiber, per REPL entry and per constant evaluation, as D-124, D-283 and D-195 need.
- Results in the representation of their declared types: an unboxed `Int` result cannot be converted to a uniform word after a tail call, so tail calls into generic code would stop being guaranteed.
- A convention per kind of callee: indirect and `dyn` tail calls would need adapters, and a local function that captured itself would be a reference-count cycle that leaks on every loop.

## Consequences

- Every native signature has one more parameter, which occupies a register. Foreign functions do not take it.
- An `Int` result is tagged and a `Float` result is boxed, even between monomorphic functions. Release builds may add entry points with unboxed results for calls that are not tail calls.
- A closure stores its code address as `addr | 1`, so every function is aligned to 16.
- The runtime never calls Cheby code directly. Entry thunks and one trampoline per arity use the platform C convention.
