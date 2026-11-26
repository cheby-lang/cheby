---
status: accepted
date: 2026-11-26
log: D-1912
---

# Children live inside `with_child`

A program starts a child it talks to with `process::with_child(command, body)`. The child lives while `body` runs. When `body` returns, a child still running is killed, waited for and reaped, and when `body` unwinds it is killed at once. There is no `spawn` that returns a child to keep.

```cheby
fn first_line(path: String) -> Result<Option<String>, io::Error> {
  use child <- process::with_child(process::command("head", ["-n", "1", path]))
  process::read_line(process::stdout(child))
}
```

Cheby releases a value right after its last use (D-205). A handle whose release kills the child would kill `let server = spawn(c)` before the code that talks to the server runs, whenever that code does not name `server` again. A release that detaches instead leaks a process every time code panics or is cancelled. A scope removes both traps and gives each child an owner, as `fiber::scope` gives each fiber one (ADR-0019).

## Considered options

- `spawn` returning a handle whose release kills the child: flexible, but last-use release makes the early kill a common bug.
- `spawn` returning a handle whose release detaches it: no early kill, but leaked processes and a reaper for children nobody waits for.
- `with_child` and a `spawn_detached` for children meant to outlive the program's interest: kept out until a program needs it.

## Consequences

- A child cannot be stored in a record or returned. A server that runs for a whole program is started in `main`, around the code that uses it.
- A cancelled `wait` takes nothing, since the end of `with_child` stops the child (D-1913).
- The promise has limits that the runtime cannot remove: a program ended by a signal's default action leaves its children running (D-1917).
