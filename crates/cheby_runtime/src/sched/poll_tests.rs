//! Tests of the poller in a running runtime (D-680, ADR-0059): a fiber
//! parked on a timer is woken by the poll holder at one and at four
//! workers, a push wakes the holder through the waker, and a new earlier
//! timer shortens the holder's wait.
//!
//! The monitor's own firing (D-789) is switched off in
//! these runtimes, so only a worker can fire a timer, and the targets
//! record which thread fired them. Time is asserted only as a lower bound
//! and a cap of 5 s (plan §0 'Determinism').

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::park::state as parker;
use super::testing::{Mailbox, start_root, tuned};
use super::{Runtime, TimerTarget};
use crate::entry::{EntryOutcome, MIN_STACK_SIZE};

/// How long a test waits for something that should happen at once.
const CAP: Duration = Duration::from_secs(5);

/// A runtime of `threads` workers whose monitor fires no timers; without
/// `rescue` it wakes no worker for a queued fiber either.
fn quiet(threads: usize, rescue: bool) -> Runtime {
    let runtime = tuned(threads, false, rescue);
    runtime
        .shared
        .probe
        .skip_timers
        .store(true, Ordering::SeqCst);
    runtime
}

/// Waits up to [`CAP`] for `ready`.
fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() > CAP {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// Whether worker 0 blocks in the poller.
fn holds_the_poller(runtime: &Runtime) -> bool {
    runtime
        .shared
        .worker(0)
        .is_some_and(|worker| worker.parker().state() == parker::PARKED_DRIVER)
}

/// The name of the calling thread.
fn thread_name() -> String {
    std::thread::current().name().unwrap_or("").to_owned()
}

/// Posts to a mailbox and records the thread that fired it.
struct Wake {
    mailbox: Arc<Mailbox>,
    fired_on: Arc<parking_lot::Mutex<String>>,
}

impl TimerTarget for Wake {
    fn fire(&self) {
        *self.fired_on.lock() = thread_name();
        self.mailbox.send(1, false);
    }
}

/// Sends the instant it fired at and its thread's name.
struct Report(parking_lot::Mutex<mpsc::Sender<(Instant, String)>>);

impl TimerTarget for Report {
    fn fire(&self) {
        self.0.lock().send((Instant::now(), thread_name())).ok();
    }
}

/// A root fiber parks on a timer 20 ms ahead; the poll holder fires it
/// and the fiber runs, never before the deadline.
fn parked_on_a_timer(threads: usize) {
    let runtime = quiet(threads, true);
    let fired_on = Arc::new(parking_lot::Mutex::new(String::new()));
    let outcome = {
        let fired_on = Arc::clone(&fired_on);
        start_root(&runtime, MIN_STACK_SIZE, move |me| {
            let shared = Arc::clone(me.sched.runtime().unwrap());
            let mailbox = Arc::new(Mailbox::default());
            let deadline = Instant::now()
                .checked_add(Duration::from_millis(20))
                .unwrap();
            let _handle = shared.arm_timer(
                deadline,
                Arc::new(Wake {
                    mailbox: Arc::clone(&mailbox),
                    fired_on,
                }),
            );
            mailbox.receive(me);
            let ran = Instant::now();
            assert!(ran >= deadline, "the fiber ran before its deadline");
            u64::try_from(ran.saturating_duration_since(deadline).as_micros()).unwrap()
        })
    };
    match outcome.recv_timeout(CAP) {
        Ok(EntryOutcome::Returned(late)) => {
            println!("poller: {threads} workers, the fiber ran {late} us after its deadline");
        }
        other => panic!("the fiber parked on a timer did not run: {other:?}"),
    }
    let fired_on = fired_on.lock().clone();
    assert!(
        fired_on.starts_with("cheby-worker-"),
        "fired on {fired_on:?}"
    );
    assert_eq!(runtime.shared.timers_pending(), 0);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn poller_wakes_a_fiber_parked_on_a_timer_at_one_worker() {
    parked_on_a_timer(1);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn poller_wakes_a_fiber_parked_on_a_timer_at_four_workers() {
    parked_on_a_timer(4);
}

/// The only worker blocks in the poller with no limit; a new root fiber
/// pushed from the host wakes it through the waker. The monitor's rescue
/// is off, so a lost wake would hang until the cap.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn poller_a_push_wakes_the_poll_holder() {
    let runtime = quiet(1, false);
    for round in 0..3_u64 {
        assert!(
            wait_until(|| holds_the_poller(&runtime)),
            "worker 0 never polled"
        );
        let outcome = start_root(&runtime, MIN_STACK_SIZE, move |_| round);
        match outcome.recv_timeout(CAP) {
            Ok(EntryOutcome::Returned(word)) => assert_eq!(word, round),
            other => panic!("the push did not wake the poll holder: {other:?}"),
        }
    }
}

/// The holder waits for a timer an hour ahead; a new timer 30 ms ahead
/// wakes it, it polls again with the shorter limit, and it fires the new
/// timer itself, on time and not before.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn poller_a_new_earlier_timer_shortens_the_wait() {
    let runtime = quiet(1, true);
    let shared = Arc::clone(&runtime.shared);
    assert!(
        wait_until(|| holds_the_poller(&runtime)),
        "worker 0 never polled"
    );
    let (far_sender, far) = mpsc::channel();
    let far_handle = shared.arm_timer(
        Instant::now()
            .checked_add(Duration::from_secs(3600))
            .unwrap(),
        Arc::new(Report(parking_lot::Mutex::new(far_sender))),
    );
    // The holder woke for the far timer and polls again with it.
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        wait_until(|| holds_the_poller(&runtime)),
        "worker 0 stopped polling"
    );
    let (sender, near) = mpsc::channel();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(30))
        .unwrap();
    let _near = shared.arm_timer(deadline, Arc::new(Report(parking_lot::Mutex::new(sender))));
    let Ok((fired, on)) = near.recv_timeout(CAP) else {
        panic!("the earlier timer did not shorten the holder's wait");
    };
    assert!(fired >= deadline, "the timer fired before its deadline");
    assert_eq!(on, "cheby-worker-0");
    println!(
        "poller: the shorter timer fired {} us after its deadline",
        fired.saturating_duration_since(deadline).as_micros()
    );
    assert!(far.try_recv().is_err());
    assert!(far_handle.cancel());
    assert_eq!(shared.timers_pending(), 0);
}
