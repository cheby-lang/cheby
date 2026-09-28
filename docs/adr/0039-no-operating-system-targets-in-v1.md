---
status: accepted
date: 2026-09-29
log: D-214
---

# No operating-system targets in v1

Some foreign functions exist only on some operating systems, for example `arc4random_uniform` is missing on Windows. We keep the only targets `native` and `js`, with no `@target(windows)` or similar. The standard library and runtime hide operating-system differences, and foreign code handles its own portability, typically with a small C shim. Operating-system targets would multiply the cross-target checks of D-056 and D-190 for every package, and can be added later without breaking code.
