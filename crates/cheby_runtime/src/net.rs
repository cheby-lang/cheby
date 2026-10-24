//! The `std::net` kernels (D-985, D-998 to D-1002, D-1062, D-1063):
//! TCP listeners and connections on the poller's socket seam
//! (`sched/io.rs`, D-984).
//!
//! - **Handles** (D-982, D-703): a `Listener` is runtime kind 46 and a
//!   `Connection` kind 47 (`abi::layout`), handle objects whose payload
//!   ([`ListenerState`], [`ConnectionState`]) owns a [`NetSocket`]. The
//!   finalizer drops the payload, which removes the socket from the poller
//!   and closes it; no socket sets `SO_LINGER`, so the close never waits.
//! - **Sockets on the poller** (D-983, D-984): every socket is a `mio`
//!   socket, non-blocking, registered once on its runtime's poller when it
//!   is made ([`NetSocket::new`]). `accept`, `connect`, `read_bytes` and
//!   `write_bytes` try their system call first and wait on the seam on
//!   `WouldBlock`; a cancellation interrupts the wait at once, and the seam
//!   checks for one before every try, so a cancelled `accept` or
//!   `read_bytes` has taken nothing, a cancelled `connect` drops its
//!   half-made socket, and a cancelled `write_bytes` may have sent a prefix
//!   (D-670, D-1000).
//! - **Several fibers** (D-1001): each socket has a read turn and a write
//!   turn (`sched/turn.rs`): reads and accepts are served one at a time,
//!   and so are writes, each held across its waits, so a `write_bytes` is
//!   atomic against other writes; a read and a write proceed at once.
//! - **Closing** (D-1063): `close` and `close_listener` take the socket out
//!   of its handle, close the seam's entry and both turns, which wakes every
//!   fiber parked on them with kind 12, and drop the handle's reference to
//!   the descriptor's owner ([`Owned`]). A system call that another worker
//!   is making holds its own reference for the length of the call, so the
//!   descriptor is closed when the last of them returns, never under a
//!   running call; those calls never block, so `close` never parks.
//! - **Name lookup** (D-983, D-1065, D-1165): a host that is not an IP
//!   address is resolved on the blocking pool as a detached job
//!   (`sched::blocking::run_blocking_detached`): a cancellation reaching the
//!   waiting fiber is delivered at once, the lookup runs to its end on its
//!   pool thread, and its result is dropped there. The job owns Rust data
//!   only (D-683). Every failure of name lookup is kind 11 (D-981).
//! - **Listening** (D-998, D-1036): through `socket2`, with `IPV6_V6ONLY`
//!   cleared for an IPv6 wildcard, `SO_REUSEADDR` on Unix and never on
//!   Windows, and a backlog of 128, as Rust's `std` and `mio` do
//!   (GAP(net-listen-options)). `TCP_NODELAY` is set on every connection
//!   from `accept` and `connect` (D-1002).
//! - **Errors** (D-981, D-484): each kernel returns `Err((kind, message))`
//!   ([`NetError`]): 8 `ECONNREFUSED`; 9 `ECONNRESET`, `ECONNABORTED` and a
//!   socket's `EPIPE`, with macOS's `EPROTOTYPE` on a write that races the
//!   peer's reset (GAP(net-error-mapping)); 10 `EADDRINUSE`; 11 every
//!   failure of name lookup; 12 a closed handle; 13 a port outside 0 to
//!   65535 or a host that is empty or holds U+0000; 2 `EACCES` and `EPERM`
//!   from `bind`; every other failure 0 with the system's message.
//! - **Without a scheduler** (D-983, D-714): every kernel ends the program
//!   with a fatal runtime error that names its operation, so on Windows
//!   `std::net` is fatal until M6.
//!
//! A connection's state ([`ConnectionState`]) is a Rust struct behind the
//! handle's payload pointer, so the read-ahead buffer that a later
//! `net::read_line` needs (D-1076) joins it, served under the read turn,
//! without changing the handle's layout or any kernel's signature.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use parking_lot::Mutex;
use socket2::{Domain, Protocol, Socket, Type};

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::Cancelled;
use crate::fiber_ops::{needs_scheduler, running};
use crate::handles::{self, HandleKind, HandlePayload};
use crate::io::{err_pair, os_message};
use crate::panic::fatal;
use crate::sched::Shared;
use crate::sched::blocking::run_blocking_detached;
use crate::sched::io::{Direction, IoHandle, is_closed_error};
use crate::sched::turn::{Turn, TurnError};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::{string, values};

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["net"];

