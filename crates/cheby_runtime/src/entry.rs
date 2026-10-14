//! Running entries (F7, §10.10): `run_entry` runs one entry thunk on its
//! own stack and reports how it ended; `run_main` adds `cheby run`'s
//! policy (exit statuses, flushing, the debug leak check).
//!
//! Two backends run entries. `run_main` and the hosts of `cheby test` and
//! `xtask test-std` run them as root fibers of a [`Runtime`] made for the
//! run (D-690, [`crate::sched`]). Isolated entries, constant evaluation
//! and the REPL (D-713), and every entry where fibers are not supported
//! (Windows until M6, D-714), run on a dedicated OS thread with the
//! requested stack size: [`run_entry`] and [`run_task`] (D-457). A panic
//! unwinds the entry to its catch point by the return flag (D-733), its
//! cleanup pads release its objects, its thread or fiber ends normally
//! (D-713), and the host learns of the panic from the fiber's record
//! (`unwind::record::finish_entry`, D-699).
//!
//! # The current entry
//!
//! Kernels take no `ctx` (D-453), yet some need the running entry: a kernel
//! that calls a Cheby closure ([`crate::callback`], D-526), maps and sets
//! that take the entry's hash seed (D-538), and the stub bound to
//! unresolved symbols (D-532). The entry's state therefore lives in its
//! fiber control block (D-685): its hash seed, its trampolines, its
//! constants and its accounting domain, read through thin readers of the
//! current fiber ([`current_ctx`], [`current_seed`], [`callbacks`]).
//! `run_entry` heap-allocates the block and makes it `tls.rs`'s current
//! fiber on the entry's thread (D-686); a worker makes each fiber it
//! resumes current. Generated code still passes `ctx` explicitly
//! (ADR-0055).
//!
//! # Limits of constant evaluation
//!
//! An entry in [`EntryMode::ConstEval`] runs under the limits of its
//! [`ConstEvalLimits`] (D-195, D-536):
//!
//! - **Time.** The host waiting in [`run_entry`] is the watchdog: when the
//!   deadline passes it requests an interrupt with the reason
//!   [`DEADLINE`](crate::ctx::reason::DEADLINE) on the entry's context
//!   (D-688), so the next function entry or tail call takes the slow path
//!   of the stack check, which ends the entry with [`Limit::Time`]. A
//!   long single kernel call is not interrupted.
//! - **Memory.** The budget is the live bytes of the entry's accounting
//!   domain when the entry starts plus the limit, so only what this
//!   evaluation keeps live counts: earlier constants, the objects of an
//!   abandoned evaluation and REPL session values do not
//!   (D-617). Every allocation checks it before it
//!   asks the system for memory ([`check_budget`]), so the peak never
//!   passes it and a single huge request is E0076, not an abort; kernels
//!   allocate through the same path, and the Rust-side worklists of the
//!   runtime's walks are scratch buffers that the budget counts too
//!   ([`alloc::scratch`]). Exceeding it abandons the entry with
//!   [`Limit::Memory`].
//!
//! The host learns which limit ended an entry from the build's
//! [`ConstTable`] ([`crate::consteval::Abandoned::limit`]).
//!
//! [`Limit::Time`]: crate::consteval::Limit::Time
//! [`Limit::Memory`]: crate::consteval::Limit::Memory

use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::abi::status;
use crate::alloc::{self, Domain, LEAK_CHECK_ENABLED};
use crate::consteval::{self, ConstTable, Limit};
use crate::ctx::{ExecCtx, FiberInit};
use crate::fiber::StackError;
use crate::generic;
use crate::io;
use crate::panic::{PanicReport, stack_position};
use crate::sched::{CodeToken, DetachedGroup, FiberSched, Runtime, RuntimeConfig};
use crate::stats;
use crate::tls;
use crate::unwind;

/// An entry thunk: generated with the platform C calling convention, it
/// takes the context and returns a raw `i64` status (`main`, tests) or an
/// owned value word (constants, REPL entries).
pub type EntryFn = unsafe extern "C" fn(ctx: *mut ExecCtx) -> u64;

/// A trampoline of arity 0 (`cb$tramp$0`, D-526): C ABI, the context, the
/// closure (borrowed); returns the owned result.
pub type Tramp0 = unsafe extern "C" fn(ctx: *mut ExecCtx, closure: u64) -> u64;
/// A trampoline of arity 1: the context, the closure (borrowed), one owned
/// uniform argument; returns the owned result.
pub type Tramp1 = unsafe extern "C" fn(ctx: *mut ExecCtx, closure: u64, a0: u64) -> u64;
/// A trampoline of arity 2.
pub type Tramp2 = unsafe extern "C" fn(ctx: *mut ExecCtx, closure: u64, a0: u64, a1: u64) -> u64;
/// A trampoline of arity 3.
pub type Tramp3 =
    unsafe extern "C" fn(ctx: *mut ExecCtx, closure: u64, a0: u64, a1: u64, a2: u64) -> u64;
/// A trampoline of arity 4.
pub type Tramp4 = unsafe extern "C" fn(
    ctx: *mut ExecCtx,
    closure: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
) -> u64;

/// The trampolines an entry's kernels may call closures through (D-526),
/// one per arity, each matching `cheby_mir::abi::trampoline_sig(n)`. The
/// JIT fills those its batches compiled.
#[derive(Debug, Clone, Copy, Default)]
pub struct Callbacks {
    /// Arity 0.
    pub t0: Option<Tramp0>,
    /// Arity 1.
    pub t1: Option<Tramp1>,
    /// Arity 2.
    pub t2: Option<Tramp2>,
    /// Arity 3.
    pub t3: Option<Tramp3>,
    /// Arity 4.
    pub t4: Option<Tramp4>,
}

/// The default stack of a fiber (D-125).
pub const DEFAULT_STACK_SIZE: usize = 8 << 20;

/// The stack of the root fiber running `main` (D-125, D-457).
pub const DEFAULT_ROOT_STACK_SIZE: usize = 64 << 20;

/// Room left below the stack limit: the distance from the top of the stack
/// to the entry's first frame, plus the runtime and foreign calls made
/// below the last checked frame (§11.8.1, ADR-0032).
pub const STACK_HEADROOM: usize = 256 << 10;

/// The smallest stack an entry may ask for.
pub const MIN_STACK_SIZE: usize = 2 * STACK_HEADROOM;

/// What kind of entry runs, which decides how panics are reported and
/// what the code may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryMode {
    /// `main` under `cheby run`.
    Run,
    /// A test block under `cheby test` (G4).
    Test,
    /// A REPL entry (G5).
    Repl,
    /// A constant's initializer (E6): panics are captured, not printed, and
    /// externals and IO are forbidden (§4.5).
    ConstEval(ConstEvalLimits),
}

/// The limits of a constant evaluation (D-195, D-536) and its seed
/// (D-269, D-538). `None` is no limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConstEvalLimits {
    /// When the evaluation must stop: checked at function entries and tail
    /// calls once the host's watchdog interrupts the entry.
    pub deadline: Option<Instant>,
    /// The most bytes the evaluation may add to the live bytes of the
    /// entry's accounting domain at once (kernels included), counted from
    /// what was live when the entry started.
    pub memory: Option<u64>,
    /// The hash seed it runs with (D-538).
    pub seed: u64,
}

/// How to run an entry.
#[derive(Debug, Clone)]
pub struct EntryConfig {
    /// The size of the entry's stack in bytes, at least [`MIN_STACK_SIZE`].
    pub stack_size: usize,
    /// What kind of entry it is.
    pub mode: EntryMode,
    /// The trampolines the entry's kernels may call closures through
    /// (D-526).
    pub callbacks: Callbacks,
    /// The accounting domain the entry charges (plan §4.3.4): the build's.
    pub domain: Arc<Domain>,
    /// The build's constants, for lazy forcing during constant evaluation
    /// (D-535): `None` outside constant evaluation.
    pub consts: Option<Arc<ConstTable>>,
    /// Whether the entry's code was compiled with unwind edges: false only
    /// under `CHEBY_UNWIND_EDGES=off` (D-742), where a raise in the entry,
    /// or in a fiber it spawns, ends the process (D-770).
    /// Stored in its control block; the JIT fills it from its code
    /// generation options.
    pub unwind_edges: bool,
}

impl EntryConfig {
    /// An entry of the given mode with the default fiber stack, no
    /// trampolines, the calling thread's current accounting domain and
    /// unwind edges.
    #[must_use]
    pub fn new(mode: EntryMode) -> Self {
        Self {
            stack_size: DEFAULT_STACK_SIZE,
            mode,
            callbacks: Callbacks::default(),
            domain: alloc::current_domain(),
            consts: None,
            unwind_edges: true,
        }
    }
}

