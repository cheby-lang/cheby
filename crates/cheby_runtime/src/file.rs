//! The `std::file` kernels (D-985, D-992 to D-997, D-1066, D-1071): file
//! handles, directories, scratch directories and the standard streams as
//! handles.
//!
//! - **Handles** (D-982, D-703): a `File` is runtime kind 45
//!   (`abi::layout::FILE`), a handle object whose payload ([`FileState`])
//!   is either a file on disk ([`Disk`], shared through an `Arc`) or one of
//!   the standard streams. The finalizer drops the payload: a file's
//!   descriptor closes when its last owner goes, ignoring any error, and a
//!   stream handle owns no descriptor, so descriptors 0 to 2 are never
//!   closed (D-1071). Nothing is ever left to write, since writes are not
//!   buffered (D-994).
//! - **Where the work runs** (D-983, D-995, D-1065): opening, closing,
//!   writing, every directory function and a read that empties the
//!   handle's buffer are blocking-pool jobs over Rust data
//!   (`sched::blocking::run_blocking`), and a cancellation meanwhile waits
//!   for the job and releases its result (D-670, D-816). `read_line` and
//!   `read_bytes` are served from the handle's 64 KiB read-ahead buffer
//!   (`io::LineReader`, as standard input's reader is, D-823) on the
//!   calling fiber's thread when it holds what they need. A refill is a
//!   detached job (`run_blocking_claiming`): under the buffer's lock it
//!   reads until the buffer holds a line (or some bytes), then claims its
//!   fiber's wait and only then takes the line, so a cancellation that wins
//!   the wait first is delivered at once, and the refill's bytes stay in
//!   the buffer, which the handle owns; the refill holds its own reference
//!   to the handle's [`Disk`]. The next read waits for a refill still
//!   running, since it needs the buffer's lock: the fast path only tries
//!   the lock (a worker never waits on it), and a read that cannot have it
//!   goes to the pool, whose job waits for it (D-1065).
//! - **One operation at a time** (`file.md`, "Several fibers, one file"):
//!   every operation on a handle takes the handle's turn (`sched/turn.rs`),
//!   a FIFO lock over fibers, and holds it until it returns, so operations
//!   run one at a time, in the order they reach the handle. A cancelled
//!   read releases the turn as it unwinds; the refill it abandoned keeps
//!   only the buffer's lock.
//! - **Closing** (D-993): `close` closes the descriptor on the pool and
//!   reports `close`'s error (on Unix through `libc::close`; an
//!   interrupted close is a success, since the descriptor is gone). Every
//!   later operation on the handle is kind 12. The handle of a standard
//!   stream is only marked closed.
//! - **Errors** (D-981, D-484): each kernel returns `Err((kind, message))`
//!   ([`FileError`]): 1 not found, 2 permission denied, 3 invalid UTF-8
//!   (a line or a name), 4 already exists, 5 is a directory (also a
//!   directory opened for reading, which Unix allows, and the `EPERM` or
//!   access denied of macOS and Windows for a directory), 6 not a
//!   directory, 7 directory not empty, 12 closed, 13 invalid input (a path
//!   with U+0000, before any job, or a handle in the wrong mode), and every
//!   other failure 0 with the system's message.
//! - **Scratch directories** (D-1066, D-1067): `sys_create_temp_dir`
//!   creates `cheby-<16 hex digits>` with mode 0700 under the run's
//!   temporary root ([`set_temp_root`]) or the host's temporary directory.
//!   [`TestRoot`] is `cheby test`'s root for a run.
//! - **Standard streams** (D-1071): reads of a `stdin()` handle go through
//!   `io`'s one standard-input reader, after a flush of standard output;
//!   writes to `stdout()` go through standard output's buffer, then a
//!   flush, and writes to `stderr()` at once (`io::write_std_stream`).
//! - **Without a scheduler** (D-714, D-983): the jobs run inline, so
//!   `std::file` works on Windows until M6.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::abi::Word;
use crate::capture::Stream;
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::fiber_ops::running;
use crate::handles::{self, HandleKind, HandlePayload};
use crate::io::{
    LineReader, err_pair, os_message, read_stdin_bytes, read_stdin_line, stream_failure,
    write_std_stream,
};
use crate::panic::fatal;
use crate::sched::blocking::{run_blocking, run_blocking_claiming};
use crate::sched::turn::{Turn, TurnError};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::{list, string, values};

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["file"];

/// The kind code of every other failure, with the system's message.
const KIND_OTHER: i64 = 0;
/// `io::is_not_found` (tier 1's kind 1).
const KIND_NOT_FOUND: i64 = 1;
/// `io::is_permission_denied` (tier 1's kind 2).
const KIND_PERMISSION_DENIED: i64 = 2;
/// `io::is_invalid_data` (tier 1's kind 3, D-311).
const KIND_INVALID_DATA: i64 = 3;
/// `io::is_already_exists` (D-981).
const KIND_ALREADY_EXISTS: i64 = 4;
/// `io::is_a_directory` (D-981).
const KIND_IS_A_DIRECTORY: i64 = 5;
/// `io::is_not_a_directory` (D-981).
const KIND_NOT_A_DIRECTORY: i64 = 6;
/// `io::is_directory_not_empty` (D-981).
const KIND_DIRECTORY_NOT_EMPTY: i64 = 7;
/// `io::is_closed` (D-981).
const KIND_CLOSED: i64 = 12;
/// `io::is_invalid_input` (D-981).
const KIND_INVALID_INPUT: i64 = 13;

/// The message of a kind whose text `std::io` fixes (D-345): it never
/// reaches the program.
const FIXED: &str = "";

/// How `sys_open` opens a file (D-992, D-1066): its `mode` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `open`: reading, from the start (0).
    Read,
    /// `create`: writing, created or truncated (1).
    Create,
    /// `append`: writing at the end, created if missing (2).
    Append,
    /// `create_new`: writing, created only if nothing exists (3).
    CreateNew,
}

