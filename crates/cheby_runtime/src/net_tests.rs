//! Tests of the `std::net` kernels (D-998 to D-1002, D-1062, D-1063,
//! D-1065, D-1165): cancelled waits that take nothing, several fibers on
//! one socket, closing a socket that fibers wait on, the handles, the error
//! kinds, and name lookups that a cancellation abandons.
//!
//! Every socket is a loopback one, on a port the system chooses; nothing
//! reaches the network. Name lookups go through `hooks`, keyed by host
//! names under `.test`, which no resolver is asked about. Fibers run the
//! kernels' work functions (`listen_on`, `accept_on`, `read_on`, …) on
//! Rust-level roots, and the handle and lookup tests call the kernels
//! themselves. Every wait for an event is bounded by `testing::hang`
//! (D-872), and every retry loop by a count (risk R25). The races are
//! modelled in `sched/io_models.rs`: a close against a parked waiter
//! (model 3), two accepters (model 4) and a cancelled turn waiter (model
//! 5).

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::hooks::{self, LookupHook};
use super::*;
use crate::alloc::Domain;
use crate::entry::EntryOutcome;
use crate::fiber_ops::cancel;
use crate::fiber_ops::detached_tests::{
    self, close_scope, open_scope, release, spawn_into, unwinding, yield_until,
};
use crate::fiber_ops::scope::{ScopeHandle, ScopeState};
use crate::generic::{self, Reached};
use crate::sched::testing;
use crate::{rc, values};

/// The thread counts of every test that starts a runtime.
const THREADS: [usize; 2] = [1, 4];

/// The running fiber's runtime.
fn rt(me: &ExecCtx) -> Arc<Shared> {
    Arc::clone(me.sched.runtime().unwrap())
}

/// The running fiber's context pointer, as kernels take it.
fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// The state of a borrowed `Scope` handle.
fn state_of(scope: Word) -> Arc<ScopeState> {
    // SAFETY: a live `Scope` handle, borrowed.
    Arc::clone(&unsafe { handles::payload::<ScopeHandle>(scope, HandleKind::Scope) }.0)
}

/// Cancels the scope `scope` from outside and closes it, which waits for
/// its fibers; releases `fiber`.
fn cancel_and_close(me: &ExecCtx, scope: Word, fiber: Word) {
    cancel::cancel(&state_of(scope));
    close_scope(me, scope);
    release(fiber);
}

/// How a test fiber ends: unwinding for a cancellation, as a kernel's
/// raise would make it (D-670), or with `Nil`.
fn fiber_end(me: &ExecCtx, cancelled: bool) -> Word {
    let ctx = ctx_of(me);
    crate::unwind::guard::guarded(Some(ctx), || {
        if cancelled {
            crate::unwind::raise::raise_cancel(Some(ctx));
        }
        values::nil()
    })
}

/// Runs `body` as a root at `threads` workers and requires that it returns
/// 0; then stops the runtime.
fn run_root(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) {
    let runtime = detached_tests::runtime(threads);
    let domain = Domain::new();
    let outcome = detached_tests::root_outcome(&runtime, &domain, body);
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(0), "at {threads} workers");
}

/// A listener on `127.0.0.1` and a port the system chooses.
fn listening(me: &ExecCtx) -> Arc<ListenerState> {
    Arc::new(listen_on(me, &rt(me), "127.0.0.1", 0).unwrap().unwrap())
}

/// The loopback address of `listener`.
fn address_of(listener: &ListenerState) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], listener.port))
}

/// A connected loopback pair: the fiber's `Connection` state, accepted,
/// and the host's blocking `std` end.
fn pair(me: &ExecCtx) -> (Arc<ConnectionState>, TcpStream) {
    let listener = listening(me);
    let host = TcpStream::connect(address_of(&listener)).unwrap();
    let connection = accept_on(me, &rt(me), &listener).unwrap().unwrap();
    host.set_read_timeout(Some(testing::hang())).unwrap();
    (Arc::new(connection), host)
}

/// What a test fiber's operation gave.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    /// The wait was cancelled.
    Cancelled,
    /// The operation failed with this kind.
    Failed(i64),
    /// It succeeded, with these bytes (read, or read after an accept).
    Done(Vec<u8>),
}

/// The slot a fiber records its [`Got`] in.
type Slot = Arc<Mutex<Option<Got>>>;

/// [`Got`] of a work function's result, with `data` reading a success.
fn got<T>(
    result: &Result<Result<T, NetError>, Cancelled>,
    data: impl FnOnce(&T) -> Vec<u8>,
) -> Got {
    match result {
        Err(Cancelled) => Got::Cancelled,
        Ok(Err(error)) => Got::Failed(error.kind),
        Ok(Ok(value)) => Got::Done(data(value)),
    }
}

/// Spawns into `scope` a fiber that accepts on `listener`, then reads one
/// byte of the connection it got; records the outcome in `slot`.
fn spawn_accepter(me: &ExecCtx, scope: Word, listener: &Arc<ListenerState>, slot: &Slot) -> Word {
    let (listener, slot) = (Arc::clone(listener), Arc::clone(slot));
    spawn_into(me, scope, move |me| {
        let accepted = accept_on(me, &rt(me), &listener);
        let outcome = match &accepted {
            Ok(Ok(connection)) => match read_on(me, connection, 1) {
                Ok(Ok(Some(data))) => Got::Done(data),
                other => got(&other, |_| Vec::new()),
            },
            other => got(other, |_| Vec::new()),
        };
        let cancelled = outcome == Got::Cancelled;
        *slot.lock() = Some(outcome);
        fiber_end(me, cancelled)
    })
}

