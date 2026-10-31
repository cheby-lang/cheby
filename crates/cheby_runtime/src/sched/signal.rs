//! SIGINT and SIGTERM for `process::until_interrupted` (D-1073, ADR-0071):
//! a self-pipe on the poller, and the walk that cancels every open scope of
//! `fiber::interruptible`, as an expired timer cancels a timed scope
//! (D-790). GAP(signal-self-pipe).
//!
//! - **The registry** ([`Registry`], process-global): the scopes armed by
//!   `fiber::sys_arm_interrupt` (`fiber_ops/interrupt.rs`), held weakly,
//!   so the registry keeps no scope alive, and their number. Its lock is a
//!   leaf: it is taken with no runtime lock held, and nothing but the
//!   system calls of this module is done under it.
//! - **Installation.** When the number of armed scopes rises from 0 to 1
//!   ([`arm`]), the handler is installed for SIGINT and SIGTERM, with
//!   `SA_RESETHAND`, so the kernel restores the default action as it
//!   delivers a signal, and `SA_RESTART`; when it falls back to 0
//!   ([`disarm`]), the dispositions found at the installation are
//!   restored. So a program that never opens such a scope pays nothing,
//!   and a signal while none is open takes the action it always took. A
//!   signal that the process ignored when the scope opened stays ignored.
//!   Once a signal has arrived ([`FIRED`]) or been acted on ([`First`]),
//!   the handler is never installed again: the default action stays for
//!   the rest of the process, so a second Ctrl-C always ends it (D-1073),
//!   however many scopes open later.
//! - **The handler** (`on_signal`) only sets [`FIRED`] and writes one
//!   byte, the signal's index, to the self-pipe, and keeps `errno`: it is
//!   async-signal-safe. The pipe is a nonblocking socket pair made once
//!   and never closed.
//! - **The poller.** Each runtime whose fiber arms a scope registers its
//!   own descriptor of the pipe's reading end with its poller, under the
//!   token [`SIGNAL_TOKEN`] (`sched/io.rs`), and from then on its monitor
//!   polls on each tick as for a socket (D-984). The poll's dispatch
//!   drains the pipe into [`PENDING`] (`drain`), and whoever polled runs
//!   [`run_pending`] after releasing the poller, as it fires due timers.
//! - **The walk** ([`run_pending`]): under the registry's lock, the first
//!   signal restores the dispositions, so a second Ctrl-C ends the process
//!   at once, and every armed scope that is its target is upgraded; then,
//!   with no lock held, each is cancelled with itself as the target
//!   (`cancel::cancel`). Its targets are the scopes armed before it
//!   arrived: a scope armed after [`FIRED`] was set is none, so a signal
//!   is never charged to a scope opened after the one it interrupted
//!   closed (the review's RB10-1). A scope that has ended is skipped by the
//!   walk (`cancel_step`), so a walk that loses the race to a close does
//!   nothing. A signal drained while none of its targets is armed, or
//!   after the dispositions were restored, takes its action again: the
//!   process sends it to itself (`reraise`). So does the first signal when
//!   every scope its walk cancelled returned `Ok`, its body having
//!   returned before the cancellation reached it (D-671's rule): the
//!   signal reached nothing, as if no scope had been open, and the close
//!   of the last of them sends it again ([`settle`]). GAP(signal-self-pipe).
//! - **Hosts.** Only a runtime made by `run_main`, for `cheby run` and
//!   executables, arms (`Shared::signals`); the runtimes of `cheby test`,
//!   `xtask test-std` and the REPL's session never do, nor does an entry
//!   that runs no runtime, so Ctrl-C keeps the host's behavior there. On
//!   Windows nothing is armed until M6. GAP(interrupt-hosts).
//! - **Deadlocks.** An armed scope adds nothing to any pending count, so a
//!   program whose fibers all block inside one is still reported (D-984's
//!   rule for an idle `Listener`).
//!
//! `sched/signal_models.rs` models the handler, the drain and walk, and a
//! closing scope in `loom`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::Shared;
use crate::fiber_ops::cancel;
use crate::fiber_ops::scope::ScopeState;

/// Where a runtime's SIGINT and SIGTERM go (`RuntimeConfig::signals`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// They keep the host's behavior: `cheby test`, `xtask test-std` and
    /// every in-process host.
    Host,
    /// They cancel the armed scopes: `run_main`'s runtime.
    Scopes,
}

