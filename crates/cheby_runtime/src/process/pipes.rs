//! A child's pipes on the poller (D-1914, D-984): the `Reader` and
//! `Writer` handles' states.
//!
//! - **On the poller.** The program's end of each pipe is non-blocking and
//!   registered once on the runtime's poller, in its one direction
//!   (`IoHandle::register_for`), and the child's end stays blocking. A read
//!   or a write tries its system call first and waits on the seam on
//!   `WouldBlock`, as a socket's does (`net.rs`): a cancellation
//!   interrupts the wait at once, and the seam checks for one before every
//!   try (D-670).
//! - **Reads** go through one read-ahead buffer per `Reader`
//!   (`io::LineReader`, as `std::file`'s, D-995), under the reader's turn
//!   (`sched/turn.rs`), so they are served one at a time (D-1001). Bytes a
//!   read took from the pipe go to the buffer first, and the read takes its
//!   line or bytes from the buffer only after a last cancellation check: a
//!   cancelled read has taken nothing, and the bytes stay for the next read
//!   (D-1914).
//! - **Writes** write all of their data under the writer's turn, held
//!   across the waits, so a write is atomic against other writes; a
//!   cancellation between two system calls leaves a prefix written
//!   (D-1000). `EPIPE` is kind 14 (D-1915): the runtime ignores SIGPIPE
//!   (D-925).
//! - **Closing** (`close`, and `finish` for every pipe of the child) takes
//!   the descriptor out of the state, closes the seam's entry and the turn,
//!   which wakes every fiber parked on them with kind 12, and drops the
//!   state's reference to the descriptor's owner ([`PipeFd`]), which
//!   deregisters and closes it once no system call on it runs. It never
//!   waits. A pipe's finalizer drops the same owner (D-1919).
//! - **A stream that is not piped** gives a handle whose every operation is
//!   kind 13 (D-1914), and kind 12 once the child finished.

use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mio::unix::SourceFd;
use parking_lot::Mutex;

use super::error::ProcError;
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::handles::HandlePayload;
use crate::io::LineReader;
use crate::net::BLOCK;
use crate::sched::Shared;
use crate::sched::io::{Direction, IoHandle};
use crate::sched::turn::{Turn, TurnError, TurnGuard};

/// A work function's outcome: a cancellation, or the result.
pub(super) type Work<T> = Result<Result<T, ProcError>, Cancelled>;

/// The owner of a pipe's descriptor: it deregisters the descriptor from the
/// poller, then closes it, when the last reference goes. A system call holds
/// a reference while it runs.
#[derive(Debug)]
pub(super) struct PipeFd {
    file: fs::File,
    /// The descriptor's entry on the poller, for the deregistration.
    io: IoHandle,
}

impl Drop for PipeFd {
    fn drop(&mut self) {
        // Then `file` drops, which closes the descriptor.
        let raw = self.file.as_raw_fd();
        self.io.deregister(&mut SourceFd(&raw));
    }
}

/// The program's end of one pipe, on the poller, with its turn.
#[derive(Debug)]
pub(super) struct Pipe {
    /// The seam's entry: waits, and the wake at a close.
    io: IoHandle,
    /// The descriptor's owner, `None` once closed.
    fd: Mutex<Option<Arc<PipeFd>>>,
    /// Reads, or writes: one direction per pipe.
    turn: Turn,
}

impl Pipe {
    /// Registers `fd`, non-blocking, on `runtime`'s poller for `dir`.
    ///
    /// # Errors
    ///
    /// The registration's error; `fd` is then closed.
    pub(super) fn new(runtime: &Shared, fd: OwnedFd, dir: Direction) -> io::Result<Self> {
        let raw = fd.as_raw_fd();
        let interest = match dir {
            Direction::Read => mio::Interest::READABLE,
            Direction::Write => mio::Interest::WRITABLE,
        };
        let io = IoHandle::register_for(runtime, &mut SourceFd(&raw), interest)?;
        let owner = PipeFd {
            file: fs::File::from(fd),
            io: io.share(),
        };
        Ok(Self {
            io,
            fd: Mutex::new(Some(Arc::new(owner))),
            turn: Turn::new(),
        })
    }

