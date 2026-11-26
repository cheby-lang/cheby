//! A started child: its process, its exit on the poller, and the `Child`
//! handle's state (D-1912, D-1913, D-1916, D-1919).
//!
//! - **The process** ([`Process`]): its id and, under one lock, whether it
//!   was reaped and its status. A signal is sent under the lock and only to
//!   a child not yet reaped, so a process id that the system may already
//!   give to a new process is never signalled (`signal`). Dropping it kills
//!   a child not yet reaped and hands it to the reaper (`registry.rs`): so
//!   do a `Child`'s finalizer and a start that a cancellation abandoned.
//! - **The exit on the poller** ([`ExitWatch`]): a pidfd on Linux, a
//!   kqueue with `EVFILT_PROC` and `NOTE_EXIT` on macOS, registered with the
//!   runtime's poller, readable once the child exited, so a fiber that
//!   waits counts as pending IO (D-984) and is never reported as
//!   deadlocked. A wait asks the descriptor itself whether the exit came
//!   (`poll` with no timeout on the pidfd, `kevent` with no timeout on the
//!   kqueue, which takes the event) and only then reaps, with a `waitpid`
//!   that returns at once; otherwise it parks on the seam. A child that
//!   exited before the kqueue saw it (`ESRCH`) has no descriptor and is
//!   reaped at once.
//! - **`wait`** (D-1913) waits under the child's turn, so one fiber waits on
//!   the descriptor at a time; a cancelled wait takes nothing, and the
//!   status is kept, so a second wait returns it at once.
//! - **`finish`** (D-1912) kills a child not yet reaped, waits for its exit
//!   on the poller and reaps it; a cancellation meanwhile hands it to the
//!   reaper, and so does a kill that the system refuses, without waiting
//!   (D-1928). It then closes every pipe and marks the child finished, so
//!   every later operation on it or its pipes is kind 12.
//! - **Captured output** (D-1918): the temporary files of an inherited
//!   standard output and error under `cheby test` join the test's capture
//!   when the child is reaped by `wait` or `finish`, standard output's
//!   bytes first, read on the blocking pool. A child left to the reaper leaves its files with the
//!   capture, which joins them at the test's end (D-1922).

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mio::unix::SourceFd;
use parking_lot::Mutex;

use super::error::ProcError;
use super::pipes::{Pipe, ReaderState, Work, WriterState};
use super::registry;
use super::spawn::Spawned;
use crate::abi::Word;
use crate::capture::{Capture, Stream, file_contents};
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::Cancelled;
use crate::handles::HandlePayload;
use crate::sched::Shared;
use crate::sched::blocking::run_blocking;
use crate::sched::io::{Direction, IoHandle};
use crate::sched::turn::{Turn, TurnError};

/// How a reaped child ended (D-1910).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// It exited with this status, 0 to 255.
    Exited(i32),
    /// This signal ended it.
    Signaled(i32),
}

impl Status {
    /// The status of a raw `waitpid` status.
    fn of(raw: libc::c_int) -> Self {
        if libc::WIFSIGNALED(raw) {
            Self::Signaled(libc::WTERMSIG(raw))
        } else {
            Self::Exited(libc::WEXITSTATUS(raw))
        }
    }

    /// `sys_wait`'s pair: `(0, code)` exited, `(1, signal)` signaled.
    pub(crate) fn pair(self) -> (i64, i64) {
        match self {
            Self::Exited(code) => (0, i64::from(code)),
            Self::Signaled(signal) => (1, i64::from(signal)),
        }
    }
}

/// What a [`Process`]'s lock guards.
#[derive(Debug, Default)]
struct ProcState {
    /// Reaped (`status` is then known) or handed to the reaper: the id may
    /// name another process now.
    gone: bool,
    status: Option<Status>,
    /// The exit descriptor reported the exit, which a kqueue gives once.
    exit_seen: bool,
}

/// A child's process (module documentation).
#[derive(Debug)]
pub(crate) struct Process {
    pid: libc::pid_t,
    /// A leaf lock.
    state: Mutex<ProcState>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.abandon();
    }
}

impl Process {
    /// The process of the child `pid`, which the registry holds.
    pub(super) fn new(pid: libc::pid_t) -> Self {
        Self {
            pid,
            state: Mutex::new(ProcState::default()),
        }
    }

    /// The child's process id.
    pub(super) fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// The status, once reaped.
    pub(super) fn status(&self) -> Option<Status> {
        self.state.lock().status
    }

    /// Whether the child was reaped or handed over.
    pub(super) fn is_gone(&self) -> bool {
        self.state.lock().gone
    }

