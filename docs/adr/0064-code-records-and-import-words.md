---
status: accepted
date: 2026-10-21
log: D-913, D-914
---

# Code records and import words

The cache stores one format-neutral code record per module, build kind and settings: the machine code, data and symbolic relocations that Cranelift's `define_function_bytes` and `define_data` take, with the module record, call sites, source positions and reuse-report lines. Every host loads records into cranelift-jit, and `cheby build` replays them into one object per module through `cranelift_object`. Objects are never cached.

The code is non-PIC. Every generated symbol is colocated, across modules too, so references between generated names are PC-relative. Runtime functions, runtime statics and foreign symbols are reached through module-local import words, 8-byte data objects with an absolute data relocation. Text then holds only PC-relative relocations, which PIE executables, arm64 Mach-O and cranelift-jit all accept. A link experiment confirmed this on macOS arm64 and Linux x86-64 and arm64 before any code relied on it.

This refines D-254: cranelift-jit already takes precompiled bytes, so `cheby run` needs no loader of its own, and our own loader is built only if loading misses OQ-P11's budget.

## Considered options

- PIC objects, with runtime and foreign symbols through the GOT: cranelift-jit refuses PIC, so `cheby run` would need our own loader on the critical path. Cranelift's PIC mode also loads every name through the GOT, colocated ones included. GNU ld on x86-64 and ld64 relax those loads, but GNU ld on arm64 keeps one extra load per slot, layout word and string read.
- Caching ELF and Mach-O objects: a loader for each format now, and COFF in M6.
- Separate code for `cheby run` and executables: against ADR-0047, and twice the compile work.
- Non-PIC objects linked with `-no-pie`: loses ASLR, and arm64 Mach-O needs PIE.

## Consequences

- One artifact serves the JIT, the cache and executables, and an executable runs byte for byte the code `cheby run` executes.
- A runtime call loads its import word, as many instructions as an absolute address costs today.
- Replaying a record is a pure function of the record, so the per-program parts, such as the image and the stubs of unreached missing symbols, live in one program object per link.
- COFF in M6 needs an object writer, not a second cache.
- Declaring a few hundred thousand symbols by name in cranelift-jit may cost tens of milliseconds on the largest projects. That is the measured trigger for our own loader.
- A platform where import words fail falls back to PIC with our own loader.