    /// Whether the pipe was closed.
    fn is_closed(&self) -> bool {
        self.fd.lock().is_none()
    }

    /// Takes the pipe's turn for `me`.
    fn take(&self, me: &ExecCtx) -> Result<Result<TurnGuard<'_>, ProcError>, Cancelled> {
        match self.turn.take(me) {
            Ok(turn) => Ok(Ok(turn)),
            Err(TurnError::Cancelled) => Err(Cancelled),
            Err(TurnError::Closed) => Ok(Err(ProcError::closed())),
        }
    }

    /// Runs `op` on the descriptor, waiting on the poller in `dir` while it
    /// would block; an interrupted call is made again. The caller holds the
    /// turn.
    fn wait<T>(
        &self,
        me: &ExecCtx,
        dir: Direction,
        op: &mut dyn FnMut(&fs::File) -> io::Result<T>,
    ) -> Work<T> {
        let outcome = self.io.wait(me, dir, &mut || {
            let owner =
                self.fd.lock().clone().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "the pipe is closed")
                })?;
            loop {
                match op(&owner.file) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    done => return done,
                }
            }
        })?;
        Ok(outcome.map_err(|error| {
            if self.is_closed() {
                ProcError::closed()
            } else {
                ProcError::of_pipe(&error)
            }
        }))
    }

    /// Closes the pipe now: wakes every fiber parked on it with kind 12 and
    /// gives up the state's reference to the descriptor. Never waits.
    /// Whether it was open.
    pub(super) fn close(&self) -> bool {
        let Some(owner) = self.fd.lock().take() else {
            return false;
        };
        self.io.close();
        self.turn.close();
        drop(owner);
        true
    }

    /// The seam's handle (for tests).
    #[cfg(test)]
    pub(super) fn io(&self) -> &IoHandle {
        &self.io
    }
}

/// A `Reader`'s state: the pipe of a child's standard output or error, or
/// none for a stream that is not piped, and its read-ahead buffer.
#[derive(Debug)]
pub(crate) struct ReaderState {
    /// Set by the child's `finish` (D-1912): every operation is then kind
    /// 12.
    finished: AtomicBool,
    pipe: Option<Pipe>,
    /// The bytes read and not yet returned (D-1914). A leaf lock, never
    /// held across a wait.
    buffer: Mutex<LineReader>,
}

impl ReaderState {
    /// The state over `pipe`.
    pub(super) fn new(pipe: Option<Pipe>) -> Self {
        Self {
            finished: AtomicBool::new(false),
            pipe,
            buffer: Mutex::new(LineReader::new()),
        }
    }

    /// The child finished: closes the pipe.
    pub(super) fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        if let Some(pipe) = &self.pipe {
            pipe.close();
        }
    }

    /// Reads until `take` can take what the read wants from the buffer
    /// (module documentation): kind 12 after `finish`, 13 for a stream that
    /// is not piped.
    fn read<T>(
        &self,
        me: &ExecCtx,
        take: &mut dyn FnMut(&mut LineReader) -> Option<io::Result<T>>,
    ) -> Work<T> {
        if self.finished.load(Ordering::SeqCst) {
            return Ok(Err(ProcError::closed()));
        }
        let Some(pipe) = &self.pipe else {
            return Ok(Err(ProcError::invalid_input()));
        };
        let _turn = match pipe.take(me)? {
            Ok(turn) => turn,
            Err(error) => return Ok(Err(error)),
        };
        loop {
            // A cancelled read takes nothing: what the pipe gave stays in
            // the buffer.
            cancel::check(me)?;
            let taken = take(&mut self.buffer.lock());
            if let Some(taken) = taken {
                return Ok(taken.map_err(|error| ProcError::of_pipe(&error)));
            }
            let filled = pipe.wait(me, Direction::Read, &mut |file| {
                let mut input = file;
                self.buffer.lock().read_available(&mut input)
            })?;
            if let Err(error) = filled {
                return Ok(Err(error));
            }
        }
    }

    /// `read_line`'s work: the next line without its ending (D-305), or
    /// `None` at the end.
    pub(super) fn read_line(&self, me: &ExecCtx) -> Work<Option<Vec<u8>>> {
        self.read(me, &mut LineReader::take_line)
    }

    /// `read_bytes`'s work: at least one and at most `limit` bytes (below
    /// 1 counts as 1, D-298), at most the 64 KiB block, or `None` at the
    /// end.
    pub(super) fn read_bytes(&self, me: &ExecCtx, limit: i64) -> Work<Option<Vec<u8>>> {
        let size = usize::try_from(limit.max(1)).map_or(BLOCK, |limit| limit.min(BLOCK));
        self.read(me, &mut |buffer| buffer.take_bytes(size))
    }

    /// `read_all`'s work: every byte to the end of the stream (D-1911).
    pub(super) fn read_all(&self, me: &ExecCtx) -> Work<Vec<u8>> {
        self.read(me, &mut LineReader::take_all)
    }
}

