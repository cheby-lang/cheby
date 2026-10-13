//! The monitor thread of a runtime, Go's `sysmon` (D-688, architecture
//! §3.6).
//!
//! Every 10 ms it looks at each worker. A worker that has run the same
//! fiber since the previous look (its count of resumed fibers did not
//! change) for 10 ms or more gets a yield request on that fiber
//! ([`reason::YIELD`]), which its next function-entry check turns into a
//! `park(Requeue)`. So a fiber that loops through function entries yields
//! between 10 and 20 ms after it started running, and a fiber inside a
//! long kernel yields when the kernel returns. Only while some other fiber
//! waits in a run queue: a lone running fiber is left alone, since a yield
//! would only move it off its worker's warm caches
//! (D-737; no program can observe the
//! difference). The migration stress mode preempts lone fibers too, so
//! that every long-running program migrates between workers.
//!
//! When every worker is idle the monitor backs off to 100 ms; a worker that
//! wakes up, or a new root fiber, kicks it back to 10 ms. It also wakes a
//! parked worker when the global queue holds fibers and nobody searches,
//! a safety net for the idle protocol.
//!
//! The monitor reads a worker's running fiber with the worker's `looking`
//! flag set. A worker that clears its running fiber waits while the flag
//! is set before that fiber can be queued, woken or retired, so the
//! monitor never touches a retired control block, whichever worker
//! retires it (I-8).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::Shared;
use crate::ctx::{self, reason};

/// How long a fiber may run before the monitor asks it to yield, and how
/// often the monitor looks while some worker is busy (D-688).
pub(crate) const PREEMPT_AFTER: Duration = Duration::from_millis(10);

/// How often the monitor looks while every worker is idle.
const IDLE_INTERVAL: Duration = Duration::from_millis(100);

/// The monitor's sleep, shared with the workers that wake it.
#[derive(Debug, Default)]
pub(crate) struct MonitorWake {
    lock: Mutex<()>,
    condvar: Condvar,
    /// Whether it sleeps the long, idle interval.
    backed_off: AtomicBool,
}

impl MonitorWake {
    /// Brings a backed-off monitor back to its short interval. A worker
    /// calls it after it published the fiber it runs.
    pub(crate) fn kick(&self) {
        if self.backed_off.load(Ordering::SeqCst) {
            let guard = self.lock.lock();
            self.backed_off.store(false, Ordering::SeqCst);
            self.condvar.notify_one();
            drop(guard);
        }
    }

    /// Wakes the monitor so that it sees the runtime stopping.
    pub(crate) fn stop(&self) {
        let guard = self.lock.lock();
        self.condvar.notify_one();
        drop(guard);
    }
}

/// What the monitor saw of one worker at its previous look.
#[derive(Clone, Copy, Debug)]
struct Sample {
    resumes: u64,
    since: Instant,
}

/// The body of the monitor thread.
pub(super) fn main(shared: &Shared) {
    let now = Instant::now();
    let mut samples = vec![
        Sample {
            resumes: u64::MAX,
            since: now,
        };
        shared.workers.len()
    ];
    loop {
        {
            let mut guard = shared.monitor.lock.lock();
            if shared.stopping() {
                break;
            }
            let interval = if shared.monitor.backed_off.load(Ordering::SeqCst) {
                IDLE_INTERVAL
            } else {
                PREEMPT_AFTER
            };
            shared.monitor.condvar.wait_for(&mut guard, interval);
        }
        if shared.stopping() {
            break;
        }
        let now = Instant::now();
        let mut busy = false;
        // In the migration stress mode a lone fiber yields too, so that it
        // moves between workers (D-687).
        let waiting = shared.stress_migrate || shared.fibers_waiting();
        for (worker, sample) in shared.workers.iter().zip(samples.iter_mut()) {
            let gate = worker.gate().lock();
            worker.set_looking(true);
            let running = worker.running();
            let resumes = worker.resumes();
            if running.is_null() {
                sample.resumes = u64::MAX;
            } else {
                busy = true;
                if sample.resumes != resumes {
                    *sample = Sample {
                        resumes,
                        since: now,
                    };
                } else if waiting
                    && now.saturating_duration_since(sample.since) >= PREEMPT_AFTER
                    // SAFETY: the fiber was running when it was read, after
                    // `looking` was set. Its worker clears `running` and
                    // then waits while `looking` is set
                    // (`WorkerShared::wait_for_monitor`) before the fiber
                    // can be queued, woken or retired, so it cannot be
                    // retired, by this worker or another one, before
                    // `looking` is cleared below. Only atomics of the
                    // control block are used.
                    && let Some(fiber) = unsafe { running.as_ref() }
                {
                    #[cfg(test)]
                    shared.probe.before_request(running);
                    ctx::request(fiber, reason::YIELD);
                    sample.since = now;
                }
            }
            worker.set_looking(false);
            drop(gate);
        }
        let queued = !shared.injector.is_empty();
        if queued && shared.rescue && shared.idle.num_searching() == 0 {
            shared.notify_parked();
        }
        if busy || queued {
            shared.monitor.backed_off.store(false, Ordering::SeqCst);
        } else {
            // A worker stores its running fiber and then reads this flag
            // (`kick`), both sequentially consistent: either it sees the
            // flag and kicks, or this re-check sees its fiber.
            shared.monitor.backed_off.store(true, Ordering::SeqCst);
            if shared.workers.iter().any(|w| !w.running().is_null()) {
                shared.monitor.backed_off.store(false, Ordering::SeqCst);
            }
        }
    }
}