/// How an entry ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOutcome {
    /// The thunk returned this word.
    Returned(u64),
    /// The entry panicked: it unwound to its catch point and its thread or
    /// fiber ended (D-699, D-713).
    Panicked(PanicReport),
    /// The program ended with this status while the entry ran (D-709): a
    /// write failed (D-306). The fibers of a runtime are frozen and its
    /// halt has started; the host completes it ([`Runtime::halt`]), drops
    /// no Cheby value of that runtime and keeps the entry's code alive
    /// until the halt returns. An isolated entry's thread is parked for
    /// good: its host keeps (leaks) the code it runs and every value it may
    /// reach.
    Exited(i32),
}

/// A failure of the runtime itself, not of the program.
#[derive(Debug)]
pub enum RuntimeError {
    /// The requested stack is smaller than [`MIN_STACK_SIZE`].
    StackTooSmall {
        /// The requested size.
        requested: usize,
    },
    /// The entry's thread, or a thread of the runtime, could not be
    /// started.
    Spawn(std::io::Error),
    /// The entry ended without reporting an outcome.
    EntryLost,
    /// A `main` thunk returned something other than 0 or 1.
    InvalidStatus(u64),
    /// `CHEBY_THREADS` holds something other than a whole number of at
    /// least 1 (D-658): the value as written. Its message is D-658's line
    /// without `error: `; the host prints it and exits with 1.
    InvalidThreads(String),
    /// This platform has no fiber switch (Windows until M6, D-714): the
    /// host runs its entries on the thread backend instead.
    Unsupported,
    /// No stack slot could be taken for a root fiber.
    Stack(StackError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackTooSmall { requested } => {
                write!(
                    f,
                    "an entry stack of {requested} bytes is smaller than the minimum of {MIN_STACK_SIZE}"
                )
            }
            Self::Spawn(error) => write!(f, "cannot start the entry's thread: {error}"),
            Self::EntryLost => f.write_str("the entry's thread ended without an outcome"),
            Self::InvalidStatus(status) => {
                write!(f, "the entry returned the invalid status {status}")
            }
            Self::InvalidThreads(value) => write!(
                f,
                "{} must be a whole number of at least 1, not \"{value}\"",
                crate::sched::THREADS_ENV
            ),
            Self::Unsupported => f.write_str("fibers are not supported on this platform (D-714)"),
            Self::Stack(error) => write!(f, "cannot take a stack for the entry: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Stack(error) => Some(error),
            Self::StackTooSmall { .. }
            | Self::EntryLost
            | Self::InvalidStatus(_)
            | Self::InvalidThreads(_)
            | Self::Unsupported => None,
        }
    }
}

/// The control block of the fiber running on this thread, or `None`
/// outside any entry (D-685, D-686). The block stays alive while its fiber
/// runs; do not keep the reference past the entry, or across a fiber
/// switch.
pub(crate) fn current_fiber<'a>() -> Option<&'a ExecCtx> {
    // SAFETY: `tls`'s current fiber is null or the control block of the
    // fiber running on this thread: `spawn_entry` makes its heap-allocated
    // block current only while the entry runs on this thread, and frees it
    // only after clearing it, whether the entry returned or unwound; a
    // worker makes a fiber current while it runs it, and retires the
    // fiber only after clearing it. The caller runs on this thread, inside
    // the entry, and only shared references to the block are ever made.
    unsafe { tls::current_fiber().as_ref() }
}

/// The execution context of the entry running on this thread, or `None`
/// outside any entry.
#[must_use]
pub fn current_ctx() -> Option<*mut ExecCtx> {
    let ctx = tls::current_fiber();
    (!ctx.is_null()).then_some(ctx)
}

/// The hash seed new maps and sets take (D-538): the running constant's
/// seed during constant evaluation, the per-process seed otherwise.
#[must_use]
pub fn current_seed() -> u64 {
    current_fiber().map_or_else(generic::process_seed, ExecCtx::seed)
}

/// The trampolines of the entry running on this thread, or `None` outside
/// any entry.
#[must_use]
pub fn callbacks() -> Option<Callbacks> {
    current_fiber().map(ExecCtx::callbacks)
}

/// The constants of the build whose constant the running entry evaluates
/// (D-535), or `None` outside constant evaluation. The table stays alive
/// while the entry runs; do not keep the reference past the entry.
pub(crate) fn current_consts<'a>() -> Option<&'a ConstTable> {
    current_fiber()?.consts()
}

/// Runs `f` with `seed` as the entry's hash seed, then restores the
/// previous one: lazy forcing of a constant evaluates it under its own
/// seed (plan §7.2). Outside any entry, `f` just runs.
pub fn with_seed<R>(seed: u64, f: impl FnOnce() -> R) -> R {
    let Some(ctx) = current_fiber() else {
        return f();
    };
    let saved = ctx.replace_seed(seed);
    let result = f();
    ctx.replace_seed(saved);
    result
}

/// The memory limit of constant evaluation (D-536) for the running fiber;
/// see [`check_budget_of`].
#[inline]
pub(crate) fn check_budget(bytes: u64) {
    check_budget_of(current_fiber(), bytes);
}

/// The memory limit of constant evaluation (D-536): called before every
/// allocation of `bytes` bytes by the fiber `ctx`, so a request over the
/// budget never reaches the system allocator. When the fiber has a budget
/// and its domain's live bytes, plus the scratch buffers the fiber holds
/// ([`alloc::scratch`]), plus `bytes` would exceed it, the entry is
/// abandoned with [`Limit::Memory`]. Outside constant evaluation it reads
/// one word of the fiber's control block; host code has no budget.
#[inline]
pub(crate) fn check_budget_of(ctx: Option<&ExecCtx>, bytes: u64) {
    let Some(ctx) = ctx else {
        return;
    };
    let budget = ctx.budget();
    if budget == u64::MAX {
        return;
    }
    let scratch = ctx.scratch().0.load(std::sync::atomic::Ordering::Relaxed);
    let live = u64::try_from(ctx.domain().live_bytes())
        .unwrap_or(0)
        .saturating_add(scratch);
    if live.saturating_add(bytes) > budget {
        // No further check while the entry is abandoned.
        ctx.set_budget(u64::MAX);
        consteval::limit_exceeded(Limit::Memory);
    }
}

/// The address of a running entry's context, for the host's watchdog: set
/// once the context exists and cleared, under the same lock, before it is
/// dropped, whether the entry returned or unwound (D-713).
#[derive(Debug, Default)]
struct Interrupter {
    ctx: Mutex<usize>,
}

impl Interrupter {
    fn set(&self, ctx: *const ExecCtx) {
        *self.ctx.lock().unwrap_or_else(PoisonError::into_inner) = ctx.expose_provenance();
    }

    fn clear(&self) {
        *self.ctx.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    }

    /// Asks the entry to enter the runtime at its next function entry with
    /// `reason` (D-688), if it still runs.
    fn interrupt(&self, reason: u32) {
        let guard = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        let ctx: *const ExecCtx = std::ptr::with_exposed_provenance(*guard);
        // SAFETY: a non-zero address is a live context: the entry's thread
        // clears it under this lock before the context is dropped. Only
        // the atomic reason
        // word and stack limit are written, which any thread may do.
        if let Some(ctx) = unsafe { ctx.as_ref() } {
            crate::ctx::request(ctx, reason);
        }
        drop(guard);
    }
}

/// Runs one entry thunk on its own stack with a fresh [`ExecCtx`], and
/// returns when it returns or panics. The building block of `run`, `test`,
/// the REPL and constant evaluation.
///
/// The entry's thread charges `config.domain`, and records the entry for
/// kernels: its context, its seed (the constant's under
/// [`EntryMode::ConstEval`], the per-process seed otherwise) and
/// `config.callbacks`.
///
/// # Errors
///
/// [`RuntimeError`] if the stack is too small or the entry's thread cannot
/// run.
///
/// # Safety
///
/// - `entry` must be an entry thunk: a function with exactly the
///   [`EntryFn`] ABI that follows this runtime's conventions (it uses
///   `ctx` only as an [`ExecCtx`], and passes only valid words to the
///   runtime's symbols).
/// - The code and data `entry` reaches, such as the JIT memory it was
///   compiled into, must stay valid until this function returns. The
///   entry's thread has ended by then, whether the entry returned or
///   unwound (D-713), so nothing is required after the return.
/// - The trampolines in `config.callbacks` must have their declared ABI
///   and stay valid as long as `entry`'s code.
pub unsafe fn run_entry(
    entry: EntryFn,
    config: &EntryConfig,
) -> Result<EntryOutcome, RuntimeError> {
    let body = move |ctx| {
        // SAFETY: the caller guarantees that `entry` is an entry thunk with
        // the `EntryFn` ABI whose code stays valid until `run_entry`
        // returns, which is after this call returns (the host waits for
        // the outcome); `ctx` is the entry's fresh context.
        unsafe { entry(ctx) }
    };
    // SAFETY: as above, which is all `spawn_entry` needs of the body.
    unsafe { spawn_entry(config, body) }
}