/// Spawns into `scope` a fiber that reads at most `limit` bytes of
/// `connection`; records the outcome in `slot`.
fn spawn_reader(
    me: &ExecCtx,
    scope: Word,
    connection: &Arc<ConnectionState>,
    limit: i64,
    slot: &Slot,
) -> Word {
    let (connection, slot) = (Arc::clone(connection), Arc::clone(slot));
    spawn_into(me, scope, move |me| {
        let read = read_on(me, &connection, limit);
        let outcome = match &read {
            Ok(Ok(None)) => Got::Failed(-1),
            other => got(other, |data| data.clone().unwrap_or_default()),
        };
        let cancelled = outcome == Got::Cancelled;
        *slot.lock() = Some(outcome);
        fiber_end(me, cancelled)
    })
}

/// Spawns into `scope` a fiber that writes `data` to `connection`; records
/// the outcome in `slot`.
fn spawn_writer(
    me: &ExecCtx,
    scope: Word,
    connection: &Arc<ConnectionState>,
    data: Vec<u8>,
    slot: &Slot,
) -> Word {
    let (connection, slot) = (Arc::clone(connection), Arc::clone(slot));
    spawn_into(me, scope, move |me| {
        let written = write_on(me, &connection, &data);
        let outcome = got(&written, |()| Vec::new());
        let cancelled = outcome == Got::Cancelled;
        *slot.lock() = Some(outcome);
        fiber_end(me, cancelled)
    })
}

/// Whether a fiber waits on `socket`'s seam in `dir`.
fn armed<S: mio::event::Source>(socket: &NetSocket<S>, dir: Direction) -> bool {
    socket.io().armed(dir)
}

/// How many fibers wait for `socket`'s turn of `dir`.
fn queued<S: mio::event::Source>(socket: &NetSocket<S>, dir: Direction) -> usize {
    socket.turn_of(dir).probe().1
}