impl Mode {
    /// The mode of `sys_open`'s argument.
    fn of(mode: i64) -> Self {
        match mode {
            0 => Self::Read,
            1 => Self::Create,
            2 => Self::Append,
            3 => Self::CreateNew,
            other => fatal(format_args!("`file::sys_open` got the mode {other}")),
        }
    }
}

/// The operation whose failure is mapped, where the mapping depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// An opener.
    Open,
    /// `remove_file`.
    RemoveFile,
    /// `create_dir_all`: something other than a directory on the path is
    /// kind 4 (`file.md`).
    CreateDirAll,
    /// Any other operation.
    Other,
}

/// A failed file operation as Rust data: its kind code (D-981) and, for
/// kind 0 only, the system's message. A pool job may make it; the fiber
/// turns it into the `Err` pair ([`FileError::word`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileError {
    pub(crate) kind: i64,
    pub(crate) message: String,
}

impl FileError {
    /// A failure of a kind with a predicate, whose text `std::io` fixes.
    fn fixed(kind: i64) -> Self {
        Self {
            kind,
            message: FIXED.to_owned(),
        }
    }

    /// The error of an operation on a closed handle.
    fn closed() -> Self {
        Self::fixed(KIND_CLOSED)
    }

    /// The error of a handle used in a mode it was not opened for, or of a
    /// path with U+0000.
    fn invalid_input() -> Self {
        Self::fixed(KIND_INVALID_INPUT)
    }

    /// The kind of an IO error, before any operation's own mapping.
    fn kind_of(error: &io::Error) -> i64 {
        match error.kind() {
            io::ErrorKind::NotFound => KIND_NOT_FOUND,
            io::ErrorKind::PermissionDenied => KIND_PERMISSION_DENIED,
            io::ErrorKind::InvalidData => KIND_INVALID_DATA,
            io::ErrorKind::AlreadyExists => KIND_ALREADY_EXISTS,
            io::ErrorKind::IsADirectory => KIND_IS_A_DIRECTORY,
            io::ErrorKind::NotADirectory => KIND_NOT_A_DIRECTORY,
            io::ErrorKind::DirectoryNotEmpty => KIND_DIRECTORY_NOT_EMPTY,
            _ => KIND_OTHER,
        }
    }

    /// The failure of `op` on `path` (module documentation). macOS answers
    /// `unlink` of a directory with `EPERM`, and Windows refuses to open a
    /// directory with "access denied": both are kind 5 on every platform
    /// (`file.md`). The directory test is a system call, so it runs in the
    /// job, with the operation.
    fn of_path(error: &io::Error, path: &str, op: Op) -> Self {
        let kind = match (Self::kind_of(error), op) {
            (KIND_PERMISSION_DENIED, Op::Open | Op::RemoveFile)
                if fs::symlink_metadata(path).is_ok_and(|found| found.is_dir()) =>
            {
                KIND_IS_A_DIRECTORY
            }
            (KIND_NOT_A_DIRECTORY, Op::CreateDirAll) => KIND_ALREADY_EXISTS,
            (kind, _) => kind,
        };
        Self::of_kind(kind, error)
    }

    /// The failure of an operation on an open handle's descriptor.
    fn of_io(error: &io::Error) -> Self {
        Self::of_kind(Self::kind_of(error), error)
    }

    /// `kind`, with `error`'s message only for kind 0.
    fn of_kind(kind: i64, error: &io::Error) -> Self {
        if kind == KIND_OTHER {
            Self {
                kind,
                message: os_message(error),
            }
        } else {
            Self::fixed(kind)
        }
    }

    /// The failure of a standard stream, as `std::io`'s kernels report it.
    fn of_stream(error: &io::Error) -> Self {
        let (kind, message) = stream_failure(error);
        Self { kind, message }
    }

    /// The `Err` pair, made on the fiber.
    fn word(&self) -> Word {
        err_pair(self.kind, &self.message)
    }
}

/// A file on disk: the descriptor and the read-ahead buffer under one
/// lock, shared by the handle and the jobs that work on it, so a refill
/// that a cancellation abandoned keeps the file open until it returns
/// (D-1065).
#[derive(Debug)]
pub(crate) struct Disk {
    /// Opened for reading (`open`), else for writing.
    reads: bool,
    /// Set by the close job, under the lock, once the descriptor is gone.
    closed: AtomicBool,
    /// The descriptor and the buffer. A leaf lock: a pool thread holds it
    /// while it reads or writes, and a fiber only tries it.
    inner: Mutex<Inner>,
}

/// What [`Disk`]'s lock guards.
#[derive(Debug)]
struct Inner {
    /// The descriptor, `None` once closed.
    file: Option<fs::File>,
    /// The read-ahead buffer of a handle opened for reading (D-995).
    buffer: LineReader,
    /// The path the handle was opened with, for the tests' hooks.
    #[cfg(test)]
    path: String,
}

/// Which standard stream a handle of `sys_std_stream` stands for
/// (D-1071).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdStream {
    Stdin,
    Stdout,
    Stderr,
}

/// What a `File` handle is: the flag of D-1071 on kind 45.
#[derive(Debug)]
enum Target {
    /// A file on disk.
    Disk(Arc<Disk>),
    /// A standard stream, with this handle's own closed flag.
    Std {
        stream: StdStream,
        closed: AtomicBool,
    },
}

/// A `File`'s payload (module documentation).
#[derive(Debug)]
pub(crate) struct FileState {
    /// Operations run one at a time, in arrival order.
    turn: Turn,
    target: Target,
}

