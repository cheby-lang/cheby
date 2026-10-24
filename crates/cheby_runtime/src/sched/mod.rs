//! The M:N scheduler (D-012, D-175, D-687 to D-690): one [`Runtime`] per
//! host run, with its workers, run queues, idle protocol and monitor.
//!
//! - **A runtime per host run** (D-690). `cheby run`, a `cheby test` run and
//!   each `xtask test-std` module batch create a [`Runtime`], run their
//!   entries as root fibers with [`Runtime::run_root`], and shut it down.
//!   The stack pool and the layout table are process-global; the program's
//!   arguments belong to the runtime. Isolated entries (constant
//!   evaluation, the REPL, D-713) keep D-457's dedicated threads.
//! - **Fibers** live at the top of their stack slots (D-677, D-685). The
//!   scheduling part of their control block is [`fiber`]'s; a fiber
//!   suspends only through [`park`], which hands the worker a post-switch
//!   action ([`AfterSwitch`]) such as releasing the lock the fiber
//!   registered as a waiter under (D-687).
//! - **Run queues** (D-689): per worker a runnext slot, a Chase-Lev deque
//!   and a queue of fibers only it may run; one global injector. See
//!   `worker` for the search order and `idle` for Tokio's protocol.
//! - **Migration and pinning** (D-687): fibers resume on any worker except
//!   while a kernel callback is on their stack. With
//!   `CHEBY_RT_STRESS_MIGRATE=1`, debug builds resume every unpinned fiber
//!   on another worker than the one it last ran on.
//! - **Preemption** (D-688): the `monitor` asks a fiber that ran 10 ms to
//!   yield.
//! - **Code lifetime** (D-708): a runtime holds the [`CodeToken`] of every
//!   JIT whose code its fibers run, and drops them only after its workers
//!   have stopped.
//! - **Thread count** (D-658): [`threads_from_env`] reads `CHEBY_THREADS`.
//! - **Windows** (D-714): there is no switch, so [`Runtime::new`] returns
//!   [`RuntimeError::Unsupported`] and hosts use the thread backend.
//!
//! - **Lifecycle** (D-073, D-708 to D-710, `lifecycle`): detached fibers
//!   live in groups ([`DetachedGroup`]) and outlive their roots; stopping a
//!   runtime ([`Runtime::shutdown`], `Drop`) tears every group down, then
//!   joins the workers and the monitor, then releases the code tokens. A
//!   halt ([`Runtime::halt`]) freezes every fiber instead, and a
//!   termination (D-709) starts one.

pub(crate) mod blocking;
mod deadlock;
mod fiber;
mod idle;
pub(crate) mod io;
mod lifecycle;
mod monitor;
mod park;
mod poll;
mod timer;
mod wait;
mod worker;

use std::ffi::OsStr;
use std::fmt;
use std::sync::atomic::{self, AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_deque::Injector;
use parking_lot::Mutex;

pub(crate) use fiber::{
    AfterSwitch, FiberRef, FiberSched, StartFn, Unlock, Unlocks, WaitOp, begin_park, cancel_park,
    leave_callback, park,
};
pub(crate) use lifecycle::terminate;
pub use lifecycle::{DetachedGroup, RootDone, RootHandle, TeardownReport};
pub(crate) use timer::{Deadline, TimerHandle, TimerTarget, deadline_after};
pub(crate) use wait::{WaitHeader, WaitNode, WaitQueue, WaitState};

use self::deadlock::{DomainTable, RootEntry};
use self::fiber::FiberParams;
use self::idle::Idle;
use self::monitor::MonitorWake;
use self::worker::{WorkerShared, Workers, life};
use crate::alloc;
use crate::ctx::{ExecCtx, FiberInit};
use crate::entry::{
    DEFAULT_STACK_SIZE, EntryConfig, EntryFn, EntryMode, EntryOutcome, MIN_STACK_SIZE, RuntimeError,
};
use crate::fiber::{StackError, switch};
use crate::generic;
use crate::tls;

/// The environment variable that sets the number of worker threads
/// (D-658).
pub const THREADS_ENV: &str = "CHEBY_THREADS";

/// The most worker threads a runtime has (D-658).
pub const MAX_THREADS: usize = 1024;

/// The debugging switch that resumes unpinned fibers on another worker
/// (D-687), honored by debug builds only.
pub const STRESS_MIGRATE_ENV: &str = "CHEBY_RT_STRESS_MIGRATE";

/// The thread count `value` of `CHEBY_THREADS` gives (D-658): unset or
/// empty is `cpus`, capped at [`MAX_THREADS`]; a decimal whole number of at
/// least 1 is clamped to it; anything else is
/// [`RuntimeError::InvalidThreads`].
///
/// (D-740) A decimal whole number is ASCII digits only,
/// leading zeros allowed (`007` is 7), with no sign and no surrounding
/// whitespace; one too large for the machine's word is still a whole
/// number and clamps to [`MAX_THREADS`].
///
/// # Errors
///
/// [`RuntimeError::InvalidThreads`] with the value as written.
pub fn parse_threads(value: Option<&OsStr>, cpus: usize) -> Result<usize, RuntimeError> {
    let default = cpus.clamp(1, MAX_THREADS);
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match value.to_str() {
        Some(text)
            if text.bytes().all(|byte| byte.is_ascii_digit())
                && text.bytes().any(|byte| byte != b'0') =>
        {
            // A number too large for `usize` is still a whole number.
            Ok(text
                .parse::<usize>()
                .map_or(MAX_THREADS, |count| count.min(MAX_THREADS)))
        }
        _ => Err(RuntimeError::InvalidThreads(
            value.to_string_lossy().into_owned(),
        )),
    }
}

/// The thread count `CHEBY_THREADS` sets for this process, read now
/// (D-658): hosts that run no runtime, such as the REPL, call it to reject
/// an invalid value at the same point.
///
/// # Errors
///
/// [`RuntimeError::InvalidThreads`] for an invalid value.
pub fn threads_from_env() -> Result<usize, RuntimeError> {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    parse_threads(std::env::var_os(THREADS_ENV).as_deref(), cpus)
}

/// How to make a [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// The number of worker threads, clamped to 1 to [`MAX_THREADS`];
    /// `None` reads `CHEBY_THREADS` (D-658). In-process hosts, such as
    /// tests, may bound it.
    pub threads: Option<usize>,
    /// The program's arguments, without the program name (D-347, D-690).
    pub args: Vec<String>,
    /// The program name `std::env` reports; empty means "not set" (D-347).
    pub program_name: String,
    /// The stack size in bytes of a fiber spawned without one (D-660's
    /// `fiber-stack`); [`Shared::fiber_stack`] reports it.
    pub fiber_stack: usize,
}