    /// Sends `signal` unless the child was reaped: `Ok` also for a child
    /// that exited (D-1913).
    ///
    /// # Errors
    ///
    /// `kill`'s error.
    pub(super) fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        let state = self.state.lock();
        if state.gone {
            return Ok(());
        }
        // SAFETY: `kill` has no memory effects; under the lock, the child
        // is not reaped, so `pid` names it.
        if unsafe { libc::kill(self.pid, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        drop(state);
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// Reaps the child, whose exit was seen, so `waitpid` returns at once;
    /// keeps the status.
    ///
    /// # Errors
    ///
    /// `waitpid`'s error, or kind 0 for a child handed to the reaper.
    fn reap(&self) -> io::Result<Status> {
        let mut state = self.state.lock();
        if let Some(status) = state.status {
            return Ok(status);
        }
        if state.gone {
            return Err(io::Error::other("the child was handed to the reaper"));
        }
        state.gone = true;
        registry::closed(self.pid);
        let raw = registry::wait_blocking(self.pid)?;
        let status = Status::of(raw);
        state.status = Some(status);
        Ok(status)
    }

    /// Records that the exit descriptor reported the exit.
    fn see_exit(&self) {
        self.state.lock().exit_seen = true;
    }

    /// Whether the exit descriptor reported the exit.
    fn exit_seen(&self) -> bool {
        self.state.lock().exit_seen
    }

    /// Kills a child not yet reaped and hands it to the reaper; never
    /// blocks (D-703, D-1919).
    pub(super) fn abandon(&self) {
        let mut state = self.state.lock();
        if state.gone {
            return;
        }
        // SAFETY: as in `signal`: under the lock, the child is not reaped.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        state.gone = true;
        registry::closed(self.pid);
        registry::hand_over(self.pid);
    }
}

/// The exit descriptor of `process`, made right after the start (module
/// documentation): `None` when the child already exited.
///
/// # Errors
///
/// The system's error, kind 0, after which the child is killed and
/// reaped; on Linux one that names `pidfd_open`, also before 5.3 (D-1916).
pub(super) fn exit_descriptor(process: &Process) -> Result<Option<OwnedFd>, ProcError> {
    let made = open_exit(process.pid());
    if made.is_err() {
        // SAFETY: as in `Process::signal`: nothing reaped the child yet.
        unsafe { libc::kill(process.pid(), libc::SIGKILL) };
        let _ = process.reap();
    }
    made
}

/// A pidfd for `pid` (Linux 5.3, D-1916).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_exit(pid: libc::pid_t) -> Result<Option<OwnedFd>, ProcError> {
    // SAFETY: `pidfd_open` takes two integers and returns a descriptor,
    // close-on-exec, or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        // Any failure, `ENOSYS` before Linux 5.3 or `EPERM` from a seccomp
        // filter among them, is kind 0 and names the call; the caller kills
        // and reaps the child.
        let error = io::Error::last_os_error();
        let message = if error.raw_os_error() == Some(libc::ENOSYS) {
            "starting a process needs pidfd_open, which Linux has from 5.3 on".to_owned()
        } else {
            format!(
                "starting a process needs pidfd_open, which failed: {}",
                crate::io::os_message(&error)
            )
        };
        return Err(ProcError::other(&message));
    }
    let fd = libc::c_int::try_from(fd).map_err(|_| ProcError::other("pidfd_open"))?;
    // SAFETY: the call made `fd`, owned by nothing else.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// A kqueue that reports `pid`'s exit, or `None` when it already exited
/// (`ESRCH`).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn open_exit(pid: libc::pid_t) -> Result<Option<OwnedFd>, ProcError> {
    // SAFETY: `kqueue` takes nothing and returns a descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(ProcError::of_other(&io::Error::last_os_error()));
    }
    // SAFETY: the call made `kq`, owned by nothing else.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    super::spawn::set_cloexec(&kq).map_err(|error| ProcError::of_other(&error))?;
    let ident = usize::try_from(pid).map_err(|_| ProcError::other("the child's id"))?;
    // SAFETY: a zeroed `kevent` is valid; its fields are set below.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = ident;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: one live change, no event list and no timeout.
    let added = unsafe {
        libc::kevent(
            kq.as_raw_fd(),
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if added < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(ProcError::of_other(&error));
    }
    Ok(Some(kq))
}

/// Whether the exit descriptor `fd` reports the exit now, without
/// waiting: the pidfd is readable.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn exited(fd: &OwnedFd) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live `pollfd`, no timeout.
    let ready = unsafe { libc::poll(&raw mut poll, 1, 0) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready > 0)
}

