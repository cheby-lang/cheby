//! `loom` models of the socket wait's protocol (D-984, `sched/io.rs`): the
//! try, arm, re-check and park of a waiting fiber against the dispatch of
//! a readiness event, and a cancellation against the dispatch, with the
//! domain's IO-wait count that falls only after the wake it stands for
//! (D-857).
//!
//! Each model is the protocol, not the runtime's code, as in
//! `sched/deadlock_models.rs`: `loom` atomics stand for a fiber's state
//! word, resume count and wait site, for the waiter's header, for the
//! socket's data and for the domain's count and the wait's token; a `loom`
//! mutex stands for the socket entry's lock, which guards the readiness bit
//! and the waiter slot. A thread plays one fiber and the worker that runs
//! it: it parks as the runtime parks (`PARKING`, the entry's lock released
//! after the switch, then the worker's compare-and-swap to `PARKED`, or a
//! requeue when a waker found it `PARKING`), and it resumes as
//! `Worker::run_fiber` resumes it. The run queue is a flag under a lock
//! with a condition variable. loom 0.7 needs no `--cfg loom`:
//!
//! ```text
//! cargo test -p cheby_runtime sched::io_models
//! ```
//!
//! loom runs each model on generator coroutines that switch stacks, so
//! every model is ignored on Windows until M6, like the fiber tests.
//!
//! | Model step                 | Mirrors (`crates/cheby_runtime/src/`, function)                 |
//! | -------------------------- | ---------------------------------------------------------------- |
//! | `wait`                     | `sched/io.rs` `IoHandle::wait`                                   |
//! | `Entry::ready`, `dispatch` | `sched/io.rs` `IoEntry::ready`, `Waiter::wake`                   |
//! | `Fiber::begin_park`        | `sched/fiber.rs` `begin_park`                                    |
//! | `Fiber::end_park`          | `sched/worker.rs` `Worker::parked`                               |
//! | `Fiber::try_wake`          | `sched/mod.rs` `wake`                                            |
//! | `Fiber::resume`            | `sched/worker.rs` `Worker::run_fiber`                            |
//! | `Pending::release`         | `sched/deadlock.rs` `IoPending::release`                         |
//! | the canceller              | `fiber_ops/cancel.rs` `reach` (`WaitHeader::try_cancel`), `cancel` |
//! | `survey`                   | `sched/deadlock.rs` `pass`, `survey` (steps 3 and 4)             |
//! | the closer, `Owner`        | `sched/io.rs` `IoHandle::close`; `net.rs` `NetSocket::close`, `Owned` |
//! | `release_turn`, the takes  | `sched/turn.rs` `Turn::release`, `Turn::take`                    |
//!
//! The models, each with `#[should_panic]` twins that break one rule:
//!
//! 1. A fiber waits for a socket to become readable while the event comes
//!    (the data, then the readiness bit and the dispatch): the fiber
//!    always ends, having read the data, and the count is back to 0. Twin:
//!    no re-check after arming, so an event between the try and the arming
//!    is lost and the fiber parks for good.
//! 2. A fiber parked on a socket (its header published, its direction
//!    armed, its domain's count 1) against the dispatcher and a
//!    cancellation at once, with a survey of the domain: exactly one of
//!    them wakes the fiber, the fiber returns the cancellation exactly when
//!    the canceller won, the count falls exactly once, and the survey never
//!    declares a deadlock. In the runtime a fiber parked on a socket
//!    records no wait site, which stops a pass on its own; the model gives
//!    it one so that the count's order is checked alone, as model 2 of
//!    `deadlock_models` does for the pool. Twins: the dispatcher lowers the
//!    count before its wake (a false deadlock), and a token released by
//!    every caller instead of once (the count falls below 0).
//! 3. (D-1063) A fiber waits to read while a closer closes the handle
//!    (`IoHandle::close` and `NetSocket::close`: the entry marked closed
//!    and its waiter taken under the entry's lock, the handle's reference
//!    to the descriptor's owner dropped) and a dispatcher delivers an
//!    event: the fiber is woken exactly once, whether the close comes
//!    before its arming, between its try and its arming, or after it
//!    parked, and ends with the closed handle's error; the count is back to
//!    0; and the descriptor closes only after the fiber's call, which
//!    holds its own reference, has returned. Twins: a close that leaves
//!    the waiter armed (the fiber parks for good), and a close that shuts
//!    the descriptor at once (under a running call).
//! 4. (D-1001) Two fibers accept on one listener through its read turn
//!    (`sched/turn.rs`): the second parks behind the first and is handed
//!    the turn, at most one holds it at a time, and each takes exactly one
//!    of two connections. Twin: a take that does not queue behind the
//!    holder (two fibers hold the turn).
//! 5. (D-670, D-1000) A fiber queued for a turn, its header published,
//!    against the holder's release and the cancellation walk at once:
//!    exactly one wakes it, it holds the turn exactly when the release won
//!    and then releases it, a cancelled fiber takes nothing, and the turn
//!    ends free. Twin: a release that stores `Claimed` without the swap
//!    (the turn stays held by a fiber that left).
//!
//! What the models leave out: the system's poller (the dispatcher thread is
//! the poll holder after its poll returned an event), the token table and
//! its generations (`io_a_late_event_for_a_closed_socket_wakes_nothing`
//! tests them), the write direction (the same code as the read direction),
//! and the deadlock detector's backoff and collect step.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

