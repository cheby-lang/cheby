//! Tests of the socket seam (D-984, `sched/io.rs`): fibers parked on
//! sockets and woken by the poll holder or by the monitor's poll, the
//! per-domain count of armed waits against the deadlock detector (D-724,
//! D-857, D-858, D-860), late events for closed sockets, and cancellation
//! (D-670).
//!
//! Every socket is a loopback one: TCP pairs made through a listener on
//! `127.0.0.1` with a port the system chooses, and listeners bound the
//! same way; nothing reaches the network. The host side of a pair is a
//! blocking `std` stream the test thread writes to or reads from. Waits
//! for an event are bounded with `testing::hang` and `testing::scaled`
//! (D-872); the one latency bound (the monitor's poll) is scaled and is
//! not asserted under an emulator.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::Shared;
use super::deadlock::{self, DomainCounts, HEADER};
use super::io::{Direction, IoHandle};
use super::testing::{self, entry_check, scaled};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, MIN_STACK_SIZE};
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::fiber_ops::detached_tests::{
    self, close_scope, new_channel, open_scope, receive_on, release, spawn_into, yield_until,
};
use crate::fiber_ops::scope::{ScopeHandle, ScopeState};
use crate::handles::{self, HandleKind};
use crate::values;

/// The thread counts of the tests that run at several.
const THREADS: [usize; 3] = [1, 2, 4];

/// A connected loopback pair: the fiber's end, non-blocking for `mio`, and
/// the host's blocking `std` end.
fn pair() -> (mio::net::TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (fiber, _) = listener.accept().unwrap();
    fiber.set_nonblocking(true).unwrap();
    (mio::net::TcpStream::from_std(fiber), host)
}

/// A non-blocking loopback listener on a port the system chooses, and its
/// address.
fn listener() -> (mio::net::TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    (mio::net::TcpListener::from_std(listener), address)
}

/// The runtime of the running fiber.
fn shared_of(me: &ExecCtx) -> Arc<Shared> {
    Arc::clone(me.sched.runtime().unwrap())
}

/// The pending counts of the running fiber's domain.
fn counts_of(me: &ExecCtx) -> Arc<DomainCounts> {
    deadlock::counts_of(me).unwrap()
}

/// A handle the host thread watches while the fiber waits on it.
type HandleSlot = Arc<Mutex<Option<Arc<IoHandle>>>>;

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

/// Waits on the host until the handle published in `slot` has a fiber
/// armed in `dir`.
fn wait_armed(slot: &HandleSlot, dir: Direction) {
    wait_until(|| slot.lock().as_ref().is_some_and(|handle| handle.armed(dir)));
}

/// Reads one chunk from `stream` on the calling fiber, parking on the
/// socket until it is readable: the bytes read, or the wait's
/// cancellation.
fn read_some(
    me: &ExecCtx,
    handle: &IoHandle,
    stream: &mut mio::net::TcpStream,
) -> Result<Vec<u8>, Cancelled> {
    let mut buffer = [0_u8; 64];
    let read = handle.wait(me, Direction::Read, &mut || stream.read(&mut buffer))?;
    let count = read.unwrap();
    Ok(buffer.get(..count).unwrap().to_vec())
}

/// The root's outcome, or a panic after [`testing::hang`].
fn outcome_of(root: &mpsc::Receiver<EntryOutcome>) -> EntryOutcome {
    root.recv_timeout(testing::hang())
        .expect("the root did not end")
}

/// D-984: a fiber parked on a socket wakes when the socket becomes
/// readable, from the poll holder; its domain's count is 1 while it waits
/// and 0 once it runs again. At one, two and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_parked_fiber_wakes_on_readiness() {
    for threads in THREADS {
        let runtime = testing::runtime(threads);
        let slot: HandleSlot = Arc::default();
        let counts: Arc<Mutex<Option<Arc<DomainCounts>>>> = Arc::default();
        let (mut stream, mut host) = pair();
        let root = {
            let (slot, counts) = (Arc::clone(&slot), Arc::clone(&counts));
            testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
                let handle = Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
                *counts.lock() = Some(counts_of(me));
                *slot.lock() = Some(Arc::clone(&handle));
                let got = read_some(me, &handle, &mut stream).unwrap();
                assert_eq!(got, b"ping");
                assert_eq!(counts_of(me).io(), 0, "the count did not fall");
                handle.deregister(&mut stream);
                7
            })
        };
        wait_armed(&slot, Direction::Read);
        assert_eq!(counts.lock().as_ref().unwrap().io(), 1);
        host.write_all(b"ping").unwrap();
        assert_eq!(outcome_of(&root), EntryOutcome::Returned(7));
        drop(runtime);
    }
}