/// The token of the self-pipe's reading end in every poller: an index
/// part that no socket's token has (`sched/io.rs`'s `token_of`).
#[cfg(unix)]
pub(super) const SIGNAL_TOKEN: mio::Token = mio::Token(super::io::INDEX_MASK);

/// Signals drained from the pipe and not acted on yet, by index (0 SIGINT,
/// 1 SIGTERM).
static PENDING: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// Set by the handler as a signal arrives, before it writes its byte: a
/// scope armed after it is no target of that signal and installs nothing
/// (module documentation). Never cleared, but by the tests' `hooks::reset`.
static FIRED: AtomicBool = AtomicBool::new(false);

/// The armed scopes and the process's dispositions (module
/// documentation).
static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());

/// An armed scope.
struct Slot {
    /// The scope, held weakly.
    scope: Weak<ScopeState>,
    /// Whether the first signal may cancel it: it was armed before that
    /// signal arrived.
    target: bool,
    /// Whether the first signal's walk cancelled it: its close then
    /// settles the signal ([`settle`]).
    reached: bool,
}

/// What the first signal of the process has done (module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum First {
    /// No signal was acted on yet.
    Waiting,
    /// The walk of the signal of index `index` cancelled scopes of which
    /// `left` have not closed; `stopped` once one of them closed failed.
    Walking {
        index: usize,
        left: usize,
        stopped: bool,
    },
    /// The first signal was acted on: the handler is never installed
    /// again.
    Done,
}

/// The registry (module documentation).
struct Registry {
    /// The armed scopes by slot; `None` is free.
    slots: Vec<Option<Slot>>,
    /// The free slots.
    free: Vec<usize>,
    /// The number of armed scopes.
    open: usize,
    /// While the handler is installed: the dispositions it replaced, by
    /// index, `None` for a signal left alone because it was ignored.
    saved: Option<[Option<sys::Action>; 2]>,
    /// The self-pipe, made at the first arm and never closed.
    pipe: Option<sys::Pipe>,
    /// The first signal's progress.
    first: First,
}

impl Registry {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            open: 0,
            saved: None,
            pipe: None,
            first: First::Waiting,
        }
    }

    /// Restores what [`sys::install`] replaced, if the handler is
    /// installed.
    fn restore(&mut self) {
        if let Some(saved) = self.saved.take() {
            sys::restore(&saved);
        }
    }
}

/// Arms `scope`, which the running fiber of `shared` has just opened, and
/// returns its slot, which [`disarm`] takes; `None` when the runtime gives
/// its scopes no signal (`cheby test`, `xtask test-std`, Windows until M6)
/// or the pipe could not be made or registered, so that signals keep the
/// action they had. The first armed scope of the process installs the
/// handler, unless a signal arrived or was acted on already: then the
/// scope is no target of it and the default action stays.
pub(crate) fn arm(shared: &Shared, scope: &Arc<ScopeState>) -> Option<usize> {
    if !shared.signals() || !sys::SUPPORTED {
        return None;
    }
    let mut registry = REGISTRY.lock();
    if registry.pipe.is_none() {
        registry.pipe = sys::Pipe::new().ok();
    }
    let pipe = registry.pipe.as_ref()?;
    pipe.register(shared.driver.io()?).ok()?;
    let index = if let Some(index) = registry.free.pop() {
        index
    } else {
        registry.slots.push(None);
        registry.slots.len().saturating_sub(1)
    };
    // Under the registry's lock, after the handler's store: a signal that
    // arrived before this load is not this scope's (module documentation).
    let live = registry.first == First::Waiting && !FIRED.load(Ordering::SeqCst);
    let slot = registry.slots.get_mut(index)?;
    *slot = Some(Slot {
        scope: Arc::downgrade(scope),
        target: live,
        reached: false,
    });
    registry.open = registry.open.saturating_add(1);
    if live && registry.saved.is_none() {
        registry.saved = Some(sys::install());
    }
    Some(index)
}

/// Disarms the scope in `slot`, which [`arm`] returned: its close calls it
/// on every path out. The last armed scope restores the dispositions the
/// handler replaced, unless a signal restored them already. True when the
/// first signal's walk cancelled the scope: the close then calls
/// [`settle`] once it has decided.
pub(crate) fn disarm(slot: usize) -> bool {
    let mut registry = REGISTRY.lock();
    let Some(held) = registry.slots.get_mut(slot).and_then(Option::take) else {
        drop(registry);
        crate::panic::fatal(format_args!(
            "a scope disarmed a signal slot it did not hold"
        ));
    };
    registry.free.push(slot);
    registry.open = registry.open.saturating_sub(1);
    if registry.open == 0 {
        registry.restore();
    }
    held.reached
}