/// A fiber's states, as `sched/fiber.rs` numbers them.
mod state {
    pub(super) const RUNNABLE: u8 = 0;
    pub(super) const RUNNING: u8 = 1;
    pub(super) const PARKING: u8 = 2;
    pub(super) const PARKING_WOKEN: u8 = 3;
    pub(super) const PARKED: u8 = 4;
    pub(super) const DONE: u8 = 5;
}

/// A waiter header's states (`WaitState`).
mod header {
    pub(super) const WAITING: u8 = 0;
    pub(super) const CLAIMED: u8 = 1;
    pub(super) const CANCELLED: u8 = 3;
}

/// No wait site.
const NO_SITE: usize = 0;
/// The site model 2 gives its fiber.
const SITE: usize = 0x40;

/// The text of a false deadlock.
const FALSE_DEADLOCK: &str = "a deadlock was declared, and a wake came after it";

/// One fiber's scheduling words and its run queue.
#[derive(Debug)]
struct Fiber {
    state: AtomicU8,
    resumes: AtomicU64,
    site: AtomicUsize,
    queued: Mutex<bool>,
    pushed: Condvar,
}

impl Fiber {
    fn new(state: u8, site: usize) -> Self {
        Self {
            state: AtomicU8::new(state),
            resumes: AtomicU64::new(1),
            site: AtomicUsize::new(site),
            queued: Mutex::new(false),
            pushed: Condvar::new(),
        }
    }

    /// `begin_park`.
    fn begin_park(&self) {
        self.state.store(state::PARKING, Ordering::SeqCst);
    }

    /// `cancel_park`: the fiber did not park after all.
    fn cancel_park(&self) {
        self.state.store(state::RUNNING, Ordering::SeqCst);
    }

    /// `Worker::parked`, after the switch and the release of the entry's
    /// lock.
    fn end_park(&self) {
        match self.state.compare_exchange(
            state::PARKING,
            state::PARKED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(state::PARKING_WOKEN) => {
                self.state.store(state::RUNNABLE, Ordering::SeqCst);
                self.push();
            }
            Err(other) => panic!("a parking fiber was in the state {other}"),
        }
    }

    /// `sched::wake`; false for a fiber neither parked nor parking.
    fn try_wake(&self) -> bool {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let next = match current {
                state::PARKED => state::RUNNABLE,
                state::PARKING => state::PARKING_WOKEN,
                _ => return false,
            };
            match self
                .state
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        if current == state::PARKED {
            self.push();
        }
        true
    }

    fn push(&self) {
        *self.queued.lock().unwrap() = true;
        self.pushed.notify_all();
    }

    /// `Worker::run_fiber`: waits until queued, then runs.
    fn resume(&self) {
        let mut queued = self.queued.lock().unwrap();
        while !*queued {
            queued = self.pushed.wait(queued).unwrap();
        }
        *queued = false;
        drop(queued);
        self.state.store(state::RUNNING, Ordering::SeqCst);
        let resumes = self.resumes.load(Ordering::Relaxed);
        self.resumes
            .store(resumes.wrapping_add(1), Ordering::Relaxed);
    }
}

