---
status: accepted
date: 2026-11-22
log: D-1777
---

# What crosses from std's bodies into a package's build

A module is checked and compiled from the interfaces of what it imports, never from their bodies (D-246). That rule keeps builds local: a body edit rebuilds one module. It also kept three things out of reach that cost run time. A call of a std function bound to a runtime kernel went through a wrapper that boxed its result (D-455). A std function that borrows a parameter could not be called with a borrow from a package (D-812). And no std function was ever inlined.

Std is not an ordinary dependency. It ships inside the toolchain, and every cache entry is keyed by the build ID, which covers std's sources. A fact read from a std body cannot go stale, and it cannot make a rebuild less local, because std never changes under a build.

So three facts from std's bodies may be used by the build of any package:

- The native symbol a std function is bound to, in every profile. A direct call compiles to the kernel's foreign call at the call site (D-1765).
- Std's release borrow masks, by name, in release.
- The bodies of std's functions, for inlining, in release (D-1776).

Between the modules of a package, and between packages, the debug profile is as it was: interfaces only.

Release code must then meet the std code that the masks describe. In release, std's code always comes from its release records, and it is compiled in the process when there is no cache. Function symbols carry the borrow mask in release (D-1768), so a caller built against another mask fails to link.

## Considered options

- **Keep std behind its interfaces.** Nothing to decide, and every call into std stays a wrapped call through a slot. The language comparison's profiles of 2026-11-22 put 41% of `spectral_norm` in boxes that one wrapper made.
- **Put bodies and bindings into every module's interface artifact.** One mechanism for std and packages, but a body edit would then rebuild dependents, in debug too, which D-246 and D-249's early cutoff exist to prevent.
- **A side table inside the compiler.** It would duplicate what std's sources already say and drift from them.

## Consequences

- A package's release build depends on std's bodies. That is free while std is part of the toolchain. A std that could be replaced or patched by a package would need this revisited.
- Other packages' bodies still do not cross. Release inlining across a package's own modules reads bodies from the same build (D-1776), and release records for package modules need exported bodies, which are later work.
- The JS backend must lower the same direct kernel calls from MIR, or keep calling the wrappers there.
