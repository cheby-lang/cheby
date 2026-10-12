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
//! Shutdown in the slice: no fiber outlives the root that started it yet,
//! so stopping a runtime is joining its idle workers and its monitor.

mod fiber;
mod idle;
mod monitor;
mod worker;

use std::ffi::OsStr;
use std::fmt;
use std::sync::atomic::{self, AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_deque::Injector;
use parking_lot::Mutex;

#[cfg_attr(
    not(test),
    expect(
        unused_imports,
        reason = "the channel kernels park with a lock (S6a, wave 3)"
    )
)]
pub(crate) use fiber::Unlock;
pub(crate) use fiber::{AfterSwitch, FiberRef, FiberSched, StartFn, park};

use self::fiber::FiberParams;
use self::idle::Idle;
use self::monitor::MonitorWake;
use self::worker::WorkerShared;
use crate::alloc;
use crate::ctx::{ExecCtx, FiberInit};
use crate::entry::{EntryConfig, EntryFn, EntryMode, EntryOutcome, MIN_STACK_SIZE, RuntimeError};
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
/// GAP(cheby-threads-syntax): a decimal whole number is ASCII digits only,
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
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// The number of worker threads, clamped to 1 to [`MAX_THREADS`];
    /// `None` reads `CHEBY_THREADS` (D-658). In-process hosts, such as
    /// tests, may bound it.
    pub threads: Option<usize>,
    /// The program's arguments, without the program name (D-347, D-690).
    pub args: Vec<String>,
    /// The program name `std::env` reports; empty means "not set" (D-347).
    pub program_name: String,
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
    /// The workers, by index.
    workers: Box<[WorkerShared]>,
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
    /// The monitor's sleep.
    monitor: MonitorWake,
    /// Test hooks of the monitor.
    #[cfg(test)]
    probe: testing::MonitorProbe,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("workers", &self.workers.len())
            .field("live", &self.live.load(Ordering::Relaxed))
            .field("stopping", &self.stopping.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Worker `index`.
    fn worker(&self, index: usize) -> Option<&WorkerShared> {
        self.workers.get(index)
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
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

    /// Whether any queue a thief can reach holds a fiber.
    fn work_pending(&self) -> bool {
        !self.injector.is_empty() || self.workers.iter().any(WorkerShared::has_stealable)
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
            worker.unpark();
        }
    }

    /// Queues a fiber only worker `index` may run, waking it if parked.
    fn push_targeted(&self, index: usize, fiber: FiberRef) {
        let Some(worker) = self.worker(index) else {
            crate::panic::fatal(format_args!("a fiber's worker does not exist"));
        };
        worker.push_targeted(fiber);
        if self.idle.unpark_specific(index) {
            worker.unpark();
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
        let count = self.workers.len();
        if sched.pinned() {
            self.push_targeted(sched.worker(), fiber);
            return;
        }
        if self.stress_migrate && count > 1 {
            let target = sched
                .worker()
                .wrapping_add(1)
                .checked_rem(count)
                .unwrap_or(0);
            self.push_targeted(target, fiber);
            return;
        }
        let here = from.and_then(|index| self.worker(index));
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the fiber and channel kernels use it (S6a, wave 3)"
    )
)]
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

/// Why [`spawn_fiber`] could not start a fiber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnError {
    /// The spawner is an isolated entry, which has no runtime (D-692).
    NoRuntime,
    /// No stack slot: D-642's `out of memory` for the spawner (D-677).
    Stack(StackError),
}

/// Starts a fiber in the spawner's runtime that runs `start` on a stack
/// slot of `stack_size` bytes (D-677). Its control block takes the
/// spawner's entry mode, hash seed, accounting domain (D-691),
/// trampolines (D-526) and unwinding backend; it has no host to report to.
/// With `next` it takes the current worker's runnext slot, so it runs
/// when the spawner blocks; otherwise it goes to the current worker's
/// deque, where idle workers steal it.
///
/// # Errors
///
/// [`SpawnError`] when the spawner has no runtime or no slot is left.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "fiber::spawn's kernel uses it (S6a, wave 3)")
)]
pub(crate) fn spawn_fiber(
    spawner: &ExecCtx,
    stack_size: usize,
    start: StartFn,
    next: bool,
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
            unwind_backend: spawner.unwind_backend(),
            ..FiberInit::placeholder()
        },
        stack_size,
    };
    let fiber =
        fiber::create(shared, spawner.sched.worker(), params, start).map_err(SpawnError::Stack)?;
    let place = if next { Where::Next } else { Where::Local };
    shared.schedule(fiber, current_worker(shared), place);
    Ok(fiber)
}