/// How a wait's token leaves the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// Once, by the first caller's swap (the runtime's).
    Once,
    /// By every caller: model 2's second twin.
    EveryCaller,
}

/// One armed wait's token (`IoPending`).
#[derive(Debug)]
struct Pending {
    count: Arc<AtomicUsize>,
    counted: AtomicBool,
    release: Release,
}

impl Pending {
    /// Arms one wait: the count rises.
    fn armed(count: &Arc<AtomicUsize>, release: Release) -> Arc<Self> {
        count.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            count: Arc::clone(count),
            counted: AtomicBool::new(true),
            release,
        })
    }

    /// `IoPending::release`.
    fn release(&self) {
        let lower = match self.release {
            Release::Once => self.counted.swap(false, Ordering::SeqCst),
            Release::EveryCaller => true,
        };
        if lower {
            assert!(
                self.count.fetch_sub(1, Ordering::SeqCst) != 0,
                "a domain's armed IO wait count fell below 0"
            );
        }
    }
}

/// A waiter in the entry's slot: its header and its token.
#[derive(Debug, Clone)]
struct Waiter {
    header: Arc<AtomicU8>,
    pending: Arc<Pending>,
}

/// What the entry's lock guards (`EntryState`), for the read direction.
#[derive(Debug, Default)]
struct Slot {
    ready: bool,
    waiter: Option<Waiter>,
}

/// Whether the wait re-checks the readiness bit after arming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recheck {
    /// The runtime's.
    Yes,
    /// Model 1's twin.
    No,
}

/// The dispatcher's order after it settled a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dispatch {
    /// Wake, then lower the count (`Waiter::wake`).
    WakeThenRelease,
    /// Lower the count, then wake: model 2's first twin.
    ReleaseThenWake,
}

/// `Waiter::wake`: settles the header `Waiting -> Claimed` and wakes the
/// fiber, then lowers the count; a header the walk won is left alone.
fn dispatch(fiber: &Fiber, waiter: &Waiter, order: Dispatch) {
    if waiter
        .header
        .compare_exchange(
            header::WAITING,
            header::CLAIMED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }
    match order {
        Dispatch::WakeThenRelease => {
            assert!(fiber.try_wake(), "the dispatcher's fiber was not parking");
            waiter.pending.release();
        }
        Dispatch::ReleaseThenWake => {
            waiter.pending.release();
            assert!(fiber.try_wake(), "the dispatcher's fiber was not parking");
        }
    }
}

/// `IoEntry::ready`: the bit, and the waiter taken, under the entry's
/// lock; the wake after it.
fn ready(entry: &Mutex<Slot>, fiber: &Fiber, order: Dispatch) {
    let waiter = {
        let mut slot = entry.lock().unwrap();
        slot.ready = true;
        slot.waiter.take()
    };
    if let Some(waiter) = waiter {
        dispatch(fiber, &waiter, order);
    }
}

/// At most three preemptions per execution, as the other models bound
/// them.
fn explore(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(3);
    }
    builder.check(f);
}

/// Model 1's socket: its entry, whether its data has arrived, its waiting
/// fiber and that fiber's domain's count.
#[derive(Debug)]
struct Socket {
    count: Arc<AtomicUsize>,
    entry: Mutex<Slot>,
    data: AtomicBool,
    fiber: Fiber,
}

/// How one pass of `IoHandle::wait`'s loop ended.
#[derive(Debug)]
enum Attempt {
    /// The call found the data.
    Done,
    /// The re-check found the bit set: the loop goes on.
    Again,
    /// The fiber parked, with this waiter armed.
    Parked(Waiter),
}

/// One pass of `IoHandle::wait`'s loop on `socket`: try, then arm,
/// re-check (unless the twin leaves it out) and park.
fn attempt(socket: &Socket, recheck: Recheck) -> Attempt {
    // Try: the bit cleared, then the call.
    socket.entry.lock().unwrap().ready = false;
    if socket.data.load(Ordering::SeqCst) {
        return Attempt::Done;
    }
    let waiter = Waiter {
        header: Arc::new(AtomicU8::new(header::WAITING)),
        pending: Pending::armed(&socket.count, Release::Once),
    };
    socket.fiber.begin_park();
    let mut slot = socket.entry.lock().unwrap();
    // Arm.
    slot.waiter = Some(waiter.clone());
    // Re-check.
    if recheck == Recheck::Yes && slot.ready {
        slot.waiter = None;
        drop(slot);
        socket.fiber.cancel_park();
        waiter.pending.release();
        return Attempt::Again;
    }
    // Park: the worker releases the lock after the switch, then ends the
    // park.
    drop(slot);
    socket.fiber.end_park();
    Attempt::Parked(waiter)
}

