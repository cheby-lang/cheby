---
status: accepted
date: 2026-09-28
log: D-147
---

# Package-visible by default, with `pub` and `priv`

Items are visible to every module of their package by default, `pub` exports them to other packages, and a new `priv` keyword restricts them to their own module. This replaces the earlier module-private default (D-026), which is what Rust and Gleam do. Most items in a package are shared between its modules, so the common case needs no keyword, and `pub` then marks exactly the package's public API. The cost is a third visibility keyword, and helpers are no longer module-private unless marked `priv`.