/// The kind code of every other failure, with the system's message.
const KIND_OTHER: i64 = 0;
/// `io::is_permission_denied` (tier 1's kind 2), from `bind`.
const KIND_PERMISSION_DENIED: i64 = 2;
/// `io::is_connection_refused` (D-981).
const KIND_CONNECTION_REFUSED: i64 = 8;
/// `io::is_connection_reset` (D-981).
const KIND_CONNECTION_RESET: i64 = 9;
/// `io::is_address_in_use` (D-981).
const KIND_ADDRESS_IN_USE: i64 = 10;
/// `io::is_host_not_found` (D-981).
const KIND_HOST_NOT_FOUND: i64 = 11;
/// `io::is_closed` (D-981, D-1063).
const KIND_CLOSED: i64 = 12;
/// `io::is_invalid_input` (D-981).
const KIND_INVALID_INPUT: i64 = 13;

/// The listen backlog (GAP(net-listen-options)): Rust's `std` and `mio`'s
/// `TcpListener::bind` use 128.
const BACKLOG: i32 = 128;

/// The most a read takes at once: the kernel's block (D-1062). `limit`
/// caps a read's result and never sizes an allocation above it.
pub(crate) const BLOCK: usize = 64 << 10;

/// The system call that failed, where the mapping of its error depends on
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// `bind`: `EACCES` and `EPERM` are tier 1's permission denied.
    Bind,
    /// Any other call.
    Other,
}

/// A failed network operation as Rust data: its kind code (D-981) and,
/// for kind 0 only, the system's message. A pool job may make it; the
/// fiber turns it into the `Err` pair ([`NetError::word`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetError {
    pub(crate) kind: i64,
    pub(crate) message: String,
}

impl NetError {
    /// A failure of a kind with a predicate, whose text `std::io` fixes.
    fn fixed(kind: i64) -> Self {
        Self {
            kind,
            message: String::new(),
        }
    }

    /// A failure of kind 0 with `message`.
    fn other(message: &str) -> Self {
        Self {
            kind: KIND_OTHER,
            message: message.to_owned(),
        }
    }

    /// The failure of the system call `op` (module documentation).
    fn of(error: &io::Error, op: Op) -> Self {
        if is_closed_error(error) {
            return Self::fixed(KIND_CLOSED);
        }
        let kind = match error.kind() {
            io::ErrorKind::ConnectionRefused => KIND_CONNECTION_REFUSED,
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => KIND_CONNECTION_RESET,
            io::ErrorKind::AddrInUse => KIND_ADDRESS_IN_USE,
            io::ErrorKind::PermissionDenied if op == Op::Bind => KIND_PERMISSION_DENIED,
            _ if is_platform_reset(error) => KIND_CONNECTION_RESET,
            _ => KIND_OTHER,
        };
        if kind == KIND_OTHER {
            Self::other(&os_message(error))
        } else {
            Self::fixed(kind)
        }
    }

    /// The `Err` pair, made on the fiber.
    fn word(&self) -> Word {
        err_pair(self.kind, &self.message)
    }
}

/// GAP(net-error-mapping): a platform's variant of a named case maps with
/// that case. macOS answers a write that races the peer's reset with
/// `EPROTOTYPE` (41) where other systems give `EPIPE` or `ECONNRESET`, so
/// it is kind 9 there.
fn is_platform_reset(error: &io::Error) -> bool {
    #[cfg(target_vendor = "apple")]
    {
        error.raw_os_error() == Some(libc::EPROTOTYPE)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = error;
        false
    }
}

/// The error of an operation on a closed handle.
fn closed() -> NetError {
    NetError::fixed(KIND_CLOSED)
}

/// What a [`NetSocket`]'s handle keeps of its descriptor: the socket, which
/// it deregisters and closes when the last reference goes (module
/// documentation, "Closing").
#[derive(Debug)]
pub(crate) struct Owned<S: mio::event::Source> {
    socket: S,
    /// The socket's entry on the poller, for the deregistration.
    io: IoHandle,
}

impl<S: mio::event::Source> Drop for Owned<S> {
    fn drop(&mut self) {
        // Then `socket` drops, which closes the descriptor.
        self.io.deregister(&mut self.socket);
    }
}

impl<S: mio::event::Source> Owned<S> {
    /// The socket, for a system call.
    pub(crate) fn socket(&self) -> &S {
        &self.socket
    }
}

