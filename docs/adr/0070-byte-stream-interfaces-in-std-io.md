---
status: accepted
date: 2026-10-23
log: D-1072
---

# Byte-stream interfaces in `std::io`

`std::io` declares two interfaces, `Source` and `Sink`, with one function each, and `io::copy` between them. `file::File`, `net::Connection` and the standard-stream handles satisfy both through functions their modules already have, because satisfaction is structural and looks only at the type's own module (§8.2).

```cheby
pub interface Source {
  fn read_bytes(Self, Int) -> Result<Option<bytes::Bytes>, Error>
}

pub interface Sink {
  fn write_bytes(Self, bytes::Bytes) -> Result<Nil, Error>
}

fn send_file(path: String, connection: net::Connection) -> Result<Int, io::Error> {
  use source <- file::with_open(path)
  io::copy(source, connection)
}
```

D-046 made interfaces runtime values because of Go's `io.Reader`, but std had no stream interface, so no function could take "a file or a socket". This fixes std's stream architecture: later byte sources, such as a child process's pipes, a buffered reader or a decompressor, are expected to satisfy these two interfaces. That is hard to reverse after 1.0.

The design depends on names. `File` and `Connection` share an interface only because their functions have the same names, so `net::read` and `net::write` became `read_bytes` and `write_bytes` first (D-1062). `io::other(message)` lets user code build an `io::Error`, so that a user's `Source` or `Sink` can fail honestly. Before, only std could build one.

Values are immutable (D-017), so a user's type satisfies `Source` only by wrapping something with state, such as a handle or a channel. A record that holds `Bytes` would return the same bytes on every call.

## Considered options

- Channel-based streams as the core abstraction, such as a `pump(owner, source, capacity)` that returns a `Receiver<Bytes>`. They compose with selectors, but cost a fiber per stream and a second path for cancellation. Such a helper can still be built over `Source` later.
- `Source` with a buffer, a `read_line` or seek functions. `Connection` has none of them, so it could not satisfy the interface.
- The interfaces without `io::other`. `io::Error` would stay std-only, but a user's `Sink` over a channel could only panic or claim success.
- A later tier. The names were the urgent part, but code generic over a file or a socket would wait past M3.

## Consequences

- `std::io` imports neither `std::file` nor `std::net`, so there is no import cycle (D-145).
- Both interfaces are `dyn`-safe, so `List<dyn io::Sink>` works (§8.6).
- An `io::Error` built by `other` has kind 0. No predicate holds for it, and its text is the message.
- A later process API gives its pipes these interfaces (D-1078).
