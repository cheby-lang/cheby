---
status: accepted
date: 2026-09-28
log: D-077
---

# Unwind fiber stacks on panic

When a fiber panics on native targets, the runtime unwinds its stack and releases the RC references held by each frame, so a dead fiber leaks nothing. This uses Cranelift's `try_call` landing pads and its unwinder crate (available since 2025). Structured concurrency (ADR-0019) turns panics into ordinary `Result`s, so recovering from them is routine and cannot leak memory each time.

## Considered options

- No unwinding, leaking whatever the fiber held: simpler codegen, but unbounded leaks in long-running servers.

## Consequences

Every call that has live RC references in its frame needs cleanup metadata, which costs some code size and constrains codegen. On JS the GC reclaims memory regardless.
