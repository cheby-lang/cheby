---
status: accepted
date: 2026-09-29
log: D-241
---

# No positional tuple access

Tuples (D-076) were read with `.0`, `.1`, … or with a tuple pattern. With nested tuples this led to chains such as `entry.1.0`, whose meaning is invisible at the use site. The lexer also read `x.0.1` as `x`, `.`, `0.1`, because `0.1` is a float literal, and the spec did not address it.

We remove positional access. Tuple elements are read only with tuple patterns, in `let`, `case`, `use` binders and, with D-242, the parameters of closures and local functions. `x.0` is a compile error that suggests a pattern, and interpolation paths hold only field names. Positional variant fields already could not be read with `.0`, so tuples and variants now follow one rule: positional data is taken apart with a pattern, and named data is read with `.name`. A warning on nested and wide tuple types (D-243) points longer-lived groupings to named types.

## Considered options

- Keep `.N` and fix the lexer: two ways to read a tuple, and chains stay possible.
- Allow one level of `.N` and reject chains: an arbitrary-feeling rule that still keeps two ways.
- Anonymous structural records: named fields without a declaration, but they clash with nominal types, field-access inference (D-213), type descriptors (D-236) and `exposed`.

## Consequences

- Callbacks over pairs use pattern parameters, such as `list::map(cells, fn((x, y)) { … })`.
- Top-level functions that take a tuple destructure it with `let` in the body, since their parameters stay plain names (D-242).
- `x.0` and `x.0.1` are no longer valid, so the float-literal ambiguity is gone.
- Adding `.N` back later breaks no program.
