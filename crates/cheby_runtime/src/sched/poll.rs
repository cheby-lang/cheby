//! The integrated poller (D-680, ADR-0059, architecture §2.1): one
//! `mio::Poll` and one `mio::Waker` per runtime, behind the [`Poller`] and
//! [`PollWake`] traits so that another backend can come later.
//!
//! - **One idle worker polls.** A worker that found no work and parks
//!   takes the poller with `try_lock` ([`Driver::try_hold`]) and blocks in
//!   it with the earliest timer deadline as its timeout; every other idle
//!   worker parks on its own condition variable (`park`). The poll holder
//!   stays registered as parked in `Idle`, so a push that notifies it wakes
//!   it through the waker ([`super::park::Parker::unpark`]).
//! - **Timers.** After each poll the holder fires the timers that are due
//!   (`Shared::fire_due`), with the poller released. A poll that returns
//!   before its deadline (epoll rounds to milliseconds, a signal
//!   interrupts it) fires nothing early: the firing re-checks the clock,
//!   and the holder polls again. A timer armed with a deadline earlier than
//!   the holder's wakes it ([`Driver::timer_armed`]), and it recomputes its
//!   timeout.
//! - **M2 registers only the waker** (D-681): sockets come in M3 on the
//!   same seam, and D-724's detector sees no IO registration.
//! - **Locks.** The poller's lock is held only by the idle worker that
//!   polls, never while a timer fires or a fiber runs, and nothing else is
//!   taken under it.

use std::fmt;
use std::io;
use std::sync::atomic::{self, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};

use super::timer::{NO_TIMER, Timers};
use crate::panic::fatal;

