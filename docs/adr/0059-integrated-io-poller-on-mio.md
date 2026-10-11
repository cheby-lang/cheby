---
status: accepted
date: 2026-10-11
log: D-680
---

# An integrated IO poller on mio

Each runtime has one Go-style integrated poller on `mio` 1.x, with one `mio::Poll` and one `mio::Waker`. A worker that finds no work takes the poller and blocks in it, with the earliest timer deadline as its timeout. Every other idle worker parks. A push of new work unparks a parked worker, or wakes the poller when no worker is parked. This is Go's `findRunnable` and Tokio's driver lock. This settles OQ-P8.

M2 registers only the waker and the timers. Sockets register on the same seam with `std::net` in M3 (D-681). File IO goes to the blocking pool (D-683). The poller sits behind an internal trait, so another backend can be added after 1.0.

mio is the proven cross-platform readiness layer. It is Tokio's backend, and its Windows support is solved.

## Considered options

- io_uring: Linux-only, and disabled by Docker's default seccomp profile. It is completion-based, so a cancelled fiber would wait for the kernel to cancel its operation before unwinding could free the buffer. Its main gain, file IO, already goes to the blocking pool.
- A condvar-only poller for M2: simpler now, but M3 would rewrite the parking path for sockets.

## Consequences

- The whole M2 suite runs on real epoll and kqueue, so the parking protocol is tested before sockets arrive.
- `cheby_runtime` depends on `mio`.
- io_uring is not used in v1.