/// D-984: a fiber waiting to accept on a loopback listener wakes when a
/// client connects.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_listener_wakes_a_fiber_waiting_to_accept() {
    for threads in [1, 4] {
        let runtime = testing::runtime(threads);
        let slot: HandleSlot = Arc::default();
        let (mut server, address) = listener();
        let root = {
            let slot = Arc::clone(&slot);
            testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
                let handle = Arc::new(IoHandle::register(&shared_of(me), &mut server).unwrap());
                *slot.lock() = Some(Arc::clone(&handle));
                let accepted = handle
                    .wait(me, Direction::Read, &mut || server.accept())
                    .unwrap();
                let (_, peer) = accepted.unwrap();
                handle.deregister(&mut server);
                u64::from(peer.ip().is_loopback())
            })
        };
        wait_armed(&slot, Direction::Read);
        let client = TcpStream::connect(address).unwrap();
        assert_eq!(outcome_of(&root), EntryOutcome::Returned(1));
        drop(client);
        drop(runtime);
    }
}

/// How many bytes the writing fiber sends: far more than the loopback
/// buffers of every platform hold, so it parks before the host reads.
const WRITE_TOTAL: usize = 32 << 20;

/// D-984: a fiber that writes more than a socket's buffers hold, through
/// waits, parks to write once they are full, and wakes once the host reads
/// enough of them; it writes every byte.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_fiber_waiting_to_write_wakes_when_the_peer_reads() {
    let runtime = testing::runtime(2);
    let slot: HandleSlot = Arc::default();
    let (mut stream, mut host) = pair();
    let written = Arc::new(AtomicUsize::new(0));
    let root = {
        let (slot, written) = (Arc::clone(&slot), Arc::clone(&written));
        testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let handle = Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
            *slot.lock() = Some(Arc::clone(&handle));
            let chunk = vec![0x5a_u8; 64 << 10];
            while written.load(Ordering::SeqCst) < WRITE_TOTAL {
                let count = handle
                    .wait(me, Direction::Write, &mut || stream.write(&chunk))
                    .unwrap()
                    .unwrap();
                written.fetch_add(count, Ordering::SeqCst);
            }
            handle.deregister(&mut stream);
            drop(stream);
            1
        })
    };
    // The host reads nothing until the fiber waits to write.
    wait_armed(&slot, Direction::Write);
    host.set_read_timeout(Some(testing::hang())).unwrap();
    // Drain until the fiber has written everything and closed its end.
    let mut buffer = vec![0_u8; 64 << 10];
    let mut read = 0_usize;
    loop {
        let count = host.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        read = read.saturating_add(count);
    }
    assert_eq!(outcome_of(&root), EntryOutcome::Returned(1));
    assert_eq!(read, written.load(Ordering::SeqCst));
    drop(runtime);
}

