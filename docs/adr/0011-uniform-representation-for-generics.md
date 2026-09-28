---
status: accepted
date: 2026-09-28
log: D-021
---

# Uniform representation for generics

Generic code is compiled once over a uniform boxed representation, with small integers tagged and unboxed (as in Koka, OCaml and Gleam). Monomorphization of hot paths may be added later as an optimization. This keeps JIT/REPL compiles fast, code small, maps naturally to JS, and suits Perceus RC. The cost is boxing overhead for floats, sized ints and small records in generic code until specialization exists.