impl Default for RuntimeConfig {
    /// `CHEBY_THREADS`' count, no arguments, no program name, and
    /// [`DEFAULT_STACK_SIZE`] for spawned fibers.
    fn default() -> Self {
        Self {
            threads: None,
            args: Vec::new(),
            program_name: String::new(),
            fiber_stack: DEFAULT_STACK_SIZE,
        }
    }
}

/// Keeps a JIT's code memory alive (D-708, I-6): the memory is freed when
/// the last token is dropped. [`Runtime::attach`] keeps one for as long as
/// the runtime's fibers may run that code.
#[derive(Clone)]
pub struct CodeToken(Arc<dyn Send + Sync>);

impl CodeToken {
    /// A token that keeps `keep_alive` alive.
    #[must_use]
    pub fn new(keep_alive: Arc<dyn Send + Sync>) -> Self {
        Self(keep_alive)
    }

    /// Whether both tokens keep the same code alive.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for CodeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CodeToken")
            .field(&Arc::as_ptr(&self.0).cast::<()>())
            .finish()
    }
}

/// [`Shared`]'s exit status before one is fixed.
const NO_STATUS: i64 = i64::MIN;

/// Where [`Shared::schedule`] puts a runnable fiber that is not pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Where {
    /// The current worker's runnext slot: the waker is about to block.
    Next,
    /// The current worker's deque.
    Local,
    /// The global queue.
    Global,
}

/// What every thread of a runtime shares.
pub(crate) struct Shared {
    /// The worker slots, by index (D-795).
    workers: Workers,
    /// The join handle of every worker thread started and not joined yet,
    /// with its slot (C-7): `set_thread_count` adds to it from a fiber,
    /// and the host's stop and halt drain and join it. Its lock also
    /// serializes starting and retiring workers; it is taken before a
    /// worker's gate, the idle list or a targeted queue, never under them.
    threads: Mutex<Vec<(usize, JoinHandle<()>)>>,
    /// The global queue.
    injector: Injector<FiberRef>,
    /// Which workers are parked or searching.
    idle: Idle,
    /// Set once by [`Runtime::shutdown`] or its `Drop`.
    stopping: AtomicBool,
    /// Fibers created and not retired yet.
    live: AtomicUsize,
    /// The next fiber number.
    next_id: AtomicU64,
    /// The program's arguments (D-690).
    args: Vec<String>,
    /// The program name.
    program_name: String,
    /// The migration stress mode (D-687).
    stress_migrate: bool,
    /// Whether the monitor wakes a worker for a queued fiber that nobody
    /// looks for; only tests of the idle protocol turn it off.
    rescue: bool,
    /// Whether the monitor detects deadlocks ([`Tuning::detect_deadlocks`]).
    detect_deadlocks: bool,
    /// The monitor's sleep.
    monitor: MonitorWake,
    /// The timer heap (D-682).
    timers: timer::Timers,
    /// The poller one idle worker blocks in (D-680).
    driver: poll::Driver,
    /// The default stack size of a spawned fiber (D-660).
    fiber_stack: usize,
    /// The thread count's target ([`Shared::set_thread_count`]).
    thread_target: AtomicUsize,
    /// The group of `run_root`'s roots (D-799).
    default_group: Arc<DetachedGroup>,
    /// Every group a root ran in, for the teardown at shutdown; a leaf
    /// lock.
    groups: Mutex<Vec<Weak<DetachedGroup>>>,
    /// Set once by a halt (D-073, D-708): workers take no fiber, a fiber
    /// freezes at its next entry check, and no timer fires.
    halting: AtomicBool,
    /// The program's exit status once fixed, by a termination (D-709) or
    /// by `run_main` after its flush (D-785); [`NO_STATUS`] until then.
    /// The first one wins.
    exit_status: AtomicI64,
    /// Set by a termination (D-709).
    terminated: AtomicBool,
    /// The registered roots, by root number, with the hosts waiting for
    /// their outcome: a termination sends each of them `Exited` (D-709),
    /// and the deadlock detector reads them (`deadlock::RootEntry`). A
    /// leaf lock: entries are copied out under it and used after it.
    roots: Mutex<Vec<Arc<RootEntry>>>,
    /// Each domain's pending timers, pool jobs and armed IO waits (D-857,
    /// D-984, `deadlock::DomainTable`).
    domains: DomainTable,
    /// The next root number.
    next_root: AtomicU64,
    /// The code the runtime's fibers may run (D-708): released by the
    /// host's stop or halt once every worker is joined. A blocking-pool
    /// job holds clones of these tokens while it runs (`blocking`), so
    /// code a pool thread runs outlives a stop.
    code: Mutex<Vec<CodeToken>>,
    /// Pinned fibers woken after their worker ended at a stop, left alone
    /// (D-821); [`Shared::left_at_stop`] reads it.
    left_at_stop: AtomicUsize,
    /// Set once by the host's stop when it gives up on the fibers waiting
    /// on the blocking pool (D-815,
    /// `blocking::poll_stop`): from then on a pool completion wakes
    /// nothing, the workers end without waiting for it, and a blocking call
    /// runs inline, as after a refused pool thread.
    pool_given_up: AtomicBool,
    /// This runtime's fibers that submitted a pool job whose completion
    /// has not finished its wake yet (`blocking`): while it is above 0, a
    /// worker does not end at a stop that has not given up.
    pool_waiting: AtomicUsize,
    /// The completions of this runtime's pool jobs so far: the stop gives
    /// up only when none came between its two polls.
    pool_done: AtomicU64,
    /// Test hooks of the monitor.
    #[cfg(test)]
    probe: testing::MonitorProbe,
    /// A test hook: the next fiber created finds no stack region (D-677).
    #[cfg(test)]
    pub(crate) fail_stack: AtomicBool,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("workers", &self.workers.live())
            .field("live", &self.live.load(Ordering::Relaxed))
            .field("stopping", &self.stopping.load(Ordering::Relaxed))
            .field("timers", &self.timers_pending())
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Worker slot `index`, if a worker ever started there; it stays for
    /// the runtime's life (D-795).
    fn worker(&self, index: usize) -> Option<&WorkerShared> {
        self.workers.get(index)
    }