/// Runs a host task on an entry of its own, as [`run_entry`] runs a
/// thunk: with a fresh context, `config`'s domain, seed, constants and
/// trampolines recorded for kernels, its limits enforced, and panics
/// reported as an [`EntryOutcome::Panicked`]. Constant evaluation runs its
/// rebuild of maps and sets this way (D-539), since hashing a key may call
/// a type's `@std_hooks` closure through a trampoline.
///
/// # Errors
///
/// As for [`run_entry`].
///
/// # Safety
///
/// - The trampolines in `config.callbacks`, and whatever code `task`
///   reaches through them or through the words it handles, must stay
///   valid until this function returns.
/// - `task` follows the runtime's conventions for the words it handles.
pub unsafe fn run_task(
    config: &EntryConfig,
    task: impl FnOnce() -> u64 + Send + 'static,
) -> Result<EntryOutcome, RuntimeError> {
    // SAFETY: forwarded from the caller.
    unsafe { spawn_entry(config, move |_| task()) }
}

/// The body of [`run_entry`] and [`run_task`].
///
/// # Safety
///
/// `body` may run code that must stay valid until this returns; see
/// [`run_entry`].
unsafe fn spawn_entry(
    config: &EntryConfig,
    body: impl FnOnce(*mut ExecCtx) -> u64 + Send + 'static,
) -> Result<EntryOutcome, RuntimeError> {
    let stack_size = config.stack_size;
    if stack_size < MIN_STACK_SIZE {
        return Err(RuntimeError::StackTooSmall {
            requested: stack_size,
        });
    }
    let mode = config.mode.clone();
    let callbacks = config.callbacks;
    let unwind_edges = config.unwind_edges;
    let domain = Arc::clone(&config.domain);
    let consts = config.consts.clone();
    let (deadline, memory_limit) = match &mode {
        EntryMode::ConstEval(limits) => (limits.deadline, limits.memory),
        EntryMode::Run | EntryMode::Test | EntryMode::Repl => (None, None),
    };
    let interrupter = Arc::new(Interrupter::default());
    let entry_interrupter = Arc::clone(&interrupter);
    let (sender, receiver) = mpsc::channel();
    let started = Instant::now();
    let stats_domain = Arc::clone(&config.domain);
    // Memory for ending in order an entry that the system refuses memory.
    alloc::ensure_reserve();
    let thread = std::thread::Builder::new()
        .name("cheby-entry".to_owned())
        .stack_size(stack_size)
        .spawn(move || {
            // The first frame is within a few KiB of the top of the stack;
            // the headroom covers that distance.
            let top = stack_position();
            let limit = top
                .saturating_sub(stack_size)
                .saturating_add(STACK_HEADROOM);
            let seed = match &mode {
                EntryMode::ConstEval(limits) => limits.seed,
                EntryMode::Run | EntryMode::Test | EntryMode::Repl => generic::process_seed(),
            };
            // Host code on this thread, outside the entry, charges the
            // entry's domain too.
            alloc::set_current_domain(Arc::clone(&domain));
            // D-617: the limit counts what this
            // evaluation adds to the domain, not what was live before it.
            let budget = memory_limit.map_or(u64::MAX, |limit| {
                u64::try_from(domain.live_bytes())
                    .unwrap_or(0)
                    .saturating_add(limit)
                    .min(u64::MAX - 1)
            });
            // An isolated entry's control block lives on the heap (D-685).
            let ctx = Box::into_raw(Box::new(ExecCtx::new(FiberInit {
                soft_limit: limit,
                hard_limit: limit,
                stack_top: top,
                entry_mode: mode,
                outcome: Some(sender.clone()),
                seed,
                budget,
                domain,
                consts,
                callbacks,
                unwind_edges,
                sched: FiberSched::isolated(),
            })));
            entry_interrupter.set(ctx.cast_const());
            // SAFETY: `ctx` is a live control block, freed below only after
            // it stops being current, whether the entry returned or
            // unwound.
            unsafe { tls::set_current_fiber(ctx) };
            // The caller of `spawn_entry` keeps the code `body` runs valid
            // until the host stops waiting, which is after this call
            // returns.
            let word = catch_unwinding(|| body(ctx));
            // The entry's catch point (D-699, D-701): a panic unwound to
            // here, and the record says so, not the word (a `main` thunk's
            // 0 is success).
            // SAFETY: `ctx` is the live block made above.
            let outcome = match unwind::record::finish_entry(unsafe { &*ctx }) {
                Some(report) => EntryOutcome::Panicked(report),
                None => EntryOutcome::Returned(word),
            };
            entry_interrupter.clear();
            // SAFETY: null is always allowed.
            unsafe { tls::set_current_fiber(std::ptr::null_mut()) };
            // SAFETY: allocated above by `Box::into_raw`; no longer current,
            // and the watchdog no longer reaches it (cleared under its
            // lock). Dropping it releases the entry's constants and domain
            // before the host hears of the outcome.
            drop(unsafe { Box::from_raw(ctx) });
            sender.send(outcome).ok();
        })
        .map_err(RuntimeError::Spawn)?;
    // A panicked entry unwound to its catch point, and its thread ends like
    // a returned one's (D-713). A terminated entry's thread is parked for
    // good (D-709): it is never joined, and keeps its control block and
    // everything it holds.
    let result = match wait(&receiver, deadline, &interrupter) {
        Ok(outcome @ EntryOutcome::Exited(_)) => {
            drop(thread);
            Ok(outcome)
        }
        Ok(outcome) => {
            thread.join().map_err(|_| RuntimeError::EntryLost)?;
            Ok(outcome)
        }
        Err(_) => {
            thread.join().ok();
            Err(RuntimeError::EntryLost)
        }
    };
    stats_domain.stats().add_entry(started.elapsed());
    result
}

/// Runs an entry's body and returns its word, or 0 when a Cheby unwinding
/// leaves it: a host task's Rust body ([`run_task`]) raises with no guard
/// above it, and the unwinding ends here, at the entry's catch point,
/// where the fiber's record tells the host (D-699). Any other Rust panic
/// goes on as before.
fn catch_unwinding(body: impl FnOnce() -> u64) -> u64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(word) => word,
        Err(payload) if payload.is::<unwind::raise::ChebyUnwind>() => 0,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// How often the watchdog repeats its interrupt after the deadline, in
/// case the entry was inside a kernel the first time.
const REINTERRUPT: Duration = Duration::from_millis(10);

