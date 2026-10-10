//! Running entries (F7, §10.10): `run_entry` runs one entry thunk on its
//! own stack and reports how it ended; `run_main` adds `cheby run`'s
//! policy (exit statuses, flushing, the debug leak check).
//!
//! M1 runs each entry on a dedicated OS thread with the requested stack
//! size (D-457). A panic reports to the waiting host and parks the entry's
//! thread forever, abandoning its stack and live objects, which is D-283's
//! behavior without a stack-switching library. OQ-P7 replaces the thread
//! with a runtime-owned stack behind the same API.
//!
//! # The current entry
//!
//! Kernels take no `ctx` (D-453), yet some need the running entry: a kernel
//! that calls a Cheby closure ([`crate::callback`], D-526), maps and sets
//! that take the entry's hash seed (D-538), and the stub bound to
//! unresolved symbols (D-532). `run_entry` therefore records, on the
//! entry's thread, the entry's context, its hash seed and its trampolines
//! ([`current_ctx`], [`current_seed`], [`callbacks`]), and makes the
//! entry's accounting domain the thread's current one. Generated code
//! still passes `ctx` explicitly (ADR-0055). M2's fiber switching must
//! update the record when it switches fibers.

use std::cell::Cell;
use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use crate::abi::status;
use crate::alloc::{self, Domain, LEAK_CHECK_ENABLED};
use crate::ctx::ExecCtx;
use crate::generic;
use crate::io;
use crate::panic::{PanicReport, stack_position};

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

/// The limits of a constant evaluation (D-195, D-269). Carried from the
/// slice on; enforced from E6.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConstEvalLimits {
    /// When the evaluation must stop.
    pub deadline: Option<Instant>,
    /// The most bytes it may allocate.
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
}

impl EntryConfig {
    /// An entry of the given mode with the default fiber stack, no
    /// trampolines, and the calling thread's current accounting domain.
    #[must_use]
    pub fn new(mode: EntryMode) -> Self {
        Self {
            stack_size: DEFAULT_STACK_SIZE,
            mode,
            callbacks: Callbacks::default(),
            domain: alloc::current_domain(),
        }
    }
}

/// How an entry ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOutcome {
    /// The thunk returned this word.
    Returned(u64),
    /// The entry panicked; its thread is parked forever.
    Panicked(PanicReport),
}

/// A failure of the runtime itself, not of the program.
#[derive(Debug)]
pub enum RuntimeError {
    /// The requested stack is smaller than [`MIN_STACK_SIZE`].
    StackTooSmall {
        /// The requested size.
        requested: usize,
    },
    /// The entry's thread could not be started.
    Spawn(std::io::Error),
    /// The entry's thread ended without reporting an outcome.
    EntryLost,
    /// A `main` thunk returned something other than 0 or 1.
    InvalidStatus(u64),
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
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::StackTooSmall { .. } | Self::EntryLost | Self::InvalidStatus(_) => None,
        }
    }
}

/// The record of the entry running on this thread.
#[derive(Debug, Clone, Copy)]
struct Current {
    ctx: *mut ExecCtx,
    seed: u64,
    callbacks: Callbacks,
}

thread_local! {
    static CURRENT: Cell<Option<Current>> = const { Cell::new(None) };
}

fn current() -> Option<Current> {
    CURRENT.try_with(Cell::get).ok().flatten()
}

/// The execution context of the entry running on this thread, or `None`
/// outside any entry.
#[must_use]
pub fn current_ctx() -> Option<*mut ExecCtx> {
    current().map(|current| current.ctx)
}

/// The hash seed new maps and sets take (D-538): the running constant's
/// seed during constant evaluation, the per-process seed otherwise.
#[must_use]
pub fn current_seed() -> u64 {
    current().map_or_else(generic::process_seed, |current| current.seed)
}

/// The trampolines of the entry running on this thread, or `None` outside
/// any entry.
#[must_use]
pub fn callbacks() -> Option<Callbacks> {
    current().map(|current| current.callbacks)
}

/// Runs `f` with `seed` as the entry's hash seed, then restores the
/// previous one: lazy forcing of a constant evaluates it under its own
/// seed (plan §7.2). Outside any entry, `f` just runs.
pub fn with_seed<R>(seed: u64, f: impl FnOnce() -> R) -> R {
    let Some(saved) = current() else {
        return f();
    };
    CURRENT
        .try_with(|cell| cell.set(Some(Current { seed, ..saved })))
        .ok();
    let result = f();
    CURRENT
        .try_with(|cell| {
            if let Some(now) = cell.get() {
                cell.set(Some(Current {
                    seed: saved.seed,
                    ..now
                }));
            }
        })
        .ok();
    result
}

