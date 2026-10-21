---
status: accepted
date: 2026-10-21
log: D-1003
---

# `std::http` in Cheby over `std::net`

On native targets, `std::http`'s client and server are written in Cheby over `std::net` and `std::fiber`. The only kernels parse request and response heads, through the `httparse` crate, and run the TLS state machine. A handler is an ordinary Cheby call on a fiber, and every wait is a socket wait on the poller (ADR-0059), which `fiber::timeout` and scopes can cancel. On JS, private kernels bind to `fetch` and the host's server API.

D-274 puts in Cheby what can be written over kernels, and D-484 keeps kernels from calling Cheby closures. A Rust HTTP library would break one or the other.

## Considered options

- A Rust client such as `ureq` on the blocking pool: a pool job cannot be cancelled (D-670), so example 031's timeouts would wait for every slow fetch, and concurrency is capped at the pool's 512 threads.
- `hyper` on a Tokio runtime inside the Cheby runtime: a second scheduler, and the server's handler would be called from a kernel.
- Parsing heads in Cheby too: possible, but it puts a byte-level parser in std before the optimizer is measured.

## Consequences

- M3 writes and tests an HTTP/1.1 implementation of a few hundred lines of Cheby: framing, chunked bodies, redirects and keep-alive. Its speed in Cheby is unmeasured.
- Cancellation, deadlock detection and panic isolation work for HTTP as for any other Cheby code.
- `httparse` is linked into every executable that uses the runtime.