/// Waits for an entry's outcome. With a deadline the host is the
/// watchdog of D-536: once the deadline passes it interrupts the entry,
/// whose next function entry or tail call ends it with the time limit,
/// and repeats the request until the entry ends.
fn wait(
    receiver: &mpsc::Receiver<EntryOutcome>,
    deadline: Option<Instant>,
    interrupter: &Interrupter,
) -> Result<EntryOutcome, mpsc::RecvError> {
    let Some(deadline) = deadline else {
        return receiver.recv();
    };
    loop {
        let now = Instant::now();
        let timeout = if now >= deadline {
            interrupter.interrupt(crate::ctx::reason::DEADLINE);
            REINTERRUPT
        } else {
            deadline.saturating_duration_since(now)
        };
        match receiver.recv_timeout(timeout) {
            Ok(outcome) => return Ok(outcome),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(mpsc::RecvError),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// What `run_main` does with the fibers left when `main` has returned
/// (D-073, D-708).
///
/// GAP(freeze-only-cli): at a normal exit only `cheby run`, right before
/// the process exits, asks for [`AtExit::Freeze`], and only in a build
/// without the debug leak check; every in-process host tears down (D-708).
/// A termination (D-709) halts in every host whatever this says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AtExit {
    /// Tear the remaining fibers down: unwind them without running user
    /// code, release what they hold, then stop the runtime (D-708, D-710).
    /// Every in-process host asks for this.
    #[default]
    Teardown,
    /// Freeze the remaining fibers where they stand and never reuse their
    /// stacks, because the process exits next (D-073, D-708).
    Freeze,
}

/// How `cheby run` runs `main`.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The program's arguments, without the program name (D-347).
    pub args: Vec<String>,
    /// The program name, `env::program_name()` (D-347).
    pub program_name: String,
    /// The root fiber's stack (D-125): the manifest's `main-stack`
    /// (D-660), [`DEFAULT_ROOT_STACK_SIZE`] by default.
    pub root_stack_size: usize,
    /// The stack of every fiber spawned without a size of its own: the
    /// manifest's `fiber-stack` (D-660), [`DEFAULT_STACK_SIZE`] by default.
    pub fiber_stack: usize,
    /// What happens to the fibers left when `main` has returned (D-073,
    /// D-708); [`AtExit::Teardown`] by default.
    pub at_exit: AtExit,
    /// The trampolines `main`'s kernels may call closures through (D-526).
    pub callbacks: Callbacks,
    /// The build's accounting domain, which the leak check reads.
    pub domain: Arc<Domain>,
    /// The number of worker threads of the run's runtime; `None` reads
    /// `CHEBY_THREADS` (D-658). In-process hosts may bound it.
    pub threads: Option<usize>,
    /// Whether `main`'s code was compiled with unwind edges: false only
    /// under `CHEBY_UNWIND_EDGES=off` (D-742; see
    /// [`EntryConfig::unwind_edges`]). The JIT fills it from its code
    /// generation options.
    pub unwind_edges: bool,
    /// The token of the code `main` runs (a JIT's, [`CodeToken`]), which
    /// the run's runtime holds as [`Runtime::attach`] does: released once
    /// every thread of the runtime is joined, and kept (leaked) when a
    /// worker whose fiber is blocked inline in a system call is never
    /// joined (D-786), since that worker returns into the code when the
    /// call returns (D-708, I-6). `None` for code that lives for the
    /// whole process.
    pub code: Option<CodeToken>,
}

impl Default for RunConfig {
    /// No arguments, the default root and fiber stacks, no trampolines,
    /// the calling thread's current accounting domain, `CHEBY_THREADS`'s
    /// thread count, unwind edges, a teardown at exit and no code token.
    fn default() -> Self {
        Self {
            args: Vec::new(),
            program_name: String::new(),
            root_stack_size: DEFAULT_ROOT_STACK_SIZE,
            fiber_stack: DEFAULT_STACK_SIZE,
            at_exit: AtExit::Teardown,
            callbacks: Callbacks::default(),
            domain: alloc::current_domain(),
            threads: None,
            unwind_edges: true,
            code: None,
        }
    }
}

/// Runs `main`'s entry thunk and returns the exit status (§10.10, D-182):
///
/// - The thunk's 0 or 1 (`main` returned `Nil`/`Ok(Nil)`, or `Err(e)`
///   after printing `e`), after flushing standard output. A failed flush
///   gives 1 (D-306, D-346).
/// - 101 after a panic, which has already flushed and printed. The debug
///   leak check also runs after a panic, and a leak gives 102 instead
///   (D-710).
/// - In debug builds, 102 if objects of the build's domain are still live
///   when the status is 0 or 1 (D-458), unless the system allocator
///   refused a request of the domain, which skips the check and counts the
///   skip (D-771). Constants frozen before `main` have left the domain,
///   so every live object is a leak.
///
/// `main` runs as the root fiber of a [`Runtime`] made for the run, with
/// the program's arguments, `config.threads` workers (D-690) and
/// `config.fiber_stack` as the default stack of the fibers it spawns
/// (D-660). Once `main` has ended, standard output is flushed and the
/// status fixed, then the runtime stops as `config.at_exit` asks (D-073,
/// D-708), then the stats report, then the leak check (§10.10's note).
/// Where fibers are not supported (Windows until M6, D-714), `main` runs
/// on the thread backend instead.
///
/// With `CHEBY_RT_STATS=1` it first prints `main`'s wall time and the
/// domain's counters to standard error ([`stats::report`]).
///
/// # Errors
///
/// [`RuntimeError`] if the runtime cannot start (an invalid
/// `CHEBY_THREADS` among others, D-658), or the entry cannot run or
/// returns an invalid status.
///
/// # Safety
///
/// As for [`run_entry`]: `entry` must be a `main` entry thunk with the
/// [`EntryFn`] ABI, and its code must stay valid until this returns. Code
/// that can be freed (a JIT's) must also come with its token in
/// `config.code`: a worker left blocked inline in a system call (D-786) is
/// never joined and returns into the code after this has returned.
pub unsafe fn run_main(entry: EntryFn, config: RunConfig) -> Result<i32, RuntimeError> {
    let runtime = match Runtime::new(&RuntimeConfig {
        threads: config.threads,
        args: config.args.clone(),
        program_name: config.program_name.clone(),
        fiber_stack: config.fiber_stack,
    }) {
        Ok(runtime) => Some(runtime),
        Err(RuntimeError::Unsupported) => {
            set_program(config.args, config.program_name);
            None
        }
        Err(error) => return Err(error),
    };
    // The runtime holds the code's token from now on, and keeps it if a
    // worker is left blocked inline in a system call (D-786, I-6); on the
    // thread backend this function holds it.
    let code = match (&runtime, config.code) {
        (Some(runtime), Some(code)) => {
            runtime.attach(code);
            None
        }
        (_, code) => code,
    };
    let domain = config.domain;
    let entry_config = EntryConfig {
        stack_size: config.root_stack_size,
        mode: EntryMode::Run,
        callbacks: config.callbacks,
        domain: Arc::clone(&domain),
        consts: None,
        unwind_edges: config.unwind_edges,
    };
    // `main`'s detached fibers (GAP(detached-groups)).
    let group = DetachedGroup::new();
    let started = Instant::now();
    let outcome = match &runtime {
        // SAFETY: the caller guarantees `run_root`'s preconditions for
        // `entry`: its code stays valid until this function returns, which
        // is after the runtime stopped below.
        Some(runtime) => unsafe { runtime.run_root_in(entry, &entry_config, &group) },
        // SAFETY: as above, for `run_entry`.
        None => unsafe { run_entry(entry, &entry_config) },
    };
    // A termination (D-709): the program ends with its status at once.
    // The world is halted (the thread backend's entry thread is parked
    // for good), nothing more is printed, and there are no stats and no
    // leak check: frozen fibers hold objects (D-306).
    if let Ok(EntryOutcome::Exited(status)) = outcome {
        if let Some(runtime) = runtime {
            runtime.halt();
        }
        // The thread backend's entry thread is never joined: its code is
        // kept (leaked) for it.
        std::mem::forget(code);
        return Ok(status);
    }
    // The exit sequence (§10.10's note): flush standard output and fix the
    // status, then stop the runtime, then the stats report, then the leak
    // check. The root's catch point already printed a panic report (D-701).
    let fixed = outcome.and_then(|outcome| {
        let flushed = io::flush_stdout().is_ok();
        exit_status(&outcome, flushed).map(|status| (outcome, status, flushed))
    });
    let (outcome, status, flushed) = match fixed {
        Ok(fixed) => fixed,
        Err(error) => {
            if let Some(runtime) = runtime {
                runtime.shutdown();
            }
            return Err(error);
        }
    };
    let (status, stopped) = match runtime {
        Some(runtime) => stop_at_exit(runtime, &group, status, config.at_exit),
        None => (status, Stopped::Clean),
    };
    // A termination once `main` had ended (D-785): the fixed status
    // stands, the halt has run, and nothing more is printed.
    if stopped == Stopped::Terminated {
        return Ok(status);
    }
    // D-771: after the system allocator refused a request of the build's
    // domain, the update it stopped may have leaked the structure in
    // flight, which the program did not leak; D-786: a fiber blocked
    // inline in a system call was skipped by the teardown and holds its
    // objects; frozen fibers hold theirs (D-708). In each case the check
    // is skipped and the skip counted (D-710).
    let skip = domain.system_refused() || stopped != Stopped::Clean;
    let check_leaks = !(LEAK_CHECK_ENABLED && skip);
    if !check_leaks {
        domain.stats().skip_leak_check();
    }
    // The host's own writes: a failure ends the program with 1, printing
    // nothing more (D-306).
    if stats::enabled()
        && io::try_write_stderr(&stats::report(
            started.elapsed(),
            domain.peak_bytes(),
            domain.stats(),
        ))
        .is_err()
    {
        return Ok(status::ERR);
    }
    // A failed flush gives 1 and skips the check (D-306, D-346); after a
    // panic the cleanup pads released the root's objects, and a leak turns
    // 101 into 102 (D-710).
    let checked = flushed || matches!(outcome, EntryOutcome::Panicked(_));
    let live = domain.live_objects();
    if checked
        && check_leaks
        && let Some(mut report) = leak_report(live)
    {
        // D-711, D-784: the leaked runtime kinds, right after D-458's line.
        if let Some(kinds) = alloc::leaked_kinds_line(domain.live_kinds(), live) {
            report.push_str(&kinds);
            report.push('\n');
        }
        if io::try_write_stderr(&report).is_err() {
            return Ok(status::ERR);
        }
        return Ok(status::LEAK_CHECK);
    }
    Ok(status)
}

/// How `run_main`'s runtime stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    /// Torn down, every thread joined: the leak check may run.
    Clean,
    /// Torn down, except fibers blocked inline in a system call (D-786).
    Skipped,
    /// Frozen (D-073, D-708).
    Frozen,
    /// A termination happened (D-709) and the runtime halted.
    Terminated,
}

