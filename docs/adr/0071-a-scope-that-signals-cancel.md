---
status: accepted
date: 2026-10-23
log: D-1073
---

# A scope that signals cancel

`process::until_interrupted(body)` runs `body` in a scope that the first SIGINT or SIGTERM cancels. The cancellation unwinds everything inside, so handles close, `with_*` scopes end and buffered output is flushed when `main` returns. The result says whether the body finished, panicked or was interrupted.

```cheby
fn main() -> Result<Nil, http::Error> {
  let served = process::until_interrupted(fn() {
    http::serve(8080, fn(_) { http::text(200, "home") })
  })
  case served {
    Ok(outcome) => outcome
    Err(process::Interrupted) => Ok(Nil)
    Err(process::Panicked(failure)) => panic as fiber::message(failure)
  }
}
```

The decision is surprising without context. While at least one such scope is open, the runtime changes the process's signal disposition: a signal no longer kills the process, it cancels every open scope. After the first signal the default action is restored, so a second Ctrl-C still ends a program whose shutdown is stuck. With no scope open, signals behave as they always did.

It reuses the primitive that timeouts already have: a scope with a trigger that only cancels (D-103, D-790). A signal arrives as an event on the poller, and the poll holder runs the cancellation walk as it does for an expired timer.

## Considered options

- A channel of signals, `process::interrupts() -> channel::Receiver<Nil>`, as Go's `signal.Notify`. With `fiber::cancel` (D-1060) it can stop a scope, but every program that wants a graceful stop would write the same fiber, `select` and `cancel`, could get their order wrong, and would need a fiber of its own.
- An exposed `Signal` ADT with SIGHUP, SIGUSR1 and others. It freezes at 1.0, and Windows and browsers lack most of them.
- Counting an open scope as pending work, so that a program can wait only for Ctrl-C. It hides real deadlocks behind a signal that may never come.
- No signal handling in tier 3, as D-1024 had it. Servers would keep dying on SIGTERM with their output lost.

## Consequences

- `Interrupt` is an exposed type with `Interrupted` and `Panicked(fiber::Panic)`, the shape of `fiber::Timeout`. It freezes at 1.0.
- Under `cheby test` and in the REPL no signal reaches the scope, so tests stay deterministic and Ctrl-C keeps the host's meaning.
- An open scope does not keep the deadlock detector quiet.
- The runtime needs a signal source on its poller on POSIX now, a console handler on Windows in M6, and host listeners on JS in M4. A browser never interrupts the scope.
- Process spawning stays out of tier 3 (D-1024).
