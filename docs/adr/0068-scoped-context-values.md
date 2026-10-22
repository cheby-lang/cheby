---
status: accepted
date: 2026-10-23
log: D-1044
---

# Scoped context values

`std::context` lets a program provide a value for the duration of a callback, and lets any code that runs meanwhile read it, in the providing fiber and in every fiber spawned from it. A key is a typed constant with a default. Nothing is mutated: when the callback returns or unwinds, the fiber's context is what it was before.

```cheby
pub const pool: context::Key<Option<db::Pool>> = context::key_without_default()

pub fn main() -> Result<Nil, db::Error> {
  use connected <- result::try(db::connect(url))
  use <- context::provide(pool, Some(connected))
  server::run()
}

test "invoices are dated by the clock in context" {
  use <- context::provide(time::clock, time::fixed_clock(launch))
  assert invoice::date(invoice::create(order)) == launch
}
```

This is dynamic scoping in a language that otherwise passes everything explicitly. A function's signature no longer shows that it reads the clock or the logger. The trade is made because the alternatives put the cost on every function between the code that provides a dependency and the code that reads it, and because tests need to replace exactly these dependencies. Std reads only three of them from context: the clock, the host random source and, once it exists, the logger (D-1046).

## Considered options

- Explicit parameters, or one record of dependencies passed everywhere. This needs no feature, but every call along a chain carries it.
- Context parameters resolved by type, as in Scala 3's `using` and Kotlin's context parameters. They are checked at compile time and cost nothing at run time, but every function along a chain declares them, and a default would let a function silently bypass a test's value.
- Effect handlers, as in Koka and OCaml 5. They are the most general option, and they also give generators and early exit. But they need effect rows in every signature, effect-polymorphic signatures throughout std, and a large language change before 1.0.

## Consequences

- A test provides a clock, a seeded random stream or a logger with one `use` line. Tests are root fibers of their own (D-723), so values never leak between tests that run at once.
- A missing dependency is found at run time: `get` returns the key's default, or `None` for a key without one.
- Context follows spawns, not messages. A long-lived fiber that serves requests over a channel, such as an actor or a worker pool, keeps the context it was spawned with and never sees its senders'. A sender that needs it there sends the value with the message, and the receiver provides it around the work.
- Each fiber holds its context, and a spawn copies it. The runtime of both targets needs this per-fiber state (D-1049).
- An effect whose operations all resume at once is a typed form of a context key. If Cheby gains effect handlers after 1.0, `std::context` can be rebuilt on them without changing its API.
