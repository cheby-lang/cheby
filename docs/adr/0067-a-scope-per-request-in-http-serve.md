---
status: accepted
date: 2026-10-21
log: D-1012
---

# A scope per request in `http::serve`

`http::serve_on` runs each connection as a fiber in a scope of its own, and each request's handler inside a scope of its own on that fiber. A handler that panics gives its request a `500` and prints its panic report to standard error, as a detached fiber's report is printed. The connection stays open, and no other request is cancelled. A cancellation from outside `serve`, such as a timeout around it, still cancels every connection and handler.

Under the rules of scopes, a panic in one member cancels every other member (D-040, §10.3). A server must survive a bad request, so this is the one place in std where a panic does not cancel its siblings.

When accepting fails in a way that does not pass, `serve_on` cancels its own connections through a package-visible runtime operation, `fiber::cancel_members`, waits until they have unwound and returns `Err`. Std's public API has no way to cancel a scope's members from inside it.

## Considered options

- Handlers as plain members of `serve`'s scope: one panicking request ends the server.
- Handlers in detached fibers: nothing could cancel them (§10.4), and their panics would still be printed.
- Returning the panic to the caller of `serve`: also ends the server.
- A private panic marker to end the connections on an accept failure, instead of `cancel_members`.

## Consequences

- Every handler stays inside `serve`'s structure, so timeouts and enclosing scopes still reach it.
- A handler's panic report goes to standard error. A fixture that drives a server, such as 030's in M5, must allow for it.
- `std::fiber` gains a private kernel that only std uses.