/// The exit status of `main`'s `outcome`, fixed right after standard
/// output was flushed (`flushed`): 101 after a panic, 1 after a failed
/// flush (D-306, D-346), else the thunk's 0 or 1 (§10.10, D-182).
///
/// # Errors
///
/// [`RuntimeError::InvalidStatus`] for any other raw status.
fn exit_status(outcome: &EntryOutcome, flushed: bool) -> Result<i32, RuntimeError> {
    match *outcome {
        EntryOutcome::Exited(status) => Ok(status),
        EntryOutcome::Panicked(_) => Ok(status::PANIC),
        EntryOutcome::Returned(_) if !flushed => Ok(status::ERR),
        EntryOutcome::Returned(0) => Ok(status::SUCCESS),
        EntryOutcome::Returned(1) => Ok(status::ERR),
        EntryOutcome::Returned(other) => Err(RuntimeError::InvalidStatus(other)),
    }
}

/// Stops `run_main`'s runtime once standard output is flushed and the
/// status fixed (`status`), as `at_exit` asks (D-073, D-708,
/// GAP(freeze-only-cli)), and returns the status that stands with how the
/// runtime stopped:
///
/// - the status is fixed in the runtime first, so a termination from now
///   on leaves it (D-785), and one that came first wins (D-709);
/// - [`AtExit::Teardown`] tears `main`'s group down (D-708, D-710), then
///   shuts the runtime down; a termination during the teardown stops it,
///   and the halt completes instead;
/// - [`AtExit::Freeze`] halts: the detached fibers freeze where they stand.
fn stop_at_exit(
    runtime: Runtime,
    group: &DetachedGroup,
    status: i32,
    at_exit: AtExit,
) -> (i32, Stopped) {
    let shared = runtime.shared_handle();
    let status = shared.fix_status(status);
    if shared.terminated() {
        runtime.halt();
        return (status, Stopped::Terminated);
    }
    let stopped = match at_exit {
        AtExit::Freeze => {
            runtime.halt();
            Stopped::Frozen
        }
        AtExit::Teardown => {
            let report = runtime.teardown(group);
            if report.halted {
                runtime.halt();
            } else {
                runtime.shutdown();
            }
            if report.skipped > 0 {
                Stopped::Skipped
            } else {
                Stopped::Clean
            }
        }
    };
    if shared.terminated() {
        return (status, Stopped::Terminated);
    }
    (status, stopped)
}

/// The leak-check failure printed when `live` objects remain at exit, or
/// `None` if there are none or this build does not count them.
#[must_use]
pub fn leak_report(live: i64) -> Option<String> {
    match live {
        _ if !LEAK_CHECK_ENABLED => None,
        0 => None,
        1 => Some("error: leak check failed: 1 object is still live at exit\n".to_owned()),
        n => Some(format!(
            "error: leak check failed: {n} objects are still live at exit\n"
        )),
    }
}

#[derive(Debug)]
struct Program {
    args: Vec<String>,
    name: String,
}

static PROGRAM: RwLock<Program> = RwLock::new(Program {
    args: Vec::new(),
    name: String::new(),
});

/// Sets the arguments, without the program name, and the name (D-347)
/// that `std::env` reads in isolated entries, which run outside any
/// runtime: the REPL's, and every entry of the thread backend (D-714). A
/// runtime keeps its own (D-690, [`RuntimeConfig`]). An empty name means
/// "not set": `env::program_name` then gives the process's first argument.
pub fn set_program(args: Vec<String>, name: String) {
    *PROGRAM.write().unwrap_or_else(PoisonError::into_inner) = Program { args, name };
}

/// The running program's name, or `None` when no host has set one.
#[must_use]
pub fn program_name_if_set() -> Option<String> {
    Some(program_name()).filter(|name| !name.is_empty())
}

/// The running program's arguments, without the program name (D-347),
/// read by `std::env`: the current fiber's runtime's (D-690), or the
/// isolated entries' record ([`set_program`]).
#[must_use]
pub fn program_args() -> Vec<String> {
    if let Some(runtime) = current_fiber().and_then(|ctx| ctx.sched.runtime()) {
        return runtime.args().to_vec();
    }
    PROGRAM
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .args
        .clone()
}