/// The allocation budget of constant evaluation (plan §7.3, D-536): called
/// with every allocation's size. A documented no-op in wave 1; the memory
/// limit is enforced by W4-C from the domain's peak bytes.
#[inline]
pub(crate) fn charge_budget(bytes: u64) {
    let _ = bytes;
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
///   compiled into, must stay valid until this function returns. After a
///   panic the entry's thread is parked forever inside the runtime and
///   never runs that code again, so nothing is required after the return.
/// - The trampolines in `config.callbacks` must have their declared ABI
///   and stay valid as long as `entry`'s code.
pub unsafe fn run_entry(
    entry: EntryFn,
    config: &EntryConfig,
) -> Result<EntryOutcome, RuntimeError> {
    let stack_size = config.stack_size;
    if stack_size < MIN_STACK_SIZE {
        return Err(RuntimeError::StackTooSmall {
            requested: stack_size,
        });
    }
    let mode = config.mode.clone();
    let callbacks = config.callbacks;
    let domain = Arc::clone(&config.domain);
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("cheby-entry".to_owned())
        .stack_size(stack_size)
        .spawn(move || {
            // The first frame is within a few KiB of the top of the stack;
            // the headroom covers that distance.
            let limit = stack_position()
                .saturating_sub(stack_size)
                .saturating_add(STACK_HEADROOM);
            let seed = match &mode {
                EntryMode::ConstEval(limits) => limits.seed,
                EntryMode::Run | EntryMode::Test | EntryMode::Repl => generic::process_seed(),
            };
            alloc::set_current_domain(domain);
            let mut ctx = ExecCtx::new(limit, mode, Some(sender.clone()));
            CURRENT
                .try_with(|cell| {
                    cell.set(Some(Current {
                        ctx: &raw mut ctx,
                        seed,
                        callbacks,
                    }));
                })
                .ok();
            // SAFETY: the caller of `run_entry` guarantees that `entry` is
            // an entry thunk with the `EntryFn` ABI whose code stays valid
            // until `run_entry` returns, which is after this call returns
            // (the host waits on the channel). `ctx` outlives the call: a
            // panic parks this thread forever, keeping the frame alive.
            let word = unsafe { entry(&raw mut ctx) };
            CURRENT.try_with(|cell| cell.set(None)).ok();
            sender.send(EntryOutcome::Returned(word)).ok();
        })
        .map_err(RuntimeError::Spawn)?;
    match receiver.recv() {
        Ok(outcome @ EntryOutcome::Returned(_)) => {
            thread.join().map_err(|_| RuntimeError::EntryLost)?;
            Ok(outcome)
        }
        // The thread is parked forever; dropping its handle detaches it.
        Ok(outcome @ EntryOutcome::Panicked(_)) => Ok(outcome),
        Err(mpsc::RecvError) => {
            thread.join().ok();
            Err(RuntimeError::EntryLost)
        }
    }
}

/// How `cheby run` runs `main`.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The program's arguments, without the program name (D-347).
    pub args: Vec<String>,
    /// The program name, `env::program_name()` (D-347).
    pub program_name: String,
    /// The root fiber's stack (D-125).
    pub root_stack_size: usize,
    /// The trampolines `main`'s kernels may call closures through (D-526).
    pub callbacks: Callbacks,
    /// The build's accounting domain, which the leak check reads.
    pub domain: Arc<Domain>,
}

impl Default for RunConfig {
    /// No arguments, the default root stack, no trampolines, and the
    /// calling thread's current accounting domain.
    fn default() -> Self {
        Self {
            args: Vec::new(),
            program_name: String::new(),
            root_stack_size: DEFAULT_ROOT_STACK_SIZE,
            callbacks: Callbacks::default(),
            domain: alloc::current_domain(),
        }
    }
}