/// Waits on the host until `ready` holds, or panics after
/// [`testing::hang`].
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "the host waited too long"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Whether `listener` (non-blocking) has no connection waiting during
/// `window`.
fn accepts_nothing(listener: &TcpListener, window: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < window {
        match listener.accept() {
            Ok(_) => return false,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept failed: {error}"),
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

/// D-1000, D-1001, `net.md` §`accept`: two fibers accept on one listener,
/// one holding the read turn and parked on the poller, one queued for the
/// turn; one of them is cancelled, then a client connects: the other gets
/// the connection, the cancelled one took nothing, and nothing is left to
/// accept. Cancelling the holder and cancelling the queued fiber, at one
/// and four workers.
fn cancelled_accept(threads: usize, cancel_holder: bool) {
    let slots: [Slot; 2] = [Arc::default(), Arc::default()];
    let left = Arc::new(AtomicBool::new(true));
    {
        let (slots, left) = (slots.clone(), Arc::clone(&left));
        run_root(threads, move |me| {
            let listener = listening(me);
            // Scopes close innermost first: the cancelled fiber's is inner.
            let kept = open_scope(me);
            let gone = open_scope(me);
            let (first_scope, second_scope) = if cancel_holder {
                (gone, kept)
            } else {
                (kept, gone)
            };
            let first = spawn_accepter(me, first_scope, &listener, &slots[0]);
            yield_until(me, || armed(&listener.socket, Direction::Read));
            let second = spawn_accepter(me, second_scope, &listener, &slots[1]);
            yield_until(me, || queued(&listener.socket, Direction::Read) == 1);
            let (gone_fiber, kept_fiber) = if cancel_holder {
                (first, second)
            } else {
                (second, first)
            };
            cancel_and_close(me, gone, gone_fiber);
            // The other one waits on the poller again, holding the turn.
            yield_until(me, || {
                armed(&listener.socket, Direction::Read)
                    && queued(&listener.socket, Direction::Read) == 0
            });
            let mut client = TcpStream::connect(address_of(&listener)).unwrap();
            client.write_all(&[7]).unwrap();
            close_scope(me, kept);
            release(kept_fiber);
            // Nothing else waits on the listener.
            let socket = listener.socket.current().unwrap();
            left.store(
                !matches!(socket.socket().accept(), Err(error) if error.kind() == ErrorKind::WouldBlock),
                Ordering::SeqCst,
            );
            drop(client);
            0
        });
    }
    let (gone, kept) = if cancel_holder { (0, 1) } else { (1, 0) };
    assert_eq!(
        slots[gone].lock().clone(),
        Some(Got::Cancelled),
        "{threads} workers"
    );
    assert_eq!(
        slots[kept].lock().clone(),
        Some(Got::Done(vec![7])),
        "{threads} workers"
    );
    assert!(!left.load(Ordering::SeqCst), "a connection was left");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_cancelled_accept_takes_nothing() {
    for threads in THREADS {
        cancelled_accept(threads, true);
        cancelled_accept(threads, false);
    }
}

/// D-670, D-1000: bytes arrive one at a time, every few hundred
/// microseconds, while the root starts read after read and cancels each
/// one as soon as it waits on the poller (or has returned): the reads that
/// returned got every byte, in order, and none was lost to a cancelled
/// read. At one and four workers.
fn cancelled_reads(threads: usize) {
    const TOTAL: usize = 300;
    let sent: Vec<u8> = (0..TOTAL)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    let seen: Arc<Mutex<(Vec<u8>, usize)>> = Arc::default();
    {
        let (sent, seen) = (sent.clone(), Arc::clone(&seen));
        run_root(threads, move |me| {
            let (connection, mut host) = pair(me);
            let writer = thread::spawn(move || {
                for byte in sent {
                    host.write_all(&[byte]).unwrap();
                    thread::sleep(Duration::from_micros(300));
                }
                host
            });
            let mut received = Vec::new();
            let mut cancelled = 0_usize;
            // Bounded (risk R25): far more rounds than bytes.
            for _ in 0..1_000_000 {
                if received.len() >= TOTAL {
                    break;
                }
                let slot: Slot = Arc::default();
                let scope = open_scope(me);
                let fiber = spawn_reader(me, scope, &connection, 1_024, &slot);
                yield_until(me, || {
                    armed(&connection.socket, Direction::Read) || slot.lock().is_some()
                });
                cancel_and_close(me, scope, fiber);
                match slot.lock().take() {
                    Some(Got::Done(data)) => received.extend(data),
                    Some(Got::Cancelled) => cancelled = cancelled.saturating_add(1),
                    other => panic!("a read gave {other:?}"),
                }
            }
            let host = writer.join().unwrap();
            drop(host);
            *seen.lock() = (received, cancelled);
            0
        });
    }
    let (received, cancelled) = seen.lock().clone();
    assert_eq!(
        received, sent,
        "bytes were lost or reordered at {threads} workers"
    );
    assert!(cancelled > 0, "no read was cancelled while it waited");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_cancelled_read_keeps_bytes() {
    for threads in THREADS {
        cancelled_reads(threads);
    }
}

/// D-1000, D-1065: a `connect` cancelled while it waits for the lookup of
/// its host leaves no connection: the lookup runs to its end on its pool
/// thread, its answer (the listener's address) is dropped there, and the
/// listener has nothing to accept. A `connect` of a fiber already
/// targeted by a cancellation sends no connection request either. At one
/// and four workers.
fn cancelled_connects(threads: usize) {
    let host = TcpListener::bind("127.0.0.1:0").unwrap();
    host.set_nonblocking(true).unwrap();
    let address = host.local_addr().unwrap();
    let name = format!("net-cancelled-connect-{threads}.test");
    let hook = LookupHook::new(&name, vec![address], false);
    hooks::install(&hook);
    let slots: [Slot; 2] = [Arc::default(), Arc::default()];
    let drops_at_return = Arc::new(Mutex::new(usize::MAX));
    {
        let (slots, hook, drops_at_return) = (
            slots.clone(),
            Arc::clone(&hook),
            Arc::clone(&drops_at_return),
        );
        run_root(threads, move |me| {
            // During the lookup.
            let scope = open_scope(me);
            let fiber = {
                let (slot, name) = (Arc::clone(&slots[0]), name.clone());
                spawn_into(me, scope, move |me| {
                    let made = connect_to(me, &rt(me), &name, i64::from(address.port()));
                    let outcome = got(&made, |_| Vec::new());
                    let cancelled = outcome == Got::Cancelled;
                    *slot.lock() = Some(outcome);
                    fiber_end(me, cancelled)
                })
            };
            yield_until(me, || hook.started.load(Ordering::SeqCst));
            cancel_and_close(me, scope, fiber);
            *drops_at_return.lock() = hook.drops.load(Ordering::SeqCst);
            // Already targeted when it connects.
            let go = Arc::new(AtomicBool::new(false));
            let started = Arc::new(AtomicBool::new(false));
            let scope = open_scope(me);
            let fiber = {
                let (slot, go, started) =
                    (Arc::clone(&slots[1]), Arc::clone(&go), Arc::clone(&started));
                spawn_into(me, scope, move |me| {
                    started.store(true, Ordering::SeqCst);
                    yield_until(me, || go.load(Ordering::SeqCst));
                    let made = connect_to(me, &rt(me), "127.0.0.1", i64::from(address.port()));
                    let outcome = got(&made, |_| Vec::new());
                    let cancelled = outcome == Got::Cancelled;
                    *slot.lock() = Some(outcome);
                    fiber_end(me, cancelled)
                })
            };
            yield_until(me, || started.load(Ordering::SeqCst));
            cancel::cancel(&state_of(scope));
            go.store(true, Ordering::SeqCst);
            close_scope(me, scope);
            release(fiber);
            0
        });
    }
    assert_eq!(
        *drops_at_return.lock(),
        0,
        "the lookup ended before the fiber returned"
    );
    hook.open();
    wait_until(|| hook.drops.load(Ordering::SeqCst) == 1);
    hooks::remove(&hook);
    assert_eq!(slots[0].lock().clone(), Some(Got::Cancelled));
    assert_eq!(slots[1].lock().clone(), Some(Got::Cancelled));
    assert!(
        accepts_nothing(&host, testing::scaled(Duration::from_millis(300))),
        "a cancelled connect left a connection"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_cancelled_connect_leaves_no_connection() {
    for threads in THREADS {
        cancelled_connects(threads);
    }
}

/// D-1001, `net.md` §`accept`: two fibers accept on one listener and
/// two clients connect; each fiber gets exactly one of them. At one
/// and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_two_accepters_each_get_one() {
    for threads in THREADS {
        let slots: [Slot; 2] = [Arc::default(), Arc::default()];
        {
            let slots = slots.clone();
            run_root(threads, move |me| {
                let listener = listening(me);
                let scope = open_scope(me);
                let first = spawn_accepter(me, scope, &listener, &slots[0]);
                let second = spawn_accepter(me, scope, &listener, &slots[1]);
                yield_until(me, || {
                    armed(&listener.socket, Direction::Read)
                        && queued(&listener.socket, Direction::Read) == 1
                });
                let clients: Vec<TcpStream> = [1_u8, 2]
                    .iter()
                    .map(|byte| {
                        let mut client = TcpStream::connect(address_of(&listener)).unwrap();
                        client.write_all(&[*byte]).unwrap();
                        client
                    })
                    .collect();
                close_scope(me, scope);
                release(first);
                release(second);
                drop(clients);
                0
            });
        }
        let mut bytes: Vec<Vec<u8>> = slots
            .iter()
            .map(|slot| match slot.lock().clone() {
                Some(Got::Done(data)) => data,
                other => panic!("an accepter gave {other:?} at {threads} workers"),
            })
            .collect();
        bytes.sort();
        assert_eq!(bytes, [vec![1], vec![2]], "at {threads} workers");
    }
}

/// D-1001, D-684: two fibers write 4 MiB each to one connection at
/// once, far more than its buffers hold, so each parks mid-write; the
/// peer reads one whole buffer, then the other, never interleaved. At
/// one and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_writes_are_atomic() {
    const EACH: usize = 4 << 20;
    for threads in THREADS {
        let read: Arc<Mutex<Vec<u8>>> = Arc::default();
        let slots: [Slot; 2] = [Arc::default(), Arc::default()];
        {
            let (read, slots) = (Arc::clone(&read), slots.clone());
            run_root(threads, move |me| {
                let (connection, mut host) = pair(me);
                let reader = thread::spawn(move || {
                    // Late, so that both writers have started.
                    thread::sleep(Duration::from_millis(50));
                    let mut all = Vec::with_capacity(EACH * 2);
                    let mut chunk = vec![0_u8; 64 << 10];
                    while all.len() < EACH * 2 {
                        let count = host.read(&mut chunk).unwrap();
                        assert_ne!(count, 0, "the peer ended early");
                        all.extend_from_slice(chunk.get(..count).unwrap());
                    }
                    all
                });
                let scope = open_scope(me);
                let a = spawn_writer(me, scope, &connection, vec![b'a'; EACH], &slots[0]);
                let b = spawn_writer(me, scope, &connection, vec![b'b'; EACH], &slots[1]);
                close_scope(me, scope);
                release(a);
                release(b);
                *read.lock() = reader.join().unwrap();
                0
            });
        }
        for slot in &slots {
            assert_eq!(slot.lock().clone(), Some(Got::Done(Vec::new())));
        }
        let read = read.lock();
        assert_eq!(read.len(), EACH * 2);
        let switches = read
            .windows(2)
            .filter(|pair| pair.first() != pair.get(1))
            .count();
        assert_eq!(switches, 1, "two writes interleaved at {threads} workers");
    }
}

/// D-1001: a fiber parked in `read_bytes` does not hold up a write on
/// the same connection: another fiber writes 1 MiB, which the peer
/// drains, while the reader stays parked; then the peer writes, and the
/// reader gets it. At one and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_read_and_write_at_once() {
    const TOTAL: usize = 1 << 20;
    for threads in THREADS {
        let slots: [Slot; 2] = [Arc::default(), Arc::default()];
        let parked_after_write = Arc::new(AtomicBool::new(false));
        {
            let (slots, parked_after_write) = (slots.clone(), Arc::clone(&parked_after_write));
            run_root(threads, move |me| {
                let (connection, mut host) = pair(me);
                let mut drain = host.try_clone().unwrap();
                let drainer = thread::spawn(move || {
                    let mut total = 0_usize;
                    let mut chunk = vec![0_u8; 64 << 10];
                    while total < TOTAL {
                        let count = drain.read(&mut chunk).unwrap();
                        assert_ne!(count, 0, "the peer ended early");
                        total = total.saturating_add(count);
                    }
                });
                let scope = open_scope(me);
                let reader = spawn_reader(me, scope, &connection, 16, &slots[0]);
                yield_until(me, || armed(&connection.socket, Direction::Read));
                let writer = spawn_writer(me, scope, &connection, vec![9; TOTAL], &slots[1]);
                yield_until(me, || slots[1].lock().is_some());
                parked_after_write.store(
                    armed(&connection.socket, Direction::Read) && slots[0].lock().is_none(),
                    Ordering::SeqCst,
                );
                drainer.join().unwrap();
                host.write_all(b"x").unwrap();
                close_scope(me, scope);
                release(reader);
                release(writer);
                0
            });
        }
        assert!(
            parked_after_write.load(Ordering::SeqCst),
            "the read did not stay parked"
        );
        assert_eq!(slots[1].lock().clone(), Some(Got::Done(Vec::new())));
        assert_eq!(slots[0].lock().clone(), Some(Got::Done(b"x".to_vec())));
    }
}

/// D-1063: closing a connection wakes a fiber parked in `read_bytes`
/// with kind 12; every later operation, a second close included, fails
/// the same way; the peer reads the end of the stream. At one and four
/// workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_close_wakes_a_parked_reader_and_refuses_later_operations() {
    for threads in THREADS {
        let slot: Slot = Arc::default();
        let later: Arc<Mutex<Vec<i64>>> = Arc::default();
        let peer_end = Arc::new(Mutex::new(None));
        {
            let (slot, later, peer_end) =
                (Arc::clone(&slot), Arc::clone(&later), Arc::clone(&peer_end));
            run_root(threads, move |me| {
                let (connection, mut host) = pair(me);
                let scope = open_scope(me);
                let reader = spawn_reader(me, scope, &connection, 16, &slot);
                yield_until(me, || armed(&connection.socket, Direction::Read));
                connection.socket.close().unwrap();
                close_scope(me, scope);
                release(reader);
                let kinds = [
                    got(&read_on(me, &connection, 16), |_| Vec::new()),
                    got(&write_on(me, &connection, b"late"), |()| Vec::new()),
                    got(&shutdown_on(me, &connection), |()| Vec::new()),
                    got(&Ok(connection.socket.close()), |()| Vec::new()),
                ];
                *later.lock() = kinds
                    .iter()
                    .map(|kind| match kind {
                        Got::Failed(kind) => *kind,
                        _ => -1,
                    })
                    .collect();
                let mut buffer = [0_u8; 4];
                *peer_end.lock() = Some(host.read(&mut buffer).map_err(|error| error.kind()));
                0
            });
        }
        assert_eq!(slot.lock().clone(), Some(Got::Failed(12)));
        assert_eq!(*later.lock(), [12, 12, 12, 12]);
        assert_eq!(
            *peer_end.lock(),
            Some(Ok(0)),
            "the peer did not see the end"
        );
    }
}

/// D-1063: closing a listener wakes the fiber parked in `accept` on the
/// poller and the one queued for the turn, each with kind 12, and a
/// later `accept` fails so; `local_port` still answers. At one and four
/// workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_close_wakes_every_accepter() {
    for threads in THREADS {
        let slots: [Slot; 2] = [Arc::default(), Arc::default()];
        let after = Arc::new(Mutex::new(None));
        {
            let (slots, after) = (slots.clone(), Arc::clone(&after));
            run_root(threads, move |me| {
                let listener = listening(me);
                let port = listener.port;
                let scope = open_scope(me);
                let first = spawn_accepter(me, scope, &listener, &slots[0]);
                yield_until(me, || armed(&listener.socket, Direction::Read));
                let second = spawn_accepter(me, scope, &listener, &slots[1]);
                yield_until(me, || queued(&listener.socket, Direction::Read) == 1);
                listener.socket.close().unwrap();
                close_scope(me, scope);
                release(first);
                release(second);
                let accepted = got(&accept_on(me, &rt(me), &listener), |_| Vec::new());
                *after.lock() = Some((accepted, listener.port == port));
                0
            });
        }
        for slot in &slots {
            assert_eq!(
                slot.lock().clone(),
                Some(Got::Failed(12)),
                "{threads} workers"
            );
        }
        assert_eq!(after.lock().clone(), Some((Got::Failed(12), true)));
    }
}

/// D-1063: closing a connection wakes a fiber parked in `write_bytes`
/// (the peer reads nothing, so the buffers fill) with kind 12. At one
/// and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_close_wakes_a_parked_writer() {
    for threads in THREADS {
        let slot: Slot = Arc::default();
        {
            let slot = Arc::clone(&slot);
            run_root(threads, move |me| {
                let (connection, host) = pair(me);
                let scope = open_scope(me);
                let writer = spawn_writer(me, scope, &connection, vec![1; 32 << 20], &slot);
                yield_until(me, || armed(&connection.socket, Direction::Write));
                connection.socket.close().unwrap();
                close_scope(me, scope);
                release(writer);
                drop(host);
                0
            });
        }
        assert_eq!(slot.lock().clone(), Some(Got::Failed(12)));
    }
}

/// D-1063, D-1001: a close while a system call on the socket runs on
/// another worker (here, its reference to the descriptor's owner, held
/// as a running call holds it) does not close the descriptor: the peer
/// sees no end of the stream until the call's reference goes, then it
/// does. The close itself returns at once.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_close_keeps_the_descriptor_under_a_running_call() {
    let seen = Arc::new(Mutex::new(None));
    {
        let seen = Arc::clone(&seen);
        run_root(2, move |me| {
            let (connection, mut host) = pair(me);
            let running = connection.socket.current().unwrap();
            let started = Instant::now();
            connection.socket.close().unwrap();
            let closed_in = started.elapsed();
            host.set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut buffer = [0_u8; 4];
            let during = host.read(&mut buffer).map_err(|error| error.kind());
            drop(running);
            host.set_read_timeout(Some(testing::hang())).unwrap();
            let after = host.read(&mut buffer).map_err(|error| error.kind());
            *seen.lock() = Some((during, after, closed_in));
            0
        });
    }
    let (during, after, closed_in) = (*seen.lock()).unwrap();
    assert!(
        matches!(during, Err(ErrorKind::WouldBlock | ErrorKind::TimedOut)),
        "the descriptor closed under a running call: {during:?}"
    );
    assert_eq!(after, Ok(0), "the descriptor did not close after the call");
    assert!(
        closed_in < testing::scaled(Duration::from_secs(1)),
        "{closed_in:?}"
    );
}