    /// Whether the runtime stops or halts: idle workers and the monitor
    /// end, and a timer armed now never fires.
    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst) || self.halting()
    }

    /// Starts a halt (D-073, D-708), once or again: from now on no worker
    /// takes a fiber, every running fiber freezes at its next entry check
    /// (`SAFEPOINT`, requested through the monitor's gate), no timer fires,
    /// the idle workers, the poll holder and the monitor wake to end, and a
    /// teardown waiting on a group stops waiting. Joining the threads is
    /// the host's ([`Runtime::halt`]). Callable from a fiber (a
    /// termination) and from the host, with no runtime lock held.
    pub(crate) fn begin_halt(&self) {
        self.halting.store(true, Ordering::SeqCst);
        // No socket event wakes a fiber from now on, as no timer fires.
        self.driver.halt();
        self.request_safepoints();
        self.wake_idle();
        self.monitor.stop();
        for group in self.live_groups() {
            group.wake_teardown();
        }
    }

    fn next_fiber_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn fiber_created(&self) {
        self.live.fetch_add(1, Ordering::SeqCst);
    }

    fn fiber_retired(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    /// The program's arguments, without its name (D-347, D-690).
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    /// The program name; empty when not set.
    pub(crate) fn program_name(&self) -> &str {
        &self.program_name
    }

    /// The default stack of a spawned fiber (D-660's `fiber-stack`), as
    /// [`RuntimeConfig::fiber_stack`] gave it.
    pub(crate) fn fiber_stack(&self) -> usize {
        self.fiber_stack
    }

    /// C4: the thread count (D-657): the worker count until
    /// [`Shared::set_thread_count`] sets one, then the count it set, so a
    /// count just set is the count read next (runtime.md's example), while
    /// the workers it retires finish their fibers' slices.
    pub(crate) fn thread_count(&self) -> usize {
        self.thread_target.load(Ordering::SeqCst)
    }

    /// C4: sets the thread count to `count`, clamped to 1 to
    /// [`MAX_THREADS`] (D-298, D-657), without waiting for any fiber
    /// (D-689, `worker`'s module documentation): new workers start at
    /// once, in the lowest free slots, after taking back the retirements
    /// not acted on yet; the live workers with the highest indexes retire
    /// at their next scheduling point, through a per-worker flag and a
    /// `YIELD` request on the fiber each runs, never `SAFEPOINT` (C-5).
    /// Once the runtime stops or halts it only records the count. Callable
    /// from a fiber (the kernel) and from the host, with no runtime lock
    /// held. If the system refuses a thread, the runtime goes on with the
    /// workers it has (D-795).
    pub(crate) fn set_thread_count(self: &Arc<Self>, count: usize) {
        let count = count.clamp(1, MAX_THREADS);
        let mut threads = self.threads.lock();
        self.thread_target.store(count, Ordering::SeqCst);
        if self.stopping() {
            return;
        }
        // The threads of retired workers that have ended: joining them
        // does not block.
        let (ended, running): (Vec<_>, Vec<_>) = std::mem::take(&mut *threads)
            .into_iter()
            .partition(|(_, handle)| handle.is_finished());
        *threads = running;
        for (_, handle) in ended {
            handle.join().ok();
        }
        let live: Vec<usize> = self
            .workers
            .indexed()
            .filter(|(_, worker)| worker.life() == life::LIVE)
            .map(|(index, _)| index)
            .collect();
        if count > live.len() {
            let mut needed = count.saturating_sub(live.len());
            for (_, worker) in self.workers.indexed() {
                if needed == 0 {
                    break;
                }
                if worker.cancel_retire() {
                    needed = needed.saturating_sub(1);
                }
            }
            for index in 0..MAX_THREADS {
                if needed == 0 {
                    break;
                }
                let free = self
                    .worker(index)
                    .is_none_or(|worker| worker.life() == life::DEAD);
                if !free {
                    continue;
                }
                if self.start_worker(index, &mut threads).is_err() {
                    break;
                }
                needed = needed.saturating_sub(1);
            }
            // Slots still `LEAVING` cover the rest: their workers rejoin
            // instead of ending (`leave_or_rejoin`).
        } else {
            let extra = live.len().saturating_sub(count);
            for &index in live.iter().rev().take(extra) {
                self.retire_worker(index);
            }
        }
    }

    /// Starts a worker thread in free slot `index` (never started, or
    /// `DEAD`), under the thread registry's lock, whose guard is
    /// `threads`.
    ///
    /// # Errors
    ///
    /// The system's error if the thread cannot start; the slot stays free.
    fn start_worker(
        self: &Arc<Self>,
        index: usize,
        threads: &mut Vec<(usize, JoinHandle<()>)>,
    ) -> std::io::Result<()> {
        let Some(worker) = self.workers.fill(index) else {
            return Err(std::io::Error::other("no worker slot left"));
        };
        // The slot's last thread, if any, no longer uses it: marking it
        // `DEAD` was the last thing it did there, either under this lock
        // when it retired (`leave_or_rejoin`) or, at a stop, under the
        // slot's targeted-queue lock (`WorkerShared::end_if_idle`,
        // D-821). Once stopping, a thread starts here
        // only if `set_thread_count` looked at the flag just before the
        // stop; the stop's join waits for it, and it ends through the same
        // stop exit as the others.
        worker.set_live();
        self.idle.add_worker();
        let shared = Arc::clone(self);
        match std::thread::Builder::new()
            .name(format!("cheby-worker-{index}"))
            .spawn(move || worker::main(&shared, index))
        {
            Ok(handle) => {
                threads.push((index, handle));
                Ok(())
            }
            Err(error) => {
                drop(worker.end());
                self.idle.remove_worker(index);
                Err(error)
            }
        }
    }

    /// Asks live worker `index` to retire at its next scheduling point:
    /// sets its flag, asks the fiber it runs to yield (D-737 leaves a lone
    /// fiber alone; a retirement does not), and wakes it if it is parked.
    fn retire_worker(&self, index: usize) {
        let Some(worker) = self.worker(index) else {
            return;
        };
        if !worker.request_retire() {
            return;
        }
        self.look_at_running(index, |fiber| {
            if let Some(fiber) = fiber {
                crate::ctx::request(fiber, crate::ctx::reason::YIELD);
            }
        });
        worker.unpark(self);
    }

    /// The last step of worker `index`'s retirement, once no fiber is
    /// pinned to it or the runtime stops: the worker rejoins as live and
    /// false is returned if the live workers are fewer than the thread
    /// count now (it went up while this one was leaving) and the runtime
    /// runs on, or if the runtime stops without halting: a stop runs every
    /// runnable fiber to its end (`Runtime::shutdown`), and the live
    /// workers may have ended before this one handed its fibers to the
    /// global queue, so it ends as a live worker does, once nothing is
    /// runnable (review finding CONC3-3 of gate 3). Otherwise its slot
    /// becomes `DEAD`, a fiber still queued for it goes to the global
    /// queue, and true is returned: the thread must end without touching
    /// the slot again. Under the thread registry's lock, so no new thread
    /// starts in the slot before this returns.
    fn leave_or_rejoin(&self, index: usize) -> bool {
        let threads = self.threads.lock();
        let Some(worker) = self.worker(index) else {
            return true;
        };
        let rejoin = if self.halting() {
            false
        } else if self.stopping() {
            true
        } else {
            self.workers.live() < self.thread_count()
        };
        if rejoin {
            worker.set_live();
            self.idle.add_worker();
            return false;
        }
        let left = worker.end();
        drop(threads);
        if !left.is_empty() {
            for fiber in left {
                // SAFETY: a queued fiber is not retired.
                if unsafe { fiber.ctx() }.sched.pinned() {
                    crate::panic::fatal(format_args!(
                        "a fiber pinned to a retired worker was left behind"
                    ));
                }
                self.injector.push(fiber);
            }
            self.notify_parked();
        }
        true
    }

    /// Whether a halt has started (D-073, D-708).
    pub(crate) fn halting(&self) -> bool {
        self.halting.load(Ordering::SeqCst)
    }

    /// How many pinned fibers a stop left alone after their worker ended
    /// (D-821). `run_main` reads it after the stop:
    /// above 0, its leak check is skipped and the skip counted, as for a
    /// fiber the teardown skipped (D-786, D-710).
    pub(crate) fn left_at_stop(&self) -> usize {
        self.left_at_stop.load(Ordering::SeqCst)
    }

    /// A test hook: counts one more pinned fiber left alone at a stop, as
    /// [`Shared::push_targeted`] does.
    #[cfg(test)]
    pub(crate) fn count_left_at_stop(&self) {
        self.left_at_stop.fetch_add(1, Ordering::SeqCst);
    }

    /// The runtime's default group: `run_root`'s roots' (D-799).
    pub(crate) fn default_group(&self) -> &Arc<DetachedGroup> {
        &self.default_group
    }

    /// Remembers `group` for the teardown at shutdown, once; forgets the
    /// groups that no longer exist.
    fn register_group(&self, group: &Arc<DetachedGroup>) {
        if Arc::ptr_eq(group, &self.default_group) {
            return;
        }
        let mut groups = self.groups.lock();
        groups.retain(|known| known.strong_count() > 0);
        if !groups
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), Arc::as_ptr(group)))
        {
            groups.push(Arc::downgrade(group));
        }
    }

    /// Reads worker `index`'s running fiber through the monitor's gate
    /// (I-8) and gives it to `look`, or `None` when the worker runs none.
    /// The worker clears its running fiber and then waits while a look is
    /// in progress before the fiber can be queued, woken or retired, so
    /// the control block lives while `look` runs; `look` uses only its
    /// atomics.
    fn look_at_running<R>(&self, index: usize, look: impl FnOnce(Option<&ExecCtx>) -> R) -> R {
        let Some(worker) = self.worker(index) else {
            return look(None);
        };
        let gate = worker.gate().lock();
        worker.set_looking(true);
        let running = worker.running();
        // SAFETY: read after `looking` was set: the fiber cannot be
        // retired before `looking` is cleared below
        // (`WorkerShared::wait_for_monitor`), and only its atomics are used.
        let result = look(unsafe { running.as_ref() });
        worker.set_looking(false);
        drop(gate);
        result
    }

    /// Whether worker `index` runs a fiber blocked inline in a system
    /// call (D-786): marked and `RUNNING`. A fiber parking on the blocking
    /// pool is marked only once it is `PARKING` (`blocking::on_pool`), so
    /// it is never taken for one (D-815); while the
    /// look runs, its worker cannot requeue it.
    fn worker_blocked(&self, index: usize) -> bool {
        self.look_at_running(index, |fiber| {
            fiber.is_some_and(|fiber| {
                fiber.sched.blocking()
                    && fiber.sched.state().load(Ordering::SeqCst) == fiber::state::RUNNING
            })
        })
    }

    /// Asks every running fiber to stop at its next entry check
    /// (`SAFEPOINT`, D-073, D-708), through the monitor's gate, never on a
    /// control block that may have been retired (I-8). Only the halt asks
    /// (C-5).
    fn request_safepoints(&self) {
        for index in 0..self.workers.filled() {
            self.look_at_running(index, |fiber| {
                if let Some(fiber) = fiber {
                    crate::ctx::request(fiber, crate::ctx::reason::SAFEPOINT);
                }
            });
        }
    }

    /// Every group that still exists: the default group first, then the
    /// others in the order their first root ran.
    fn live_groups(&self) -> Vec<Arc<DetachedGroup>> {
        let mut live = vec![Arc::clone(&self.default_group)];
        live.extend(self.groups.lock().iter().filter_map(Weak::upgrade));
        live
    }

    /// Whether any queue a thief can reach holds a fiber.
    fn work_pending(&self) -> bool {
        !self.injector.is_empty()
            || self
                .workers
                .iter()
                .any(|worker| worker.takes_local() && worker.has_stealable())
    }

    /// Whether any fiber waits in a run queue, so that preempting a running
    /// one lets it run. A lone running fiber is never preempted: a yield
    /// would only move it off its worker's warm caches.
    fn fibers_waiting(&self) -> bool {
        self.work_pending() || self.workers.iter().any(WorkerShared::has_waiting)
    }

    /// Wakes one parked worker if nobody searches (Tokio's notify).
    fn notify_parked(&self) {
        // Orders the push that precedes the call before the read of the
        // idle state, as the parking side's re-check orders the other way.
        atomic::fence(Ordering::SeqCst);
        if let Some(index) = self.idle.worker_to_notify()
            && let Some(worker) = self.worker(index)
        {
            worker.unpark(self);
        }
    }

    /// Wakes every idle worker: those parked on their condition variables
    /// and the one blocked in the poller (C-3: a stop or a halt that has
    /// set its flag calls it, so it never waits for the next timer). Each
    /// woken worker re-checks the flags before it parks again.
    pub(crate) fn wake_idle(&self) {
        for worker in self.workers.iter() {
            worker.unpark(self);
        }
        // The poll holder's parker woke it above; this wake also covers a
        // holder between its parker's states.
        self.driver.wake();
    }

    /// Queues a fiber only worker `index` may run, waking it if parked. A
    /// worker that is leaving is out of `Idle` and is woken directly; one
    /// that has ended takes no fiber, so an unpinned one (the migration
    /// stress mode's) goes to the global queue instead, and a pinned one
    /// woken after its worker ended at a stop is left alone and counted
    /// (D-821).
    fn push_targeted(&self, index: usize, fiber: FiberRef) {
        let Some(worker) = self.worker(index) else {
            crate::panic::fatal(format_args!("a fiber's worker does not exist"));
        };
        if let Err(fiber) = worker.push_targeted(fiber) {
            // SAFETY: a runnable fiber is not retired.
            if unsafe { fiber.ctx() }.sched.pinned() {
                // D-821: only its worker may run it
                // (D-687), and that worker ended at the stop, which no host
                // of Cheby code reaches (it tears its groups down first). As
                // D-786 skips a stuck fiber, the fiber is never run, requeued
                // or retired, its slot and control block stay, it is
                // counted in `left_at_stop` (D-710: `run_main` reads the
                // count after its stop, and skips its leak check and counts
                // the skip when it is not 0), and the stop goes on. Outside a stop, a retiring worker
                // stays while a fiber is pinned to it, so this is a broken
                // invariant.
                if self.stopping() {
                    self.left_at_stop.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                crate::panic::fatal(format_args!("a fiber is pinned to a retired worker"));
            }
            self.injector.push(fiber);
            self.notify_parked();
            return;
        }
        // The push happened before this read; a worker stores `LEAVING`
        // before it looks at its queue, so either it sees the fiber or
        // this read sees it leaving.
        if self.idle.unpark_specific(index) || worker.life() == life::LEAVING {
            worker.unpark(self);
        }
    }

    /// Makes a runnable fiber run: on its worker while it is pinned
    /// (D-687), on another worker in the migration stress mode, otherwise
    /// where `place` says. `from` is the worker the calling thread is, if
    /// it is one of this runtime's: its runnext slot and deque are used
    /// only from that thread.
    fn schedule(&self, fiber: FiberRef, from: Option<usize>, place: Where) {
        // SAFETY: a runnable fiber is not retired.
        let sched = &unsafe { fiber.ctx() }.sched;
        if sched.pinned() {
            self.push_targeted(sched.worker(), fiber);
            return;
        }
        // To the next live worker, if there is another one.
        if self.stress_migrate
            && let Some(target) = self.workers.next_live_after(sched.worker())
        {
            self.push_targeted(target, fiber);
            return;
        }
        // A worker that has drained its queues to leave takes no more.
        let here = from
            .and_then(|index| self.worker(index))
            .filter(|worker| worker.takes_local());
        match (place, here) {
            (Where::Next, Some(worker)) => {
                if let Some(old) = worker.put_next(fiber) {
                    // SAFETY: `from` is the calling thread's worker.
                    unsafe { worker.local() }.push(old);
                    self.notify_parked();
                }
            }
            (Where::Local, Some(worker)) => {
                // SAFETY: as above.
                unsafe { worker.local() }.push(fiber);
                self.notify_parked();
            }
            _ => {
                self.injector.push(fiber);
                self.notify_parked();
            }
        }
    }
}

