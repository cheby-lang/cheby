---
status: accepted
date: 2026-09-28
log: D-045
---

# One interface implementation per module

Interfaces are satisfied by same-named functions in a type's module (ADR-0015), so a module can satisfy a given interface for only one type. Modules that define several types must split them into submodules to give each its own `show`, `compare` and so on. Function names are never overloaded.

## Considered options

- Overloading by first-argument type within a module: effectively methods.
- Explicit binding (`impl Show for Point = point_to_string`): turns interfaces nominal again.
