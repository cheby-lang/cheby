---
status: accepted
date: 2026-09-28
log: D-012
---

# M:N parallel fiber scheduler from day one

Fibers are scheduled M:N across OS threads from the first version, not single-threaded first. Real-world use needs parallelism, and retrofitting it later would touch the runtime, the RC scheme and FFI assumptions. The cost is that Perceus RC must be safe across threads without making every RC operation atomic. A Koka-style shared/unshared marking is the expected approach.
