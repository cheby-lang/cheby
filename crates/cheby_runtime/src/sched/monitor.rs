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
//! On every tick it also fires the timers that are due
//! (D-682, D-789): nobody polls while every worker runs
//! a fiber, and D-737 leaves a lone fiber alone, so without this a
//! `timeout` around CPU-bound work at one thread would never fire. A fiber
//! a timer wakes goes to the global queue, which makes the next tick
//! preempt the fiber running on its worker.
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::Shared;
use super::deadlock::{self, Detector};
use crate::ctx::{self, ExecCtx, reason};

/// Whether `fiber`, which runs on a worker, is blocked inline in a system
/// call (D-786: marked and `RUNNING`) in a group a teardown marked stuck:
/// it does not count as a running fiber of its domain (D-857 under UQ-12
/// (b)). Read under the worker's gate; the group's lock is the fiber's
/// own, which the fiber, blocked in its call, does not hold.
fn stuck_inline(fiber: &ExecCtx) -> bool {
    fiber.sched.blocking()
        && fiber.sched.state().load(Ordering::SeqCst) == super::fiber::state::RUNNING
        && fiber.conc.group().is_some_and(|group| group.stuck())
}

/// Whether the tests asked the monitor to survey on every pass, with no
/// sleep and no backoff (the probe's `eager_detection`).
fn eager_detection(shared: &Shared) -> bool {
    #[cfg(test)]
    {
        shared.probe.eager_detection.load(Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        let _ = shared;
        false
    }
}

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
    /// Set by the last unparked worker when it parks (D-860): the monitor
    /// surveys for deadlocks at once, skipping the backoff.
    check_now: AtomicBool,
}

impl MonitorWake {
    /// Asks the monitor to look for deadlocks at once (D-860): the last
    /// unparked worker calls it when it parks. One store, then the lock and
    /// a notify, on the idle path only.
    pub(crate) fn check_now(&self) {
        self.check_now.store(true, Ordering::SeqCst);
        let guard = self.lock.lock();
        self.condvar.notify_one();
        drop(guard);
    }

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
    // One sample per worker slot, grown as slots fill
    // (D-795).
    let mut samples: Vec<Sample> = Vec::new();
    let mut detector = Detector::default();
    // The domains whose fibers run on a worker at this look (D-860).
    let mut busy_domains: Vec<usize> = Vec::new();
    loop {
        {
            let mut guard = shared.monitor.lock.lock();
            if shared.stopping() {
                break;
            }
            let interval = if eager_detection(shared) {
                Duration::ZERO
            } else if shared.monitor.backed_off.load(Ordering::SeqCst) {
                IDLE_INTERVAL
            } else {
                PREEMPT_AFTER
            };
            // A check-now set before the lock was taken is seen here; one
            // set later notifies under the lock, which wakes the wait.
            if !shared.monitor.check_now.load(Ordering::SeqCst) {
                shared.monitor.condvar.wait_for(&mut guard, interval);
            }
        }
        if shared.stopping() {
            break;
        }
        let check_now = shared.monitor.check_now.swap(false, Ordering::SeqCst);
        busy_domains.clear();
        let now = Instant::now();
        // D-789: with every worker running a lone
        // CPU-bound fiber nobody polls, so the monitor fires what is due
        // itself, with no lock held and no Cheby code run. A fiber a timer
        // wakes goes to the global queue, so `fibers_waiting` below sees
        // it and the spinner on its worker is asked to yield (D-737).
        #[cfg(test)]
        let fire = !shared.probe.skip_timers.load(Ordering::SeqCst);
        #[cfg(not(test))]
        let fire = true;
        if fire {
            shared.fire_due(now);
        }
        let mut busy = false;
        // In the migration stress mode a lone fiber yields too, so that it
        // moves between workers (D-687).
        let waiting = shared.stress_migrate || shared.fibers_waiting();
        samples.resize(
            shared.workers.filled(),
            Sample {
                resumes: u64::MAX,
                since: now,
            },
        );
        for (index, worker) in shared.workers.indexed() {
            let Some(sample) = samples.get_mut(index) else {
                continue;
            };
            // A slot whose worker has ended runs nothing.
            if worker.life() == super::worker::life::DEAD {
                sample.resumes = u64::MAX;
                continue;
            }
            let gate = worker.gate().lock();
            worker.set_looking(true);
            let running = worker.running();
            let resumes = worker.resumes();
            if running.is_null() {
                sample.resumes = u64::MAX;
            } else {
                busy = true;
                // SAFETY: as below: read after `looking` was set, so the
                // fiber is not retired before `looking` is cleared; only
                // its immutable domain and its atomics are read, and its
                // group's flag when it is blocked inline in a system call.
                if let Some(fiber) = unsafe { running.as_ref() }
                    && shared.detect_deadlocks
                    && !stuck_inline(fiber)
                {
                    busy_domains.push(Arc::as_ptr(fiber.domain()).addr());
                }
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
        // D-860: with no worker gate held. A check-now also covers the
        // domains a timer or pool completion settled since.
        deadlock::detect(shared, &mut detector, &busy_domains, check_now, now);
        // While every worker is idle one of them holds the poller and
        // fires timers on time, so pending timers do not keep the monitor
        // at its short interval.
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
