---
status: accepted
date: 2026-10-12
log: D-733
---

# Unwinding by return-flag propagation

On native targets, a panicking or cancelled fiber unwinds by returning. A Cheby frame that unwinds runs its cleanup pad and returns 0, which is never a uniform word (D-453). A caller tests the result of each Cheby call that may unwind, and after a C call that may unwind it loads the unwind flag at `ctx+12` of the fiber control block (D-685). Rust guards at the runtime's C-ABI entry points set the flag and return a sentinel (D-697).

The backend sits on the shared parts that do not depend on it: the MIR unwind pass with its chained cleanup pads (D-698), the catch points and the unwind record in the fiber (D-699), and the runtime's guards. It replaces D-077's Cranelift `try_call` landing pads and unwinder. What a program can observe does not change: the same references are released at the same points.

The user chose it at M2's checkpoint 1 from D-702's measurement, taken at `a1ae525` on an Apple M3, against the baseline without unwinding.

| Measure                                                     | Return flag                     | `try_call`                   |
| ----------------------------------------------------------- | ------------------------------- | ---------------------------- |
| Build throughput, 100k and 1M shapes                        | −14.1%, −14.3%                  | −18.3%, −18.1%               |
| Run time, RC group, geometric mean, against no unwind edges | +2.1% (rounds +1.7, +3.3, +1.5) | +17.5%                       |
| Code size, 100k shapes                                      | +19.0%                          | +34.1%                       |
| Recursion depth with a value live across the call           | the same as without             | about 3.7 times shallower    |
| Own `unsafe` blocks and assembly                            | none                            | 18 blocks, 67 assembly lines |

Both passed the whole M1 suite. The MIR pass alone costs 3.4% to 4.0% of build throughput under either backend, so both miss D-702's 3% compile reference (D-735).

## Considered options

- `try_call` with catch-all handlers, generated exit stubs for C callees, a frame-pointer unwinder and a registry of handler tables (D-077's mechanism, prototyped as package U2b): slower on every measure. With Cranelift 0.135.5 a `try_call` to a `Tail` callee clobbers every register, so a frame with a handler spills every value live across the call. It needs resume code per architecture, including each Windows target in M6, and its tables serialized with M3's cached code.
- No unwinding, leaking what a panicking fiber holds: rejected by D-077 and still rejected. Structured concurrency makes caught panics routine (ADR-0019).

## Consequences

- Every call that may unwind costs a compare and a branch on its normal path, and every pad is code. A package after U4 reduces the compile cost: one walk for the pass and its validation, a table of kernels that never raise, and cheaper pads (D-735).
- The backend has no architecture-specific code, so Windows in M6 and cached code in M3 need nothing extra for Cheby frames. Rust frames in AOT links still need the platform unwinder.
- Package U4 makes this backend the default and deletes the `try_call` prototype: its exit record at FCB offsets 16 and 24, its exit stubs and its table registry.
- A Cheby call's result of 0 means "unwinding", so no value reaching Cheby code as a uniform word may be 0. A call of an unboxed-result entry point tests the flag instead of its result (D-734). Foreign results must be wrapped as D-590's external values before they reach Cheby code, which M3 does; until then a NULL foreign result reads as an unwinding with no record.