/// The worker the calling thread is, if it runs a fiber of `shared`.
fn current_worker(shared: &Arc<Shared>) -> Option<usize> {
    // SAFETY: the current fiber's control block lives while it runs on
    // this thread, and the reference ends with this function.
    let current = unsafe { tls::current_fiber().as_ref() }?;
    let runtime = current.sched.runtime()?;
    Arc::ptr_eq(runtime, shared).then(|| current.sched.worker())
}

/// Makes a parked fiber runnable (architecture §3.4). With `from_running`
/// the waker is a running fiber about to block, and the woken fiber takes
/// its worker's runnext slot, which keeps a capacity-0 hand-off on one
/// worker. Returns false, doing nothing, for a fiber that is neither
/// parked nor parking.
///
/// Wake only a fiber that registered as a waiter under a lock it hands to
/// [`park`] (so it is parking or parked once the waker holds that lock),
/// or one a lockless waker owns otherwise; a fiber is woken once per park.
///
/// # Safety
///
/// `fiber` is not retired: it is parked, or parking with the lock the
/// caller holds.
pub(crate) unsafe fn wake(fiber: FiberRef, from_running: bool) -> bool {
    // SAFETY: forwarded from the caller.
    let ctx = unsafe { fiber.ctx() };
    let state = ctx.sched.state();
    let mut current = state.load(Ordering::SeqCst);
    loop {
        let next = match current {
            fiber::state::PARKED => fiber::state::RUNNABLE,
            fiber::state::PARKING => fiber::state::PARKING_WOKEN,
            _ => return false,
        };
        match state.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(seen) => current = seen,
        }
    }
    if current == fiber::state::PARKED {
        let Some(shared) = ctx.sched.runtime() else {
            crate::panic::fatal(format_args!("a parked fiber has no runtime"));
        };
        let place = if from_running {
            Where::Next
        } else {
            Where::Local
        };
        shared.schedule(fiber, current_worker(shared), place);
    }
    true
}