/// What [`Runtime::new`] takes beyond its [`RuntimeConfig`]; tests change
/// it.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    /// The migration stress mode; `None` reads the environment.
    stress_migrate: Option<bool>,
    /// See [`Shared::rescue`].
    rescue: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            stress_migrate: None,
            rescue: true,
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
    workers: Vec<JoinHandle<()>>,
    monitor: Option<JoinHandle<()>>,
    /// The code its fibers may run (D-708).
    code: Mutex<Vec<CodeToken>>,
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
        let shared = Arc::new(Shared {
            workers: (0..threads).map(|_| WorkerShared::new()).collect(),
            injector: Injector::new(),
            idle: Idle::new(threads),
            stopping: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            args: config.args.clone(),
            program_name: config.program_name.clone(),
            stress_migrate: tuning.stress_migrate.unwrap_or_else(stress_from_env),
            rescue: tuning.rescue,
            monitor: MonitorWake::default(),
            #[cfg(test)]
            probe: testing::MonitorProbe::default(),
        });
        let mut runtime = Self {
            shared,
            workers: Vec::with_capacity(threads),
            monitor: None,
            code: Mutex::new(Vec::new()),
        };
        for index in 0..threads {
            let shared = Arc::clone(&runtime.shared);
            let handle = std::thread::Builder::new()
                .name(format!("cheby-worker-{index}"))
                .spawn(move || worker::main(&shared, index))
                .map_err(RuntimeError::Spawn)?;
            runtime.workers.push(handle);
        }
        let shared = Arc::clone(&runtime.shared);
        let monitor = std::thread::Builder::new()
            .name("cheby-monitor".to_owned())
            .spawn(move || monitor::main(&shared))
            .map_err(RuntimeError::Spawn)?;
        runtime.monitor = Some(monitor);
        Ok(runtime)
    }

    /// The number of worker threads.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.shared.workers.len()
    }

    /// Keeps `code` alive until the runtime has stopped (D-708). Attach
    /// the token of every JIT whose code the runtime's fibers may run,
    /// before running its entries; attaching a token twice keeps one.
    pub fn attach(&self, code: CodeToken) {
        let mut tokens = self.code.lock();
        if !tokens.iter().any(|token| token.same(&code)) {
            tokens.push(code);
        }
    }

    /// Runs one entry thunk as a root fiber on a stack slot of
    /// `config.stack_size` bytes and blocks the calling thread until it
    /// returns or panics (D-690). The fiber charges `config.domain` and
    /// gets `config`'s trampolines, constants and unwinding backend; its
    /// hash seed is the constant's under [`EntryMode::ConstEval`], the
    /// per-process seed otherwise. Until unwinding exists, a panic is
    /// reported as in M1 and the fiber's slot is quarantined
    /// (GAP(interim-panics)).
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
        let started = Instant::now();
        let receiver = self.spawn_root(
            config,
            Box::new(move |ctx| {
                // SAFETY: the caller guarantees that `entry` is an entry
                // thunk whose code stays valid while it runs; `ctx` is the
                // root fiber's control block.
                unsafe { entry(ctx) }
            }),
        )?;
        let outcome = receiver.recv().map_err(|_| RuntimeError::EntryLost)?;
        config.domain.stats().add_entry(started.elapsed());
        Ok(outcome)
    }

    /// Starts a root fiber that runs `body` and reports its word, or its
    /// panic, to the returned receiver.
    fn spawn_root(
        &self,
        config: &EntryConfig,
        body: Box<dyn FnOnce(*mut ExecCtx) -> u64 + Send>,
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
                unwind_backend: config.unwind,
                ..FiberInit::placeholder()
            },
            stack_size: config.stack_size,
        };
        let start: StartFn = Box::new(move |ctx| {
            let word = body(ctx);
            // SAFETY: `ctx` is this fiber's live control block.
            if let Some(outcome) = unsafe { &*ctx }.outcome() {
                outcome.send(EntryOutcome::Returned(word)).ok();
            }
        });
        let fiber = fiber::create(&self.shared, 0, params, start).map_err(RuntimeError::Stack)?;
        self.shared.schedule(fiber, None, Where::Global);
        Ok(receiver)
    }

    /// Stops the runtime: its workers finish the fibers that are runnable,
    /// then every thread is joined, and only then are the code tokens
    /// released (D-708). Call it from the host, never from one of its
    /// fibers.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        for worker in &self.shared.workers {
            worker.unpark();
        }
        self.shared.monitor.stop();
        for handle in self.workers.drain(..) {
            handle.join().ok();
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.join().ok();
        }
        self.code.lock().clear();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
