---
status: accepted
date: 2026-09-28
log: D-019
---

# Cranelift-only native execution, no interpreter

A single mid-level IR feeds both the Cranelift JIT (REPL, `run`) and Cranelift object-file output (AOT), and the JS emitter branches off the same IR. There is no interpreter tier. This means only one native backend to maintain, at the cost of somewhat slower REPL start-up and a harder debugging story than an interpreter would give.