/// The running program's name (D-347), as [`program_args`] finds it.
#[must_use]
pub fn program_name() -> String {
    if let Some(runtime) = current_fiber().and_then(|ctx| ctx.sched.runtime()) {
        return runtime.program_name().to_owned();
    }
    PROGRAM
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .name
        .clone()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::abi::{self, PanicKind};
    use crate::alloc::tests::{serial, tuple};
    use crate::panic::{cheby_rt_panic, cheby_rt_stack_slow};
    use crate::rc;
    use crate::string::tests::StaticImage;

    /// [`super::run_entry`] for this module's thunks.
    fn run_entry(entry: EntryFn, config: &EntryConfig) -> Result<EntryOutcome, RuntimeError> {
        // SAFETY: every thunk passed here is one of this module's
        // `unsafe extern "C"` functions: it has the `EntryFn` ABI, follows
        // the runtime's conventions, and its code is static.
        unsafe { super::run_entry(entry, config) }
    }

    /// [`super::run_main`] for this module's thunks.
    fn run_main(entry: EntryFn, config: RunConfig) -> Result<i32, RuntimeError> {
        // SAFETY: as in `run_entry` above.
        unsafe { super::run_main(entry, config) }
    }

    fn const_eval() -> EntryConfig {
        EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits::default()))
    }

    unsafe extern "C" fn returns_seven(_: *mut ExecCtx) -> u64 {
        7
    }

    #[test]
    fn returned_values_reach_the_host() {
        let outcome = run_entry(returns_seven, &EntryConfig::new(EntryMode::Test)).unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(7));
    }

    #[test]
    fn small_stacks_are_refused() {
        let config = EntryConfig {
            stack_size: 4096,
            ..EntryConfig::new(EntryMode::Test)
        };
        assert!(matches!(
            run_entry(returns_seven, &config),
            Err(RuntimeError::StackTooSmall { .. })
        ));
    }

    unsafe extern "C" fn panics_with_a_message(ctx: *mut ExecCtx) -> u64 {
        thread_local! {
            static MESSAGE: StaticImage = StaticImage::new("computation failed");
            static LOCATION: StaticImage = StaticImage::new("src/main.cheby:4:7");
        }
        let message = MESSAGE.with(StaticImage::word);
        let location = LOCATION.with(StaticImage::word);
        // SAFETY: `ctx` is the running entry's; the strings live as long
        // as this thread, and the report copies them.
        unsafe { cheby_rt_panic(ctx, PanicKind::Explicit.code(), message, location) };
        // The panic is recorded and the flag set (D-733): return at once.
        0
    }

    #[test]
    fn a_panic_reaches_the_host_and_the_process_keeps_running() {
        let outcome = run_entry(panics_with_a_message, &const_eval()).unwrap();
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("expected a panic, got {outcome:?}");
        };
        assert_eq!(report.kind, PanicKind::Explicit);
        assert_eq!(report.message, "computation failed");
        assert_eq!(report.location, "src/main.cheby:4:7");
        // The host goes on to run the next entry.
        let outcome = run_entry(returns_seven, &const_eval()).unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(7));
    }

    unsafe extern "C" fn panics_without_a_message(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: `ctx` is the running entry's; no message or location.
        unsafe { cheby_rt_panic(ctx, PanicKind::DivisionByZero.code(), 0, 0) };
        // The panic is recorded and the flag set (D-733): return at once.
        0
    }

    #[test]
    fn panics_without_message_use_the_default() {
        let outcome = run_entry(panics_without_a_message, &const_eval()).unwrap();
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("expected a panic, got {outcome:?}");
        };
        assert_eq!(report.message, "division by zero");
        assert_eq!(report.location, crate::panic::UNKNOWN_LOCATION);
    }

    /// The function-entry check generated code performs; returns whether
    /// the slow path raised, after which generated code would run its
    /// cleanup pad and return (D-733).
    unsafe fn entry_check(ctx: *mut ExecCtx) -> bool {
        // SAFETY: `ctx` is the running entry's.
        let limit = unsafe { &*ctx }.stack_limit.load(Ordering::Relaxed);
        if stack_position() < limit {
            // SAFETY: as above.
            unsafe { cheby_rt_stack_slow(ctx, 0) };
        }
        // SAFETY: as above.
        crate::unwind::flag::get(unsafe { &*ctx }) != abi::unwind_flag::NONE
    }

    /// Runs the Rust body of a test thunk in a guard, as a kernel's entry
    /// point does (D-697): a raise inside it sets the flag and returns 0.
    fn guarded(ctx: *mut ExecCtx, body: impl FnOnce() -> u64) -> u64 {
        crate::unwind::guard::guarded(Some(ctx), body)
    }

    unsafe extern "C" fn interrupted(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: `ctx` is the running entry's.
        let context = unsafe { &*ctx };
        context.request_interrupt();
        // SAFETY: as above.
        let raised = unsafe { entry_check(ctx) };
        let restored = context.stack_limit.load(Ordering::Relaxed) == context.real_stack_limit();
        // A second check passes on the fast path.
        // SAFETY: as above.
        let raised = raised || unsafe { entry_check(ctx) };
        if raised {
            return 0;
        }
        context
            .interrupts()
            .wrapping_mul(10)
            .wrapping_add(u64::from(restored))
    }

    #[test]
    #[cfg_attr(miri, ignore = "compares real stack addresses")]
    fn an_interrupt_reaches_the_slow_path_and_continues() {
        let outcome = run_entry(interrupted, &EntryConfig::new(EntryMode::Test)).unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(11));
    }

    /// The interrupts [`large_frames`] saw handled before it overflowed.
    static LARGE_FRAME_INTERRUPTS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn large_frames(ctx: *mut ExecCtx) -> u64 {
        use crate::ctx::{reason, request};
        use crate::panic::{LARGE_FRAME_MARGIN, cheby_rt_stack_slow_frame};
        // SAFETY: `ctx` is the running entry's.
        let context = unsafe { &*ctx };
        // Every reason is taken; outside constant evaluation none has an
        // effect here, and the frame fits: CANCEL without a target is
        // stale, since a cancellation is delivered from the fiber's target
        // (D-688, `fiber_ops::cancel::check`), and this entry has none.
        for reason in [
            reason::SAFEPOINT,
            reason::CANCEL,
            reason::DEADLINE,
            reason::YIELD,
        ] {
            request(context, reason);
            // SAFETY: as above; no location.
            unsafe { cheby_rt_stack_slow_frame(ctx, 0, 1 << 10) };
        }
        LARGE_FRAME_INTERRUPTS.store(context.interrupts(), Ordering::SeqCst);
        let room = stack_position().saturating_sub(context.real_stack_limit());
        let room = u64::try_from(room).unwrap();
        let fits = room.saturating_sub(LARGE_FRAME_MARGIN + (16 << 10));
        // SAFETY: as above.
        unsafe { cheby_rt_stack_slow_frame(ctx, 0, fits) };
        LARGE_FRAME_INTERRUPTS.store(context.interrupts().wrapping_add(100), Ordering::SeqCst);
        // A frame that leaves less than 64 KiB above the limit overflows.
        let overflows = room.saturating_sub(LARGE_FRAME_MARGIN);
        // SAFETY: as above.
        unsafe { cheby_rt_stack_slow_frame(ctx, 0, overflows) };
        0
    }

    /// D-679, D-688: the slow path of a large function's entry check
    /// handles interrupts like the ordinary one, and overflows when the
    /// frame plus 64 KiB does not fit above the real limit.
    #[test]
    #[cfg_attr(miri, ignore = "compares real stack addresses")]
    fn large_frames_overflow_when_frame_and_margin_do_not_fit() {
        let outcome = run_entry(large_frames, &EntryConfig::new(EntryMode::Test)).unwrap();
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("expected a stack overflow, got {outcome:?}");
        };
        assert_eq!(report.kind, PanicKind::StackOverflow);
        assert_eq!(LARGE_FRAME_INTERRUPTS.load(Ordering::SeqCst), 104);
    }

    fn recurse(ctx: *mut ExecCtx, depth: u64) -> u64 {
        // SAFETY: `ctx` is the running entry's.
        if unsafe { entry_check(ctx) } {
            return 0;
        }
        let frame = std::hint::black_box([depth; 64]);
        if depth == u64::MAX {
            return depth;
        }
        let below = recurse(ctx, depth.wrapping_add(1));
        frame.iter().fold(below, |sum, n| sum.wrapping_add(*n))
    }

    unsafe extern "C" fn overflows(ctx: *mut ExecCtx) -> u64 {
        recurse(ctx, 0)
    }

    #[test]
    #[cfg_attr(miri, ignore = "compares real stack addresses")]
    fn deep_recursion_panics_with_stack_overflow() {
        let config = EntryConfig {
            stack_size: MIN_STACK_SIZE * 2,
            ..const_eval()
        };
        let outcome = run_entry(overflows, &config).unwrap();
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("expected a panic, got {outcome:?}");
        };
        assert_eq!(report.kind, PanicKind::StackOverflow);
        assert_eq!(report.message, "stack overflow");
    }

    unsafe extern "C" fn allocates_and_frees(_: *mut ExecCtx) -> u64 {
        let word = tuple(&[tuple(&[])]);
        // SAFETY: the only reference.
        unsafe { rc::drop(word) };
        // Through the print kernel, without visible output.
        let empty = StaticImage::new("");
        // SAFETY: a live static string, borrowed for the call.
        unsafe { crate::io::cheby_io_print(empty.word()) };
        0
    }

    unsafe extern "C" fn leaks_one(_: *mut ExecCtx) -> u64 {
        let _ = tuple(&[]);
        1
    }

    unsafe extern "C" fn returns_bad_status(_: *mut ExecCtx) -> u64 {
        2
    }

    /// 0 when the program's arguments are `["a"]` and its name `p`.
    unsafe extern "C" fn reads_its_program(_: *mut ExecCtx) -> u64 {
        u64::from(program_args() != ["a"] || program_name() != "p")
    }

    #[test]
    fn run_main_maps_outcomes_to_statuses() {
        // `run_main` sets the program's arguments, which the other
        // `run_main` test also sets.
        let _guard = serial();
        let domain = Domain::new();
        let config = || RunConfig {
            args: vec!["a".to_owned()],
            program_name: "p".to_owned(),
            domain: Arc::clone(&domain),
            ..RunConfig::default()
        };
        assert_eq!(
            run_main(allocates_and_frees, config()).unwrap(),
            status::SUCCESS
        );
        // D-690: `main` reads the arguments of its runtime.
        assert_eq!(
            run_main(reads_its_program, config()).unwrap(),
            status::SUCCESS
        );
        assert!(matches!(
            run_main(returns_bad_status, config()),
            Err(RuntimeError::InvalidStatus(2))
        ));
        let leaked = run_main(leaks_one, config()).unwrap();
        if LEAK_CHECK_ENABLED {
            assert_eq!(leaked, status::LEAK_CHECK);
        } else {
            assert_eq!(leaked, status::ERR);
        }
        // The leak was charged to the build's domain, not to this thread.
        assert_eq!(domain.live_objects(), 1);
        assert_eq!(alloc::live_objects(), 0);
        assert_eq!(abi::status::PANIC, 101);
    }

    /// 0 when its runtime's default fiber stack is 3 MiB, else 1.
    unsafe extern "C" fn reads_its_fiber_stack(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the root fiber's live control block.
        let me = unsafe { &*ctx };
        let stack = me.sched.runtime().map(|runtime| runtime.fiber_stack());
        // Without fibers (Windows until M6) there is no runtime to read.
        u64::from(stack.is_some_and(|stack| stack != 3 << 20))
    }

    /// D-660: `run_main` gives its runtime `RunConfig.fiber_stack`; D-073,
    /// D-708: both exit choices end a program whose fibers all ended
    /// (package L1r makes them differ); the defaults are 8 MiB and a
    /// teardown.
    #[test]
    fn run_main_passes_its_fiber_stack_and_exit_choice() {
        let _guard = serial();
        let defaults = RunConfig::default();
        assert_eq!(defaults.fiber_stack, DEFAULT_STACK_SIZE);
        assert_eq!(defaults.at_exit, AtExit::Teardown);
        assert_eq!(AtExit::default(), AtExit::Teardown);
        for at_exit in [AtExit::Teardown, AtExit::Freeze] {
            let config = RunConfig {
                fiber_stack: 3 << 20,
                at_exit,
                ..RunConfig::default()
            };
            assert_eq!(
                run_main(reads_its_fiber_stack, config).unwrap(),
                status::SUCCESS,
                "{at_exit:?}"
            );
        }
    }

    /// §10.10, D-306: the status is fixed at the flush: 101 after a panic
    /// whatever the flush did, 1 after a failed flush, else the thunk's.
    #[test]
    fn exit_statuses_are_fixed_at_the_flush() {
        let returned = |raw| EntryOutcome::Returned(raw);
        assert_eq!(super::exit_status(&returned(0), true).unwrap(), 0);
        assert_eq!(super::exit_status(&returned(1), true).unwrap(), 1);
        assert_eq!(super::exit_status(&returned(0), false).unwrap(), 1);
        assert_eq!(super::exit_status(&returned(7), false).unwrap(), 1);
        assert!(matches!(
            super::exit_status(&returned(7), true),
            Err(RuntimeError::InvalidStatus(7))
        ));
    }

    #[test]
    fn run_main_exits_101_after_a_panic() {
        let _guard = serial();
        // Prints the panic report to the test's standard error.
        let status = run_main(panics_with_a_message, RunConfig::default()).unwrap();
        assert_eq!(status, abi::status::PANIC);
    }

    unsafe extern "C" fn allocates_two(_: *mut ExecCtx) -> u64 {
        tuple(&[tuple(&[])])
    }

    #[test]
    fn entries_charge_their_configured_domain() {
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        let EntryOutcome::Returned(word) = run_entry(allocates_two, &config).unwrap() else {
            panic!("the entry panicked");
        };
        assert_eq!(domain.live_objects(), 2);
        assert_eq!(alloc::live_objects(), 0);
        // Freed on this thread with the build's domain current.
        alloc::with_domain(Arc::clone(&domain), || {
            // SAFETY: the entry returned its only reference.
            unsafe { rc::drop(word) };
        });
        assert_eq!(domain.live_objects(), 0);
        // By default an entry charges the calling thread's domain.
        let EntryOutcome::Returned(word) =
            run_entry(allocates_two, &EntryConfig::new(EntryMode::Test)).unwrap()
        else {
            panic!("the entry panicked");
        };
        assert_eq!(alloc::live_objects(), 2);
        // SAFETY: as above.
        unsafe { rc::drop(word) };
        assert_eq!(alloc::live_objects(), 0);
    }

    unsafe extern "C" fn reports_its_record(ctx: *mut ExecCtx) -> u64 {
        let same_ctx = current_ctx() == Some(ctx);
        let outer = current_seed();
        let inner = with_seed(99, current_seed);
        let restored = current_seed() == outer;
        let has_t1 = callbacks().is_some_and(|callbacks| callbacks.t1.is_some());
        u64::from(same_ctx && restored && inner == 99 && has_t1).wrapping_add(outer.wrapping_mul(2))
    }

    unsafe extern "C" fn identity(_: *mut ExecCtx, _: u64, a0: u64) -> u64 {
        a0
    }

    #[test]
    fn entries_record_their_context_seed_and_callbacks() {
        let config = EntryConfig {
            callbacks: Callbacks {
                t1: Some(identity),
                ..Callbacks::default()
            },
            ..EntryConfig::new(EntryMode::ConstEval(ConstEvalLimits {
                seed: 21,
                ..ConstEvalLimits::default()
            }))
        };
        assert_eq!(
            run_entry(reports_its_record, &config).unwrap(),
            EntryOutcome::Returned(43)
        );
        let run = EntryConfig {
            callbacks: config.callbacks,
            ..EntryConfig::new(EntryMode::Run)
        };
        assert_eq!(
            run_entry(reports_its_record, &run).unwrap(),
            EntryOutcome::Returned(generic::process_seed().wrapping_mul(2).wrapping_add(1))
        );
        // Outside any entry.
        assert_eq!(current_ctx(), None);
        assert_eq!(current_seed(), generic::process_seed());
        assert_eq!(with_seed(5, current_seed), generic::process_seed());
        assert!(callbacks().is_none());
    }

    unsafe extern "C" fn spins(ctx: *mut ExecCtx) -> u64 {
        // A loop of calls: every iteration passes a function entry, until
        // one raises.
        // SAFETY: `ctx` is the running entry's.
        while !unsafe { entry_check(ctx) } {
            std::hint::spin_loop();
        }
        0
    }

    fn limited(table: &Arc<ConstTable>, limits: ConstEvalLimits) -> EntryConfig {
        EntryConfig {
            consts: Some(Arc::clone(table)),
            domain: Domain::new(),
            ..EntryConfig::new(EntryMode::ConstEval(limits))
        }
    }

    /// D-536: the host's watchdog interrupts a constant evaluation that
    /// passes its deadline at its next function entry, which ends it with
    /// the time limit.
    #[test]
    fn the_deadline_interrupts_at_the_next_function_entry() {
        let table = ConstTable::new();
        let started = Instant::now();
        let config = limited(
            &table,
            ConstEvalLimits {
                deadline: started.checked_add(Duration::from_millis(50)),
                ..ConstEvalLimits::default()
            },
        );
        let EntryOutcome::Panicked(report) = run_entry(spins, &config).unwrap() else {
            panic!("the loop returned");
        };
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(report.message, Limit::Time.message());
        assert_eq!(table.abandon().limit, Some(Limit::Time));
        // Without a deadline nothing interrupts: an entry that returns is
        // not affected by an earlier one's limit.
        let EntryOutcome::Returned(7) =
            run_entry(returns_seven, &limited(&table, ConstEvalLimits::default())).unwrap()
        else {
            panic!("the entry did not return");
        };
        assert_eq!(table.abandon().limit, None);
    }

    unsafe extern "C" fn grows(ctx: *mut ExecCtx) -> u64 {
        guarded(ctx, || {
            let mut chain = tuple(&[]);
            loop {
                chain = tuple(&[chain]);
            }
        })
    }

    /// D-536: the memory limit counts the live bytes of the entry's domain,
    /// and the allocation that passes it ends the entry.
    #[test]
    fn the_memory_budget_ends_an_entry_that_holds_too_much() {
        let table = ConstTable::new();
        let config = limited(
            &table,
            ConstEvalLimits {
                memory: Some(64 << 10),
                ..ConstEvalLimits::default()
            },
        );
        let EntryOutcome::Panicked(report) = run_entry(grows, &config).unwrap() else {
            panic!("the loop returned");
        };
        assert_eq!(report.message, Limit::Memory.message());
        assert_eq!(table.abandon().limit, Some(Limit::Memory));
        // The allocation that would pass the budget is refused, so the
        // peak stays within one allocation below it.
        let peak = config.domain.peak_bytes();
        assert!(peak > (64 << 10) - 16 && peak <= 64 << 10, "{peak}");
        // The abandoned chain stays live in that domain, but a later
        // evaluation in it counts only what it adds (D-617).
        let again = EntryConfig {
            domain: Arc::clone(&config.domain),
            ..limited(
                &table,
                ConstEvalLimits {
                    memory: Some(64),
                    ..ConstEvalLimits::default()
                },
            )
        };
        let EntryOutcome::Returned(0) = run_entry(allocates_and_frees, &again).unwrap() else {
            panic!("an evaluation after an abandoned one was charged for its objects");
        };
        // In a fresh domain, freeing what is allocated keeps an entry under
        // the budget: only live bytes count.
        let config = limited(
            &table,
            ConstEvalLimits {
                memory: Some(64),
                ..ConstEvalLimits::default()
            },
        );
        let EntryOutcome::Returned(0) = run_entry(allocates_and_frees, &config).unwrap() else {
            panic!("the entry did not return");
        };
    }

    unsafe extern "C" fn allocates_too_much(ctx: *mut ExecCtx) -> u64 {
        guarded(ctx, || {
            let _ = alloc::alloc_object(1 << 50, 0);
            1
        })
    }

    /// D-536: a single request over the budget ends the entry with the
    /// memory limit before anything is allocated, rather than aborting the
    /// process when the system cannot provide it.
    #[test]
    fn a_single_request_over_the_budget_is_refused_before_allocating() {
        let table = ConstTable::new();
        let config = limited(
            &table,
            ConstEvalLimits {
                memory: Some(1 << 30),
                ..ConstEvalLimits::default()
            },
        );
        let EntryOutcome::Panicked(report) = run_entry(allocates_too_much, &config).unwrap() else {
            panic!("the allocation returned");
        };
        assert_eq!(report.message, Limit::Memory.message());
        assert_eq!(table.abandon().limit, Some(Limit::Memory));
        assert_eq!(config.domain.peak_bytes(), 0);
    }

    #[test]
    fn leak_reports_count_objects() {
        if LEAK_CHECK_ENABLED {
            assert_eq!(leak_report(0), None);
            assert_eq!(
                leak_report(1).as_deref(),
                Some("error: leak check failed: 1 object is still live at exit\n")
            );
            assert_eq!(
                leak_report(3).as_deref(),
                Some("error: leak check failed: 3 objects are still live at exit\n")
            );
        } else {
            assert_eq!(leak_report(3), None);
        }
    }

    /// The catch points of entries under the return flag (D-699, D-701,
    /// D-710, D-733), and code compiled without unwind edges
    /// (D-770).
    mod unwind {
        use super::*;

        unsafe extern "C" fn returns_zero(_: *mut ExecCtx) -> u64 {
            0
        }

        unsafe extern "C" fn panics_after_a_leak(ctx: *mut ExecCtx) -> u64 {
            let _ = tuple(&[]);
            // SAFETY: `ctx` is the running entry's; no message or location.
            unsafe { cheby_rt_panic(ctx, PanicKind::Assert.code(), 0, 0) };
            0
        }

        unsafe extern "C" fn panics_cleanly(ctx: *mut ExecCtx) -> u64 {
            // SAFETY: `ctx` is the running entry's; no message or location.
            unsafe { cheby_rt_panic(ctx, PanicKind::Assert.code(), 0, 0) };
            0
        }

        fn entry_config(mode: EntryMode) -> EntryConfig {
            EntryConfig {
                domain: Domain::new(),
                ..EntryConfig::new(mode)
            }
        }

        fn run_config() -> RunConfig {
            RunConfig {
                domain: Domain::new(),
                threads: Some(1),
                ..RunConfig::default()
            }
        }

        #[test]
        fn a_main_returning_status_0_did_not_panic() {
            // `main`'s thunk returns the exit status, and 0 is success: the
            // host asks the record, not the word (plan §2.5).
            let outcome = run_entry(returns_zero, &entry_config(EntryMode::Run)).unwrap();
            assert_eq!(outcome, EntryOutcome::Returned(0));
            let _guard = serial();
            let status = run_main(returns_zero, run_config()).unwrap();
            assert_eq!(status, abi::status::SUCCESS);
        }

        #[test]
        fn a_panic_reaches_the_host_from_the_record() {
            // The thunk returns 0 after its panic, and its thread ends.
            let outcome = run_entry(panics_with_a_message, &entry_config(EntryMode::Test)).unwrap();
            let EntryOutcome::Panicked(report) = outcome else {
                panic!("expected a panic, got {outcome:?}");
            };
            assert_eq!(report.message, "computation failed");
            assert_eq!(report.location, "src/main.cheby:4:7");
            let config = entry_config(EntryMode::ConstEval(ConstEvalLimits::default()));
            let outcome = run_entry(panics_without_a_message, &config).unwrap();
            let EntryOutcome::Panicked(report) = outcome else {
                panic!("expected a panic, got {outcome:?}");
            };
            assert_eq!(report.kind, PanicKind::DivisionByZero);
            // The next entry runs.
            let outcome = run_entry(returns_seven, &entry_config(EntryMode::Test)).unwrap();
            assert_eq!(outcome, EntryOutcome::Returned(7));
        }

        /// D-710: the leak check runs after every panic.
        #[test]
        fn a_leak_after_a_panic_turns_101_into_102() {
            let _guard = serial();
            // Each prints the panic report to the test's standard error.
            let status = run_main(panics_cleanly, run_config()).unwrap();
            assert_eq!(status, abi::status::PANIC);
            let status = run_main(panics_after_a_leak, run_config()).unwrap();
            let leaked = if LEAK_CHECK_ENABLED {
                abi::status::LEAK_CHECK
            } else {
                abi::status::PANIC
            };
            assert_eq!(status, leaked);
        }

        /// Prints `text` to standard output without a newline, so that it
        /// stays in the buffer until a flush.
        fn print(text: &str) {
            let word = crate::values::string(text);
            // SAFETY: a live string, borrowed for the call.
            unsafe { crate::io::cheby_io_print(word) };
            // SAFETY: the only reference.
            unsafe { rc::drop(word) };
        }

        unsafe extern "C" fn edgeless_panic(ctx: *mut ExecCtx) -> u64 {
            print("before the panic");
            // SAFETY: as in `panics_with_a_message`.
            unsafe { panics_with_a_message(ctx) }
        }

        unsafe extern "C" fn edgeless_out_of_memory(ctx: *mut ExecCtx) -> u64 {
            print("before the panic");
            // SAFETY: `ctx` is the running entry's; the size has no layout.
            unsafe { crate::alloc::cheby_rt_alloc(ctx, u64::MAX, 0) }
        }

        unsafe extern "C" fn edgeless_overflow(ctx: *mut ExecCtx) -> u64 {
            print("before the panic");
            // SAFETY: as in `overflows`.
            unsafe { overflows(ctx) }
        }

        unsafe extern "C" fn edgeless_deadline(ctx: *mut ExecCtx) -> u64 {
            print("before the panic");
            // SAFETY: as in `spins`.
            unsafe { spins(ctx) }
        }

        /// The cases of [`edgeless_raises_end_the_process_with_the_report`]:
        /// the thunk, its entry mode, and what its report says.
        fn edgeless_case(name: &str) -> Option<(EntryFn, EntryMode, &'static str)> {
            let deadline = EntryMode::ConstEval(ConstEvalLimits {
                deadline: Instant::now().checked_add(Duration::from_millis(20)),
                ..ConstEvalLimits::default()
            });
            match name {
                "panic" => Some((
                    edgeless_panic,
                    EntryMode::Test,
                    "panicked at src/main.cheby:4:7:\ncomputation failed\n",
                )),
                "out_of_memory" => Some((
                    edgeless_out_of_memory,
                    EntryMode::Run,
                    "panicked at <unknown location>:\nout of memory\n",
                )),
                "overflow" => Some((edgeless_overflow, EntryMode::Run, "\nstack overflow\n")),
                "deadline" => Some((edgeless_deadline, deadline, Limit::Time.message())),
                _ => None,
            }
        }

        /// D-770: in code compiled without unwind edges, a
        /// panic, an out of memory, a stack overflow and a deadline each
        /// flush standard output, print the report (even in a test or a
        /// constant evaluation, whose hosts would capture it), and end the
        /// process with the fatal error and the status 101, before anything
        /// returns into the code that would not test the flag. Each case
        /// runs in a child process.
        #[test]
        #[cfg_attr(miri, ignore = "starts a process")]
        fn edgeless_raises_end_the_process_with_the_report() {
            const CHILD: &str = "CHEBY_RT_TEST_EDGELESS";
            const NAME: &str =
                "entry::tests::unwind::edgeless_raises_end_the_process_with_the_report";
            if let Some(case) = std::env::var_os(CHILD) {
                let (thunk, mode, _) = edgeless_case(&case.to_string_lossy()).unwrap();
                let config = EntryConfig {
                    stack_size: MIN_STACK_SIZE * 2,
                    unwind_edges: false,
                    consts: matches!(mode, EntryMode::ConstEval(_)).then(ConstTable::new),
                    ..entry_config(mode)
                };
                let outcome = run_entry(thunk, &config);
                panic!("the edgeless entry ended: {outcome:?}");
            }
            for case in ["panic", "out_of_memory", "overflow", "deadline"] {
                let (_, _, report) = edgeless_case(case).unwrap();
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                    .env(CHILD, case)
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                // An exit status, never a signal.
                assert_eq!(
                    out.status.code(),
                    Some(abi::status::PANIC),
                    "{case}: {stderr}"
                );
                assert!(stdout.contains("before the panic"), "{case}: {stdout}");
                assert!(stderr.contains(report), "{case}: {stderr}");
                assert!(
                    stderr.contains(
                        "fatal runtime error: an unwinding in code compiled without unwind \
                         edges (CHEBY_UNWIND_EDGES=off)\n"
                    ),
                    "{case}: {stderr}"
                );
                assert!(
                    !stderr.contains("the edgeless entry ended"),
                    "{case}: {stderr}"
                );
            }
        }
    }
}