/// A socket registered on its runtime's poller, with its read and write
/// turns (module documentation).
#[derive(Debug)]
pub(crate) struct NetSocket<S: mio::event::Source> {
    /// The seam's entry: waits, and the wake at a close.
    io: IoHandle,
    /// The descriptor's owner, `None` once closed. A system call holds a
    /// clone while it runs.
    socket: Mutex<Option<Arc<Owned<S>>>>,
    /// Reads, or accepts on a listener.
    reads: Turn,
    /// Writes and `shutdown_write`.
    writes: Turn,
}

impl<S: mio::event::Source> NetSocket<S> {
    /// Registers `socket` on `runtime`'s poller (D-984).
    ///
    /// # Errors
    ///
    /// The registration's error; `socket` is then closed.
    pub(crate) fn new(runtime: &Shared, mut socket: S) -> io::Result<Self> {
        let io = IoHandle::register(runtime, &mut socket)?;
        let owned = Arc::new(Owned {
            socket,
            io: io.share(),
        });
        Ok(Self {
            io,
            socket: Mutex::new(Some(owned)),
            reads: Turn::new(),
            writes: Turn::new(),
        })
    }

    /// The descriptor's owner for one system call, or the closed handle's
    /// error.
    pub(crate) fn current(&self) -> io::Result<Arc<Owned<S>>> {
        self.socket
            .lock()
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "the socket is closed"))
    }

    /// Whether the handle was closed.
    fn is_closed(&self) -> bool {
        self.socket.lock().is_none()
    }

    /// The turn of `dir`.
    fn turn(&self, dir: Direction) -> &Turn {
        match dir {
            Direction::Read => &self.reads,
            Direction::Write => &self.writes,
        }
    }

    /// Runs `op` on the socket, waiting on the poller in `dir` while it
    /// would block. The descriptor's owner is held only during each call.
    /// The caller holds `dir`'s turn, or is the only user of the socket.
    fn wait_on<T>(
        &self,
        me: &ExecCtx,
        dir: Direction,
        op: &mut dyn FnMut(&S) -> io::Result<T>,
    ) -> Result<Result<T, NetError>, Cancelled> {
        let outcome = self.io.wait(me, dir, &mut || {
            let owned = self.current()?;
            op(owned.socket())
        })?;
        Ok(outcome.map_err(|error| {
            if self.is_closed() {
                closed()
            } else {
                NetError::of(&error, Op::Other)
            }
        }))
    }

    /// Runs `op` as [`NetSocket::wait_on`] does, under `dir`'s turn.
    fn serialized<T>(
        &self,
        me: &ExecCtx,
        dir: Direction,
        op: &mut dyn FnMut(&S) -> io::Result<T>,
    ) -> Result<Result<T, NetError>, Cancelled> {
        let _turn = match self.turn(dir).take(me) {
            Ok(turn) => turn,
            Err(TurnError::Cancelled) => return Err(Cancelled),
            Err(TurnError::Closed) => return Ok(Err(closed())),
        };
        self.wait_on(me, dir, op)
    }

    /// Closes the handle now (D-1063): wakes every fiber parked on it with
    /// kind 12 and gives up the handle's reference to the descriptor, which
    /// closes once no system call on it runs. Never waits.
    ///
    /// # Errors
    ///
    /// Kind 12 when it was already closed.
    pub(crate) fn close(&self) -> Result<(), NetError> {
        let Some(owned) = self.socket.lock().take() else {
            return Err(closed());
        };
        self.io.close();
        self.reads.close();
        self.writes.close();
        drop(owned);
        Ok(())
    }

    /// The seam's handle (for tests).
    #[cfg(test)]
    pub(crate) fn io(&self) -> &IoHandle {
        &self.io
    }

    /// The turn of `dir` (for tests).
    #[cfg(test)]
    pub(crate) fn turn_of(&self, dir: Direction) -> &Turn {
        self.turn(dir)
    }
}

/// A `Listener`'s payload: the socket and the port it listens on, kept so
/// that `local_port` answers after a close too, with the port the
/// listener had (GAP(net-local-port-after-close): `local_port` cannot
/// fail, D-998).
#[derive(Debug)]
pub(crate) struct ListenerState {
    pub(crate) socket: NetSocket<mio::net::TcpListener>,
    pub(crate) port: u16,
}

