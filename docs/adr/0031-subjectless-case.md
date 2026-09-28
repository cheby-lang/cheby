---
status: accepted
date: 2026-09-28
log: D-109
---

# Subjectless case for condition chains

`case { cond => …, cond => …, _ => … }` evaluates `Bool` conditions in order and takes the first true arm, like Elixir's `cond`. A final `_` arm is required. This keeps `case` as the only branching construct (ADR-0001) while avoiding deeply nested `case b { True => … False => case … }` chains. Nobody should read it as a backdoor `if`: it is deliberately part of `case`, and there is still no `if` keyword.