/// Hands the fibers waiting on the running fiber's worker to the other
/// workers before that fiber blocks the worker inline in a system call
/// (standard input, file IO, `@blocking`; D-786: when the blocking pool
/// could start no thread, `sched/blocking.rs`), as Go's
/// `entersyscall` hands its P off: the runnext slot's fiber, which no
/// thief takes, goes to the global queue, and an idle worker is woken to
/// take it and to steal from the local deque. Without this a fiber woken
/// just before the call (a `send` that wakes `main`, then `io::read_line`)
/// waited until the call returned (review finding ROB2-1 of gate 2).
/// Fibers pinned to the worker (D-687) stay: only it may run them. Does
/// nothing for an isolated entry, which has no runtime.
pub(crate) fn hand_off_before_blocking(me: &ExecCtx) {
    let Some(shared) = me.sched.runtime() else {
        return;
    };
    // `me` runs on this thread, so this is its worker, and the runnext
    // slot and deque are this thread's.
    let Some(worker) = shared.worker(me.sched.worker()) else {
        return;
    };
    let next = worker.take_next();
    let waiting = next.is_some() || worker.has_stealable();
    if let Some(next) = next {
        shared.injector.push(next);
    }
    if waiting {
        shared.notify_parked();
    }
}

/// Why [`spawn_fiber`] could not start a fiber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnError {
    /// The spawner is an isolated entry, which has no runtime (D-692).
    NoRuntime,
    /// No stack slot: D-642's `out of memory` for the spawner (D-677).
    Stack(StackError),
}

/// Starts a fiber in the spawner's runtime that runs `start`:
/// [`create_fiber`], then [`schedule_new`].
///
/// # Errors
///
/// [`SpawnError`] when the spawner has no runtime or no slot is left.
#[cfg(test)]
pub(crate) fn spawn_fiber(
    spawner: &ExecCtx,
    stack_size: usize,
    start: StartFn,
    next: bool,
) -> Result<FiberRef, SpawnError> {
    let fiber = create_fiber(spawner, stack_size, start)?;
    schedule_new(fiber, next);
    Ok(fiber)
}