/// D-984, D-789: every worker runs a fiber that spins through the
/// function-entry check and never parks, so no worker polls; the monitor
/// polls on its tick, the woken fiber goes to the global queue, and the
/// next tick preempts a spinner so that it runs. At one and two workers,
/// within a cap of 5 s (scaled); the latency, from the write to the run,
/// is bounded by a few ticks (250 ms, scaled) except under an emulator,
/// and printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_wakes_while_every_worker_spins() {
    for threads in [1, 2] {
        let runtime = testing::runtime(threads);
        let slot: HandleSlot = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let written_at: Arc<Mutex<Option<Instant>>> = Arc::default();
        let (mut stream, mut host) = pair();
        let root = {
            let (slot, stop, started, written_at) = (
                Arc::clone(&slot),
                Arc::clone(&stop),
                Arc::clone(&started),
                Arc::clone(&written_at),
            );
            testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
                let handle = Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
                for _ in 0..threads {
                    let (stop, started) = (Arc::clone(&stop), Arc::clone(&started));
                    testing::spawn(me, false, move |fiber| {
                        started.fetch_add(1, Ordering::SeqCst);
                        while !stop.load(Ordering::SeqCst) {
                            entry_check(fiber);
                        }
                    });
                }
                *slot.lock() = Some(Arc::clone(&handle));
                let got = read_some(me, &handle, &mut stream);
                let ran_at = Instant::now();
                let all_spun = started.load(Ordering::SeqCst) == threads;
                stop.store(true, Ordering::SeqCst);
                assert_eq!(got.unwrap(), b"tick");
                assert!(all_spun, "not every worker ran a spinner");
                handle.deregister(&mut stream);
                let written = written_at.lock().unwrap();
                u64::try_from(ran_at.saturating_duration_since(written).as_nanos()).unwrap()
            })
        };
        wait_armed(&slot, Direction::Read);
        wait_until(|| started.load(Ordering::SeqCst) == threads);
        // Every worker spins now: the fiber parked first, and each spinner
        // runs on its own worker.
        *written_at.lock() = Some(Instant::now());
        host.write_all(b"tick").unwrap();
        let cap = scaled(Duration::from_secs(5));
        let result = root.recv_timeout(cap);
        stop.store(true, Ordering::SeqCst);
        let late = match result {
            Ok(EntryOutcome::Returned(nanos)) => Duration::from_nanos(nanos),
            other => panic!("the waiting fiber did not run within {cap:?}: {other:?}"),
        };
        println!(
            "io_wakes_while_every_worker_spins: {threads} workers, the woken fiber ran {:.3} ms after the write",
            late.as_secs_f64() * 1000.0
        );
        if !testing::emulated() {
            let bound = scaled(Duration::from_millis(250));
            assert!(late < bound, "{late:?} is more than {bound:?}");
        }
        drop(runtime);
    }
}

/// An entry of `mode` with the detached harness's trampoline and `domain`.
fn config(domain: &Arc<Domain>, mode: EntryMode) -> EntryConfig {
    EntryConfig {
        mode,
        ..detached_tests::config(domain)
    }
}

/// D-724, D-857, D-858, D-860, D-984: a root waiting on a socket nobody
/// writes to is never reported, for longer than the detector's longest
/// backoff (160 ms) and its eager surveys, since its armed wait keeps its
/// domain's count above 0; once the host writes, it returns. At one and
/// four workers, with detection on as in every host.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_waiting_fiber_is_never_reported_as_deadlocked() {
    for threads in [1, 4] {
        let runtime = testing::tuned_detecting(threads, false, true);
        let domain = Domain::new();
        let slot: HandleSlot = Arc::default();
        let (mut stream, mut host) = pair();
        let root = {
            let slot = Arc::clone(&slot);
            testing::start_root_with(&runtime, &config(&domain, EntryMode::Run), move |me| {
                let handle = Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
                *slot.lock() = Some(Arc::clone(&handle));
                let got = read_some(me, &handle, &mut stream).unwrap();
                handle.deregister(&mut stream);
                u64::try_from(got.len()).unwrap()
            })
        };
        wait_armed(&slot, Direction::Read);
        // Surveys at every pass, with no backoff, as well as the ticks.
        runtime
            .shared
            .probe
            .eager_detection
            .store(true, Ordering::SeqCst);
        let quiet = root.recv_timeout(scaled(Duration::from_millis(600)));
        assert!(quiet.is_err(), "the waiting root ended: {quiet:?}");
        runtime
            .shared
            .probe
            .eager_detection
            .store(false, Ordering::SeqCst);
        host.write_all(b"late").unwrap();
        assert_eq!(outcome_of(&root), EntryOutcome::Returned(4));
        runtime.shutdown();
    }
}

/// Receives on a new channel whose sender this fiber holds: blocks for good
/// unless the fiber is cancelled. Releases both ends; returns `Nil`, or 0
/// with the flag set when cancelled.
fn receive_forever(me: &ExecCtx) -> Word {
    let (tx, rx) = new_channel(me, 0);
    let got = receive_on(me, rx);
    release(tx);
    release(rx);
    match got {
        Some(Ok(word) | Err(word)) => {
            release(word);
            values::nil()
        }
        None => 0,
    }
}