impl FileState {
    /// The state of a file opened in `mode`.
    fn disk(file: fs::File, mode: Mode, path: &str) -> Self {
        let _ = path;
        Self {
            turn: Turn::new(),
            target: Target::Disk(Arc::new(Disk {
                reads: mode == Mode::Read,
                closed: AtomicBool::new(false),
                inner: Mutex::new(Inner {
                    file: Some(file),
                    buffer: LineReader::new(),
                    #[cfg(test)]
                    path: path.to_owned(),
                }),
            })),
        }
    }

    /// A new handle's state for `stream`.
    fn std(stream: StdStream) -> Self {
        Self {
            turn: Turn::new(),
            target: Target::Std {
                stream,
                closed: AtomicBool::new(false),
            },
        }
    }

    /// Whether the handle was closed.
    fn is_closed(&self) -> bool {
        match &self.target {
            Target::Disk(disk) => disk.closed.load(Ordering::SeqCst),
            Target::Std { closed, .. } => closed.load(Ordering::SeqCst),
        }
    }

    /// Takes the handle's turn for `me` (module documentation), or the
    /// closed handle's error.
    fn take(&self, me: &ExecCtx) -> Result<Result<TurnGuardOf<'_>, FileError>, Cancelled> {
        match self.turn.take(me) {
            Ok(turn) if self.is_closed() => {
                drop(turn);
                Ok(Err(FileError::closed()))
            }
            Ok(turn) => Ok(Ok(turn)),
            Err(TurnError::Cancelled) => Err(Cancelled),
            Err(TurnError::Closed) => Ok(Err(FileError::closed())),
        }
    }
}

/// The turn of a handle, held for one operation.
type TurnGuardOf<'a> = crate::sched::turn::TurnGuard<'a>;

impl HandlePayload for FileState {
    fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {
        // Dropping the state drops the handle's reference to its `Disk`:
        // the descriptor closes with the last one, ignoring any error
        // (D-982); a refill still running holds its own. A stream's handle
        // owns no descriptor (D-1071).
    }
}

/// What a read returns: `Some` bytes (a line without its ending, or a
/// block), or `None` at the end.
type Taken = Option<Vec<u8>>;

/// A work function's outcome: a cancellation, or the result.
type Work<T> = Result<Result<T, FileError>, Cancelled>;

/// `open`, `create`, `append` and `create_new`'s work (D-992, D-1066): kind
/// 13 for a path with U+0000 before any job; the open on the pool.
pub(crate) fn open_on(me: &ExecCtx, path: &str, mode: i64) -> Work<FileState> {
    let mode = Mode::of(mode);
    if path.contains('\0') {
        return Ok(Err(FileError::invalid_input()));
    }
    let path = path.to_owned();
    run_blocking(me, move || open_file(&path, mode))
}

/// The blocking-pool job of an opener. A directory opened for reading,
/// which Unix allows, is kind 5 (`file.md`).
fn open_file(path: &str, mode: Mode) -> Result<FileState, FileError> {
    #[cfg(test)]
    hooks::pause(path, "open");
    let mut options = fs::OpenOptions::new();
    match mode {
        Mode::Read => options.read(true),
        Mode::Create => options.write(true).create(true).truncate(true),
        Mode::Append => options.append(true).create(true),
        Mode::CreateNew => options.write(true).create_new(true),
    };
    let file = options
        .open(path)
        .map_err(|error| FileError::of_path(&error, path, Op::Open))?;
    if mode == Mode::Read && file.metadata().is_ok_and(|found| found.is_dir()) {
        return Err(FileError::fixed(KIND_IS_A_DIRECTORY));
    }
    Ok(FileState::disk(file, mode, path))
}

/// `close`'s work (D-993): kind 12 for a closed handle; a file's descriptor
/// closed on the pool, with its error; a stream's handle only marked.
pub(crate) fn close_on(me: &ExecCtx, state: &FileState) -> Work<()> {
    let _turn = match state.take(me)? {
        Ok(turn) => turn,
        Err(error) => return Ok(Err(error)),
    };
    match &state.target {
        Target::Std { closed, .. } => {
            closed.store(true, Ordering::SeqCst);
            Ok(Ok(()))
        }
        Target::Disk(disk) => {
            let disk = Arc::clone(disk);
            run_blocking(me, move || close_disk(&disk))
        }
    }
}

/// The blocking-pool job of `close`: takes the descriptor out under the
/// lock, which waits for a refill still running, marks the handle closed
/// and closes the descriptor.
fn close_disk(disk: &Disk) -> Result<(), FileError> {
    let mut inner = disk.inner.lock();
    #[cfg(test)]
    hooks::pause(&inner.path, "close");
    let Some(file) = inner.file.take() else {
        return Err(FileError::closed());
    };
    disk.closed.store(true, Ordering::SeqCst);
    drop(inner);
    close_descriptor(file).map_err(|error| FileError::of_io(&error))
}

/// Closes `file`'s descriptor and reports the system's error (D-993). An
/// interrupted `close` has closed the descriptor all the same.
#[cfg(unix)]
fn close_descriptor(file: fs::File) -> io::Result<()> {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: `fd` is the descriptor `file` owned, given up by
    // `into_raw_fd`, so it is closed once, here.
    if unsafe { libc::close(fd) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(error)
    }
}

/// Closes `file`; Windows reports nothing useful on closing a handle.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix close, which reports an error"
)]
fn close_descriptor(file: fs::File) -> io::Result<()> {
    drop(file);
    Ok(())
}

/// What a read takes from the buffer: a line, or at most `limit` bytes.
#[derive(Clone, Copy, Debug)]
enum Want {
    Line,
    Bytes(usize),
}

impl Want {
    /// What the buffer already holds of it, without reading.
    fn buffered(self, buffer: &mut LineReader) -> Option<Vec<u8>> {
        match self {
            Self::Line => buffer.buffered_line(),
            Self::Bytes(limit) => buffer.buffered_bytes(limit),
        }
    }

