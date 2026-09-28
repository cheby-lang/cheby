---
status: accepted
date: 2026-09-28
log: D-020
---

# Structural interfaces instead of traits

Ad-hoc polymorphism uses Go-style structural interfaces. A type satisfies an interface implicitly when it has the required operations, with no `impl Trait for T` declaration. This deliberately departs from the otherwise Rust-inspired design. There is no orphan rule to fight and less boilerplate. The cost is that conformance is accidental rather than declared, and interface errors are reported at use sites.

## Considered options

- Rust-style traits: explicit, coherent, but more ceremony.
- No ad-hoc polymorphism (Gleam): too limiting for generic collections, equality and printing.
