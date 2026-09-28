---
status: accepted
date: 2026-09-28
log: D-100, D-101
---

# Deterministic handle reference counting on all targets

Channels close automatically when their last `Sender` or `Receiver` goes away (D-054, D-087), and FFI/runtime handle types such as files and sockets may register a drop function that runs when their last reference goes away. On native this falls out of Perceus. On JS, where ordinary data is left to the JS GC, the compiler still emits reference-count increments and decrements for handle types only, at the points where Perceus would. The same program therefore behaves the same on both targets. User-defined ADTs never have destructors.

## Considered options

- JS `FinalizationRegistry`: non-deterministic and may never run, so receivers could hang forever.
- Explicit `close()` only: loses automatic closing and makes leaks easy.

## Consequences

- The JS backend needs the ownership and drop analysis for handle types, not just the native backend.
- A handle stored inside GC-managed data on JS is released when that data is dropped according to the same Perceus analysis, so the analysis must be complete for any type that can contain a handle.