/// The payload of an owned `Result` word, `Ok` or `Err`, dup'd; the
/// result is dropped.
fn result_of(word: Word) -> Result<Word, Word> {
    // SAFETY: a live `Result` owned by the test.
    unsafe {
        let parts = values::read_result(word);
        let payload = match parts {
            Ok(value) | Err(value) => value,
        };
        rc::dup(payload);
        rc::drop(word);
        parts.map(|_| payload).map_err(|_| payload)
    }
}

/// The kind and message of an owned `(Int, String)` pair, dropped.
fn pair_of(word: Word) -> (i64, String) {
    // SAFETY: a live `(Int, String)` owned by the test.
    unsafe {
        let fields = values::read_tuple(word);
        let out = match fields.as_slice() {
            [kind, message] => (
                values::word_to_i64(*kind),
                values::str_of(*message).to_owned(),
            ),
            _ => (-1, String::new()),
        };
        rc::drop(word);
        out
    }
}

/// D-982, D-233: through the kernels, a `Listener` and a `Connection`
/// print as `<Listener>` and `<Connection>`, and `==` and hashing stop
/// on them; a closed connection's second close is `Err((12, ""))`; the
/// handles' finalizers run when the last reference goes and nothing
/// leaks.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_handles_print_and_refuse_equality() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let runtime = detached_tests::runtime(2);
    let domain = Domain::new();
    let outcome = {
        let seen = Arc::clone(&seen);
        detached_tests::root_outcome(&runtime, &domain, move |me| {
            let ctx = ctx_of(me);
            let host = values::string("127.0.0.1");
            // SAFETY: the running fiber's context and a live string.
            let listener = result_of(unsafe { cheby_net_sys_listen(ctx, host, 0) }).unwrap();
            // SAFETY: as above, and a live `Listener`.
            let port = unsafe { cheby_net_sys_local_port(ctx, listener) };
            // SAFETY: as above.
            let connection = result_of(unsafe { cheby_net_sys_connect(ctx, host, port) }).unwrap();
            let mut lines = Vec::new();
            for word in [listener, connection] {
                // SAFETY: live handles.
                unsafe {
                    lines.push(generic::debug_string(word));
                    lines.push(format!("{:?}", generic::equal(word, word)));
                    lines.push(format!(
                        "{:?}",
                        generic::hash_with_seed(word, 7).map(|_| ())
                    ));
                }
            }
            // SAFETY: as above, and a live `Connection`.
            let closed = result_of(unsafe { cheby_net_sys_close(ctx, connection) });
            lines.push(format!("{}", closed == Ok(values::nil())));
            // SAFETY: as above.
            let again = result_of(unsafe { cheby_net_sys_close(ctx, connection) });
            lines.push(format!("{:?}", again.map_err(pair_of)));
            for word in [host, listener, connection] {
                // SAFETY: owned by the test.
                unsafe { rc::drop(word) };
            }
            *seen.lock() = lines;
            0
        })
    };
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(0));
    let function = format!("{:?}", Err::<bool, _>(Reached::Function));
    let hashed = format!("{:?}", Err::<(), _>(Reached::Function));
    assert_eq!(
        *seen.lock(),
        [
            "<Listener>".to_owned(),
            function.clone(),
            hashed.clone(),
            "<Connection>".to_owned(),
            function,
            hashed,
            "true".to_owned(),
            format!("{:?}", Err::<Word, _>((12_i64, String::new()))),
        ]
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-981, `net.md`, D-1184: the system's errors map to the
/// kinds: refused 8; reset, aborted and a broken pipe 9 (and macOS's
/// `EPROTOTYPE`); in use 10; `EACCES` and `EPERM` from `bind` 2, from any
/// other call 0; a closed handle 12; every other code 0 with the system's
/// message.
#[test]
#[cfg(unix)]
fn net_errors_map_to_their_kinds() {
    let of = |code: i32, op: Op| NetError::of(&io::Error::from_raw_os_error(code), op);
    for (code, kind) in [
        (libc::ECONNREFUSED, 8),
        (libc::ECONNRESET, 9),
        (libc::ECONNABORTED, 9),
        (libc::EPIPE, 9),
        (libc::EADDRINUSE, 10),
    ] {
        assert_eq!(of(code, Op::Other), NetError::fixed(kind), "{code}");
        assert_eq!(of(code, Op::Bind), NetError::fixed(kind), "{code}");
    }
    for code in [libc::EACCES, libc::EPERM] {
        assert_eq!(of(code, Op::Bind), NetError::fixed(2), "{code}");
        assert_eq!(of(code, Op::Other).kind, 0, "{code}");
    }
    let unreachable = of(libc::ENETUNREACH, Op::Other);
    assert_eq!(unreachable.kind, 0);
    assert_ne!(unreachable.message, "");
    assert!(
        !unreachable.message.contains("os error"),
        "{}",
        unreachable.message
    );
    assert_eq!(
        of(libc::EPROTOTYPE, Op::Other).kind,
        if cfg!(target_vendor = "apple") { 9 } else { 0 }
    );
    let closed = io::Error::new(io::ErrorKind::NotConnected, "x");
    assert_eq!(NetError::of(&closed, Op::Other).kind, 0);
}

/// `net.md` §Addresses: a port outside 0 to 65535 and a host that is
/// empty or holds U+0000 are kind 13 before any lookup, through
/// `listen` and `connect`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_bad_addresses_are_invalid_input() {
    let kinds: Arc<Mutex<Vec<i64>>> = Arc::default();
    {
        let kinds = Arc::clone(&kinds);
        run_root(1, move |me| {
            let runtime = rt(me);
            let mut out = Vec::new();
            for (host, port) in [
                ("127.0.0.1", -1),
                ("127.0.0.1", 65_536),
                ("", 80),
                ("a\0b", 80),
            ] {
                out.push(got(&listen_on(me, &runtime, host, port), |_| Vec::new()));
                out.push(got(&connect_to(me, &runtime, host, port), |_| Vec::new()));
            }
            *kinds.lock() = out
                .iter()
                .map(|outcome| match outcome {
                    Got::Failed(kind) => *kind,
                    _ => -1,
                })
                .collect();
            0
        });
    }
    assert_eq!(*kinds.lock(), [13; 8]);
}