/// The readiness source a runtime blocks in: `mio` today (D-680), maybe an
/// `io_uring` backend after 1.0.
pub(crate) trait Poller: Send {
    /// Blocks the calling thread until [`PollWake::wake`] is called or
    /// `timeout` passes (`None`: no limit). It may return earlier.
    ///
    /// # Errors
    ///
    /// The system's error, other than an interrupted call (an early
    /// return).
    fn poll(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

/// Wakes the thread blocked in a [`Poller`], or makes its next poll return
/// at once.
pub(crate) trait PollWake: Send + Sync {
    /// Wakes the poll.
    ///
    /// # Errors
    ///
    /// The system's error.
    fn wake(&self) -> io::Result<()>;
}

/// The waker's token: the only registration in M2 (D-681).
const WAKE_TOKEN: mio::Token = mio::Token(0);

/// The `mio` poller.
struct MioPoller {
    poll: mio::Poll,
    events: mio::Events,
}

impl Poller for MioPoller {
    fn poll(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self.poll.poll(&mut self.events, timeout) {
            Err(error) if error.kind() != io::ErrorKind::Interrupted => Err(error),
            // The only event is the waker's, which needs no handling.
            _ => Ok(()),
        }
    }
}

/// The `mio` waker.
struct MioWake(mio::Waker);

impl PollWake for MioWake {
    fn wake(&self) -> io::Result<()> {
        self.0.wake()
    }
}

/// Nobody polls, in [`Driver::deadline`].
const NOT_POLLING: u64 = 0;

/// The longest timeout one poll is given (a day): far below every OS's
/// limit (`kevent` refuses a timeout of 1.5e10 s with `EINVAL`), and long
/// enough that the extra polls of a far deadline cost nothing.
const MAX_POLL: Duration = Duration::from_hours(24);

/// A runtime's poller and waker, and what the holder waits for.
pub(super) struct Driver {
    /// The poller; held by the one idle worker that polls.
    poller: Mutex<Box<dyn Poller>>,
    /// Its waker.
    waker: Box<dyn PollWake>,
    /// The holder's deadline in nanoseconds since the timers' epoch, plus
    /// one (`u64::MAX`: no deadline), or [`NOT_POLLING`].
    deadline: AtomicU64,
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("deadline", &self.deadline.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Driver {
    /// A `mio` poller with its waker registered.
    ///
    /// # Errors
    ///
    /// The system's error if the poller or the waker cannot be made.
    pub(super) fn new() -> io::Result<Self> {
        let poll = mio::Poll::new()?;
        let waker = mio::Waker::new(poll.registry(), WAKE_TOKEN)?;
        Ok(Self::with(
            Box::new(MioPoller {
                poll,
                events: mio::Events::with_capacity(8),
            }),
            Box::new(MioWake(waker)),
        ))
    }

    /// A driver over any poller and waker.
    fn with(poller: Box<dyn Poller>, waker: Box<dyn PollWake>) -> Self {
        Self {
            poller: Mutex::new(poller),
            waker,
            deadline: AtomicU64::new(NOT_POLLING),
        }
    }

    /// The poller, if no other worker holds it.
    pub(super) fn try_hold(&self) -> Option<MutexGuard<'_, Box<dyn Poller>>> {
        self.poller.try_lock()
    }

    /// Wakes the holder, or makes the next poll return at once.
    pub(super) fn wake(&self) {
        if let Err(error) = self.waker.wake() {
            fatal(format_args!(
                "the scheduler's poller cannot be woken: {error}"
            ));
        }
    }

    /// A timer was armed for `nanos` (since the timers' epoch): wakes the
    /// holder if that is earlier than its deadline, so it recomputes its
    /// timeout. Called after the timer is in the heap.
    ///
    /// With [`Driver::poll`]'s handshake (Dekker's pattern, a sequentially
    /// consistent fence between each side's store and load): the arming
    /// stored the heap's new earliest deadline before this load, and the
    /// holder stores its deadline before it reads the earliest one again.
    /// So either this load sees the holder's deadline and wakes it if the
    /// timer is earlier, or the holder's second read sees the new timer and
    /// it polls with it. `park_models` model (4) checks this.
    pub(super) fn timer_armed(&self, nanos: u64) {
        atomic::fence(Ordering::SeqCst);
        if nanos.saturating_add(1) < self.deadline.load(Ordering::SeqCst) {
            self.wake();
        }
    }

    /// Whether a worker polls now: a worker that retires wakes another one
    /// when nobody does.
    pub(super) fn polling(&self) -> bool {
        self.deadline.load(Ordering::SeqCst) != NOT_POLLING
    }

    /// Polls once as the holder of `poller`, with the earliest deadline of
    /// `timers` as the timeout (no limit without timers); returns at a
    /// wake, at the timeout, or earlier. Fires nothing: the caller fires
    /// the due timers after releasing the poller.
    pub(super) fn poll(&self, poller: &mut dyn Poller, timers: &Timers) {
        let mut earliest = timers.earliest();
        loop {
            self.deadline
                .store(earliest.saturating_add(1), Ordering::SeqCst);
            // Orders the store before the read, as `timer_armed` orders the
            // heap's new earliest deadline before its read of this one.
            atomic::fence(Ordering::SeqCst);
            let again = timers.earliest();
            if again >= earliest {
                break;
            }
            earliest = again;
        }
        let timeout = if earliest == NO_TIMER {
            None
        } else {
            // Past `Instant`'s range: no limit. A deadline further away
            // than the OS accepts (`kevent` refuses 1.5e10 s) is polled
            // for in steps of `MAX_POLL`: the holder polls again after each
            // (gate-1 review ROB1-2).
            timers
                .instant(earliest)
                .map(|at| at.saturating_duration_since(Instant::now()).min(MAX_POLL))
        };
        if timeout != Some(Duration::ZERO)
            && let Err(error) = poller.poll(timeout)
        {
            fatal(format_args!("the scheduler's poller failed: {error}"));
        }
        self.deadline.store(NOT_POLLING, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::sched::TimerTarget;

    /// A poller that records its timeouts and returns at once.
    struct Recorder(Timeouts);

    impl Poller for Recorder {
        fn poll(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.lock().push(timeout);
            Ok(())
        }
    }

    /// A waker that counts its wakes.
    struct Counter(Arc<AtomicUsize>);

    impl PollWake for Counter {
        fn wake(&self) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Nothing;

    impl TimerTarget for Nothing {
        fn fire(&self) {}
    }

    /// The limits a [`Recorder`] saw.
    type Timeouts = Arc<Mutex<Vec<Option<Duration>>>>;

    fn recorded() -> (Driver, Timeouts, Arc<AtomicUsize>) {
        let timeouts = Arc::new(Mutex::new(Vec::new()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let driver = Driver::with(
            Box::new(Recorder(Arc::clone(&timeouts))),
            Box::new(Counter(Arc::clone(&wakes))),
        );
        (driver, timeouts, wakes)
    }

    #[test]
    fn poller_polls_with_the_earliest_deadline_as_its_limit() {
        let (driver, timeouts, _) = recorded();
        let timers = Timers::new(Instant::now());
        {
            let mut poller = driver.try_hold().unwrap();
            assert!(driver.try_hold().is_none(), "one holder at a time");
            driver.poll(&mut **poller, &timers);
        }
        let far = Duration::from_secs(3600);
        let _handle = timers.arm(Instant::now() + far, Arc::new(Nothing), None, || false);
        driver.poll(&mut **driver.try_hold().unwrap(), &timers);
        // A deadline that has passed is not polled for at all.
        let _past = timers.arm(Instant::now(), Arc::new(Nothing), None, || false);
        driver.poll(&mut **driver.try_hold().unwrap(), &timers);
        let timeouts = timeouts.lock();
        assert_eq!(timeouts.len(), 2);
        assert_eq!(timeouts[0], None);
        let limit = timeouts[1].unwrap();
        assert!(limit <= far && limit > far.saturating_sub(Duration::from_secs(60)));
        assert!(!driver.polling());
    }

    /// A deadline like `fiber::timeout(duration::hours(9223372036), ..)`'s
    /// (gate-1 review ROB1-2): within `Instant`'s range and the timers'
    /// nanoseconds (1.5e10 s), but beyond what `kevent` accepts.
    fn far_deadline() -> Instant {
        Instant::now()
            .checked_add(Duration::from_mins(250_000_000))
            .unwrap()
    }

    /// Gate-1 review ROB1-2: a deadline further away than the OS accepts
    /// is polled for in steps of at most a day.
    #[test]
    fn poller_clamps_a_far_deadline() {
        let (driver, timeouts, _) = recorded();
        let timers = Timers::new(Instant::now());
        let _handle = timers.arm(far_deadline(), Arc::new(Nothing), None, || false);
        driver.poll(&mut **driver.try_hold().unwrap(), &timers);
        assert_eq!(timeouts.lock().as_slice(), &[Some(MAX_POLL)]);
    }

    /// Gate-1 review ROB1-2 with the system's poller: a far deadline is no
    /// longer an invalid argument (the poll returns at the wake sent first,
    /// where it was a fatal error before).
    #[test]
    #[cfg_attr(windows, ignore = "the poller on Windows is M6 (D-545, D-714)")]
    fn poller_a_far_deadline_polls_the_system() {
        let driver = Driver::new().unwrap();
        let timers = Timers::new(Instant::now());
        let _handle = timers.arm(far_deadline(), Arc::new(Nothing), None, || false);
        driver.wake();
        driver.poll(&mut **driver.try_hold().unwrap(), &timers);
        assert!(!driver.polling());
    }

    #[test]
    fn poller_an_earlier_timer_wakes_the_holder_and_a_later_one_does_not() {
        let (driver, _, wakes) = recorded();
        let timers = Timers::new(Instant::now());
        // Nobody polls: no wake.
        driver.timer_armed(5);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        // The holder waits for 1000 ns after the epoch.
        driver.deadline.store(1001, Ordering::SeqCst);
        driver.timer_armed(1000);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        driver.timer_armed(999);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        // A holder without a deadline is woken by any timer.
        driver.deadline.store(u64::MAX, Ordering::SeqCst);
        driver.timer_armed(timers.nanos(Instant::now() + Duration::from_secs(3600)));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }
}