    /// Reads `input` until the buffer can give it (a refill).
    fn fill(self, buffer: &mut LineReader, input: &mut impl io::Read) {
        match self {
            Self::Line => buffer.fill_line(input),
            Self::Bytes(_) => buffer.fill_some(input),
        }
    }

    /// Takes it from the buffer, reading `input` if it must.
    fn take(self, buffer: &mut LineReader, input: &mut impl io::Read) -> io::Result<Taken> {
        match self {
            Self::Line => buffer.read_line(input),
            Self::Bytes(limit) => buffer.read_bytes(input, limit),
        }
    }
}

/// `read_line`'s and `read_bytes`'s work (D-995, D-1065, D-1071): kind 12
/// for a closed handle, 13 for one not opened for reading; standard input
/// through `io`'s reader; a file's buffer on the fiber when it holds what
/// the read wants, else a detached refill (module documentation).
fn read_on(me: &ExecCtx, state: &FileState, want: Want) -> Work<Taken> {
    let _turn = match state.take(me)? {
        Ok(turn) => turn,
        Err(error) => return Ok(Err(error)),
    };
    let disk = match &state.target {
        Target::Std {
            stream: StdStream::Stdin,
            ..
        } => {
            let read = match want {
                Want::Line => read_stdin_line(me)?,
                Want::Bytes(limit) => read_stdin_bytes(me, limit)?,
            };
            return Ok(read.map_err(|error| FileError::of_stream(&error)));
        }
        Target::Std { .. } => return Ok(Err(FileError::invalid_input())),
        Target::Disk(disk) if !disk.reads => return Ok(Err(FileError::invalid_input())),
        Target::Disk(disk) => disk,
    };
    // The fast path (D-995, as D-823's): a cancellation pending sends the
    // read to the pool, which raises before any job.
    if cancel::check(me).is_ok()
        && let Some(mut inner) = disk.inner.try_lock()
        && let Some(found) = want.buffered(&mut inner.buffer)
    {
        return Ok(Ok(Some(found)));
    }
    let disk = Arc::clone(disk);
    let taken = run_blocking_claiming(me, move |claim| {
        let mut inner = disk.inner.lock();
        #[cfg(test)]
        hooks::pause(&inner.path, "refill");
        let Inner { file, buffer, .. } = &mut *inner;
        let Some(file) = file.as_ref() else {
            // Closed by a close that ran before this abandoned refill.
            return claim.claim().then(|| Err(FileError::closed()));
        };
        let mut input = file;
        // GAP(file-refill-failure-kept): as D-1166 has it for standard
        // input, the end of the file or a read error that an abandoned
        // refill meets stays in the buffer for the next read.
        want.fill(buffer, &mut input);
        // A line or bytes go to this read only once it has claimed its
        // wait; otherwise they stay in the buffer for the next read.
        claim.claim().then(|| {
            want.take(buffer, &mut input)
                .map_err(|e| FileError::of_io(&e))
        })
    })?;
    match taken {
        Some(read) => Ok(read),
        None => fatal(format_args!(
            "a refill of a file's buffer returned to its read and took nothing"
        )),
    }
}

/// `read_line`'s work: the next line without its ending (D-305).
pub(crate) fn read_line_on(me: &ExecCtx, state: &FileState) -> Work<Taken> {
    read_on(me, state, Want::Line)
}

/// `read_bytes`'s work: at least one and at most `limit` bytes (below 1
/// counts as 1, D-298), at most what the buffer holds (D-1062).
pub(crate) fn read_bytes_on(me: &ExecCtx, state: &FileState, limit: i64) -> Work<Taken> {
    let limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    read_on(me, state, Want::Bytes(limit))
}

/// `write`'s and `write_bytes`'s work (D-994, D-1071): kind 12 for a
/// closed handle, 13 for one opened for reading or standard input; the
/// whole of `data` as one pool job, which a cancellation waits for; a
/// standard stream's write on the fiber.
pub(crate) fn write_on(me: &ExecCtx, state: &FileState, data: Vec<u8>) -> Work<()> {
    let _turn = match state.take(me)? {
        Ok(turn) => turn,
        Err(error) => return Ok(Err(error)),
    };
    let disk = match &state.target {
        Target::Std {
            stream: StdStream::Stdin,
            ..
        } => return Ok(Err(FileError::invalid_input())),
        Target::Std {
            stream: StdStream::Stdout,
            ..
        } => return Ok(written(write_std_stream(Stream::Stdout, &data))),
        Target::Std {
            stream: StdStream::Stderr,
            ..
        } => return Ok(written(write_std_stream(Stream::Stderr, &data))),
        Target::Disk(disk) if disk.reads => return Ok(Err(FileError::invalid_input())),
        Target::Disk(disk) => Arc::clone(disk),
    };
    run_blocking(me, move || {
        let inner = disk.inner.lock();
        #[cfg(test)]
        hooks::pause(&inner.path, "write");
        match inner.file.as_ref() {
            Some(file) => {
                let mut output = file;
                output
                    .write_all(&data)
                    .map_err(|error| FileError::of_io(&error))
            }
            None => Err(FileError::closed()),
        }
    })
}

/// The result of a standard stream's write.
fn written(result: io::Result<()>) -> Result<(), FileError> {
    result.map_err(|error| FileError::of_stream(&error))
}

/// Runs `job` on the pool for a directory function on `path`, after the
/// U+0000 check (D-981).
fn on_path<T: Send + 'static>(
    me: &ExecCtx,
    path: &str,
    job: impl FnOnce(&str) -> Result<T, FileError> + Send + 'static,
) -> Work<T> {
    if path.contains('\0') {
        return Ok(Err(FileError::invalid_input()));
    }
    let path = path.to_owned();
    run_blocking(me, move || job(&path))
}