/// Whether the exit descriptor `fd` reports the exit now, without
/// waiting: the kqueue gives its `NOTE_EXIT` event, taking it.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn exited(fd: &OwnedFd) -> io::Result<bool> {
    // SAFETY: a zeroed `kevent` and `timespec` are valid.
    let (mut event, now): (libc::kevent, libc::timespec) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    // SAFETY: no changes, one live event slot, a zero timeout.
    let got = unsafe {
        libc::kevent(
            fd.as_raw_fd(),
            std::ptr::null(),
            0,
            &raw mut event,
            1,
            &raw const now,
        )
    };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(got > 0)
}

/// A child's exit descriptor on the poller.
#[derive(Debug)]
struct ExitWatch {
    fd: OwnedFd,
    io: IoHandle,
}

impl Drop for ExitWatch {
    fn drop(&mut self) {
        // Then `fd` drops, which closes the descriptor.
        let raw = self.fd.as_raw_fd();
        self.io.deregister(&mut SourceFd(&raw));
    }
}

/// The temporary files of a child's captured output and the capture they
/// join (D-1918).
#[derive(Debug)]
struct Captured {
    capture: Arc<Capture>,
    stdout: Option<Arc<fs::File>>,
    stderr: Option<Arc<fs::File>>,
}

impl Captured {
    /// Leaves the files with the capture, which joins them when the host
    /// takes the test's chunks (D-1922).
    fn keep(self) {
        for (stream, file) in [(Stream::Stdout, self.stdout), (Stream::Stderr, self.stderr)] {
            if let Some(file) = file {
                self.capture.keep_output(stream, file);
            }
        }
    }
}

/// A `Child`'s payload (module documentation). Its fields drop in order:
/// the process first, killed and handed to the reaper if it was not
/// reaped, then its exit descriptor and its pipes (D-1919).
#[derive(Debug)]
pub(crate) struct ChildState {
    process: Process,
    /// `None`: the child exited before the descriptor was made.
    exit: Option<ExitWatch>,
    /// Waits for the exit, one fiber at a time.
    waits: Turn,
    /// `finish` started.
    finishing: AtomicBool,
    /// `finish` ended: every operation is kind 12.
    finished: AtomicBool,
    pub(super) stdin: Arc<WriterState>,
    pub(super) stdout: Arc<ReaderState>,
    pub(super) stderr: Arc<ReaderState>,
    captured: Mutex<Option<Captured>>,
}

impl Drop for ChildState {
    fn drop(&mut self) {
        // D-1922: output that no `wait` or `finish` joined, of a child the
        // reaper takes: the test's capture keeps its files and joins them
        // when the host takes the test's chunks.
        if let Some(captured) = self.captured.get_mut().take() {
            captured.keep();
        }
    }
}

impl HandlePayload for ChildState {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // Dropping the state kills a child not yet reaped, hands it to the
        // reaper, and closes its descriptors (D-1919).
    }
}

impl ChildState {
    /// The state of the started child `spawned`, its descriptors registered
    /// on `runtime`'s poller; `capture` is the starting fiber's capture,
    /// which its captured output joins (D-1918).
    ///
    /// # Errors
    ///
    /// A registration's error: the child is then killed and handed to the
    /// reaper.
    pub(super) fn new(
        runtime: &Shared,
        spawned: Spawned,
        capture: Option<Arc<Capture>>,
    ) -> Result<Self, ProcError> {
        let Spawned {
            process,
            exit,
            pipes,
            captured,
        } = spawned;
        let other = |error: io::Error| ProcError::of_other(&error);
        let exit = match exit {
            Some(fd) => {
                let raw = fd.as_raw_fd();
                let io =
                    IoHandle::register_for(runtime, &mut SourceFd(&raw), mio::Interest::READABLE)
                        .map_err(other)?;
                Some(ExitWatch { fd, io })
            }
            None => None,
        };
        let [stdin, stdout, stderr] = pipes;
        let pipe = |fd: Option<OwnedFd>, dir: Direction| {
            fd.map(|fd| Pipe::new(runtime, fd, dir))
                .transpose()
                .map_err(other)
        };
        let stdin = WriterState::new(pipe(stdin, Direction::Write)?);
        let stdout = ReaderState::new(pipe(stdout, Direction::Read)?);
        let stderr = ReaderState::new(pipe(stderr, Direction::Read)?);
        let [captured_stdout, captured_stderr] = captured;
        let captured = capture.map(|capture| Captured {
            capture,
            stdout: captured_stdout.map(Arc::new),
            stderr: captured_stderr.map(Arc::new),
        });
        Ok(Self {
            process,
            exit,
            waits: Turn::new(),
            finishing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            stdin: Arc::new(stdin),
            stdout: Arc::new(stdout),
            stderr: Arc::new(stderr),
            captured: Mutex::new(captured),
        })
    }

    /// The child's process (for tests).
    #[cfg(test)]
    pub(super) fn process(&self) -> &Process {
        &self.process
    }