impl HandlePayload for ListenerState {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // Dropping the socket deregisters and closes it (D-982).
    }
}

/// A `Connection`'s payload: the socket. A later read-ahead buffer joins it
/// (D-1076, module documentation).
#[derive(Debug)]
pub(crate) struct ConnectionState {
    pub(crate) socket: NetSocket<mio::net::TcpStream>,
}

impl HandlePayload for ConnectionState {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // Dropping the socket deregisters and closes it (D-982).
    }
}

/// What a lookup job returns: Rust data only (D-683).
struct Lookup {
    addresses: io::Result<Vec<SocketAddr>>,
    /// A test's view of the job (`hooks`).
    #[cfg(test)]
    probe: Option<Arc<hooks::LookupHook>>,
}

#[cfg(test)]
impl Drop for Lookup {
    fn drop(&mut self) {
        if let Some(probe) = &self.probe {
            probe.dropped(std::thread::current().id());
        }
    }
}

impl Lookup {
    /// The blocking-pool job: resolves `host` (D-983).
    fn run(host: &str, port: u16) -> Self {
        #[cfg(test)]
        if let Some(hook) = hooks::find(host) {
            let addresses = hook.run();
            return Self {
                addresses,
                probe: Some(hook),
            };
        }
        Self {
            addresses: (host, port)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>),
            #[cfg(test)]
            probe: None,
        }
    }

    /// The addresses, or kind 11 for every failure, an empty answer
    /// included (D-981).
    fn into_addresses(mut self) -> Result<Vec<SocketAddr>, NetError> {
        match std::mem::replace(&mut self.addresses, Ok(Vec::new())) {
            Ok(addresses) if !addresses.is_empty() => Ok(addresses),
            Ok(_) | Err(_) => Err(NetError::fixed(KIND_HOST_NOT_FOUND)),
        }
    }
}

/// The addresses of `host` and `port` (`net.md` §Addresses): kind 13 for a
/// port outside 0 to 65535 or a host that is empty or holds U+0000; an IP
/// address as it is; a name resolved on the blocking pool, abandoned by a
/// cancellation (D-1065).
///
/// # Errors
///
/// [`Cancelled`] when a cancellation reached `me` before or during the
/// lookup.
fn resolve(
    me: &ExecCtx,
    host: &str,
    port: i64,
) -> Result<Result<Vec<SocketAddr>, NetError>, Cancelled> {
    let Ok(port) = u16::try_from(port) else {
        return Ok(Err(NetError::fixed(KIND_INVALID_INPUT)));
    };
    if host.is_empty() || host.contains('\0') {
        return Ok(Err(NetError::fixed(KIND_INVALID_INPUT)));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Ok(vec![SocketAddr::new(ip, port)]));
    }
    let host = host.to_owned();
    let lookup = run_blocking_detached(me, move || Lookup::run(&host, port))?;
    Ok(lookup.into_addresses())
}

/// A listening socket on `address` (GAP(net-listen-options)).
fn bind_listener(runtime: &Shared, address: SocketAddr) -> Result<ListenerState, NetError> {
    let other = |error: io::Error| NetError::of(&error, Op::Other);
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(other)?;
    if address.is_ipv6() && address.ip().is_unspecified() {
        socket.set_only_v6(false).map_err(other)?;
    }
    #[cfg(unix)]
    socket.set_reuse_address(true).map_err(other)?;
    socket.set_nonblocking(true).map_err(other)?;
    socket
        .bind(&address.into())
        .map_err(|error| NetError::of(&error, Op::Bind))?;
    socket.listen(BACKLOG).map_err(other)?;
    let listener = mio::net::TcpListener::from_std(socket.into());
    let port = listener.local_addr().map_err(other)?.port();
    let socket = NetSocket::new(runtime, listener).map_err(other)?;
    Ok(ListenerState { socket, port })
}

/// `listen`'s work: the first address `host` gives (`net.md` §`listen`).
pub(crate) fn listen_on(
    me: &ExecCtx,
    runtime: &Shared,
    host: &str,
    port: i64,
) -> Result<Result<ListenerState, NetError>, Cancelled> {
    Ok(match resolve(me, host, port)? {
        Ok(addresses) => match addresses.first() {
            Some(&address) => bind_listener(runtime, address),
            None => Err(NetError::fixed(KIND_HOST_NOT_FOUND)),
        },
        Err(error) => Err(error),
    })
}