/// Settles the first signal for a scope that its walk cancelled
/// ([`disarm`] returned true): the scope's close calls it once it has
/// decided, with `ok` when the scope returns `Ok` because its body
/// returned before the cancellation reached it (D-671's rule). When the
/// last such scope settles and none of them failed, the signal reached
/// nothing, as if no scope had been open, and takes its action (module
/// documentation). Called with no lock held.
pub(crate) fn settle(ok: bool) {
    let reraise = {
        let mut registry = REGISTRY.lock();
        let First::Walking {
            index,
            left,
            stopped,
        } = registry.first
        else {
            return;
        };
        let left = left.saturating_sub(1);
        let stopped = stopped || !ok;
        if left == 0 {
            registry.first = First::Done;
            (!stopped).then_some(index)
        } else {
            registry.first = First::Walking {
                index,
                left,
                stopped,
            };
            None
        }
    };
    if let Some(index) = reraise {
        sys::reraise(index);
    }
}

/// Counts one signal of index `byte`, as the drain reads it from the pipe.
#[cfg(unix)]
fn note(byte: u8) {
    if let Some(pending) = PENDING.get(usize::from(byte)) {
        pending.fetch_add(1, Ordering::SeqCst);
    }
}

/// Acts on the signals drained so far (module documentation): whoever
/// polled `shared`'s poller calls it after releasing it, with no lock held,
/// as it fires due timers. Nothing happens during a halt (D-708, C-4): the
/// signals stay pending for another runtime, or are lost as the process
/// ends.
pub(super) fn run_pending(shared: &Shared) {
    // Two loads on every park: nothing more without a signal.
    if PENDING
        .iter()
        .all(|pending| pending.load(Ordering::SeqCst) == 0)
        || shared.halting()
    {
        return;
    }
    let mut counts = [0_usize; 2];
    for (count, pending) in counts.iter_mut().zip(&PENDING) {
        *count = pending.swap(0, Ordering::SeqCst);
    }
    if counts.iter().all(|count| *count == 0) {
        return;
    }
    let scopes: Vec<Arc<ScopeState>> = {
        let mut registry = REGISTRY.lock();
        if registry.first == First::Waiting {
            registry.restore();
            // The first signal: SIGINT's if any came. Every other one came
            // before the restore and takes its action below.
            let index = counts.iter().position(|count| *count > 0).unwrap_or(0);
            let mut scopes = Vec::new();
            for slot in registry.slots.iter_mut().flatten() {
                if slot.target
                    && let Some(scope) = slot.scope.upgrade()
                {
                    slot.reached = true;
                    scopes.push(scope);
                }
            }
            if scopes.is_empty() {
                // Every scope open when it came has closed: it takes its
                // action below.
                registry.first = First::Done;
            } else {
                if let Some(count) = counts.get_mut(index) {
                    *count = count.saturating_sub(1);
                }
                registry.first = First::Walking {
                    index,
                    left: scopes.len(),
                    stopped: false,
                };
            }
            scopes
        } else {
            Vec::new()
        }
    };
    for scope in &scopes {
        cancel::cancel(scope);
    }
    drop(scopes);
    for (index, count) in counts.iter().enumerate() {
        if *count > 0 {
            sys::reraise(index);
        }
    }
}

/// The system's side: dispositions, the handler and the pipe.
#[cfg(unix)]
mod sys {
    use std::os::fd::AsRawFd as _;
    use std::sync::atomic::{AtomicI32, Ordering};

    use crate::sched::io::IoShared;

    /// Signals reach scopes on this platform.
    pub(super) const SUPPORTED: bool = true;

    /// The signals a scope waits for, by index: the byte the handler
    /// writes.
    const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

    /// A disposition, as `sigaction` reports it.
    pub(super) type Action = libc::sigaction;

    /// The pipe's writing end, for the handler; -1 before the pipe exists.
    static WRITE_END: AtomicI32 = AtomicI32::new(-1);