/// D-724's report of one fiber blocked in `channel::receive`, with no
/// location (no JIT code runs here).
fn one_receive() -> String {
    format!("{HEADER}  blocked in channel::receive at <unknown location>\n")
}

/// D-984's why: a program that holds a registered listener nobody waits on
/// while its fibers block on channels is still reported, with D-724's and
/// D-858's report text: an idle socket counts nothing. `cheby run`'s
/// action at one, two and four workers, and `cheby test`'s, whose root
/// unwinds and deregisters its listener.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_an_idle_listener_does_not_hide_a_deadlock() {
    for (threads, mode) in [
        (1, EntryMode::Run),
        (2, EntryMode::Run),
        (4, EntryMode::Run),
        (2, EntryMode::Test),
    ] {
        let runtime = testing::tuned_detecting(threads, false, true);
        let domain = Domain::new();
        let test = matches!(mode, EntryMode::Test);
        let root = testing::start_root_with(&runtime, &config(&domain, mode), |me| {
            let (mut server, _) = listener();
            let handle = IoHandle::register(&shared_of(me), &mut server).unwrap();
            let word = receive_forever(me);
            // Reached in `cheby test` only, while unwinding.
            handle.deregister(&mut server);
            word
        });
        assert_eq!(outcome_of(&root), EntryOutcome::Deadlocked(one_receive()));
        if test {
            runtime.shutdown();
            assert_eq!(domain.live_objects(), 0, "objects leaked");
        } else {
            runtime.halt();
        }
    }
}

/// D-984: a late event for a socket that was deregistered, whose slot a
/// new socket took with the next generation, finds nothing: the fiber
/// waiting on the new socket stays armed, and wakes only for its own
/// socket's event.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_late_event_for_a_closed_socket_wakes_nothing() {
    let runtime = testing::runtime(2);
    let slot: HandleSlot = Arc::default();
    let old: Arc<Mutex<Option<mio::Token>>> = Arc::default();
    let (mut first, _first_host) = pair();
    let (mut second, mut host) = pair();
    let root = {
        let (slot, old) = (Arc::clone(&slot), Arc::clone(&old));
        testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let shared = shared_of(me);
            let closed = IoHandle::register(&shared, &mut first).unwrap();
            let token = closed.token();
            closed.deregister(&mut first);
            drop(first);
            let handle = Arc::new(IoHandle::register(&shared, &mut second).unwrap());
            assert_ne!(handle.token(), token, "a token was reused unchanged");
            *old.lock() = Some(token);
            *slot.lock() = Some(Arc::clone(&handle));
            let got = read_some(me, &handle, &mut second).unwrap();
            handle.deregister(&mut second);
            u64::try_from(got.len()).unwrap()
        })
    };
    wait_armed(&slot, Direction::Read);
    let handle = Arc::clone(slot.lock().as_ref().unwrap());
    let token = old.lock().unwrap();
    // The low half (the slot) is the same, the generation is not.
    assert_eq!(
        token.0 & 0xffff,
        handle.token().0 & 0xffff,
        "the new socket did not take the freed slot"
    );
    // The late event: both directions, as an error would set them. The
    // delivery is synchronous: a wake would have taken the waiter.
    handle.shared().deliver(token, 0b11);
    assert!(handle.armed(Direction::Read), "a late event woke a fiber");
    host.write_all(b"own").unwrap();
    assert_eq!(outcome_of(&root), EntryOutcome::Returned(3));
    drop(runtime);
}

/// The state of a borrowed `Scope` handle.
fn state_of(scope: Word) -> Arc<ScopeState> {
    // SAFETY: a live `Scope` handle, borrowed.
    Arc::clone(&unsafe { handles::payload::<ScopeHandle>(scope, HandleKind::Scope) }.0)
}

/// What the cancelled fiber saw once its wait returned.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Seen {
    cancelled: bool,
    armed_after: bool,
    count_after: usize,
}