/// Runs `main`'s entry thunk and returns the exit status (§10.10, D-182):
///
/// - The thunk's 0 or 1 (`main` returned `Nil`/`Ok(Nil)`, or `Err(e)`
///   after printing `e`), after flushing standard output. A failed flush
///   gives 1 (D-306, D-346).
/// - 101 after a panic, which has already flushed and printed.
/// - In debug builds, 102 if objects of the build's domain are still live
///   when the status is 0 or 1 (D-458). Constants frozen before `main`
///   have left the domain, so every live object is a leak.
///
/// # Errors
///
/// [`RuntimeError`] if the entry cannot run or returns an invalid status.
///
/// # Safety
///
/// As for [`run_entry`]: `entry` must be a `main` entry thunk with the
/// [`EntryFn`] ABI, and its code must stay valid until this returns.
pub unsafe fn run_main(entry: EntryFn, config: RunConfig) -> Result<i32, RuntimeError> {
    set_program(config.args, config.program_name);
    let domain = config.domain;
    let entry_config = EntryConfig {
        stack_size: config.root_stack_size,
        mode: EntryMode::Run,
        callbacks: config.callbacks,
        domain: Arc::clone(&domain),
    };
    // SAFETY: the caller guarantees `run_entry`'s preconditions for
    // `entry`, which this function passes on unchanged.
    let outcome = unsafe { run_entry(entry, &entry_config) }?;
    let flushed = io::flush_stdout().is_ok();
    match outcome {
        EntryOutcome::Panicked(_) => Ok(status::PANIC),
        EntryOutcome::Returned(_) if !flushed => Ok(status::ERR),
        EntryOutcome::Returned(raw) => {
            let code = match raw {
                0 => status::SUCCESS,
                1 => status::ERR,
                other => return Err(RuntimeError::InvalidStatus(other)),
            };
            match leak_report(domain.live_objects()) {
                Some(report) => {
                    io::write_stderr(&report);
                    Ok(status::LEAK_CHECK)
                }
                None => Ok(code),
            }
        }
    }
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

/// Sets the running program's arguments, without the program name, and
/// its name (D-347), which `std::env` reads. `run_main` calls it with its
/// `RunConfig`; a host that runs other entries as a program, such as
/// `cheby test` (D-369) or the REPL, calls it before they run. An empty
/// name means "not set": `env::program_name` then gives the process's
/// first argument.
pub fn set_program(args: Vec<String>, name: String) {
    *PROGRAM.write().unwrap_or_else(PoisonError::into_inner) = Program { args, name };
}

/// The running program's name, or `None` when no host has set one.
#[must_use]
pub fn program_name_if_set() -> Option<String> {
    Some(program_name()).filter(|name| !name.is_empty())
}

/// The running program's arguments, without the program name (D-347),
/// read by `std::env`.
#[must_use]
pub fn program_args() -> Vec<String> {
    PROGRAM
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .args
        .clone()
}

/// The running program's name (D-347).
#[must_use]
pub fn program_name() -> String {
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
        // as this thread, which parks forever.
        unsafe { cheby_rt_panic(ctx, PanicKind::Explicit.code(), message, location) }
    }

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
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
        unsafe { cheby_rt_panic(ctx, PanicKind::DivisionByZero.code(), 0, 0) }
    }

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
    fn panics_without_message_use_the_default() {
        let outcome = run_entry(panics_without_a_message, &const_eval()).unwrap();
        let EntryOutcome::Panicked(report) = outcome else {
            panic!("expected a panic, got {outcome:?}");
        };
        assert_eq!(report.message, "division by zero");
        assert_eq!(report.location, crate::panic::UNKNOWN_LOCATION);
    }

    /// The function-entry check generated code performs.
    unsafe fn entry_check(ctx: *mut ExecCtx) {
        // SAFETY: `ctx` is the running entry's.
        let limit = unsafe { &*ctx }.stack_limit.load(Ordering::Relaxed);
        if stack_position() < limit {
            // SAFETY: as above.
            unsafe { cheby_rt_stack_slow(ctx, 0) };
        }
    }

    unsafe extern "C" fn interrupted(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: `ctx` is the running entry's.
        let context = unsafe { &*ctx };
        context.request_interrupt();
        // SAFETY: as above.
        unsafe { entry_check(ctx) };
        let restored = context.stack_limit.load(Ordering::Relaxed) == context.real_stack_limit();
        // A second check passes on the fast path.
        // SAFETY: as above.
        unsafe { entry_check(ctx) };
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

    fn recurse(ctx: *mut ExecCtx, depth: u64) -> u64 {
        // SAFETY: `ctx` is the running entry's.
        unsafe { entry_check(ctx) };
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
        assert_eq!(program_args(), ["a"]);
        assert_eq!(program_name(), "p");
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

    #[test]
    #[cfg_attr(miri, ignore = "parks a thread forever")]
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
}