/// Creates a fiber in the spawner's runtime that runs `start` on a stack
/// slot of `stack_size` bytes (D-677), without queueing it: the caller
/// records it where it must be known before it can run, then calls
/// [`schedule_new`]. Its control block takes the spawner's entry mode,
/// hash seed, accounting domain (D-691), trampolines (D-526) and whether
/// its code has unwind edges (D-770); it has no host to
/// report to. On an error, `start` is dropped
/// without running.
///
/// # Errors
///
/// [`SpawnError`] when the spawner has no runtime or no slot is left.
pub(crate) fn create_fiber(
    spawner: &ExecCtx,
    stack_size: usize,
    start: StartFn,
) -> Result<FiberRef, SpawnError> {
    let Some(shared) = spawner.sched.runtime() else {
        return Err(SpawnError::NoRuntime);
    };
    let params = FiberParams {
        init: FiberInit {
            entry_mode: spawner.entry_mode().clone(),
            outcome: None,
            seed: spawner.seed(),
            budget: u64::MAX,
            domain: Arc::clone(spawner.domain()),
            consts: None,
            callbacks: spawner.callbacks(),
            // A fiber runs code of its spawner's build (D-770).
            unwind_edges: spawner.unwind_edges(),
            ..FiberInit::placeholder()
        },
        stack_size,
    };
    // A spawn on a worker of the runtime takes its slot from that worker's
    // cache first (D-825).
    let fiber = fiber::create(
        shared,
        spawner.sched.worker(),
        current_worker(shared),
        params,
        start,
    )
    .map_err(SpawnError::Stack)?;
    // The spawner's group (D-799), as its domain.
    let group = spawner
        .conc
        .group()
        .unwrap_or_else(|| Arc::clone(shared.default_group()));
    // SAFETY: the fiber was just created and is not queued yet: nothing
    // else knows it.
    unsafe { fiber.ctx() }.conc.set_group(Some(group));
    Ok(fiber)
}

/// Queues a fiber [`create_fiber`] made. With `next` it takes the current
/// worker's runnext slot, so it runs when the spawner blocks (architecture
/// §3.5); otherwise it goes to the current worker's deque, where idle
/// workers steal it.
pub(crate) fn schedule_new(fiber: FiberRef, next: bool) {
    // SAFETY: a fiber that was just created and not queued yet is not
    // retired: nothing else knows it.
    let ctx = unsafe { fiber.ctx() };
    let Some(shared) = ctx.sched.runtime() else {
        crate::panic::fatal(format_args!("a new fiber has no runtime"));
    };
    let place = if next { Where::Next } else { Where::Local };
    shared.schedule(fiber, current_worker(shared), place);
}

/// What [`Runtime::new`] takes beyond its [`RuntimeConfig`]; tests change
/// it.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    /// The migration stress mode; `None` reads the environment.
    stress_migrate: Option<bool>,
    /// See [`Shared::rescue`].
    rescue: bool,
    /// Whether the monitor detects deadlocks (D-724, D-860): on in every
    /// shipped host; runtime unit tests whose host wakes fibers (the test
    /// mailbox, waits with a timeout) turn it off (`testing::tuned`).
    detect_deadlocks: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            stress_migrate: None,
            rescue: true,
            detect_deadlocks: true,
        }
    }
}

/// Whether the migration stress mode is on: debug builds with
/// `CHEBY_RT_STRESS_MIGRATE=1` (D-687).
fn stress_from_env() -> bool {
    cfg!(debug_assertions) && std::env::var_os(STRESS_MIGRATE_ENV).is_some_and(|value| value == "1")
}

/// A scheduler for one host run (D-690): its workers, queues and monitor.
/// Dropping it stops it like [`Runtime::shutdown`].
pub struct Runtime {
    shared: Arc<Shared>,
    /// The monitor's thread; the workers' are in [`Shared`] (C-7).
    monitor: Option<JoinHandle<()>>,
    /// Whether it has stopped or halted: `shutdown`, `halt` and `Drop` run
    /// once.
    finished: bool,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Starts a runtime: reads the thread count (`config.threads`, or
    /// `CHEBY_THREADS` once, D-658), then starts that many workers and the
    /// monitor.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidThreads`] for an invalid `CHEBY_THREADS`,
    /// checked first on every platform; [`RuntimeError::Unsupported`]
    /// where there is no fiber switch (Windows until M6, D-714), so the
    /// host falls back to the thread backend; [`RuntimeError::Spawn`] if a
    /// thread cannot start.
    pub fn new(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        Self::tuned(config, Tuning::default())
    }

    fn tuned(config: &RuntimeConfig, tuning: Tuning) -> Result<Self, RuntimeError> {
        let threads = match config.threads {
            Some(threads) => threads.clamp(1, MAX_THREADS),
            None => threads_from_env()?,
        };
        if !switch::SUPPORTED {
            return Err(RuntimeError::Unsupported);
        }
        let driver = poll::Driver::new().map_err(RuntimeError::Spawn)?;
        let shared = Arc::new(Shared {
            workers: Workers::new(),
            threads: Mutex::new(Vec::with_capacity(threads)),
            injector: Injector::new(),
            // Each worker joins as it starts.
            idle: Idle::new(0),
            stopping: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            args: config.args.clone(),
            program_name: config.program_name.clone(),
            stress_migrate: tuning.stress_migrate.unwrap_or_else(stress_from_env),
            rescue: tuning.rescue,
            detect_deadlocks: tuning.detect_deadlocks,
            monitor: MonitorWake::default(),
            timers: timer::Timers::new(Instant::now()),
            driver,
            fiber_stack: config.fiber_stack,
            thread_target: AtomicUsize::new(threads),
            default_group: DetachedGroup::new(),
            groups: Mutex::new(Vec::new()),
            halting: AtomicBool::new(false),
            exit_status: AtomicI64::new(NO_STATUS),
            terminated: AtomicBool::new(false),
            roots: Mutex::new(Vec::new()),
            domains: DomainTable::default(),
            next_root: AtomicU64::new(0),
            code: Mutex::new(Vec::new()),
            left_at_stop: AtomicUsize::new(0),
            pool_given_up: AtomicBool::new(false),
            pool_waiting: AtomicUsize::new(0),
            pool_done: AtomicU64::new(0),
            #[cfg(test)]
            probe: testing::MonitorProbe::default(),
            #[cfg(test)]
            fail_stack: AtomicBool::new(false),
        });
        let mut runtime = Self {
            shared,
            monitor: None,
            finished: false,
        };
        {
            let mut handles = runtime.shared.threads.lock();
            for index in 0..threads {
                runtime
                    .shared
                    .start_worker(index, &mut handles)
                    .map_err(RuntimeError::Spawn)?;
            }
        }
        let shared = Arc::clone(&runtime.shared);
        let monitor = std::thread::Builder::new()
            .name("cheby-monitor".to_owned())
            .spawn(move || monitor::main(&shared))
            .map_err(RuntimeError::Spawn)?;
        runtime.monitor = Some(monitor);
        Ok(runtime)
    }

    /// The number of live worker threads: those that take fibers, without
    /// the ones a lower thread count retires.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.shared.workers.live()
    }

