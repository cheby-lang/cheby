---
status: accepted
date: 2026-11-07
log: D-1380
---

# Facts beside THIR, never in it

The language server needs types inside bodies that have errors, since those are the bodies a user is editing. THIR does not hold them: a body with an error is left out (D-494), so MIR never sees an error. The server therefore gets a second output. In server runs, `cheby_types` runs `check_bodies_recording`, which returns `ModuleFacts` beside `thir::Module`. `thir::Module` does not change, and an errored body is still not returned.

Three rules keep the two outputs apart:

- Hooks only append to a log, and the log becomes facts after the checker's last diagnostic. So recording cannot change checking.
- Facts hold closed types and ids, never an `Expr`, `Pat` or `Body` of an errored body. So nothing lowers from them.
- Facts are keyed by `NodeId`, or by token start for names. So a lookup needs no rule for spans that several expressions share.

The trade is a second table of types that must be kept in step with the checker, and a hook at each place that assigns one. In return, the rule that protects the back end costs nothing to keep. With recording on and off, rendered diagnostics were identical on 925 of 925 UI files and 32 of 32 examples, and a run took 1.5% longer.

## Considered options

- `thir::Module.partial`, a part of THIR that holds errored bodies. It needs no second table, but every reader of THIR must then remember to skip it.
- Bodies returned with `ExprKind::Error`. The server would read one structure, but MIR lowering would have to meet errors, and "MIR never sees errors" would rest on a check in place of a type.
- Facts keyed by span. It needs no id in the hooks, but several expressions share a span, such as a `use` and its closure, so it needs a tie-break rule.

## Consequences

- Later features will be tempted to put one more field in THIR. They go in facts, unless the back end needs them from a body that checks.
- Facts can exist where THIR's checks do not run: `CaseFact` gives witnesses in errored bodies, which E0061 cannot (D-1387).
- A body with an error is still dropped from later checks, so `verify` sees no new E0061 or W0010 inside one. A rewrite that depends on those gates on facts (D-1401).
- A test pins that diagnostics with recording on equal those with it off.