/// Model (1): a fiber reads a socket that has no data yet, through
/// `IoHandle::wait`'s loop, while another thread makes the data arrive and
/// dispatches the event. The first parking pass races the event; once the
/// event's thread has ended, the fiber must have been woken (queued), and
/// it then runs its next pass, which finds the data. A fiber that parked
/// and was not woken would wait for good: the model panics there instead
/// of blocking, which loom could only report as a deadlock while it tears
/// the execution down.
fn wait_against_dispatch(recheck: Recheck) {
    explore(move || {
        let socket = Arc::new(Socket {
            count: Arc::new(AtomicUsize::new(0)),
            entry: Mutex::new(Slot::default()),
            data: AtomicBool::new(false),
            fiber: Fiber::new(state::RUNNING, NO_SITE),
        });
        let event = {
            let socket = Arc::clone(&socket);
            thread::spawn(move || {
                // The peer writes, then the poll returns the edge.
                socket.data.store(true, Ordering::SeqCst);
                ready(&socket.entry, &socket.fiber, Dispatch::WakeThenRelease);
            })
        };
        let waiting = {
            let socket = Arc::clone(&socket);
            thread::spawn(move || {
                loop {
                    match attempt(&socket, recheck) {
                        Attempt::Done => return None,
                        Attempt::Again => {}
                        Attempt::Parked(waiter) => return Some(waiter),
                    }
                }
            })
        };
        event.join().unwrap();
        if let Some(waiter) = waiting.join().unwrap() {
            assert!(
                *socket.fiber.queued.lock().unwrap(),
                "an event was lost: the fiber parked for good"
            );
            socket.fiber.resume();
            // Woken: disarm if the waiter is still there, then the count.
            {
                let mut slot = socket.entry.lock().unwrap();
                if slot
                    .waiter
                    .as_ref()
                    .is_some_and(|armed| Arc::ptr_eq(&armed.header, &waiter.header))
                {
                    slot.waiter = None;
                }
            }
            waiter.pending.release();
            assert!(
                matches!(attempt(&socket, recheck), Attempt::Done),
                "the woken fiber did not find the data"
            );
        }
        assert_eq!(socket.count.load(Ordering::SeqCst), 0, "a count was left");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn io_model_1_arm_recheck_and_park_never_lose_an_event() {
    wait_against_dispatch(Recheck::Yes);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "an event was lost: the fiber parked for good")]
fn io_model_1_twin_without_the_recheck_loses_an_event() {
    wait_against_dispatch(Recheck::No);
}

/// A pass of the survey over the one fiber (step 3): its resume count if
/// it is parked at a site and the count reads 0 after it.
fn pass(fiber: &Fiber, count: &AtomicUsize) -> Option<u64> {
    if fiber.state.load(Ordering::SeqCst) != state::PARKED {
        return None;
    }
    let resumes = fiber.resumes.load(Ordering::Relaxed);
    if fiber.site.load(Ordering::Relaxed) == NO_SITE {
        return None;
    }
    (count.load(Ordering::SeqCst) == 0).then_some(resumes)
}

/// The two passes (step 4): whether a deadlock is declared.
fn survey(fiber: &Fiber, count: &AtomicUsize) -> bool {
    let Some(first) = pass(fiber, count) else {
        return false;
    };
    pass(fiber, count) == Some(first)
}

/// Model (2): the fiber is parked on its socket, armed and counted, with
/// its header published; the dispatcher (the socket became readable) and
/// a canceller (the walk) race for its header while the monitor surveys.
fn cancel_against_dispatch(order: Dispatch, release: Release) {
    explore(move || {
        let count = Arc::new(AtomicUsize::new(0));
        let pending = Pending::armed(&count, release);
        let header = Arc::new(AtomicU8::new(header::WAITING));
        let entry = Arc::new(Mutex::new(Slot {
            ready: false,
            waiter: Some(Waiter {
                header: Arc::clone(&header),
                pending: Arc::clone(&pending),
            }),
        }));
        let fiber = Arc::new(Fiber::new(state::PARKED, SITE));
        let dispatcher = {
            let (entry, fiber) = (Arc::clone(&entry), Arc::clone(&fiber));
            thread::spawn(move || ready(&entry, &fiber, order))
        };
        let walk = {
            let (header, fiber) = (Arc::clone(&header), Arc::clone(&fiber));
            thread::spawn(move || {
                // `reach`: under the fiber lock, the published header.
                let won = header
                    .compare_exchange(
                        header::WAITING,
                        header::CANCELLED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok();
                // `cancel`: the wake after the locks.
                if won {
                    assert!(fiber.try_wake(), "the walk's fiber was not parked");
                }
                won
            })
        };
        let waiting = {
            let (entry, header, fiber, pending) = (
                Arc::clone(&entry),
                Arc::clone(&header),
                Arc::clone(&fiber),
                Arc::clone(&pending),
            );
            thread::spawn(move || {
                fiber.resume();
                fiber.site.store(NO_SITE, Ordering::Relaxed);
                // Woken: disarm if the walk left the waiter, then the
                // count.
                {
                    let mut slot = entry.lock().unwrap();
                    if slot
                        .waiter
                        .as_ref()
                        .is_some_and(|waiter| Arc::ptr_eq(&waiter.header, &header))
                    {
                        slot.waiter = None;
                    }
                }
                pending.release();
                let cancelled = header.load(Ordering::SeqCst) == header::CANCELLED;
                fiber.state.store(state::DONE, Ordering::SeqCst);
                cancelled
            })
        };
        let declared = survey(&fiber, &count);
        dispatcher.join().unwrap();
        let won = walk.join().unwrap();
        let cancelled = waiting.join().unwrap();
        assert!(!declared, "{FALSE_DEADLOCK}");
        assert_eq!(cancelled, won, "the wait's result is not its winner's");
        assert_eq!(count.load(Ordering::SeqCst), 0, "a count was left");
        assert!(entry.lock().unwrap().waiter.is_none(), "a waiter was left");
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn io_model_2_cancel_and_dispatch_wake_once_and_count_once() {
    cancel_against_dispatch(Dispatch::WakeThenRelease, Release::Once);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a deadlock was declared, and a wake came after it")]
fn io_model_2_twin_lowering_the_count_before_the_wake_declares_a_false_deadlock() {
    cancel_against_dispatch(Dispatch::ReleaseThenWake, Release::Once);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a domain's armed IO wait count fell below 0")]
fn io_model_2_twin_releasing_at_every_caller_lowers_the_count_twice() {
    cancel_against_dispatch(Dispatch::WakeThenRelease, Release::EveryCaller);
}

/// Model 3's descriptor: closed once the last reference to its owner goes
/// (`net.rs` `Owned`'s drop, after the deregistration).
#[derive(Debug)]
struct Descriptor {
    closed: AtomicBool,
}

/// The descriptor's owner (`net.rs` `Owned`): the handle holds one
/// reference, and each running system call one more.
#[derive(Debug)]
struct Owner {
    fd: Arc<Descriptor>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.fd.closed.store(true, Ordering::SeqCst);
    }
}

/// What model 3's entry lock guards (`EntryState`, the read direction).
#[derive(Debug, Default)]
struct ClosingSlot {
    ready: bool,
    closed: bool,
    waiter: Option<Waiter>,
}

/// How model 3's close works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseRule {
    /// The runtime's (`IoHandle::close`, `NetSocket::close`): the entry
    /// marked closed and its waiter taken under the entry's lock, the
    /// dispatcher's wake after it, and the handle's reference to the
    /// descriptor's owner dropped.
    Runtime,
    /// A twin: the entry marked closed, its waiter left armed.
    NoWake,
    /// A twin: the descriptor closed at once, whatever call runs on it.
    CloseNow,
}

/// Model (3), D-1063: a fiber reads a socket with no data through
/// `IoHandle::wait`'s loop, each try holding its own reference to the
/// descriptor's owner during the call (`NetSocket::wait_on`), while a
/// closer closes the handle and a dispatcher delivers an event (the peer's
/// data or error) at once. The fiber always ends, woken once, and returns
/// the closed handle's error; the count is back to 0; the descriptor is
/// closed at the end, and never while the fiber's call runs. Twins: a
/// close that leaves the waiter armed loses its wake, and a close that
/// shuts the descriptor at once does so under a running call.
#[expect(
    clippy::too_many_lines,
    reason = "one model: the closer, the dispatcher, the waiting fiber and the checks read best together"
)]
fn close_against_wait(rule: CloseRule) {
    explore(move || {
        let count = Arc::new(AtomicUsize::new(0));
        let fd = Arc::new(Descriptor {
            closed: AtomicBool::new(false),
        });
        let handle: Arc<Mutex<Option<Arc<Owner>>>> = Arc::new(Mutex::new(Some(Arc::new(Owner {
            fd: Arc::clone(&fd),
        }))));
        let entry = Arc::new(Mutex::new(ClosingSlot::default()));
        let fiber = Arc::new(Fiber::new(state::RUNNING, NO_SITE));
        let closer = {
            let (handle, entry, fiber, fd) = (
                Arc::clone(&handle),
                Arc::clone(&entry),
                Arc::clone(&fiber),
                Arc::clone(&fd),
            );
            thread::spawn(move || {
                let taken = handle.lock().unwrap().take();
                let waiter = {
                    let mut slot = entry.lock().unwrap();
                    slot.closed = true;
                    match rule {
                        CloseRule::NoWake => None,
                        CloseRule::Runtime | CloseRule::CloseNow => slot.waiter.take(),
                    }
                };
                if let Some(waiter) = waiter {
                    dispatch(&fiber, &waiter, Dispatch::WakeThenRelease);
                }
                if rule == CloseRule::CloseNow {
                    fd.closed.store(true, Ordering::SeqCst);
                }
                drop(taken);
            })
        };
        let dispatcher = {
            let (entry, fiber) = (Arc::clone(&entry), Arc::clone(&fiber));
            thread::spawn(move || {
                let waiter = {
                    let mut slot = entry.lock().unwrap();
                    slot.ready = true;
                    slot.waiter.take()
                };
                if let Some(waiter) = waiter {
                    dispatch(&fiber, &waiter, Dispatch::WakeThenRelease);
                }
            })
        };
        let waiting = {
            let (count, handle, entry, fiber, fd) = (
                Arc::clone(&count),
                Arc::clone(&handle),
                Arc::clone(&entry),
                Arc::clone(&fiber),
                Arc::clone(&fd),
            );
            thread::spawn(move || {
                loop {
                    // Try: a closed entry ends the wait; else the bit is
                    // cleared and the call runs on its own reference.
                    {
                        let mut slot = entry.lock().unwrap();
                        if slot.closed {
                            return None;
                        }
                        slot.ready = false;
                    }
                    let call = handle.lock().unwrap().clone();
                    let call = call?;
                    assert!(
                        !fd.closed.load(Ordering::SeqCst),
                        "the descriptor closed under a running call"
                    );
                    drop(call);
                    // `WouldBlock`: arm, re-check, park.
                    let waiter = Waiter {
                        header: Arc::new(AtomicU8::new(header::WAITING)),
                        pending: Pending::armed(&count, Release::Once),
                    };
                    fiber.begin_park();
                    let mut slot = entry.lock().unwrap();
                    slot.waiter = Some(waiter.clone());
                    if slot.closed || slot.ready {
                        slot.waiter = None;
                        drop(slot);
                        fiber.cancel_park();
                        waiter.pending.release();
                        continue;
                    }
                    drop(slot);
                    fiber.end_park();
                    return Some(waiter);
                }
            })
        };
        closer.join().unwrap();
        dispatcher.join().unwrap();
        if let Some(waiter) = waiting.join().unwrap() {
            assert!(
                *fiber.queued.lock().unwrap(),
                "a close lost its wake: the fiber parked for good"
            );
            fiber.resume();
            {
                let mut slot = entry.lock().unwrap();
                if slot
                    .waiter
                    .as_ref()
                    .is_some_and(|armed| Arc::ptr_eq(&armed.header, &waiter.header))
                {
                    slot.waiter = None;
                }
            }
            waiter.pending.release();
        }
        // The next try finds the entry closed.
        assert!(entry.lock().unwrap().closed);
        assert_eq!(count.load(Ordering::SeqCst), 0, "a count was left");
        assert!(
            fd.closed.load(Ordering::SeqCst),
            "the descriptor was never closed"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn io_model_3_close_wakes_once_and_keeps_the_descriptor_under_a_call() {
    close_against_wait(CloseRule::Runtime);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a close lost its wake: the fiber parked for good")]
fn io_model_3_twin_a_close_that_leaves_the_waiter_loses_its_wake() {
    close_against_wait(CloseRule::NoWake);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the descriptor closed under a running call")]
fn io_model_3_twin_closing_at_once_closes_under_a_running_call() {
    close_against_wait(CloseRule::CloseNow);
}

/// What a turn's lock guards (`sched/turn.rs` `TurnState`): whether it is
/// held, and the queued fibers' headers with their fiber's index.
#[derive(Debug, Default)]
struct TurnSlot {
    held: bool,
    queue: Vec<(Arc<AtomicU8>, usize)>,
}

/// How model 4's fibers take the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Take {
    /// The runtime's (`Turn::take`): a fiber that finds the turn held
    /// queues and parks until it is handed over.
    Runtime,
    /// A twin: a fiber that finds the turn held goes on, as one direction
    /// slot of the seam alone would let a second accepter do.
    NoQueue,
}

/// `Turn::release`: hands the turn to the first queued fiber whose header
/// it settles `Waiting -> Claimed` and wakes it after the lock, or frees
/// the turn.
fn release_turn(turn: &Mutex<TurnSlot>, fibers: &[Arc<Fiber>]) {
    let next = {
        let mut slot = turn.lock().unwrap();
        loop {
            if slot.queue.is_empty() {
                slot.held = false;
                break None;
            }
            let (header, index) = slot.queue.remove(0);
            if header
                .compare_exchange(
                    header::WAITING,
                    header::CLAIMED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                break Some(index);
            }
        }
    };
    if let Some(index) = next {
        let fiber = fibers.get(index).unwrap();
        assert!(fiber.try_wake(), "a turn's waiter was not parking");
    }
}

/// Model (4), D-1001 (`net.md` §`accept`): two fibers accept on one
/// listener through its read turn while two connections wait: each takes
/// the turn (the second parks behind the first, which hands it over), at
/// most one holds it at a time, and each takes exactly one connection.
/// Twin: a take that does not wait behind the holder lets both hold it.
fn two_accepters(take: Take) {
    explore(move || {
        let turn = Arc::new(Mutex::new(TurnSlot::default()));
        let fibers: Arc<Vec<Arc<Fiber>>> = Arc::new(vec![
            Arc::new(Fiber::new(state::RUNNING, NO_SITE)),
            Arc::new(Fiber::new(state::RUNNING, NO_SITE)),
        ]);
        let connections = Arc::new(Mutex::new(vec![1_u8, 2]));
        let inside = Arc::new(AtomicUsize::new(0));
        let accepters: Vec<_> = (0..2)
            .map(|index| {
                let (turn, fibers, connections, inside) = (
                    Arc::clone(&turn),
                    Arc::clone(&fibers),
                    Arc::clone(&connections),
                    Arc::clone(&inside),
                );
                thread::spawn(move || {
                    let fiber = Arc::clone(fibers.get(index).unwrap());
                    match take {
                        Take::Runtime => {
                            let mut slot = turn.lock().unwrap();
                            if slot.held {
                                let header = Arc::new(AtomicU8::new(header::WAITING));
                                fiber.begin_park();
                                slot.queue.push((Arc::clone(&header), index));
                                drop(slot);
                                fiber.end_park();
                                fiber.resume();
                                assert_eq!(header.load(Ordering::SeqCst), header::CLAIMED);
                            } else {
                                slot.held = true;
                            }
                        }
                        Take::NoQueue => {
                            let held = turn.lock().unwrap().held;
                            if !held {
                                turn.lock().unwrap().held = true;
                            }
                        }
                    }
                    // Holding the turn: one `accept` call.
                    assert_eq!(
                        inside.fetch_add(1, Ordering::SeqCst),
                        0,
                        "two fibers held one turn"
                    );
                    let connection = connections.lock().unwrap().pop();
                    inside.fetch_sub(1, Ordering::SeqCst);
                    release_turn(&turn, &fibers);
                    connection
                })
            })
            .collect();
        let mut got: Vec<u8> = accepters
            .into_iter()
            .map(|accepter| accepter.join().unwrap().unwrap())
            .collect();
        got.sort_unstable();
        assert_eq!(got, [1, 2], "a connection went to both or to neither");
        let slot = turn.lock().unwrap();
        assert!(
            !slot.held && slot.queue.is_empty(),
            "the turn was left held"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn io_model_4_two_accepters_take_the_turn_in_turn() {
    two_accepters(Take::Runtime);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "two fibers held one turn")]
fn io_model_4_twin_a_take_that_does_not_queue_lets_two_fibers_hold_the_turn() {
    two_accepters(Take::NoQueue);
}

/// How model 5's release hands the turn over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handover {
    /// The runtime's: a compare-and-swap that skips a cancelled header.
    Settle,
    /// A twin: the header stored `Claimed` blindly.
    Blind,
}

/// Model (5), D-670, D-1000: a fiber holds a turn while another is queued
/// for it, parked with its header published; the holder releases the turn
/// while the walk cancels the queued fiber. Exactly one of them wakes it;
/// the queued fiber holds the turn exactly when the release won, and then
/// releases it; a cancelled fiber takes nothing, and the turn ends free.
/// Twin: a release that stores `Claimed` without the swap hands the turn
/// to a fiber that already left, so the turn stays held for good.
fn cancel_against_handover(handover: Handover) {
    explore(move || {
        let header = Arc::new(AtomicU8::new(header::WAITING));
        let turn = Arc::new(Mutex::new(TurnSlot {
            held: true,
            queue: vec![(Arc::clone(&header), 0)],
        }));
        let fibers: Arc<Vec<Arc<Fiber>>> =
            Arc::new(vec![Arc::new(Fiber::new(state::PARKED, NO_SITE))]);
        let holder = {
            let (turn, fibers) = (Arc::clone(&turn), Arc::clone(&fibers));
            thread::spawn(move || match handover {
                Handover::Settle => release_turn(&turn, &fibers),
                Handover::Blind => {
                    let next = {
                        let mut slot = turn.lock().unwrap();
                        if slot.queue.is_empty() {
                            slot.held = false;
                            None
                        } else {
                            Some(slot.queue.remove(0))
                        }
                    };
                    if let Some((header, index)) = next {
                        header.store(header::CLAIMED, Ordering::SeqCst);
                        let _ = fibers.get(index).unwrap().try_wake();
                    }
                }
            })
        };
        let walk = {
            let (header, fibers) = (Arc::clone(&header), Arc::clone(&fibers));
            thread::spawn(move || {
                let won = header
                    .compare_exchange(
                        header::WAITING,
                        header::CANCELLED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok();
                if won {
                    assert!(
                        fibers.first().unwrap().try_wake(),
                        "the walk's fiber was not parked"
                    );
                }
                won
            })
        };
        let queued = {
            let (header, turn, fibers) =
                (Arc::clone(&header), Arc::clone(&turn), Arc::clone(&fibers));
            thread::spawn(move || {
                let fiber = Arc::clone(fibers.first().unwrap());
                fiber.resume();
                if header.load(Ordering::SeqCst) == header::CLAIMED {
                    // Holding the turn: its call, then the release.
                    release_turn(&turn, &fibers);
                    false
                } else {
                    // Cancelled: out of the queue if still there.
                    let mut slot = turn.lock().unwrap();
                    slot.queue
                        .retain(|(queued, _)| !Arc::ptr_eq(queued, &header));
                    true
                }
            })
        };
        holder.join().unwrap();
        let won = walk.join().unwrap();
        let cancelled = queued.join().unwrap();
        if handover == Handover::Settle {
            assert_eq!(cancelled, won, "the wait's result is not its winner's");
        }
        let slot = turn.lock().unwrap();
        assert!(
            !slot.held && slot.queue.is_empty(),
            "the turn was left held or handed to a cancelled fiber"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn io_model_5_a_cancelled_turn_waiter_takes_nothing() {
    cancel_against_handover(Handover::Settle);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "the turn was left held or handed to a cancelled fiber")]
fn io_model_5_twin_a_blind_handover_leaves_the_turn_held() {
    cancel_against_handover(Handover::Blind);
}
