---
status: accepted
date: 2026-11-14
log: D-1566
---

# Captured contexts

`context::capture()` returns a fiber's whole context as a value, a `Context`. `context::with(captured, body)` runs `body` on the calling fiber with that context in place of the fiber's own. The body runs as a fiber spawned where the context was captured would.

```cheby
fn submit(
  jobs: channel::Sender<(String, context::Context)>,
  job: String,
) -> Result<Nil, (String, context::Context)> {
  channel::send(jobs, (job, context::capture()))
}

fn work(jobs: channel::Receiver<(String, context::Context)>) -> Nil {
  case channel::receive(jobs) {
    Ok((job, captured)) => {
      context::with(captured, fn() { run(job) })
      work(jobs)
    }
    Err(Nil) => Nil
  }
}
```

Context follows spawns, not messages (ADR-0068). A worker pool or an actor is spawned before the requests it serves, so it never sees the context of the fiber that sends it a job: the request's logger, a test's clock, a seeded random stream. Sending each value with the message works for one key and not for all of them, and the sender cannot know which keys the job will read.

Two choices shape the API.

**`with` replaces, it does not merge.** Inside `body`, every key has the captured value or its default, and the worker's own bindings are not visible. This is the rule of a spawn, so there is one rule to learn, and it costs O(1) because a context is a persistent map (D-1571). A worker that wants a value of its own on top calls `provide` inside `body`.

**A capture counts as a spawn for the seeded stream.** `std::random`'s seeded stream is the one part of a context that changes (D-1048), and a value cannot hold it. So `capture` derives a stream state as a spawn does and leaves the capturing fiber's stream alone, and `with` starts `body` from that state (D-1567). A job's draws then depend on where it was submitted, never on which worker runs it.

## Considered options

- Merging the captured bindings over the caller's. The worker keeps its own keys where the sender provided none. But it costs O(k) per call, there are two contexts to reason about, and a key that the sender left at its default takes the worker's value, so a test cannot rely on a default.
- `with` keeps the running fiber's stream. An actor that holds one captured context keeps advancing its own stream. But in a pool a job's numbers depend on which worker took it, and seeded tests are no longer reproducible.
- No capture, as before: the sender passes values with the message. It needs no API, but every key a job reads must be known to the code that sends it.

## Consequences

- Every `with` of one captured context starts its stream from the same state, so two of them draw the same numbers. A program that wants different numbers captures once per job, which is what a sender does.
- Draws inside `body` are gone when it returns: the fiber's stream is again the one it had before the call.
- A `Context` keeps its values alive, as a spawned fiber does. A captured context that outlives its request keeps the request's values, and a handle among them stays open.
- A `Context` is a handle: it prints as `<Context>`, comparing or hashing it panics, and no constant holds one. So the empty context is the function `context::empty()`, not a constant (D-1568).
- On JS, a function passed to foreign code takes the passing fiber's context with it in the same way, and each callback's fiber starts with it (D-1569).
