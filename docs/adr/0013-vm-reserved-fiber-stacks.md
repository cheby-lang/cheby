---
status: superseded by ADR-0032
date: 2026-09-28
log: D-027
---

# Virtual-memory-reserved fiber stacks

On native targets each fiber gets a large virtual address reservation that the OS commits lazily, with a guard page at the end. Stacks are never copied or split.

## Considered options

- Growable copied stacks (Go): need precise stack maps and pointer relocation, which Cranelift supports only in a limited way.
- Segmented stacks: suffer from the hot-split problem.

## Consequences

The design relies on 64-bit address space, so 32-bit native targets are effectively out of scope. Stack overflow is detected by the guard page and must be turned into a fiber panic.