/// D-1065, D-1165, D-983: a fiber cancelled while it waits for a host
/// name's lookup returns at once, through `connect`'s kernel, which
/// raises: while the lookup is still blocked on its pool thread, the
/// fiber has unwound and its scope has closed. The lookup then runs to
/// its end, and its result is dropped on the pool thread that ran it.
/// At one and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lookup_detached_a_cancelled_lookup_returns_at_once() {
    for threads in THREADS {
        let name = format!("lookup-detached-{threads}.test");
        let hook = LookupHook::new(&name, vec![SocketAddr::from(([127, 0, 0, 1], 9))], false);
        hooks::install(&hook);
        let seen = Arc::new(Mutex::new(None));
        {
            let (hook, seen) = (Arc::clone(&hook), Arc::clone(&seen));
            run_root(threads, move |me| {
                let raised = Arc::new(AtomicBool::new(false));
                let scope = open_scope(me);
                let fiber = {
                    let (raised, name) = (Arc::clone(&raised), name.clone());
                    spawn_into(me, scope, move |me| {
                        let host = values::string(&name);
                        // SAFETY: the running fiber's context and a live
                        // string.
                        let word = unsafe { cheby_net_sys_connect(ctx_of(me), host, 80) };
                        raised.store(unwinding(me), Ordering::SeqCst);
                        // SAFETY: owned by the fiber.
                        unsafe { rc::drop(host) };
                        word
                    })
                };
                yield_until(me, || hook.started.load(Ordering::SeqCst));
                let cancelled_at = Instant::now();
                cancel_and_close(me, scope, fiber);
                let returned_in = cancelled_at.elapsed();
                *seen.lock() = Some((
                    raised.load(Ordering::SeqCst),
                    hook.drops.load(Ordering::SeqCst),
                    returned_in,
                ));
                0
            });
        }
        let (raised, drops_then, returned_in) = (*seen.lock()).unwrap();
        assert!(raised, "the kernel did not raise the cancellation");
        assert_eq!(drops_then, 0, "the fiber waited for the lookup");
        assert!(returned_in < testing::hang(), "{returned_in:?}");
        hook.open();
        wait_until(|| hook.drops.load(Ordering::SeqCst) == 1);
        assert_eq!(
            *hook.dropped_on.lock(),
            *hook.started_on.lock(),
            "the result was not dropped on its pool thread"
        );
        hooks::remove(&hook);
    }
}

