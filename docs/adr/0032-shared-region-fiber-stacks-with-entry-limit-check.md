---
status: accepted
date: 2026-09-28
log: D-124, D-125
---

# Fiber stacks in shared regions with an entry stack-limit check

On native targets, each fiber gets a fixed stack reservation (8 MiB by default, more for the root fiber) carved out of a few large virtual-memory regions. The OS commits pages only when they are touched, so an idle fiber costs a page or two of RAM. There are no per-fiber guard pages. Instead, every function entry compares the stack pointer against a per-fiber limit, and this compare is merged with the preemption yield check (D-028), as Go does. Cranelift supports such an entry stack-limit check. Stacks are still never copied or split, so no stack maps are needed. This supersedes ADR-0013.

## Considered options

- A separate mapping with a guard page per fiber (ADR-0013): each guard page splits the mapping, so each fiber costs about two kernel mappings, and Linux's default `vm.max_map_count` of 65,530 caps a program at roughly 32k fibers.
- 1 GiB reservations: no RAM cost, but only about 130k fibers fit in 128 TiB of address space, and huge virtual sizes break `ulimit -v` and alarm monitoring tools.

## Consequences

- Overflow detection depends on every function having the entry check. Foreign code called through FFI runs without it, so FFI calls should run with enough headroom or on a separate stack.
- The runtime should return touched pages of a finished fiber's stack to the OS (`madvise`) when it reuses the slot, so one deep recursion doesn't pin memory forever.
- 64-bit address space is still assumed, so 32-bit native targets remain out of scope.