    /// Whether a fiber waits on the exit descriptor (for tests).
    #[cfg(test)]
    pub(super) fn exit_armed(&self) -> bool {
        self.exit
            .as_ref()
            .is_some_and(|exit| exit.io.armed(Direction::Read))
    }

    /// Whether `finish` ended.
    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Waits on the poller until the child exits, and reaps it; the caller
    /// holds the turn.
    fn wait_exit(&self, me: &ExecCtx) -> Work<Status> {
        let Some(exit) = &self.exit else {
            return Ok(self
                .process
                .reap()
                .map_err(|error| ProcError::of_other(&error)));
        };
        let reaped = exit.io.wait(me, Direction::Read, &mut || {
            if let Some(status) = self.process.status() {
                return Ok(status);
            }
            if self.process.exit_seen() || exited(&exit.fd)? {
                self.process.see_exit();
                return self.process.reap();
            }
            Err(io::ErrorKind::WouldBlock.into())
        })?;
        Ok(reaped.map_err(|error| ProcError::of_other(&error)))
    }

    /// `wait`'s work (D-1913): the status, at once once reaped; kind 12
    /// after `finish`. A cancelled wait takes nothing.
    pub(super) fn wait(&self, me: &ExecCtx) -> Work<Status> {
        if self.is_finished() {
            return Ok(Err(ProcError::closed()));
        }
        if let Some(status) = self.process.status() {
            return Ok(Ok(status));
        }
        let _turn = match self.waits.take(me) {
            Ok(turn) => turn,
            Err(TurnError::Cancelled) => return Err(Cancelled),
            Err(TurnError::Closed) => return Ok(Err(ProcError::closed())),
        };
        let waited = self.wait_exit(me)?;
        if waited.is_ok() {
            self.join_captured(me)?;
        }
        Ok(waited)
    }

    /// `interrupt`'s, `terminate`'s and `kill`'s work: kind 12 after
    /// `finish`.
    pub(super) fn signal(&self, signal: libc::c_int) -> Result<(), ProcError> {
        if self.is_finished() {
            return Err(ProcError::closed());
        }
        self.process
            .signal(signal)
            .map_err(|error| ProcError::of_other(&error))
    }

    /// The end of `with_child`'s body (module documentation, D-1912).
    /// Later calls do nothing.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when a cancellation reached `me` before or while it
    /// waited: the child was handed to the reaper, and it is finished all
    /// the same.
    pub(super) fn finish(&self, me: &ExecCtx) -> Result<(), Cancelled> {
        if self.finishing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let killed = self.process.signal(libc::SIGKILL);
        let waited = if self.process.is_gone() {
            Ok(())
        } else if killed.is_err() {
            // D-1928: the system refused the kill (a child that changed its
            // user): no wait, which could last for ever; the reaper takes
            // the child, which may outlive `with_child`.
            self.process.abandon();
            Ok(())
        } else {
            match self.waits.take(me) {
                Ok(turn) => {
                    let waited = self.wait_exit(me).map(drop);
                    drop(turn);
                    waited
                }
                Err(TurnError::Cancelled) => Err(Cancelled),
                Err(TurnError::Closed) => Ok(()),
            }
        };
        if waited.is_err() {
            self.process.abandon();
        }
        self.stdin.finish();
        self.stdout.finish();
        self.stderr.finish();
        self.finished.store(true, Ordering::SeqCst);
        self.waits.close();
        match waited {
            Ok(()) if self.process.status().is_some() => self.join_captured(me),
            Ok(()) => Ok(()),
            Err(Cancelled) => Err(Cancelled),
        }
    }

    /// Appends the captured output to the test's capture, once (D-1918):
    /// the files are read on the blocking pool (D-683), and the bytes are
    /// appended on the fiber. A cancellation meanwhile leaves the files with
    /// the capture for the test's end, as for a child the reaper took
    /// (D-1922).
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when a cancellation reached `me` before or during the
    /// read.
    fn join_captured(&self, me: &ExecCtx) -> Result<(), Cancelled> {
        let Some(captured) = self.captured.lock().take() else {
            return Ok(());
        };
        let files = (captured.stdout.clone(), captured.stderr.clone());
        let read = run_blocking(me, move || {
            let (stdout, stderr) = files;
            let read = |file: Option<Arc<fs::File>>| file.map(|file| file_contents(&file));
            (read(stdout), read(stderr))
        });
        let Ok((stdout, stderr)) = read else {
            captured.keep();
            return Err(Cancelled);
        };
        for (stream, bytes) in [(Stream::Stdout, stdout), (Stream::Stderr, stderr)] {
            if let Some(bytes) = bytes {
                captured.capture.append_output(stream, bytes);
            }
        }
        Ok(())
    }
}
