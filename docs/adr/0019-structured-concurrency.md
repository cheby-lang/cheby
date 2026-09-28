---
status: accepted
date: 2026-09-28
log: D-040
---

# Structured concurrency for fibers

Every fiber belongs to a scope (`use s <- fiber.scope()`). A scope waits for all its children before returning. A panic in a child cancels its siblings and surfaces as a `Result` at the scope. Detached spawn exists as an explicit escape hatch, and Erlang-style links and monitors are a library. This prevents most fiber leaks (which RC cannot collect, see ADR-0014) and gives a clear place for errors to surface. The cost is that long-lived background fibers need the explicit detached form.