    /// Keeps `code` alive until the runtime has stopped (D-708). Attach
    /// the token of every JIT whose code the runtime's fibers may run,
    /// before running its entries; attaching a token twice keeps one.
    pub fn attach(&self, code: CodeToken) {
        let mut tokens = self.shared.code.lock();
        if !tokens.iter().any(|token| token.same(&code)) {
            tokens.push(code);
        }
    }

    /// Runs one entry thunk as a root fiber on a stack slot of
    /// `config.stack_size` bytes and blocks the calling thread until it
    /// returns or panics (D-690). The fiber charges `config.domain` and
    /// gets `config`'s trampolines, constants and unwind edges setting; its
    /// hash seed is the constant's under [`EntryMode::ConstEval`], the
    /// per-process seed otherwise. A panic unwinds the root to its catch
    /// point, which reports it to the caller, and its slot goes back to
    /// the pool (D-713).
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] if the stack is too small or cannot be taken, or
    /// the fiber ends without an outcome.
    ///
    /// # Safety
    ///
    /// As for [`crate::run_entry`]: `entry` is an entry thunk with the
    /// [`EntryFn`] ABI, and the code it reaches stays valid while the
    /// runtime's fibers may run it, which [`Runtime::attach`] guarantees
    /// for a JIT's code; the trampolines of `config` stay valid as long.
    pub unsafe fn run_root(
        &self,
        entry: EntryFn,
        config: &EntryConfig,
    ) -> Result<EntryOutcome, RuntimeError> {
        let group = Arc::clone(self.shared.default_group());
        // SAFETY: forwarded from the caller.
        unsafe { self.run_root_with(entry, config, group) }
    }

    /// [`Runtime::run_root`] with the root in `group`.
    ///
    /// # Safety
    ///
    /// As for [`Runtime::run_root`].
    unsafe fn run_root_with(
        &self,
        entry: EntryFn,
        config: &EntryConfig,
        group: Arc<DetachedGroup>,
    ) -> Result<EntryOutcome, RuntimeError> {
        let started = Instant::now();
        let receiver = self.spawn_root_in(
            config,
            group,
            Box::new(move |ctx| {
                // SAFETY: the caller guarantees that `entry` is an entry
                // thunk whose code stays valid while it runs; `ctx` is the
                // root fiber's control block.
                unsafe { entry(ctx) }
            }),
            None,
        )?;
        let outcome = receiver.recv().map_err(|_| RuntimeError::EntryLost)?;
        config.domain.stats().add_entry(started.elapsed());
        Ok(outcome)
    }

    /// Starts a root fiber in the default group that runs `body` and
    /// reports its word, or its panic, to the returned receiver.
    #[cfg(test)]
    fn spawn_root(
        &self,
        config: &EntryConfig,
        body: Box<dyn FnOnce(*mut ExecCtx) -> u64 + Send>,
    ) -> Result<mpsc::Receiver<EntryOutcome>, RuntimeError> {
        self.spawn_root_in(config, Arc::clone(self.shared.default_group()), body, None)
    }

    /// Starts a root fiber in `group` that runs `body` and reports its
    /// outcome to the returned receiver, or to `notice` when it has one
    /// (`Runtime::start_root_notify`).
    ///
    /// The root's base frame (D-859, critique C5 and RISK-3): when the root
    /// starts it opens its base scope (`scope::open_root_base`) and records
    /// it in its registry entry, before its body; after the body returned
    /// or unwound, the deadlock's cancellation included, it ends the base
    /// under its lock (`scope::close_root_base`), then takes its outcome at
    /// its catch point (`finish_entry`), reports it, unregisters, and
    /// retires. The deadlock detector reads the root's control block only
    /// under the base's lock while the base has not ended.
    fn spawn_root_in(
        &self,
        config: &EntryConfig,
        group: Arc<DetachedGroup>,
        body: Box<dyn FnOnce(*mut ExecCtx) -> u64 + Send>,
        notice: Option<Arc<lifecycle::RootNotice>>,
    ) -> Result<mpsc::Receiver<EntryOutcome>, RuntimeError> {
        if config.stack_size < MIN_STACK_SIZE {
            return Err(RuntimeError::StackTooSmall {
                requested: config.stack_size,
            });
        }
        // Memory for ending in order an entry that the system refuses
        // memory (D-642).
        alloc::ensure_reserve();
        let (sender, receiver) = mpsc::channel();
        // D-709: a termination sends every waiting host `Exited`; once one
        // has halted the runtime, a new root never runs and its host gets
        // `Exited` at once (D-802).
        let root = self.shared.next_root.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(RootEntry::new(
            &self.shared,
            root,
            sender.clone(),
            &config.domain,
            matches!(config.mode, EntryMode::Run),
        ));
        if !self.shared.register_root(Arc::clone(&entry)) {
            let status = self.shared.exit_status().unwrap_or(crate::abi::status::ERR);
            sender.send(EntryOutcome::Exited(status)).ok();
            return Ok(receiver);
        }
        // The detector finds the group's fibers through the domains of the
        // roots that ran in it.
        group.add_domain(&config.domain);
        let seed = match &config.mode {
            EntryMode::ConstEval(limits) => limits.seed,
            EntryMode::Run | EntryMode::Test | EntryMode::Repl => generic::process_seed(),
        };
        let params = FiberParams {
            init: FiberInit {
                entry_mode: config.mode.clone(),
                outcome: Some(sender),
                seed,
                budget: u64::MAX,
                domain: Arc::clone(&config.domain),
                consts: config.consts.clone(),
                callbacks: config.callbacks,
                unwind_edges: config.unwind_edges,
                ..FiberInit::placeholder()
            },
            stack_size: config.stack_size,
        };
        let start: StartFn = Box::new(move |ctx| {
            // SAFETY: `ctx` is this fiber's live control block.
            let me = unsafe { &*ctx };
            // The base frame (D-859): the base scope opens before the body.
            let base = crate::fiber_ops::scope::open_root_base(me);
            entry.start(FiberRef::from_ctx(me), base);
            let word = body(ctx);
            // ... and ends, under its lock, before the root reports,
            // unregisters and retires (critique C5, RISK-3).
            crate::fiber_ops::scope::close_root_base(me);
            // The root's catch point (D-699, D-701, D-859): an entry that
            // unwound returns with the record in its control block, never
            // told apart by the thunk's word (`main`'s 0 is success).
            let outcome = crate::unwind::record::finish_entry(me, word);
            match notice {
                Some(notice) => notice.report(Ok(outcome)),
                None => {
                    if let Some(sender) = me.outcome() {
                        sender.send(outcome).ok();
                    }
                }
            }
            if let Some(runtime) = me.sched.runtime() {
                runtime.unregister_root(root);
            }
            drop(entry);
        });
        // A root started by a host takes its slot from the global pool
        // (D-825).
        let fiber = match fiber::create(&self.shared, 0, None, params, start) {
            Ok(fiber) => fiber,
            Err(error) => {
                self.shared.unregister_root(root);
                return Err(RuntimeError::Stack(error));
            }
        };
        // SAFETY: the fiber was just created and is not queued yet.
        unsafe { fiber.ctx() }.conc.set_group(Some(group));
        self.shared.schedule(fiber, None, Where::Global);
        Ok(receiver)
    }

    /// Stops the runtime (D-708): tears every group down (the default
    /// group, then each group a root ran in), so no detached fiber is left;
    /// its workers finish the fibers that are runnable, then every thread
    /// is joined, and only then are the code tokens released. A fiber that
    /// waits on the blocking pool, before or during the stop, is runnable
    /// once its job completes, and the workers wait for that, until two
    /// polls 5 ms apart find every worker idle and no job completed
    /// (D-815). A fiber still waiting then is never
    /// resumed, as one blocked inline (D-786), and a root among them never
    /// reports. A runtime
    /// whose halt has started (a termination, D-709) completes the halt
    /// instead and tears nothing down. Call it from the host, never from
    /// one of its fibers. Dropping the runtime does the same.
    pub fn shutdown(mut self) {
        self.finish();
    }

    /// The body of [`Runtime::shutdown`], `Drop` and [`Runtime::halt`]:
    /// runs once.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.shared.halting() {
            for group in self.shared.live_groups() {
                if self.teardown(&group).halted {
                    break;
                }
            }
        }
        if self.shared.halting() {
            self.complete_halt();
        } else {
            self.stop();
        }
    }

    /// Stops the runtime (C7): sets `stopping`, which makes every later
    /// [`Shared::arm_timer`] return a handle that never fires; wakes every
    /// parked worker and the monitor; joins every thread; then drops every
    /// timer entry unfired ([`Shared::clear_timers`]) and only then the
    /// code tokens (D-708). The workers stay while a fiber waits on the
    /// blocking pool, whose completion makes it runnable, until the join
    /// gives up on the pool (`blocking::poll_stop`,
    /// D-815). A worker whose fiber is blocked
    /// inline in a system call (D-786) is never joined: the halt flag is
    /// set, so that fiber freezes when the call returns, and the code
    /// tokens and timers are kept (leaked), since that worker may still
    /// run the code.
    fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.wake_idle();
        self.shared.monitor.stop();
        let stuck = self.join_workers(false);
        if stuck {
            self.shared.halting.store(true, Ordering::SeqCst);
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.join().ok();
        }
        if stuck {
            std::mem::forget(std::mem::take(&mut *self.shared.code.lock()));
            return;
        }
        self.shared.clear_timers();
        self.shared.code.lock().clear();
    }

    /// Joins every worker thread except those whose fiber is blocked
    /// inline in a system call (D-786), which are left running, detached;
    /// returns whether there was one. With `freeze`, asks the fibers that
    /// still run to freeze (`SAFEPOINT`) on every round, for the halt.
    /// The workers `set_thread_count` started are among them, and those it
    /// retired too (C-7): the caller set the stop or halt flag first, so
    /// no worker starts or rejoins after the registry is drained. Without
    /// `freeze` (a stop), each round also polls the fibers waiting on the
    /// blocking pool, which keep the workers, and gives up on them as
    /// `blocking::poll_stop` says (D-815).
    fn join_workers(&mut self, freeze: bool) -> bool {
        let mut pending: Vec<(usize, JoinHandle<()>)> =
            std::mem::take(&mut *self.shared.threads.lock());
        let mut stuck = false;
        let mut round = 0_u32;
        let mut pool_seen = None;
        while !pending.is_empty() {
            if !freeze {
                let idle = pending.iter().all(|(index, _)| {
                    self.shared
                        .worker(*index)
                        .is_none_or(|worker| worker.running().is_null())
                });
                blocking::poll_stop(&self.shared, idle, &mut pool_seen);
            }
            if freeze {
                self.shared.request_safepoints();
            }
            let mut still = Vec::with_capacity(pending.len());
            for (index, handle) in pending {
                if handle.is_finished() {
                    handle.join().ok();
                } else if self.shared.worker_blocked(index) {
                    // Detached: never joined (D-786).
                    drop(handle);
                    stuck = true;
                } else {
                    still.push((index, handle));
                }
            }
            pending = still;
            if pending.is_empty() {
                break;
            }
            round = round.saturating_add(1);
            if round < 64 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        stuck
    }

    /// Completes a started halt (D-073, D-708): asks every running fiber
    /// to freeze until each worker has ended, joins every thread (the
    /// workers, among them the poll holder, and the monitor), so that no
    /// timer fires and no fiber runs afterwards (C-4), and releases the
    /// code tokens last. Frozen fibers' slots, control blocks, wait nodes
    /// and timer entries are left as they are. A fiber waiting on the
    /// blocking pool stays parked: its job's completion wakes nothing
    /// during a halt. A worker whose fiber is blocked inline in a system
    /// call (D-786: no pool thread could start) is never joined, and the
    /// code tokens are then kept (leaked): its fiber freezes when the
    /// call returns.
    fn complete_halt(&mut self) {
        self.shared.begin_halt();
        let stuck = self.join_workers(true);
        if let Some(monitor) = self.monitor.take() {
            monitor.join().ok();
        }
        if stuck {
            std::mem::forget(std::mem::take(&mut *self.shared.code.lock()));
            return;
        }
        self.shared.code.lock().clear();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod blocking_models;
#[cfg(test)]
mod blocking_tests;
#[cfg(test)]
mod deadlock_models;
#[cfg(test)]
mod deadlock_tests;
#[cfg(test)]
mod io_models;
#[cfg(test)]
mod io_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod park_models;
#[cfg(test)]
mod poll_tests;
#[cfg(test)]
mod retire_models;
#[cfg(test)]
mod slot_cache_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod thread_count_tests;
#[cfg(test)]
mod timer_tests;