    /// Installs the handler for both signals and returns what it replaced;
    /// a signal that is ignored stays ignored (`None`).
    pub(super) fn install() -> [Option<Action>; 2] {
        let mut saved = [None, None];
        for (slot, signal) in saved.iter_mut().zip(SIGNALS) {
            // SAFETY: a zeroed `sigaction` is a valid value (no handler,
            // an empty mask, no flags), used as an out-parameter.
            let mut old: Action = unsafe { std::mem::zeroed() };
            // SAFETY: reads the disposition into `old`, a valid
            // out-parameter; a null new action changes nothing.
            if unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut old) } != 0
                || old.sa_sigaction == libc::SIG_IGN
            {
                continue;
            }
            // SAFETY: as above.
            let mut new: Action = unsafe { std::mem::zeroed() };
            new.sa_sigaction = handler_address();
            new.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
            // SAFETY: `new.sa_mask` is a valid `sigset_t` to empty.
            unsafe { libc::sigemptyset(&raw mut new.sa_mask) };
            // SAFETY: `new` is a valid action whose handler is
            // `on_signal`, which is async-signal-safe; `old` is a valid
            // out-parameter.
            if unsafe { libc::sigaction(signal, &raw const new, &raw mut old) } == 0 {
                *slot = Some(old);
            }
        }
        saved
    }

    /// Installs again the dispositions [`install`] replaced.
    pub(super) fn restore(saved: &[Option<Action>; 2]) {
        for (old, signal) in saved.iter().zip(SIGNALS) {
            if let Some(old) = old {
                // SAFETY: `old` is the action `sigaction` reported, valid
                // to install again.
                unsafe { libc::sigaction(signal, old, std::ptr::null_mut()) };
            }
        }
    }

    /// `on_signal`'s address, as `sigaction` takes it.
    #[expect(
        clippy::as_conversions,
        reason = "`sigaction` takes a handler's address as an integer"
    )]
    fn handler_address() -> libc::sighandler_t {
        on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t
    }

    /// The handler (module documentation): sets `FIRED`, writes the
    /// signal's index to the pipe and keeps `errno`. Async-signal-safe: an
    /// atomic store and an atomic load (both lock-free), `write` on a
    /// nonblocking socket, and `errno`'s accessor. A full pipe drops the
    /// byte, which changes nothing, since a byte is pending.
    extern "C" fn on_signal(signal: libc::c_int) {
        let byte: u8 = u8::from(signal != libc::SIGINT);
        super::FIRED.store(true, Ordering::SeqCst);
        let fd = WRITE_END.load(Ordering::Relaxed);
        let errno = errno_location();
        // SAFETY: this thread's `errno`, valid to read and write.
        let saved = unsafe { *errno };
        // SAFETY: `fd` is the pipe's writing end, never closed, or -1,
        // which `write` refuses; `byte` is a valid one-byte buffer.
        let _ = unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
        // SAFETY: as above.
        unsafe { *errno = saved };
    }

    /// This thread's `errno`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: returns this thread's `errno`; async-signal-safe.
        unsafe { libc::__errno_location() }
    }

    /// This thread's `errno`.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: returns this thread's `errno`; async-signal-safe.
        unsafe { libc::__error() }
    }

    /// Sends signal `index` to the process again, now that its action from
    /// before the handler is back (module documentation). In this crate's
    /// tests it is recorded instead (`hooks::RERAISED`).
    pub(super) fn reraise(index: usize) {
        let Some(signal) = SIGNALS.get(index) else {
            return;
        };
        #[cfg(test)]
        super::hooks::RERAISED.lock().push(*signal);
        #[cfg(not(test))]
        // SAFETY: `kill` of the process's own id with a valid signal.
        unsafe {
            libc::kill(libc::getpid(), *signal);
        }
    }

    /// The self-pipe: a nonblocking socket pair.
    pub(super) struct Pipe {
        /// The reading end; each runtime registers a duplicate.
        read: std::os::unix::net::UnixStream,
        /// The writing end, which [`WRITE_END`] names.
        _write: std::os::unix::net::UnixStream,
    }

    impl Pipe {
        /// Makes the pair, both ends nonblocking, and publishes the
        /// writing end to the handler.
        pub(super) fn new() -> std::io::Result<Self> {
            let (read, write) = std::os::unix::net::UnixStream::pair()?;
            read.set_nonblocking(true)?;
            write.set_nonblocking(true)?;
            WRITE_END.store(write.as_raw_fd(), Ordering::SeqCst);
            Ok(Self {
                read,
                _write: write,
            })
        }

        /// Registers a duplicate of the reading end with `io`'s poller,
        /// once per runtime.
        pub(super) fn register(&self, io: &IoShared) -> std::io::Result<()> {
            io.register_signals(|| Ok(mio::net::UnixStream::from_std(self.read.try_clone()?)))
        }
    }

    /// Reads every byte the handler wrote from `read`, a runtime's
    /// descriptor of the pipe's reading end, into `PENDING`: the poll's
    /// dispatch of `SIGNAL_TOKEN`, under the poller's lock, taking no lock.
    /// The registration is edge-triggered, so it reads until the pipe is
    /// empty.
    pub(in crate::sched) fn drain(read: &mio::net::UnixStream) {
        use std::io::Read as _;
        let mut buffer = [0_u8; 64];
        let mut reader = read;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    for byte in buffer.get(..count).unwrap_or(&[]) {
                        super::note(*byte);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    /// Sets `FIRED` and writes one byte for signal `index` (0 SIGINT, 1
    /// SIGTERM) to the pipe, as the handler would, with no signal: the
    /// synthetic signal event of the runtime tests. False when no pipe
    /// exists yet.
    #[cfg(test)]
    pub(crate) fn write_synthetic(index: u8) -> bool {
        super::FIRED.store(true, Ordering::SeqCst);
        let fd = WRITE_END.load(Ordering::SeqCst);
        // SAFETY: the pipe's writing end and a one-byte buffer.
        fd >= 0 && unsafe { libc::write(fd, (&raw const index).cast(), 1) } == 1
    }

    /// Whether `signal`'s disposition is the runtime's handler now.
    #[cfg(test)]
    pub(crate) fn handler_installed(signal: libc::c_int) -> bool {
        // SAFETY: as in `install`.
        let mut old: Action = unsafe { std::mem::zeroed() };
        // SAFETY: as in `install`.
        unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut old) };
        old.sa_sigaction == handler_address()
    }
}