/// `list_dir`'s work (D-997): the entry names sorted by their UTF-8
/// bytes, without `.` and `..`; a name that is not UTF-8 is kind 3.
pub(crate) fn list_dir_on(me: &ExecCtx, path: &str) -> Work<Vec<String>> {
    on_path(me, path, |path| {
        let other = |error: io::Error| FileError::of_path(&error, path, Op::Other);
        let mut names = Vec::new();
        for entry in fs::read_dir(path).map_err(other)? {
            let name = entry.map_err(other)?.file_name();
            match name.into_string() {
                Ok(name) => names.push(name),
                Err(_) => return Err(FileError::fixed(KIND_INVALID_DATA)),
            }
        }
        names.sort_unstable();
        Ok(names)
    })
}

/// `create_dir`'s work: the parent must exist.
pub(crate) fn create_dir_on(me: &ExecCtx, path: &str) -> Work<()> {
    on_path(me, path, |path| {
        new_dir(Path::new(path), false).map_err(|error| FileError::of_path(&error, path, Op::Other))
    })
}

/// `create_dir_all`'s work: every missing directory; one that exists at
/// `path` is not an error.
pub(crate) fn create_dir_all_on(me: &ExecCtx, path: &str) -> Work<()> {
    on_path(me, path, |path| {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder
            .create(path)
            .map_err(|error| FileError::of_path(&error, path, Op::CreateDirAll))
    })
}

/// `remove_file`'s work.
pub(crate) fn remove_file_on(me: &ExecCtx, path: &str) -> Work<()> {
    on_path(me, path, |path| {
        fs::remove_file(path).map_err(|error| FileError::of_path(&error, path, Op::RemoveFile))
    })
}

/// `remove_dir`'s work: an empty directory.
pub(crate) fn remove_dir_on(me: &ExecCtx, path: &str) -> Work<()> {
    on_path(me, path, |path| {
        fs::remove_dir(path).map_err(|error| FileError::of_path(&error, path, Op::Other))
    })
}

/// `remove_dir_all`'s work (D-1066): Rust's `remove_dir_all`, which never
/// follows a symbolic link, inside the tree or at `path` (a link there is
/// removed itself), and fails on a missing path or on a file.
pub(crate) fn remove_dir_all_on(me: &ExecCtx, path: &str) -> Work<()> {
    on_path(me, path, |path| {
        remove_tree(Path::new(path)).map_err(|error| FileError::of_path(&error, path, Op::Other))
    })
}

/// `remove_dir_all`'s system calls: a symbolic link at `path` is removed
/// itself (on Windows a link to a directory is one), anything else that is
/// not a directory is `ENOTDIR`'s kind, and a directory goes with all it
/// holds, never through a link.
fn remove_tree(path: &Path) -> io::Result<()> {
    let found = fs::symlink_metadata(path)?;
    if found.file_type().is_symlink() {
        return fs::remove_file(path).or_else(|error| match error.kind() {
            // A Windows link to a directory is removed as a directory.
            io::ErrorKind::PermissionDenied | io::ErrorKind::IsADirectory => fs::remove_dir(path),
            _ => Err(error),
        });
    }
    if !found.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    fs::remove_dir_all(path)
}

/// `rename`'s work: a file at `new_path` is replaced.
pub(crate) fn rename_on(me: &ExecCtx, old_path: &str, new_path: &str) -> Work<()> {
    if new_path.contains('\0') {
        return Ok(Err(FileError::invalid_input()));
    }
    let new_path = new_path.to_owned();
    on_path(me, old_path, move |old_path| {
        fs::rename(old_path, &new_path)
            .map_err(|error| FileError::of_path(&error, old_path, Op::Other))
    })
}

/// What `info` found (D-996): the size in bytes, the kind (1 a regular
/// file, 2 a directory, 0 anything else) and the modification time as
/// whole seconds since the Unix epoch and the nanoseconds after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Info {
    pub(crate) size: i64,
    pub(crate) kind: i64,
    pub(crate) seconds: i64,
    pub(crate) nanoseconds: i64,
}

impl Info {
    /// The modification time in nanoseconds since the Unix epoch, as
    /// `file::info` builds its `time::Timestamp` from it; saturated outside
    /// the 584 years an `Int` spans.
    pub(crate) fn since_epoch_nanoseconds(self) -> i64 {
        self.seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(self.nanoseconds)
    }
}

/// `info`'s work: the metadata of `path`, following symbolic links.
pub(crate) fn info_on(me: &ExecCtx, path: &str) -> Work<Info> {
    on_path(me, path, |path| {
        let other = |error: io::Error| FileError::of_path(&error, path, Op::Other);
        let found = fs::metadata(path).map_err(other)?;
        let kind = if found.is_file() {
            1
        } else if found.is_dir() {
            2
        } else {
            0
        };
        let (seconds, nanoseconds) = since_epoch(found.modified().map_err(other)?);
        Ok(Info {
            size: i64::try_from(found.len()).unwrap_or(i64::MAX),
            kind,
            seconds,
            nanoseconds,
        })
    })
}

/// `time` as whole seconds since the Unix epoch and the nanoseconds after
/// them, from 0 to `999_999_999`, so a time before 1970 has negative
/// seconds (as `time::sys_now`, D-1021).
fn since_epoch(time: std::time::SystemTime) -> (i64, i64) {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => (
            i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            i64::from(after.subsec_nanos()),
        ),
        Err(before) => {
            let before = before.duration();
            let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            match before.subsec_nanos() {
                0 => (seconds.saturating_neg(), 0),
                nanos => (
                    seconds.saturating_neg().saturating_sub(1),
                    1_000_000_000_i64.saturating_sub(i64::from(nanos)),
                ),
            }
        }
    }
}