/// `accept`'s work: the next connection, under the listener's read turn,
/// so each connection goes to one fiber (D-1001).
pub(crate) fn accept_on(
    me: &ExecCtx,
    runtime: &Shared,
    listener: &ListenerState,
) -> Result<Result<ConnectionState, NetError>, Cancelled> {
    let accepted = listener
        .socket
        .serialized(me, Direction::Read, &mut |socket| {
            let (stream, _) = socket.accept()?;
            stream.set_nodelay(true)?;
            Ok(stream)
        })?;
    Ok(accepted.and_then(|stream| connection(runtime, stream)))
}

/// A connection's state over `stream`, registered on the poller.
fn connection(runtime: &Shared, stream: mio::net::TcpStream) -> Result<ConnectionState, NetError> {
    NetSocket::new(runtime, stream)
        .map(|socket| ConnectionState { socket })
        .map_err(|error| NetError::of(&error, Op::Other))
}

/// One connection attempt to `address`, waiting on the poller until it is
/// made or fails. A cancellation drops the half-made socket, which closes
/// it: no connection is left (D-1000).
fn connect_one(
    me: &ExecCtx,
    runtime: &Shared,
    address: SocketAddr,
) -> Result<Result<ConnectionState, NetError>, Cancelled> {
    // A cancelled fiber sends no connection request (D-816's rule for
    // kernels that would wait).
    crate::fiber_ops::cancel::check(me)?;
    let stream = match mio::net::TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => return Ok(Err(NetError::of(&error, Op::Other))),
    };
    let state = match connection(runtime, stream) {
        Ok(state) => state,
        Err(error) => return Ok(Err(error)),
    };
    // Nobody else has the socket yet: no turn. `mio`'s way to learn the
    // outcome of a non-blocking connect: writable, then the pending error
    // or a peer address.
    let made = state.socket.wait_on(me, Direction::Write, &mut |socket| {
        if let Some(error) = socket.take_error()? {
            return Err(error);
        }
        match socket.peer_addr() {
            Ok(_) => socket.set_nodelay(true),
            Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                Err(io::ErrorKind::WouldBlock.into())
            }
            Err(error) => Err(error),
        }
    })?;
    Ok(made.map(|()| state))
}

/// `connect`'s work (`net.md` §`connect`): the addresses of `host` in the
/// order the system gives them, until one connects; when every one fails,
/// the last one's error.
pub(crate) fn connect_to(
    me: &ExecCtx,
    runtime: &Shared,
    host: &str,
    port: i64,
) -> Result<Result<ConnectionState, NetError>, Cancelled> {
    let addresses = match resolve(me, host, port)? {
        Ok(addresses) => addresses,
        Err(error) => return Ok(Err(error)),
    };
    let mut last = NetError::fixed(KIND_HOST_NOT_FOUND);
    for address in addresses {
        match connect_one(me, runtime, address)? {
            Ok(state) => return Ok(Ok(state)),
            Err(error) => last = error,
        }
    }
    Ok(Err(last))
}

/// `read_bytes`'s work: at least one and at most `limit` bytes (below 1
/// counts as 1, D-298), at most [`BLOCK`], or `None` at the end of the
/// stream, under the read turn (D-999, D-1062).
pub(crate) fn read_on(
    me: &ExecCtx,
    connection: &ConnectionState,
    limit: i64,
) -> Result<Result<Option<Vec<u8>>, NetError>, Cancelled> {
    let size = usize::try_from(limit.max(1)).map_or(BLOCK, |limit| limit.min(BLOCK));
    let mut buffer = vec![0_u8; size];
    let read = connection
        .socket
        .serialized(me, Direction::Read, &mut |socket| {
            let mut socket = socket;
            socket.read(&mut buffer)
        })?;
    Ok(read.map(|count| {
        (count > 0).then(|| {
            buffer.truncate(count);
            buffer
        })
    }))
}

