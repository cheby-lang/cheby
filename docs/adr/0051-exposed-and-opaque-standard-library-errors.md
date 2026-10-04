---
status: accepted
date: 2026-10-04
log: D-304
---

# Exposed and opaque standard-library errors

After 1.0, adding a variant to an `exposed` ADT breaks every exhaustive `case` on it (D-075). Standard-library error types are therefore split by where their cases come from. An error type whose cases an outside specification fixes, such as `json::Error`, is `exposed`, and callers match on it (`json::Syntax(_)` in example 011). An error type whose cases follow the operating system or the host, such as `io::Error`, is opaque: callers ask predicate functions such as `io::is_not_found(e)` and print it with `show`, and a new kind of error is a new predicate, which breaks no program.

## Considered options

- Every error type `exposed`: simplest to use, but `io::Error` would be frozen at 1.0 while operating systems keep adding failure kinds.
- Every error type opaque: always safe to extend, but callers lose exhaustive matching on errors whose cases cannot change.

## Consequences

- Each module spec must say whether its error type is `exposed` or opaque, and why.
- Code that handles opaque errors uses predicates and a fallback arm, never an exhaustive `case`.