/// D-981: every failure of name lookup, an empty answer included, is
/// kind 11, through `listen` and `connect`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lookup_detached_failures_are_host_not_found() {
    const NOTHING: &str = "lookup-detached-nothing.test";
    let hook = LookupHook::new(NOTHING, Vec::new(), true);
    hooks::install(&hook);
    let kinds: Arc<Mutex<Vec<Got>>> = Arc::default();
    {
        let kinds = Arc::clone(&kinds);
        run_root(2, move |me| {
            let runtime = rt(me);
            *kinds.lock() = vec![
                got(&listen_on(me, &runtime, NOTHING, 0), |_| Vec::new()),
                got(&connect_to(me, &runtime, NOTHING, 80), |_| Vec::new()),
            ];
            0
        });
    }
    hooks::remove(&hook);
    assert_eq!(*kinds.lock(), [Got::Failed(11), Got::Failed(11)]);
}

/// `net.md` §`listen`: a name's first address is the one used. The
/// lookup answers `127.0.0.1` then `127.0.0.2`, and the listener is on
/// `127.0.0.1`; answered the other way round, `listen` binds
/// `127.0.0.2` or fails where that address does not exist (macOS), and
/// never falls back to `127.0.0.1`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_listen_uses_the_first_address() {
    let first = SocketAddr::from(([127, 0, 0, 1], 0));
    let second = SocketAddr::from(([127, 0, 0, 2], 0));
    let forward = LookupHook::new("net-listen-forward.test", vec![first, second], true);
    let backward = LookupHook::new("net-listen-backward.test", vec![second, first], true);
    hooks::install(&forward);
    hooks::install(&backward);
    let seen = Arc::new(Mutex::new(None));
    {
        let seen = Arc::clone(&seen);
        run_root(2, move |me| {
            let runtime = rt(me);
            let ip_of = |state: &ListenerState| {
                state
                    .socket
                    .current()
                    .and_then(|owned| owned.socket().local_addr())
                    .map(|address| address.ip())
                    .ok()
            };
            let forward = listen_on(me, &runtime, "net-listen-forward.test", 0)
                .unwrap()
                .map(|state| ip_of(&state));
            let backward = listen_on(me, &runtime, "net-listen-backward.test", 0)
                .unwrap()
                .map(|state| ip_of(&state));
            *seen.lock() = Some((forward, backward));
            0
        });
    }
    hooks::remove(&forward);
    hooks::remove(&backward);
    let (forward, backward) = seen.lock().clone().unwrap();
    assert_eq!(forward, Ok(Some(first.ip())));
    match backward {
        Ok(ip) => assert_eq!(ip, Some(second.ip())),
        Err(error) => assert_eq!(error.kind, 0, "{error:?}"),
    }
}