/// `write_bytes`'s work: all of `data`, under the write turn, held across
/// the waits, so no other write interleaves (D-999, D-1001). A cancellation
/// between two system calls leaves a prefix sent (D-1000).
pub(crate) fn write_on(
    me: &ExecCtx,
    connection: &ConnectionState,
    data: &[u8],
) -> Result<Result<(), NetError>, Cancelled> {
    let socket = &connection.socket;
    let _turn = match socket.writes.take(me) {
        Ok(turn) => turn,
        Err(TurnError::Cancelled) => return Err(Cancelled),
        Err(TurnError::Closed) => return Ok(Err(closed())),
    };
    let mut rest = data;
    while !rest.is_empty() {
        let written = socket.wait_on(me, Direction::Write, &mut |stream| {
            let mut stream = stream;
            stream.write(rest)
        })?;
        match written {
            Ok(0) => {
                return Ok(Err(NetError::other(
                    "the operating system accepted no bytes",
                )));
            }
            Ok(count) => rest = rest.get(count..).unwrap_or_default(),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(()))
}

/// `shutdown_write`'s work, under the write turn, so it follows every
/// write that reached the connection first (GAP(net-shutdown-turn): it
/// waits for a write in progress, which may itself wait on the peer, and
/// never cuts one short).
pub(crate) fn shutdown_on(
    me: &ExecCtx,
    connection: &ConnectionState,
) -> Result<Result<(), NetError>, Cancelled> {
    let socket = &connection.socket;
    let _turn = match socket.writes.take(me) {
        Ok(turn) => turn,
        Err(TurnError::Cancelled) => return Err(Cancelled),
        Err(TurnError::Closed) => return Ok(Err(closed())),
    };
    Ok(match socket.current() {
        Ok(owned) => owned
            .socket()
            .shutdown(Shutdown::Write)
            .map_err(|error| NetError::of(&error, Op::Other)),
        Err(_) => Err(closed()),
    })
}

/// The running fiber of a kernel's `ctx` and its runtime, or the fatal
/// error that names `operation` when the entry runs no scheduler (D-983,
/// D-714).
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block.
unsafe fn scheduled<'a>(ctx: *mut ExecCtx, operation: &str) -> (&'a ExecCtx, &'a Arc<Shared>) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    match me.sched.runtime() {
        Some(runtime) => (me, runtime),
        None => needs_scheduler(operation),
    }
}

/// The text of a borrowed `String` argument.
///
/// # Safety
///
/// `text` is the uniform word of a live `String`, borrowed.
unsafe fn str_arg<'a>(text: Word) -> &'a str {
    // SAFETY: forwarded from the caller.
    unsafe { string::as_str_unchecked(text) }
        .unwrap_or_else(|| fatal(format_args!("{text:#x} is not a String")))
}

/// The payload of a borrowed `Listener`.
///
/// # Safety
///
/// `listener` is a live `Listener` handle, borrowed for `'a`.
unsafe fn listener_of<'a>(listener: Word) -> &'a ListenerState {
    // SAFETY: forwarded from the caller.
    unsafe { handles::payload::<ListenerState>(listener, HandleKind::Listener) }
}

/// The payload of a borrowed `Connection`.
///
/// # Safety
///
/// `connection` is a live `Connection` handle, borrowed for `'a`.
unsafe fn connection_of<'a>(connection: Word) -> &'a ConnectionState {
    // SAFETY: forwarded from the caller.
    unsafe { handles::payload::<ConnectionState>(connection, HandleKind::Connection) }
}

/// The outcome of a kernel's work as its `Result` word: a cancellation
/// raises (D-670), a failure is the `Err` pair, and `ok` builds the value.
fn outcome<T>(
    ctx: *mut ExecCtx,
    result: Result<Result<T, NetError>, Cancelled>,
    ok: impl FnOnce(T) -> Word,
) -> Word {
    match result {
        Ok(Ok(value)) => values::ok(ok(value)),
        Ok(Err(error)) => error.word(),
        Err(Cancelled) => raise_cancel(Some(ctx)),
    }
}

/// `net::listen`'s kernel: a `Listener` on the first address of `host`
/// and `port` (D-998).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `host` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_listen(ctx: *mut ExecCtx, host: Word, port: i64) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, runtime) = unsafe { scheduled(ctx, "net::listen") };
        // SAFETY: forwarded from the caller.
        let host = unsafe { str_arg(host) };
        let result = listen_on(me, runtime, host, port);
        outcome(ctx, result, |state| {
            // SAFETY: the running fiber's context.
            unsafe { handles::new_handle(ctx, HandleKind::Listener, Box::new(state)) }
        })
    })
}

/// `net::local_port`'s kernel: the port the listener listens on.
///
/// # Safety
///
/// `listener` is a live `Listener`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_local_port(ctx: *mut ExecCtx, listener: Word) -> i64 {
    guarded(guard_ctx(ctx), || {
        // SAFETY: forwarded from the caller.
        i64::from(unsafe { listener_of(listener) }.port)
    })
}

