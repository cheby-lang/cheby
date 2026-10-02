---
status: accepted
date: 2026-10-02
log: D-246, D-247
---

# Compile speed and the locality rule

Cheby aims for Go-class compile speed, and Rust's slowness on big projects is the cautionary example. We make compile speed a language design principle with one normative rule: type-checking a module needs only the interfaces of the modules it depends on, never their function bodies. The current design already meets it (required top-level signatures, inference local to one body, no macros, acyclic imports), so the rule costs nothing today. It constrains the future: a feature that needs other modules' bodies to type-check, such as inference that leaks across signatures or Zig-style comptime parameters, is out unless it finds a local formulation. The reference compiler also carries two budgets, tracked in CI from the first milestone: at least 100k lines per second per core for a clean debug build, and under 500 ms for `cheby run` to restart after a one-function edit in a 1M-line project.

## Considered options

- An informative implementation goal only: easy to trade away one feature at a time.
- No stated goal: the way most languages drift toward slow compiles.

## Consequences

- Proposals for new language features must show that they keep the locality rule.
- Code generation and optimization may still look across modules in release builds (D-251), because the rule is about type-checking and debug builds.
