---
status: accepted
date: 2026-09-28
log: D-057
---

# No metaprogramming in v1

v1 has no macros and no derive-style annotations. Equality, hashing and debug printing are built in (D-033), and things like JSON encoding are handled by libraries or external code generators. Macros hurt the LSP, error messages and compile speed, all of which matter for a v1 that ships the whole toolchain. The cost is more hand-written boilerplate for serialization and similar tasks. Adding macros later is possible without breaking code.