/// The context a guard gets: `None` for a null `ctx`, when the current
/// fiber is used.
fn guard_ctx(ctx: *mut ExecCtx) -> Option<*mut ExecCtx> {
    (!ctx.is_null()).then_some(ctx)
}

/// `net::accept`'s kernel: the next connection, waiting on the poller.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `listener` is a live `Listener`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_accept(ctx: *mut ExecCtx, listener: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, runtime) = unsafe { scheduled(ctx, "net::accept") };
        // SAFETY: forwarded from the caller.
        let listener = unsafe { listener_of(listener) };
        let result = accept_on(me, runtime, listener);
        outcome(ctx, result, |state| {
            // SAFETY: the running fiber's context.
            unsafe { handles::new_handle(ctx, HandleKind::Connection, Box::new(state)) }
        })
    })
}

/// `net::connect`'s kernel: a connection to `host` and `port`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `host` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_connect(ctx: *mut ExecCtx, host: Word, port: i64) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, runtime) = unsafe { scheduled(ctx, "net::connect") };
        // SAFETY: forwarded from the caller.
        let host = unsafe { str_arg(host) };
        let result = connect_to(me, runtime, host, port);
        outcome(ctx, result, |state| {
            // SAFETY: the running fiber's context.
            unsafe { handles::new_handle(ctx, HandleKind::Connection, Box::new(state)) }
        })
    })
}

/// `net::read_bytes`'s kernel: `Some(Bytes)` or `None` at the end.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `connection` is a live
/// `Connection`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_read_bytes(
    ctx: *mut ExecCtx,
    connection: Word,
    limit: i64,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, _) = unsafe { scheduled(ctx, "net::read_bytes") };
        // SAFETY: forwarded from the caller.
        let connection = unsafe { connection_of(connection) };
        let result = read_on(me, connection, limit);
        outcome(ctx, result, |data| match data {
            Some(data) => values::some(values::bytes(&data)),
            None => values::none(),
        })
    })
}

/// `net::write_bytes`'s kernel: writes all of `data`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `connection` is a live
/// `Connection` and `data` a live `Bytes`, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_write_bytes(
    ctx: *mut ExecCtx,
    connection: Word,
    data: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, _) = unsafe { scheduled(ctx, "net::write_bytes") };
        // SAFETY: forwarded from the caller.
        let connection = unsafe { connection_of(connection) };
        // SAFETY: forwarded from the caller: the caller keeps `data` live
        // and unchanged for the whole call, waits included.
        let data = unsafe { values::bytes_of(data) };
        let result = write_on(me, connection, data);
        outcome(ctx, result, |()| values::nil())
    })
}

/// `net::shutdown_write`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `connection` is a live
/// `Connection`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_shutdown_write(ctx: *mut ExecCtx, connection: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let (me, _) = unsafe { scheduled(ctx, "net::shutdown_write") };
        // SAFETY: forwarded from the caller.
        let connection = unsafe { connection_of(connection) };
        let result = shutdown_on(me, connection);
        outcome(ctx, result, |()| values::nil())
    })
}

/// `net::close`'s kernel (D-1063): never parks.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `connection` is a live
/// `Connection`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_close(ctx: *mut ExecCtx, connection: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let _ = unsafe { scheduled(ctx, "net::close") };
        // SAFETY: forwarded from the caller.
        let connection = unsafe { connection_of(connection) };
        outcome(ctx, Ok(connection.socket.close()), |()| values::nil())
    })
}

/// `net::close_listener`'s kernel (D-1063): never parks.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `listener` is a live `Listener`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_close_listener(ctx: *mut ExecCtx, listener: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let _ = unsafe { scheduled(ctx, "net::close_listener") };
        // SAFETY: forwarded from the caller.
        let listener = unsafe { listener_of(listener) };
        outcome(ctx, Ok(listener.socket.close()), |()| values::nil())
    })
}

/// The package-visible `sys_connect_tls`'s kernel (D-985, D-1011):
/// GAP(net-tls-stub): until S6 implements TLS it answers an error of kind
/// 0, after the checks every socket kernel makes.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `host` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_net_sys_connect_tls(
    ctx: *mut ExecCtx,
    host: Word,
    port: i64,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let _ = unsafe { scheduled(ctx, "net::connect_tls") };
        // SAFETY: forwarded from the caller.
        let _ = unsafe { str_arg(host) };
        let _ = port;
        NetError::other("TLS connections are not supported yet").word()
    })
}