/// `net.md` §`connect`: the addresses are tried in the order the lookup
/// gives them until one connects, and when every one fails the error is
/// the last one's: a refused address then a listening one connects; a
/// broadcast address (refused by the system, kind 0) then a refused
/// one is kind 8; the other way round, kind 0.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn net_connect_tries_each_address_in_order() {
    let refused = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };
    let listening = TcpListener::bind("127.0.0.1:0").unwrap();
    let broadcast = SocketAddr::from(([255, 255, 255, 255], refused.port()));
    let cases = [
        (
            "net-connect-then.test",
            vec![refused, listening.local_addr().unwrap()],
        ),
        ("net-connect-last-refused.test", vec![broadcast, refused]),
        ("net-connect-last-other.test", vec![refused, broadcast]),
    ];
    let installed: Vec<Arc<LookupHook>> = cases
        .iter()
        .map(|(name, addresses)| {
            let hook = LookupHook::new(name, addresses.clone(), true);
            hooks::install(&hook);
            hook
        })
        .collect();
    let seen: Arc<Mutex<Vec<Got>>> = Arc::default();
    {
        let seen = Arc::clone(&seen);
        let names: Vec<&'static str> = cases.iter().map(|(name, _)| *name).collect();
        run_root(2, move |me| {
            let runtime = rt(me);
            *seen.lock() = names
                .iter()
                .map(|name| got(&connect_to(me, &runtime, name, 0), |_| Vec::new()))
                .collect();
            0
        });
    }
    for hook in &installed {
        hooks::remove(hook);
    }
    let seen = seen.lock().clone();
    assert_eq!(seen.first(), Some(&Got::Done(Vec::new())));
    assert_eq!(seen.get(1), Some(&Got::Failed(8)));
    assert_eq!(seen.get(2), Some(&Got::Failed(0)));
    drop(listening);
}

/// TLS on the poller (D-1011, D-984, D-1290): the server is a
/// host thread with a `rustls` server over a blocking loopback socket,
/// presenting the test CA's leaf; the fiber's client trusts the test CA
/// through its own configuration (`net_tls_tests.rs`).
#[cfg(not(windows))]
mod tls {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use parking_lot::Mutex;

    use super::{
        Got, Slot, THREADS, cancel_and_close, fiber_end, got, open_scope, rt, run_root, spawn_into,
        yield_until,
    };
    use crate::net::hooks::{self, LookupHook};
    use crate::net::{KIND_TLS, connect_tls_to, read_on, shutdown_on, write_on};
    use crate::net_tls::tests::{server_config, trusting_test_ca};
    use crate::sched::testing;

