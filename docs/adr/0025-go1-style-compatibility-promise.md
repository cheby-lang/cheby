---
status: accepted
date: 2026-09-28
log: D-075
---

# Go 1-style compatibility promise after 1.0

Before 1.0 anything may change. From 1.0 on, the language and stdlib only grow: no breaking changes and no Rust-style editions. Editions would multiply the work in the compiler, LSP and formatter. The cost is that mistakes found after 1.0 must be lived with or worked around additively, so pre-1.0 design has to be careful.