/// One kernel: symbol, address, C parameter types after the context
/// pointer, and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The `std::net` kernels; every one takes the running fiber's context
/// (`@std_context`, D-692).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 10] {
    const W: &str = "Word";
    const I: &str = "i64";
    [
        (
            "cheby_net_sys_listen",
            cheby_net_sys_listen as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_net_sys_local_port",
            cheby_net_sys_local_port as *const u8,
            &[W],
            I,
        ),
        (
            "cheby_net_sys_accept",
            cheby_net_sys_accept as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_net_sys_connect",
            cheby_net_sys_connect as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_net_sys_read_bytes",
            cheby_net_sys_read_bytes as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_net_sys_write_bytes",
            cheby_net_sys_write_bytes as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_net_sys_shutdown_write",
            cheby_net_sys_shutdown_write as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_net_sys_close",
            cheby_net_sys_close as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_net_sys_close_listener",
            cheby_net_sys_close_listener as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_net_sys_connect_tls",
            cheby_net_sys_connect_tls as *const u8,
            &[W, I],
            W,
        ),
    ]
}

/// Appends the `std::net` kernels, with their addresses, to `out` (plan
/// §4.3.3), and registers the kinds 46 and 47 (D-982).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    handles::register();
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The C signatures of every `std::net` kernel after the context pointer,
/// for the cross-check against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

/// Test hooks of name lookup, keyed by host name in a static (D-686: no
/// thread-local storage outside `tls`): a hooked host's lookup job waits on a
/// gate the test opens, then answers the hook's addresses, and the hook
/// records when and on which thread the job's result was dropped.
#[cfg(test)]
pub(crate) mod hooks {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::ThreadId;

    use parking_lot::{Condvar, Mutex};

    /// One hooked host.
    #[derive(Debug, Default)]
    pub(crate) struct LookupHook {
        pub(crate) host: String,
        /// What the lookup answers; empty is a failure.
        pub(crate) addresses: Vec<SocketAddr>,
        /// Opened by the test; the job waits for it.
        gate: Mutex<bool>,
        opened: Condvar,
        /// The job started, on this thread.
        pub(crate) started_on: Mutex<Option<ThreadId>>,
        pub(crate) started: AtomicBool,
        /// The job's result was dropped this many times, last on this
        /// thread.
        pub(crate) drops: AtomicUsize,
        pub(crate) dropped_on: Mutex<Option<ThreadId>>,
    }

    impl LookupHook {
        /// A hook for `host` answering `addresses`, its gate open or
        /// closed.
        pub(crate) fn new(host: &str, addresses: Vec<SocketAddr>, open: bool) -> Arc<Self> {
            Arc::new(Self {
                host: host.to_owned(),
                addresses,
                gate: Mutex::new(open),
                ..Self::default()
            })
        }

        /// The job's side: waits for the gate, then answers.
        pub(crate) fn run(&self) -> std::io::Result<Vec<SocketAddr>> {
            *self.started_on.lock() = Some(std::thread::current().id());
            self.started.store(true, Ordering::SeqCst);
            let mut open = self.gate.lock();
            while !*open {
                self.opened.wait(&mut open);
            }
            if self.addresses.is_empty() {
                Err(std::io::Error::other("no such host"))
            } else {
                Ok(self.addresses.clone())
            }
        }

        /// Opens the gate.
        pub(crate) fn open(&self) {
            *self.gate.lock() = true;
            self.opened.notify_all();
        }

        /// The job's result was dropped on `thread`.
        pub(crate) fn dropped(&self, thread: ThreadId) {
            *self.dropped_on.lock() = Some(thread);
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The installed hooks.
    static HOOKS: Mutex<Vec<Arc<LookupHook>>> = Mutex::new(Vec::new());

    /// Installs `hook` until [`remove`].
    pub(crate) fn install(hook: &Arc<LookupHook>) {
        HOOKS.lock().push(Arc::clone(hook));
    }

    /// Removes `hook`.
    pub(crate) fn remove(hook: &Arc<LookupHook>) {
        HOOKS.lock().retain(|known| !Arc::ptr_eq(known, hook));
    }

    /// The hook of `host`, if one is installed.
    pub(crate) fn find(host: &str) -> Option<Arc<LookupHook>> {
        HOOKS.lock().iter().find(|hook| hook.host == host).cloned()
    }
}

#[cfg(test)]
#[path = "net_tests.rs"]
mod tests;