    /// A host listener on `127.0.0.1` and its port.
    fn host_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// The host's end of one accepted connection, its reads bounded by
    /// [`testing::hang`].
    fn accepted(listener: &TcpListener) -> TcpStream {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(testing::hang())).unwrap();
        stream
    }

    /// The bytes of a payload of `size`, not all equal.
    fn payload(size: usize, salt: u8) -> Vec<u8> {
        (0..size)
            .map(|index| u8::try_from(index % 251).unwrap() ^ salt)
            .collect()
    }

    /// A secured connection carries 512 KiB each way, more than one
    /// socket buffer, so both directions wait on the poller; the server's
    /// `close_notify` ends the client's reads, and `shutdown_write` sends
    /// the client's.
    fn round_trip(threads: usize) {
        const SIZE: usize = 512 << 10;
        let (listener, port) = host_listener();
        let host = thread::spawn(move || {
            let session = rustls::ServerConnection::new(server_config()).unwrap();
            let mut tls = rustls::StreamOwned::new(session, accepted(&listener));
            let mut request = vec![0_u8; SIZE];
            tls.read_exact(&mut request).unwrap();
            assert_eq!(request, payload(SIZE, 1), "the request");
            tls.write_all(&payload(SIZE, 2)).unwrap();
            tls.conn.send_close_notify();
            tls.flush().unwrap();
            let mut rest = Vec::new();
            tls.read_to_end(&mut rest).unwrap();
            rest
        });
        let seen: Arc<Mutex<Vec<u8>>> = Arc::default();
        {
            let seen = Arc::clone(&seen);
            run_root(threads, move |me| {
                let config = trusting_test_ca();
                let connection = connect_tls_to(me, &rt(me), "127.0.0.1", i64::from(port), &config)
                    .unwrap()
                    .unwrap();
                write_on(me, &connection, &payload(SIZE, 1))
                    .unwrap()
                    .unwrap();
                let mut received = Vec::new();
                // Bounded (risk R25): far more rounds than blocks.
                for _ in 0..100_000 {
                    match read_on(me, &connection, 65_536).unwrap().unwrap() {
                        Some(data) => received.extend(data),
                        None => break,
                    }
                }
                shutdown_on(me, &connection).unwrap().unwrap();
                *seen.lock() = received;
                0
            });
        }
        let rest = host.join().unwrap();
        assert!(rest.is_empty(), "the client sent more than its request");
        assert!(
            *seen.lock() == payload(SIZE, 2),
            "the response at {threads} workers"
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
    )]
    fn tls_poller_round_trip() {
        for threads in THREADS {
            round_trip(threads);
        }
    }

    /// D-984: a fiber parked in the handshake, waiting for a server that
    /// read the `ClientHello` and never answers, is cancelled at once: it
    /// unwinds, and its socket is closed, which the server sees as the end
    /// of the stream long before its read times out.
    fn cancelled_handshake(threads: usize) {
        let (listener, port) = host_listener();
        let heard = Arc::new(AtomicBool::new(false));
        let host = {
            let heard = Arc::clone(&heard);
            thread::spawn(move || {
                let mut stream = accepted(&listener);
                let mut first = [0_u8; 1];
                stream.read_exact(&mut first).unwrap();
                heard.store(true, Ordering::SeqCst);
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).map(|_| first)
            })
        };
        let slot: Slot = Arc::default();
        {
            let slot = Arc::clone(&slot);
            run_root(threads, move |me| {
                let scope = open_scope(me);
                let fiber = {
                    let slot = Arc::clone(&slot);
                    spawn_into(me, scope, move |me| {
                        let config = trusting_test_ca();
                        let made =
                            connect_tls_to(me, &rt(me), "127.0.0.1", i64::from(port), &config);
                        let outcome = got(&made, |_| Vec::new());
                        let cancelled = outcome == Got::Cancelled;
                        *slot.lock() = Some(outcome);
                        fiber_end(me, cancelled)
                    })
                };
                yield_until(me, || heard.load(Ordering::SeqCst));
                cancel_and_close(me, scope, fiber);
                0
            });
        }
        assert_eq!(
            slot.lock().clone(),
            Some(Got::Cancelled),
            "{threads} workers"
        );
        let first = host.join().unwrap().expect("the server's read timed out");
        assert_eq!(first, [0x16], "a TLS handshake record");
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
    )]
    fn tls_poller_cancelled_handshake_returns_at_once() {
        for threads in THREADS {
            cancelled_handshake(threads);
        }
    }

    /// D-1291: a server that answers in plain HTTP is kind
    /// 100, and so is one whose certificate does not cover the name (a
    /// hooked `.test` name that resolves to the loopback server); a refused
    /// connection keeps its IO kind (8).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
    )]
    fn tls_poller_failure_kinds() {
        let (plain, plain_port) = host_listener();
        let plain_host = thread::spawn(move || {
            let mut stream = accepted(&plain);
            let mut hello = [0_u8; 512];
            let _ = stream.read(&mut hello).unwrap();
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let (named, named_port) = host_listener();
        let named_host = thread::spawn(move || {
            let session = rustls::ServerConnection::new(server_config()).unwrap();
            let mut tls = rustls::StreamOwned::new(session, accepted(&named));
            let mut nothing = Vec::new();
            // The client's alert ends the handshake on this side too.
            tls.read_to_end(&mut nothing).is_err()
        });
        let wrong = LookupHook::new(
            "tls-wrong-name.test",
            vec![SocketAddr::from(([127, 0, 0, 1], named_port))],
            true,
        );
        hooks::install(&wrong);
        let (refused, refused_port) = host_listener();
        drop(refused);
        let seen: Arc<Mutex<Vec<Got>>> = Arc::default();
        {
            let seen = Arc::clone(&seen);
            run_root(2, move |me| {
                let config = trusting_test_ca();
                let runtime = rt(me);
                *seen.lock() = [
                    ("127.0.0.1", plain_port),
                    ("tls-wrong-name.test", named_port),
                    ("127.0.0.1", refused_port),
                ]
                .iter()
                .map(|&(host, port)| {
                    let made = connect_tls_to(me, &runtime, host, i64::from(port), &config);
                    got(&made, |_| Vec::new())
                })
                .collect();
                0
            });
        }
        hooks::remove(&wrong);
        plain_host.join().unwrap();
        assert!(named_host.join().unwrap(), "the server heard no alert");
        assert_eq!(
            *seen.lock(),
            vec![Got::Failed(KIND_TLS), Got::Failed(KIND_TLS), Got::Failed(8)]
        );
    }
}