/// A `Writer`'s state: the pipe of a child's standard input, or none for
/// a stream that is not piped.
#[derive(Debug)]
pub(crate) struct WriterState {
    /// Set by the child's `finish` (D-1912).
    finished: AtomicBool,
    pipe: Option<Pipe>,
}

impl WriterState {
    /// The state over `pipe`.
    pub(super) fn new(pipe: Option<Pipe>) -> Self {
        Self {
            finished: AtomicBool::new(false),
            pipe,
        }
    }

    /// The child finished: closes the pipe.
    pub(super) fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        if let Some(pipe) = &self.pipe {
            pipe.close();
        }
    }

    /// The pipe, unless it is not piped (for tests).
    #[cfg(test)]
    pub(super) fn piped(&self) -> Option<&Pipe> {
        self.pipe.as_ref()
    }

    /// The pipe, or the error of an operation on this writer: kind 12
    /// after `finish`, 13 for a stream that is not piped.
    fn pipe(&self) -> Result<&Pipe, ProcError> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(ProcError::closed());
        }
        self.pipe.as_ref().ok_or_else(ProcError::invalid_input)
    }

    /// `write_bytes`'s and `write`'s work: all of `data`, under the turn,
    /// held across the waits (module documentation).
    pub(super) fn write(&self, me: &ExecCtx, data: &[u8]) -> Work<()> {
        let pipe = match self.pipe() {
            Ok(pipe) => pipe,
            Err(error) => return Ok(Err(error)),
        };
        let _turn = match pipe.take(me)? {
            Ok(turn) => turn,
            Err(error) => return Ok(Err(error)),
        };
        let mut rest = data;
        while !rest.is_empty() {
            let written = pipe.wait(me, Direction::Write, &mut |file| {
                let mut output = file;
                output.write(rest)
            })?;
            match written {
                Ok(0) => {
                    return Ok(Err(ProcError::other(
                        "the operating system accepted no bytes",
                    )));
                }
                Ok(count) => rest = rest.get(count..).unwrap_or_default(),
                Err(error) => return Ok(Err(error)),
            }
        }
        Ok(Ok(()))
    }

    /// `close`'s work: closes the pipe now, so the child reads its end.
    ///
    /// # Errors
    ///
    /// Kind 12 after `finish` or a first `close`, 13 for a stream that is
    /// not piped.
    pub(super) fn close(&self) -> Result<(), ProcError> {
        if self.pipe()?.close() {
            Ok(())
        } else {
            Err(ProcError::closed())
        }
    }
}

/// A `Reader` handle's payload: the state it shares with its child and
/// with every other handle of the same stream.
#[derive(Debug)]
pub(crate) struct ReaderHandle(pub(super) Arc<ReaderState>);

impl HandlePayload for ReaderHandle {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // The last reference to the state closes the pipe's descriptor
        // (D-1919).
    }
}

/// A `Writer` handle's payload.
#[derive(Debug)]
pub(crate) struct WriterHandle(pub(super) Arc<WriterState>);

impl HandlePayload for WriterHandle {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // As for a `Reader`.
    }
}