/// The temporary root of a `cheby test` run (D-1067), under which every
/// `file::with_temp_dir` creates its directory; `None`, the default, means
/// the host's temporary directory.
static TEMP_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Sets the temporary root of a `cheby test` run (D-1067), under which
/// every `file::with_temp_dir` creates its directory; `None` (the default)
/// means the host's temporary directory. GAP(test-temp-root-handoff):
/// `cheby test` sets it through [`TestRoot`] before any test runs.
pub fn set_temp_root(root: Option<PathBuf>) {
    *TEMP_ROOT.lock() = root;
}

/// The directory a scratch directory is created under now.
fn temp_parent() -> PathBuf {
    TEMP_ROOT.lock().clone().unwrap_or_else(std::env::temp_dir)
}

/// How often a random name is tried before the creation gives up.
const NAME_TRIES: u32 = 64;

/// Creates a new directory `<prefix><16 lower-case hex digits>` under
/// `parent`, with mode 0700 on Unix, retrying on a name that exists
/// (GAP(file-temp-names)).
fn new_scratch_dir(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..NAME_TRIES {
        let bits = getrandom::u64().map_err(|error| io::Error::other(error.to_string()))?;
        let path = parent.join(format!("{prefix}{bits:016x}"));
        match new_dir(&path, true) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Creates the directory `path`, with mode 0700 when `private` and the
/// default 0777 before the umask otherwise (GAP(file-create-modes)).
fn new_dir(path: &Path, private: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(if private { 0o700 } else { 0o777 })
            .create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = private;
        fs::DirBuilder::new().create(path)
    }
}

/// `with_temp_dir`'s creation (D-1066): a new, empty directory under the
/// run's root or the host's temporary directory; its path in the host's
/// form.
pub(crate) fn create_temp_dir_on(me: &ExecCtx) -> Work<String> {
    run_blocking(me, || {
        let parent = temp_parent();
        let created = new_scratch_dir(&parent, "cheby-")
            .map_err(|error| FileError::of_path(&error, &parent.to_string_lossy(), Op::Other))?;
        created.into_os_string().into_string().map_err(|_| {
            // A path the program cannot hold (D-311): removed again.
            FileError::fixed(KIND_INVALID_DATA)
        })
    })
}

/// The temporary root of one `cheby test` run (D-1067): created under the
/// host's temporary directory as `cheby-test-<16 hex digits>` with mode
/// 0700, given to the runtime ([`set_temp_root`]), and removed with
/// everything in it when the last guard of the process goes, after failed
/// tests too (GAP(test-temp-root-handoff)). Runs that overlap in one
/// process, such as the driver's own tests, share one root.
#[derive(Debug)]
pub struct TestRoot {
    _private: (),
}

/// The process's run root and how many [`TestRoot`]s hold it.
static RUN_ROOT: Mutex<(Option<PathBuf>, usize)> = Mutex::new((None, 0));

impl TestRoot {
    /// Creates the run's root, or joins the one a run of this process
    /// already holds.
    ///
    /// # Errors
    ///
    /// The creation's IO error; the runtime then keeps the host's
    /// temporary directory.
    pub fn create() -> io::Result<Self> {
        let mut root = RUN_ROOT.lock();
        if root.1 == 0 {
            let path = new_scratch_dir(&std::env::temp_dir(), "cheby-test-")?;
            set_temp_root(Some(path.clone()));
            root.0 = Some(path);
        }
        root.1 = root.1.saturating_add(1);
        Ok(Self { _private: () })
    }

    /// The root's path while a guard holds it.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        RUN_ROOT.lock().0.clone()
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let mut root = RUN_ROOT.lock();
        root.1 = root.1.saturating_sub(1);
        if root.1 == 0
            && let Some(path) = root.0.take()
        {
            set_temp_root(None);
            // What a test left behind goes with the root; a failure leaves
            // it in the host's temporary directory.
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// The running fiber of a kernel's `ctx`.
///
/// # Safety
///
/// `ctx` is the running fiber's control block.
unsafe fn fiber<'a>(ctx: *mut ExecCtx) -> &'a ExecCtx {
    // SAFETY: forwarded from the caller.
    unsafe { running(ctx) }
}

/// The text of a borrowed `String` argument.
///
/// # Safety
///
/// `text` is the uniform word of a live `String`, borrowed.
unsafe fn str_arg<'a>(text: Word) -> &'a str {
    // SAFETY: forwarded from the caller.
    unsafe { string::as_str_unchecked(text) }
        .unwrap_or_else(|| fatal(format_args!("{text:#x} is not a String")))
}

/// The payload of a borrowed `File`.
///
/// # Safety
///
/// `file` is a live `File` handle, borrowed for `'a`.
unsafe fn file_of<'a>(file: Word) -> &'a FileState {
    // SAFETY: forwarded from the caller.
    unsafe { handles::payload::<FileState>(file, HandleKind::File) }
}

/// The outcome of a kernel's work as its `Result` word: a cancellation
/// raises (D-670), a failure is the `Err` pair, and `ok` builds the value.
fn outcome<T>(ctx: *mut ExecCtx, result: Work<T>, ok: impl FnOnce(T) -> Word) -> Word {
    match result {
        Ok(Ok(value)) => values::ok(ok(value)),
        Ok(Err(error)) => error.word(),
        Err(Cancelled) => raise_cancel(Some(ctx)),
    }
}

/// A new `File` handle owning `state`.
///
/// # Safety
///
/// `ctx` is null or the running fiber's context.
unsafe fn new_file(ctx: *mut ExecCtx, state: FileState) -> Word {
    // SAFETY: forwarded from the caller.
    unsafe { handles::new_handle(ctx, HandleKind::File, Box::new(state)) }
}

/// `Some(text)` for a line that is UTF-8, else the invalid-data pair; the
/// line was consumed either way (D-311).
fn line_word(ctx: *mut ExecCtx, result: Work<Taken>) -> Word {
    match result {
        Ok(Ok(Some(line))) => match std::str::from_utf8(&line) {
            Ok(text) => values::ok(values::some(values::string(text))),
            Err(_) => FileError::fixed(KIND_INVALID_DATA).word(),
        },
        other => outcome(ctx, other, |_| values::none()),
    }
}

/// The body of a directory function's kernel over one path, which the
/// kernel runs in `guarded` (D-697): `work` with the running fiber and the
/// path, `Ok(Nil)` on success.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
unsafe fn path_kernel(ctx: *mut ExecCtx, path: Word, work: fn(&ExecCtx, &str) -> Work<()>) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { fiber(ctx) };
    // SAFETY: forwarded from the caller.
    let path = unsafe { str_arg(path) };
    outcome(ctx, work(me, path), |()| values::nil())
}

/// `file::open`, `create`, `append` and `create_new`'s kernel
/// (`sys_open(path, mode)`, D-1066): a new `File`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_open(ctx: *mut ExecCtx, path: Word, mode: i64) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let path = unsafe { str_arg(path) };
        outcome(ctx, open_on(me, path, mode), |state| {
            // SAFETY: the running fiber's context.
            unsafe { new_file(ctx, state) }
        })
    })
}

