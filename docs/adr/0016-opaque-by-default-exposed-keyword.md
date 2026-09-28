---
status: accepted
date: 2026-09-28
log: D-035
---

# Types are opaque by default, with an exposed keyword

Type constructors are hidden outside the type's package by default, even for `pub` types. The `exposed` keyword opts in to making constructors usable (for construction and pattern matching) by other packages. This reverses Gleam, where types are transparent and `opaque` opts out. Libraries hide their representation by default and can change it without breaking users. The cost is that plain data types need an extra keyword to be matched on from outside.