/// D-670, D-984: a fiber waiting on a socket in a scope that the root
/// cancels from outside is woken by the walk; its wait disarms its
/// direction, its domain's count falls to 0 and it returns the
/// cancellation, which the fiber raises; the root's close of the scope
/// succeeds and nothing leaks. At one, two and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_cancelled_wait_disarms_and_its_count_drops() {
    for threads in THREADS {
        let runtime = detached_tests::runtime(threads);
        let domain = Domain::new();
        let seen: Arc<Mutex<Option<Seen>>> = Arc::default();
        let while_armed = Arc::new(AtomicUsize::new(usize::MAX));
        let closed = Arc::new(AtomicBool::new(false));
        let outcome = {
            let (seen, while_armed, closed) = (
                Arc::clone(&seen),
                Arc::clone(&while_armed),
                Arc::clone(&closed),
            );
            detached_tests::root_outcome(&runtime, &domain, move |me| {
                let slot: HandleSlot = Arc::default();
                let scope = open_scope(me);
                let fiber = {
                    let (slot, seen) = (Arc::clone(&slot), Arc::clone(&seen));
                    spawn_into(me, scope, move |me| {
                        let (mut stream, host) = pair();
                        let ctx = std::ptr::from_ref(me).cast_mut();
                        crate::unwind::guard::guarded(Some(ctx), || {
                            let handle =
                                Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
                            *slot.lock() = Some(Arc::clone(&handle));
                            let got = read_some(me, &handle, &mut stream);
                            *seen.lock() = Some(Seen {
                                cancelled: got.is_err(),
                                armed_after: handle.armed(Direction::Read),
                                count_after: counts_of(me).io(),
                            });
                            handle.deregister(&mut stream);
                            drop(host);
                            if got.is_err() {
                                crate::unwind::raise::raise_cancel(Some(ctx));
                            }
                            values::nil()
                        })
                    })
                };
                yield_until(me, || {
                    slot.lock()
                        .as_ref()
                        .is_some_and(|handle| handle.armed(Direction::Read))
                });
                while_armed.store(counts_of(me).io(), Ordering::SeqCst);
                cancel::cancel(&state_of(scope));
                closed.store(close_scope(me, scope), Ordering::SeqCst);
                release(fiber);
                0
            })
        };
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!(while_armed.load(Ordering::SeqCst), 1);
        assert_eq!(
            seen.lock().clone(),
            Some(Seen {
                cancelled: true,
                armed_after: false,
                count_after: 0,
            })
        );
        assert!(closed.load(Ordering::SeqCst), "the scope's close failed");
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// D-984: a wait on a handle that was deregistered returns an error
/// without parking, and so does a second wait in a direction another
/// fiber waits in (one parked fiber per direction).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn io_a_deregistered_or_busy_direction_does_not_park() {
    let runtime = testing::runtime(2);
    let slot: HandleSlot = Arc::default();
    let (mut stream, mut host) = pair();
    let busy = Arc::new(Mutex::new(None));
    let root = {
        let (slot, busy) = (Arc::clone(&slot), Arc::clone(&busy));
        testing::start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let handle = Arc::new(IoHandle::register(&shared_of(me), &mut stream).unwrap());
            *slot.lock() = Some(Arc::clone(&handle));
            let second = Arc::clone(&handle);
            let busy = Arc::clone(&busy);
            testing::spawn(me, false, move |fiber| {
                yield_until(fiber, || second.armed(Direction::Read));
                let mut never =
                    || -> std::io::Result<()> { Err(std::io::ErrorKind::WouldBlock.into()) };
                let got = second.wait(fiber, Direction::Read, &mut never);
                *busy.lock() = Some(got.map(|result| result.map_err(|error| error.kind())));
            });
            let got = read_some(me, &handle, &mut stream).unwrap();
            handle.deregister(&mut stream);
            let mut never =
                || -> std::io::Result<()> { Err(std::io::ErrorKind::WouldBlock.into()) };
            let after = handle.wait(me, Direction::Write, &mut never).unwrap();
            assert_eq!(after.unwrap_err().kind(), std::io::ErrorKind::NotConnected);
            u64::try_from(got.len()).unwrap()
        })
    };
    wait_armed(&slot, Direction::Read);
    wait_until(|| busy.lock().is_some());
    assert_eq!(
        busy.lock().clone(),
        Some(Ok(Err(std::io::ErrorKind::ResourceBusy)))
    );
    host.write_all(b"x").unwrap();
    assert_eq!(outcome_of(&root), EntryOutcome::Returned(1));
    drop(runtime);
}