/// `file::close`'s kernel (D-993).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `file` is a live `File`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_close(ctx: *mut ExecCtx, file: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let state = unsafe { file_of(file) };
        outcome(ctx, close_on(me, state), |()| values::nil())
    })
}

/// `file::read_line`'s kernel: `Some(line)` without its ending, or `None`
/// at the end (D-305, D-311).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `file` is a live `File`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_read_line(ctx: *mut ExecCtx, file: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let state = unsafe { file_of(file) };
        line_word(ctx, read_line_on(me, state))
    })
}

/// `file::read_bytes`'s kernel: `Some(Bytes)`, or `None` at the end
/// (D-1062).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `file` is a live `File`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_read_bytes(
    ctx: *mut ExecCtx,
    file: Word,
    limit: i64,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let state = unsafe { file_of(file) };
        outcome(ctx, read_bytes_on(me, state, limit), |data| match data {
            Some(data) => values::some(values::bytes(&data)),
            None => values::none(),
        })
    })
}

/// `file::write`'s and `write_line`'s kernel: `text` as UTF-8 (D-994).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `file` is a live `File` and
/// `text` a live `String`, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_write(ctx: *mut ExecCtx, file: Word, text: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let state = unsafe { file_of(file) };
        // SAFETY: forwarded from the caller.
        let data = unsafe { str_arg(text) }.as_bytes().to_vec();
        outcome(ctx, write_on(me, state, data), |()| values::nil())
    })
}

/// `file::write_bytes`'s kernel: `data` unchanged (D-994).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `file` is a live `File` and
/// `data` a live `Bytes`, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_write_bytes(
    ctx: *mut ExecCtx,
    file: Word,
    data: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let state = unsafe { file_of(file) };
        // SAFETY: forwarded from the caller.
        let data = unsafe { values::bytes_of(data) }.to_vec();
        outcome(ctx, write_on(me, state, data), |()| values::nil())
    })
}

/// `file::list_dir`'s kernel: the sorted names (D-997).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_list_dir(ctx: *mut ExecCtx, path: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let path = unsafe { str_arg(path) };
        outcome(ctx, list_dir_on(me, path), |names| {
            list::from_iter(names.iter().map(|name| values::string(name)))
        })
    })
}

/// `file::create_dir`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_create_dir(ctx: *mut ExecCtx, path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        path_kernel(ctx, path, create_dir_on)
    })
}

/// `file::create_dir_all`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_create_dir_all(ctx: *mut ExecCtx, path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        path_kernel(ctx, path, create_dir_all_on)
    })
}

/// `file::remove_file`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_remove_file(ctx: *mut ExecCtx, path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        path_kernel(ctx, path, remove_file_on)
    })
}

/// `file::remove_dir`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_remove_dir(ctx: *mut ExecCtx, path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        path_kernel(ctx, path, remove_dir_on)
    })
}

/// `file::remove_dir_all`'s kernel (D-1066).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_remove_dir_all(ctx: *mut ExecCtx, path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        path_kernel(ctx, path, remove_dir_all_on)
    })
}

/// `file::rename`'s kernel.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `old_path` and `new_path` are
/// live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_rename(
    ctx: *mut ExecCtx,
    old_path: Word,
    new_path: Word,
) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let (old_path, new_path) = unsafe { (str_arg(old_path), str_arg(new_path)) };
        outcome(ctx, rename_on(me, old_path, new_path), |()| values::nil())
    })
}

/// `file::info`'s kernel: `(size, kind, nanoseconds since the epoch)`
/// ([`Info`]); a flat tuple of three, since a longer or nested one draws
/// W0001 in std's own declaration (D-243).
///
/// # Safety
///
/// `ctx` is the running fiber's context; `path` is a live `String`,
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_info(ctx: *mut ExecCtx, path: Word) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        // SAFETY: forwarded from the caller.
        let path = unsafe { str_arg(path) };
        outcome(ctx, info_on(me, path), |found| {
            values::tuple(&[
                values::int_word(found.size),
                values::int_word(found.kind),
                values::int_word(found.since_epoch_nanoseconds()),
            ])
        })
    })
}

/// `file::with_temp_dir`'s kernel (D-1066): a new, empty directory's path.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_file_sys_create_temp_dir(ctx: *mut ExecCtx) -> Word {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { fiber(ctx) };
        outcome(ctx, create_temp_dir_on(me), |path| values::string(&path))
    })
}

