//! The open children and the reaper (D-1917, D-1919, D-1928).
//!
//! - **The registry** ([`REGISTRY`]): every child that started and was not
//!   yet reaped or handed to the reaper, by its process id, with the key of
//!   the runtime that started it (`Shared::child_owner`). At every end of a
//!   run that the runtime controls, [`kill_owned`] marks the run's runtime
//!   ended and sends SIGKILL to each of its children, waiting for none
//!   (D-1917), so another runtime of the same process, such as another REPL
//!   session's, keeps its children. A start of an ended runtime is refused
//!   ([`is_ended`], [`begin_start`]); the end waits for the run's starts
//!   that are in the system call that creates a child ([`Starting`]), and
//!   such a child is not recorded ([`opened`] says so) but killed at once
//!   by its starter, before the end goes on, so none escapes when the
//!   process ends right after. Where the whole process ends (a fatal
//!   runtime error), [`kill_all`] kills every child. A child leaves the
//!   registry before the call that reaps it, so a process id that the system
//!   may already give to a new process is never signalled.
//! - **The reaper** ([`hand_over`]): a thread, started with the first
//!   child ([`start_reaper`]), that reaps the children handed to it and no
//!   other process. It never waits on one child: it checks every one with
//!   `waitpid(WNOHANG)`, and while some are left it sleeps, from 10 ms
//!   doubling to 1 s, or until a new one is handed over (D-1928). A child is
//!   handed over by a `Child`'s finalizer, by a cancellation of the start
//!   or of `finish`'s wait, and by a `finish` whose SIGKILL the system
//!   refused (D-1912, D-1916, D-1928). A finalizer never blocks, so it only
//!   sends the process id (D-703).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// What the registry's lock guards.
struct Registry {
    /// The open children, each with its runtime's key.
    open: BTreeMap<libc::pid_t, u64>,
    /// The keys of the runtimes whose run ended.
    ended: BTreeSet<u64>,
    /// How many starts each runtime has in the system call that creates a
    /// child ([`Starting`]).
    starting: BTreeMap<u64, usize>,
}

/// The open children, the ended runtimes and the starts under way (module
/// documentation). A leaf lock.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    open: BTreeMap::new(),
    ended: BTreeSet::new(),
    starting: BTreeMap::new(),
});

/// Signalled when a start under way ends.
static STARTS_ENDED: Condvar = Condvar::new();

/// How long the end of a run waits for its starts under way: each is one
/// `posix_spawn` or `fork`, so this bound is never reached in practice.
const START_WAIT: Duration = Duration::from_secs(1);

/// A start under way of the runtime of key `owner`, from right before the
/// system call that creates the child until the child is recorded, or
/// killed for an ended run (D-1917). The end of the run waits for it, so
/// no child it creates escapes the kill, even when the process ends right
/// after.
#[derive(Debug)]
pub(super) struct Starting(u64);

impl Drop for Starting {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock();
        if let Some(count) = registry.starting.get_mut(&self.0) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                registry.starting.remove(&self.0);
            }
        }
        drop(registry);
        STARTS_ENDED.notify_all();
    }
}

/// Begins a start of the runtime of key `owner`; `None` when its run
/// ended, and the start is then refused.
pub(super) fn begin_start(owner: u64) -> Option<Starting> {
    let mut registry = REGISTRY.lock();
    if registry.ended.contains(&owner) {
        return None;
    }
    let count = registry.starting.entry(owner).or_insert(0);
    *count = count.saturating_add(1);
    Some(Starting(owner))
}

/// How long a kill tries for the registry's lock: an end of the program
/// never hangs on it.
const KILL_LOCK_WAIT: Duration = Duration::from_millis(100);

/// Records a child that the runtime of key `owner` just started; false,
/// recording nothing, when that runtime's run ended meanwhile, and the
/// caller then kills the child and hands it over.
#[must_use = "a child of an ended run is killed by its starter"]
pub(super) fn opened(pid: libc::pid_t, owner: u64) -> bool {
    let mut registry = REGISTRY.lock();
    if registry.ended.contains(&owner) {
        return false;
    }
    registry.open.insert(pid, owner);
    true
}

/// Whether the run of the runtime of key `owner` ended: its starts are
/// refused.
pub(super) fn is_ended(owner: u64) -> bool {
    REGISTRY.lock().ended.contains(&owner)
}

/// Forgets a child that is about to be reaped or handed over.
pub(super) fn closed(pid: libc::pid_t) {
    REGISTRY.lock().open.remove(&pid);
}

/// Whether `pid` is an open child (for tests).
#[cfg(test)]
pub(super) fn is_open(pid: libc::pid_t) -> bool {
    REGISTRY.lock().open.contains_key(&pid)
}