/// The system's side on Windows: no signal reaches a scope until M6.
#[cfg(not(unix))]
mod sys {
    use crate::sched::io::IoShared;

    /// Signals reach no scope on this platform.
    pub(super) const SUPPORTED: bool = false;

    /// No disposition is kept.
    pub(super) type Action = ();

    /// Installs nothing.
    pub(super) fn install() -> [Option<Action>; 2] {
        [None, None]
    }

    /// Restores nothing.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "the Unix signature, which takes `sigaction`s"
    )]
    pub(super) fn restore(_saved: &[Option<Action>; 2]) {}

    /// Sends nothing.
    pub(super) fn reraise(_index: usize) {}

    /// No pipe.
    pub(super) struct Pipe;

    impl Pipe {
        /// Never made.
        pub(super) fn new() -> std::io::Result<Self> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }

        /// Never registered.
        #[expect(clippy::unused_self, reason = "the Unix signature")]
        pub(super) fn register(&self, _io: &IoShared) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
    }
}

#[cfg(unix)]
pub(super) use sys::drain;
#[cfg(all(test, unix))]
pub(crate) use sys::{handler_installed, write_synthetic};

/// Whether the handler is installed now, by the registry's account, for
/// the runtime tests.
#[cfg(all(test, unix))]
pub(crate) fn installed() -> bool {
    REGISTRY.lock().saved.is_some()
}

/// The number of armed scopes, for the runtime tests.
#[cfg(all(test, unix))]
pub(crate) fn armed() -> usize {
    REGISTRY.lock().open
}

/// Test hooks.
#[cfg(all(test, unix))]
pub(crate) mod hooks {
    use parking_lot::Mutex;

    /// The signals [`super::run_pending`] sent to the process again, in
    /// order, instead of sending them.
    pub(crate) static RERAISED: Mutex<Vec<libc::c_int>> = Mutex::new(Vec::new());

    /// Serializes the tests that arm scopes, install the handler or read
    /// the dispositions: the registry is the process's.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    /// Forgets the first signal and clears [`RERAISED`], as in a fresh
    /// process, so that the next test's first scope installs the handler
    /// again: each test that arms starts so, holding [`SERIAL`] with
    /// nothing armed.
    pub(crate) fn reset() {
        super::REGISTRY.lock().first = super::First::Waiting;
        super::FIRED.store(false, std::sync::atomic::Ordering::SeqCst);
        RERAISED.lock().clear();
    }
}