/// `file::stdin`, `stdout` and `stderr`'s kernel (D-1071): a new handle
/// for stream `which` (0 standard input, 1 standard output, 2 standard
/// error). It neither waits nor fails.
#[unsafe(no_mangle)]
pub extern "C" fn cheby_file_sys_std_stream(which: i64) -> Word {
    guarded(None, || {
        let stream = match which {
            0 => StdStream::Stdin,
            1 => StdStream::Stdout,
            2 => StdStream::Stderr,
            other => fatal(format_args!(
                "`file::sys_std_stream` got the stream {other}"
            )),
        };
        // SAFETY: a null context charges the running fiber's domain.
        unsafe { new_file(std::ptr::null_mut(), FileState::std(stream)) }
    })
}

/// One kernel: symbol, address, C parameter types after the context
/// pointer (if any), C result type, and whether it takes the context
/// (`@std_context`, D-692).
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
    bool,
);

/// The `std::file` kernels: every one that suspends takes the running
/// fiber's context; `sys_std_stream` neither suspends nor takes it.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one entry per kernel, sixteen of them"
)]
fn all() -> [Entry; 16] {
    const W: &str = "Word";
    const I: &str = "i64";
    [
        (
            "cheby_file_sys_open",
            cheby_file_sys_open as *const u8,
            &[W, I],
            W,
            true,
        ),
        (
            "cheby_file_sys_close",
            cheby_file_sys_close as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_read_line",
            cheby_file_sys_read_line as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_read_bytes",
            cheby_file_sys_read_bytes as *const u8,
            &[W, I],
            W,
            true,
        ),
        (
            "cheby_file_sys_write",
            cheby_file_sys_write as *const u8,
            &[W, W],
            W,
            true,
        ),
        (
            "cheby_file_sys_write_bytes",
            cheby_file_sys_write_bytes as *const u8,
            &[W, W],
            W,
            true,
        ),
        (
            "cheby_file_sys_list_dir",
            cheby_file_sys_list_dir as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_create_dir",
            cheby_file_sys_create_dir as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_create_dir_all",
            cheby_file_sys_create_dir_all as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_remove_file",
            cheby_file_sys_remove_file as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_remove_dir",
            cheby_file_sys_remove_dir as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_remove_dir_all",
            cheby_file_sys_remove_dir_all as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_rename",
            cheby_file_sys_rename as *const u8,
            &[W, W],
            W,
            true,
        ),
        (
            "cheby_file_sys_info",
            cheby_file_sys_info as *const u8,
            &[W],
            W,
            true,
        ),
        (
            "cheby_file_sys_create_temp_dir",
            cheby_file_sys_create_temp_dir as *const u8,
            &[],
            W,
            true,
        ),
        (
            "cheby_file_sys_std_stream",
            cheby_file_sys_std_stream as *const u8,
            &[I],
            W,
            false,
        ),
    ]
}

/// Appends the `std::file` kernels, with their addresses, to `out` (plan
/// §4.3.3), and registers kind 45 (D-982).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    handles::register();
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// A kernel's symbol, C parameter types after the context pointer (if
/// any), C result type, and whether it takes the context.
#[cfg(test)]
pub(crate) type Signature = (&'static str, &'static [&'static str], &'static str, bool);

/// The C signatures of every `std::file` kernel after the context pointer,
/// and whether it takes one, for the cross-check against `std/` in
/// `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<Signature> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3, kernel.4))
        .collect()
}

/// Test hooks of the pool jobs, keyed by path and operation in a static
/// (D-686: no thread-local storage outside `tls`): a hooked job waits on a
/// gate the test opens, and the hook records that the job started and
/// that it went on.
#[cfg(test)]
pub(crate) mod hooks {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use parking_lot::{Condvar, Mutex};

    /// One hooked job.
    #[derive(Debug, Default)]
    pub(crate) struct JobHook {
        pub(crate) path: String,
        pub(crate) operation: &'static str,
        /// Opened by the test; the job waits for it.
        gate: Mutex<bool>,
        opened: Condvar,
        /// The job reached the hook.
        pub(crate) started: AtomicBool,
        /// The job went on past the hook.
        pub(crate) passed: AtomicBool,
        /// How many jobs went on past the hook.
        pub(crate) passes: AtomicUsize,
    }

    impl JobHook {
        /// A closed hook for `operation` (`open`, `close`, `write` or
        /// `refill`) on `path`.
        pub(crate) fn new(path: &str, operation: &'static str) -> Arc<Self> {
            Arc::new(Self {
                path: path.to_owned(),
                operation,
                ..Self::default()
            })
        }

        /// Opens the gate.
        pub(crate) fn open(&self) {
            *self.gate.lock() = true;
            self.opened.notify_all();
        }

        /// The job's side: waits for the gate.
        fn wait(&self) {
            self.started.store(true, Ordering::SeqCst);
            let mut open = self.gate.lock();
            while !*open {
                self.opened.wait(&mut open);
            }
            self.passed.store(true, Ordering::SeqCst);
            self.passes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The installed hooks.
    static HOOKS: Mutex<Vec<Arc<JobHook>>> = Mutex::new(Vec::new());

    /// Installs `hook` until [`remove`].
    pub(crate) fn install(hook: &Arc<JobHook>) {
        HOOKS.lock().push(Arc::clone(hook));
    }

    /// Removes `hook`.
    pub(crate) fn remove(hook: &Arc<JobHook>) {
        HOOKS.lock().retain(|known| !Arc::ptr_eq(known, hook));
    }

    /// The job of `operation` on `path` waits for its hook's gate, if a
    /// hook is installed.
    pub(crate) fn pause(path: &str, operation: &str) {
        let hook = HOOKS
            .lock()
            .iter()
            .find(|hook| hook.path == path && hook.operation == operation)
            .cloned();
        if let Some(hook) = hook {
            hook.wait();
        }
    }
}

#[cfg(test)]
#[path = "file_tests.rs"]
mod tests;
