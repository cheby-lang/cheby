---
status: accepted
date: 2026-09-28
log: D-164
---

# Interface satisfaction requires functions as visible as the type

With structural interfaces (D-020) and three visibility levels (D-162), a type's module functions count toward an interface only if they are at least as visible as the type itself, and then they count everywhere the type can be named. This gives one global answer to "does `T` satisfy `I`", so dictionaries built in different packages never disagree, while package-internal types can satisfy interfaces without making their functions `pub`.

## Considered options

- **Where visible**: a function counts wherever the checking code can see it. Rejected because satisfaction would depend on the call site, so `List<T>: Show` could be true in one package and false in another.
- **Always `pub`**: only `pub` functions count. Rejected because internal types would have to export their functions, which pulls them into the public API (D-149).
