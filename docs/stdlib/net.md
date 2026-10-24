# `std::net`

TCP connections: listening, accepting, connecting, and reading and writing bytes. [`std::http`](http.md) is written over this module (D-1003). In the form `cheby doc` prints:

```cheby
pub type Listener

pub type Connection
```

- **Scope.** TCP over IPv4 and IPv6 only. There is no UDP, no Unix-domain socket and no public TLS in tier 3. Adding them later breaks nothing (D-998).
- **Handles.** `Listener` and `Connection` are handles ([§9.5](../spec/09-memory-model.md#95-handles-and-drop-functions), D-101), external types of this module whose drop functions the runtime supplies (D-647, D-703, D-982). They print as `<Listener>` and `<Connection>`, `==` and hashing panic when they reach one, W0011 warns about a comparison that can reach one, and no constant may hold one (D-233). A socket closes at [`close`](#close) or [`close_listener`](#close_listener), which wake the fibers waiting on it (D-1063), or else right after the last use of the last reference to it (D-205), on the fiber that released it (D-171), also when that fiber unwinds. The drop function removes the socket from the poller and closes it without waiting, and does nothing more for a socket already closed. Data already handed to the operating system is still sent.
- **Errors.** Every function that can fail returns `io::Error`. The kinds this module causes have predicates in [`std::io`](io.md), each with a fixed text: `io::is_connection_refused`, `io::is_connection_reset`, `io::is_address_in_use`, `io::is_host_not_found`, `io::is_closed` and `io::is_invalid_input` (D-981, D-1063).
- **The poller.** On native targets a fiber that waits to accept, connect, read or write is suspended until the runtime's event poller reports the socket ready, and its thread runs other fibers meanwhile (D-042, D-680, ADR-0059, [§10.9](../spec/10-concurrency.md#109-blocking-io), D-983, D-984). Such a wait can be cancelled (D-1000). Looking up a host name is the one step that runs on the blocking pool, because the operating system offers no way to wait for it on the poller. The lookup itself cannot be interrupted, but a cancellation that reaches a fiber waiting for one is delivered at once: the fiber unwinds without waiting, the lookup runs to its end on its pool thread, and its result is dropped (D-670, D-983, D-1065). Until the resolver returns, an abandoned lookup holds its pool thread, so a program that retries lookups in a loop can fill the pool (D-683). A fiber waiting on a socket counts as pending work, so a program whose fibers wait only on sockets is never reported as deadlocked (D-724, D-984).
- **No timeouts of its own.** No function here gives up after a time. A caller that needs a deadline wraps the call in `fiber::timeout`, which cancels the wait, a name lookup included ([§10.8](../spec/10-concurrency.md#108-timeouts), D-103, D-1065).
- **Several fibers, one connection.** Any fiber may use a `Connection` it holds. Reads are served one at a time, and so are writes, in the order they reach the connection. A read and a write may proceed at the same time, so one fiber can read while another writes. Each `write_bytes` is atomic with respect to other writes on the connection, as each call on standard output is (D-684). Several fibers may `accept` on one `Listener` (D-1001). Any fiber may close a socket that others wait on, and each of them wakes with an `Err` (D-1063).
- **Streams.** `Connection` satisfies `io::Source` and `io::Sink` through [`read_bytes`](#read_bytes) and [`write_bytes`](#write_bytes), with no code of its own, so [`io::copy`](io.md#copy) and other code generic over a stream take a connection as they take a `file::File` (§8.2, D-1062, D-1072).
- **Written in Cheby.** Every public function is written in Cheby over private `@std_context` kernels named `sys_<name>`, bound as `@external(native, "cheby_net_sys_<name>")` and `@external(js, "./net.mjs", "sys_<name>")` (D-484, D-661, D-692, D-755, D-985, D-986). None runs in constants: reaching one while computing a constant is E0077 (D-090, D-537, D-991).
- **On JS.** OQ-P14, with M4. Node, Deno and Bun have TCP sockets, which an adapter turns into the same calls. Browsers have none, and the direction is that every function there fails with a new `io::is_unsupported` kind.

The tests on this page talk to themselves over the loopback address, with a port the operating system chooses, so they need no network and run in parallel (D-990). Functions whose result depends on the network outside the machine have an _Example_.

## Addresses

A host is a `String`: either an IP address such as `127.0.0.1` or `::1`, or a name such as `localhost` that the operating system resolves. A port is an `Int` from 0 to 65535, and 0 asks the operating system to choose a free port, which [`local_port`](#local_port) then reports. A port outside that range is `Err` for which `io::is_invalid_input` holds. It is not clamped, because it is a wrong argument whose failure the caller must see (D-297, D-998).

## Listening

### `listen`

```cheby
pub fn listen(host: String, port: Int) -> Result<Listener, io::Error>
```

Starts listening for TCP connections on `host` and `port`. `host` is the address of one interface, such as `127.0.0.1` for this machine only, `0.0.0.0` for every IPv4 interface or `::` for every interface. A name is resolved, and the first address it gives is used. A `port` of 0 lets the operating system choose a free port. The operating system may queue connections that arrive before the first [`accept`](#accept).

- **Fails:** `Err` for which `io::is_invalid_input` holds when `port` is outside 0 to 65535 or `host` is empty or contains U+0000, `io::is_host_not_found` when `host` cannot be resolved, `io::is_address_in_use` when another socket already listens there, and `io::is_permission_denied` when the program may not use the port. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system calls, and a name lookup when `host` is a name. Informative.

_Note:_ the reference implementation sets `SO_REUSEADDR` on Unix and never on Windows, so a restarted server can listen on its port at once, and listens with a backlog of 128 (D-1183).

```cheby
test "listen rejects a port out of range" {
  let assert Err(reason) = net::listen("127.0.0.1", 70_000)
  assert io::is_invalid_input(reason)
  let assert Err(negative) = net::listen("127.0.0.1", -1)
  assert io::is_invalid_input(negative)
}

test "a listener on port 0 gets a port of its own" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  assert port > 0 && port <= 65_535
  assert "{listener:?}" == "<Listener>"
}
```

### `local_port`

```cheby
pub fn local_port(listener: Listener) -> Int
```

Returns the port `listener` listens on. After `listen` with port 0, it is the port the operating system chose. After [`close_listener`](#close_listener), it still returns that port, so it never fails (D-1186).

- **Cost:** O(1). Informative.

The second test of [`listen`](#listen) uses it.

### `accept`

```cheby
pub fn accept(listener: Listener) -> Result<Connection, io::Error>
```

Waits for the next connection to `listener` and returns it, with Nagle's algorithm off (D-1002). Several fibers may wait on one listener, and each connection goes to exactly one of them. A cancelled `accept` has taken no connection (D-670, D-1000).

- **Fails:** `Err` for which `io::is_closed` holds when `listener` was closed, also while `accept` waited (D-1063). `Err` when the operating system cannot accept, for example because the process has too many open files. Such a failure is usually temporary, and the listener stays usable.
- **Cost:** O(1), plus the wait. Informative.

The test of [`read_bytes`](#read_bytes) uses it.

## Connecting

### `connect`

```cheby
pub fn connect(host: String, port: Int) -> Result<Connection, io::Error>
```

Opens a TCP connection to `host` and `port`. A name is resolved on the blocking pool, and its addresses are tried in the order the operating system gives them, until one connects. Each connection is made with Nagle's algorithm off, so small writes leave at once (D-1002). A cancelled `connect` leaves no connection (D-1000).

- **Fails:** `Err` for which `io::is_invalid_input` holds when `port` is outside 0 to 65535 or `host` is empty or contains U+0000, `io::is_host_not_found` when `host` cannot be resolved, and `io::is_connection_refused` when nothing listens at the address. `Err` for every other failure of the operating system, such as an unreachable network. When every address fails, the error is the last address's.
- **Cost:** O(1), plus the name lookup and the wait. Informative.

```cheby
test "connect rejects a port out of range" {
  let assert Err(reason) = net::connect("127.0.0.1", 65_536)
  assert io::is_invalid_input(reason)
}
```

_Example_, not a `test` because name lookup depends on the machine's resolver. The top-level domain `invalid` is reserved, so no resolver finds it:

```cheby
fn check_resolver() -> Bool {
  case net::connect("cheby.invalid", 80) {
    Err(reason) => io::is_host_not_found(reason)
    Ok(_) => False
  }
}
```

## Reading and writing

A read returns as soon as any data is available, and reports the end of the stream as `Ok(None)`, as `io::read_line` reports the end of input (D-305). A write returns only when all of its data is handed to the operating system, so no caller writes a retry loop (D-999). The two functions have the names and shapes of `file::read_bytes` and `file::write_bytes`, and the name `net::write` is left for a later text write that would match `file::write` (D-1062).

### `read_bytes`

```cheby
pub fn read_bytes(connection: Connection, limit: Int) -> Result<Option<bytes::Bytes>, io::Error>
```

Waits until `connection` has data, then returns at least one and at most `limit` bytes of it. `limit` caps the result and never sizes an allocation: a read takes at most the kernel's own block, so a large `limit` costs nothing extra (D-1062). It returns `Ok(None)` once the peer has closed its side and every byte before that has been read. Calls after the end keep returning `Ok(None)`. A cancelled `read_bytes` has taken nothing: the bytes stay for the next read (D-670, D-1000).

- **Fails:** `Err` for which `io::is_connection_reset` holds when the peer reset the connection, and `io::is_closed` when `connection` was closed, also while the read waited (D-1063). `Err` for every other failure of the operating system.
- **Clamps:** `limit` below 1 counts as 1 (D-298).
- **Cost:** O(n) in the number of bytes returned, plus the wait. Informative.

```cheby
test "a connection carries bytes both ways over loopback" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  // The server echoes everything it reads, then closes its side when
  // the client has closed its own.
  fiber::spawn_detached(fn() {
    let assert Ok(peer) = net::accept(listener)
    fn echo() {
      case net::read_bytes(peer, 1_024) {
        Ok(Some(chunk)) => {
          let assert Ok(Nil) = net::write_bytes(peer, chunk)
          echo()
        }
        Ok(None) => Nil
        Err(_) => Nil
      }
    }
    echo()
  })
  let assert Ok(client) = net::connect("127.0.0.1", port)
  let assert Ok(Nil) = net::write_bytes(client, bytes::from_string("ping"))
  let assert Ok(Nil) = net::shutdown_write(client)
  fn collect(received) {
    case net::read_bytes(client, 1_024) {
      Ok(Some(chunk)) => collect(bytes::append(received, chunk))
      Ok(None) => received
      Err(_) => received
    }
  }
  assert collect(bytes::new()) == bytes::from_string("ping")
  assert "{client:?}" == "<Connection>"
}
```

The server runs in a detached fiber, which `cheby test` tears down when the test ends (D-723).

### `write_bytes`

```cheby
pub fn write_bytes(connection: Connection, data: bytes::Bytes) -> Result<Nil, io::Error>
```

Writes all of `data` to `connection`, waiting while the operating system's buffer is full. A cancelled `write_bytes` may have sent a prefix of `data`, and how much is not reported. The connection stays usable, but a protocol that cannot resume in the middle of a message should drop it (D-1000).

- **Fails:** `Err` for which `io::is_connection_reset` holds when the peer has reset or closed the connection, and `io::is_closed` when `connection` was closed, also while the write waited (D-1063). `Err` for every other failure of the operating system. Writing to a connection whose peer is gone never ends the program by a signal: the runtime ignores `SIGPIPE`, also in executables (D-925).
- **Cost:** O(n) in the length of `data`, plus the wait. Informative.

The test of [`read_bytes`](#read_bytes) uses it.

### `shutdown_write`

```cheby
pub fn shutdown_write(connection: Connection) -> Result<Nil, io::Error>
```

Closes the writing side of `connection`, so the peer's reads reach the end of the stream once it has read everything written before. It waits for a `write_bytes` in progress on `connection` and never cuts one short. A cancelled `shutdown_write` that was still waiting has shut nothing down (D-1001, D-1185). Reading stays possible. A later `write_bytes` fails.

- **Fails:** `Err` for which `io::is_connection_reset` holds when the peer has already reset the connection, and `io::is_closed` when `connection` was closed (D-1063). `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call and the wait for a write in progress. Informative.

The test of [`read_bytes`](#read_bytes) uses it.

## Closing

A socket closes on its own after its last use (D-205). These two functions close it now, which is how one fiber stops another's `accept` or `read_bytes`, as Go's `Close` does, with no new kind of cancellation (D-1063).

- **Waking.** A fiber parked in `accept`, `read_bytes` or `write_bytes` on the closed handle wakes and gets `Err` for which `io::is_closed` holds, and so does every later operation on the handle that returns a `Result`, another close included. [`local_port`](#local_port) still returns the port. Releasing a closed handle does nothing more, as for `file::close` (D-1063, D-1186).
- **No parking.** The handle keeps its descriptor until a system call that another worker is making on it returns, so a descriptor number is never reused under a running call (D-1001). Those calls never block, so a close never parks (D-1063).
- **No scoped forms.** There are no `net::with_*` functions. A socket's close reports nothing useful, so a scoped form would add only a hard end to the handle's life, which `close` gives (D-1063).

The kernels are `sys_close` and `sys_close_listener`, over the poller's wake of parked fibers (D-1063).

### `close`

```cheby
pub fn close(connection: Connection) -> Result<Nil, io::Error>
```

Closes `connection` now. The peer's reads reach the end of the stream once they have read everything written before, or the peer gets a reset where the operating system sends one because unread data is waiting on this side (D-1063).

- **Fails:** `Err` for which `io::is_closed` holds when `connection` was already closed. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "close wakes a reader, and the peer sees the end" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let assert Ok(client) = net::connect("127.0.0.1", net::local_port(listener))
  let assert Ok(peer) = net::accept(listener)
  let outcome = {
    use owner <- fiber::scope()
    let waiting = fiber::spawn(owner, fn() { net::read_bytes(client, 16) })
    let assert Ok(Nil) = net::close(client)
    fiber::join(waiting)
  }
  // Whether the read parked before the close or started after it, it
  // fails the same way.
  let assert Ok(Err(reason)) = outcome
  assert io::is_closed(reason)
  assert net::read_bytes(peer, 16) == Ok(None)
  let assert Err(again) = net::close(client)
  assert io::is_closed(again)
}
```

### `close_listener`

```cheby
pub fn close_listener(listener: Listener) -> Result<Nil, io::Error>
```

Closes `listener` now, so it accepts no more connections. Connections it has already accepted stay open (D-1063). `http::serve_on` returns `Ok(Nil)` when its listener is closed ([`http.md`](http.md#serve_on), D-1064).

- **Fails:** `Err` for which `io::is_closed` holds when `listener` was already closed. `Err` for every other failure of the operating system.
- **Cost:** O(1), plus the system call. Informative.

```cheby
test "close_listener wakes a fiber that waits to accept" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let outcome = {
    use owner <- fiber::scope()
    let waiting = fiber::spawn(owner, fn() { net::accept(listener) })
    let assert Ok(Nil) = net::close_listener(listener)
    fiber::join(waiting)
  }
  let assert Ok(Err(reason)) = outcome
  assert io::is_closed(reason)
}
```

## Not in tier 3

Each of these can be added later without breaking a program (D-998):

- UDP, Unix-domain sockets and raw sockets;
- a public TLS connection. `std::http` uses one that only std can reach (D-1011);
- the peer's and the local address of a `Connection`, and the local address of a `Listener` beyond its port;
- socket options: keep-alive probes, buffer sizes, `SO_REUSEPORT`, linger and the listen backlog;
- connecting to several addresses at once ("happy eyeballs");
- name lookup on its own, such as `resolve(host) -> List<String>`;
- `read_line` on a `Connection` (D-1076). When it comes, it reads through a read-ahead buffer in the `Connection`, as `file::read_line` reads through the `File`'s (D-995), and `read_bytes` serves that buffer first, so the two may be mixed. A connection's runtime state already leaves room for the buffer, so it comes without changing any function here.