/// Sends SIGKILL to every open child of the runtime of key `owner`, or to
/// every one for `None`, without waiting (D-1917); `owner` is marked
/// ended first, under the same lock. The children stay in the registry:
/// their `Child` handles reap them, or hand them over, if the program goes
/// on.
fn kill_where(owner: Option<u64>) {
    let Some(mut registry) = REGISTRY.try_lock_for(KILL_LOCK_WAIT) else {
        return;
    };
    if let Some(owner) = owner {
        registry.ended.insert(owner);
        // The run's starts under way record their children first.
        let _ = STARTS_ENDED.wait_while_for(
            &mut registry,
            |registry| registry.starting.contains_key(&owner),
            START_WAIT,
        );
    }
    // A `BTreeMap`: the order is the process ids'.
    for (&pid, &of) in &registry.open {
        if owner.is_none_or(|owner| owner == of) {
            // SAFETY: `kill` has no memory effects; `pid` is a child that
            // was not reaped (it leaves the registry before), so it names
            // that child, running or a zombie.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Kills every open child of the process: where the whole process ends.
pub(crate) fn kill_all() {
    kill_where(None);
}

/// Ends the run of the runtime of key `owner` and kills its open children.
pub(crate) fn kill_owned(owner: u64) {
    kill_where(Some(owner));
}

/// The reaper's channel, once its thread started; `None` when the thread
/// could not start, and handed-over children then stay zombies.
static REAPER: OnceLock<Option<Sender<libc::pid_t>>> = OnceLock::new();

/// Starts the reaper's thread unless it runs: called before a child
/// starts, so that a finalizer only ever sends to it.
pub(super) fn start_reaper() {
    let _ = reaper();
}

/// The reaper's channel, starting its thread the first time.
fn reaper() -> Option<&'static Sender<libc::pid_t>> {
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<libc::pid_t>();
            std::thread::Builder::new()
                .name("cheby-reaper".to_owned())
                .spawn(move || reap_handed(&receiver))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// The shortest sleep of the reaper while children are left.
const REAP_FIRST: Duration = Duration::from_millis(10);
/// The longest one.
const REAP_LAST: Duration = Duration::from_secs(1);

/// The reaper's loop (module documentation): ends when every sender is
/// gone, which never happens in a running program.
fn reap_handed(receiver: &Receiver<libc::pid_t>) {
    let mut left: Vec<libc::pid_t> = Vec::new();
    let mut pause = REAP_FIRST;
    loop {
        let next = if left.is_empty() {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(pause)
        };
        match next {
            Ok(pid) => {
                left.push(pid);
                pause = REAP_FIRST;
            }
            Err(RecvTimeoutError::Timeout) => pause = pause.saturating_mul(2).min(REAP_LAST),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(pid) = receiver.try_recv() {
            left.push(pid);
        }
        left.retain(|&pid| !reaped_now(pid));
    }
}

/// Reaps `pid` if it has exited, without waiting; true when it is gone,
/// reaped here or elsewhere (`ECHILD`).
fn reaped_now(pid: libc::pid_t) -> bool {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a live `c_int` the call writes.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if reaped == pid {
            return true;
        }
        if reaped == 0 {
            return false;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return true;
        }
    }
}

/// Gives the child `pid`, already out of the registry, to the reaper.
/// Never blocks (D-703).
pub(super) fn hand_over(pid: libc::pid_t) {
    if let Some(sender) = reaper() {
        let _ = sender.send(pid);
    }
}

/// Waits for the child `pid` and reaps it: its raw wait status. Called
/// only once the child exited or was killed.
///
/// # Errors
///
/// `waitpid`'s error, `ECHILD` when the child was reaped elsewhere.
pub(super) fn wait_blocking(pid: libc::pid_t) -> io::Result<libc::c_int> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a live `c_int` the call writes.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if reaped == pid {
            return Ok(status);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether SIGCHLD's action was checked (D-1927); tests clear it.
static SIGCHLD_CHECKED: Mutex<bool> = Mutex::new(false);

/// D-1927: before the process's first child starts, sets SIGCHLD's action
/// back to the default when the program inherited it as "ignore", so the
/// system keeps each child until it is reaped, and children get the
/// default action. A program that starts no child keeps what it inherited.
pub(super) fn default_ignored_sigchld() {
    let mut checked = SIGCHLD_CHECKED.lock();
    if *checked {
        return;
    }
    *checked = true;
    // SAFETY: a zeroed `sigaction` is valid; the first call only reads the
    // action into it, the second sets the default action, with no handler
    // and no pointer kept.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &raw mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
        {
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&raw mut default.sa_mask);
            libc::sigaction(libc::SIGCHLD, &raw const default, std::ptr::null_mut());
        }
    }
}

/// Lets [`default_ignored_sigchld`] check again (for tests).
#[cfg(test)]
pub(super) fn check_sigchld_again() {
    *SIGCHLD_CHECKED.lock() = false;
}
