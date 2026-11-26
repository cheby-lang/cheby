//! Starting a child (D-1908, D-1909, D-1916): the description checked on
//! the fiber, then the start as one blocking-pool job over Rust data.
//!
//! - **Checks** ([`Spec::check`]), before anything starts: kind 13 for
//!   U+0000 in the program, an argument, the directory, a variable's name
//!   or value, for an empty program, for a name that is empty or holds
//!   `=`, and for a stream code out of range or a file code without its
//!   file. A closed `File` is kind 12, found on the fiber and again under
//!   the file's lock in the job.
//! - **The program** ([`resolve`]): one without `/` is searched in the
//!   `PATH` the child gets (`/usr/bin:/bin` when it gets none, as
//!   `execvp` does); a relative one with `/` is made absolute against the
//!   program's own working directory, so `dir` does not change what it
//!   names. Found and not executable is kind 2, not found kind 1.
//! - **The environment**: the program's variables, or none after
//!   `with_empty_env`, with the edits applied in order.
//! - **Streams**: inherited, the null device, a pipe whose child end stays
//!   blocking and whose program end is non-blocking, a copy of a `File`'s
//!   descriptor (moved back over its read-ahead for standard input,
//!   `Disk::child_descriptor`), or standard output's place for a merged
//!   standard error. Under `cheby test` an inherited standard output or
//!   error is an unlinked temporary file under the run's temporary root
//!   ([`capture_file`], D-1918). Every descriptor the runtime makes is
//!   close-on-exec.
//! - **The start** gives the child descriptors 0 to 2 only, the default
//!   action of SIGPIPE and an empty signal mask, in the program's process
//!   group (D-1916). On macOS it is `posix_spawn` with
//!   `POSIX_SPAWN_CLOEXEC_DEFAULT`, which closes every other descriptor in
//!   the child, so a descriptor another thread made without an atomic
//!   close-on-exec flag (`pipe`, `socket` and `accept` there) never
//!   leaks into it. On Linux every descriptor the runtime makes is
//!   close-on-exec atomically, and Rust's `Command` starts the child, which
//!   marks every descriptor from 3 up close-on-exec with `close_range`
//!   before `exec` (D-1923). Only SIGPIPE gets its default action back:
//!   a signal the program ignores stays ignored, as POSIX has it (D-1925).
//!   Rust's `Command` resets SIGPIPE and the mask and no other
//!   disposition, and `posix_spawn` has `POSIX_SPAWN_SETSIGDEF` with
//!   SIGPIPE alone.
//! - **Standard input** (D-1921): a child that inherits it first gets the
//!   bytes the program's reader of standard input read ahead and did not
//!   take (`io::give_back_stdin`), where the descriptor can seek; where it
//!   cannot, they stay with the program.
//! - **A file without `#!`** (D-1926): one the system refuses with
//!   `ENOEXEC` runs as `/bin/sh path args...`, as POSIX has `execvp` do:
//!   glibc's `execvp`, which Rust's `Command` calls, does it on Linux,
//!   and the start retries so on macOS, where `posix_spawn` does not.
//! - **SIGCHLD** (D-1927): before the process's first child starts, an
//!   inherited "ignore" action of SIGCHLD is set back to the default, so
//!   the system keeps each child until the runtime reaps it.
//! - **The exit descriptor**: `pidfd_open` on Linux, which needs Linux 5.3
//!   (an `ENOSYS` kills and reaps the child and fails the start), and a
//!   kqueue with `EVFILT_PROC` and `NOTE_EXIT` elsewhere, where `ESRCH`
//!   means that the child already exited (`child.rs`).

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::child::{Process, exit_descriptor};
use super::error::ProcError;
use super::registry;
use crate::file::{ChildStream, Disk};

/// The `PATH` searched when the child gets none, as `execvp` has it.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// Where one of a child's streams goes: `sys_start`'s codes (D-1909).
#[derive(Debug)]
pub(super) enum Slot {
    /// 0: the program's own stream.
    Inherit,
    /// 1: the null device.
    Discard,
    /// 2: a pipe the program reads or writes.
    Piped,
    /// 3: an open `File`.
    File(ChildStream),
    /// 4, standard error only: wherever standard output goes.
    SameAsStdout,
}

impl Slot {
    /// The slot of `code` and its file for stream `index` (0 to 2).
    ///
    /// # Errors
    ///
    /// Kind 13 for a code out of range, 4 for another stream than standard
    /// error, or 3 without a file.
    pub(super) fn of(
        code: i64,
        file: Option<ChildStream>,
        index: usize,
    ) -> Result<Self, ProcError> {
        match (code, file) {
            (0, _) => Ok(Self::Inherit),
            (1, _) => Ok(Self::Discard),
            (2, _) => Ok(Self::Piped),
            (3, Some(file)) => Ok(Self::File(file)),
            (4, _) if index == 2 => Ok(Self::SameAsStdout),
            _ => Err(ProcError::invalid_input()),
        }
    }
}

/// A child to start, as Rust data the start's job owns (D-683).
#[derive(Debug)]
pub(super) struct Spec {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) dir: Option<String>,
    pub(super) clear_env: bool,
    /// The edits of the environment, in order; `None` removes a variable.
    pub(super) env: Vec<(String, Option<String>)>,
    /// Standard input, output and error.
    pub(super) slots: [Slot; 3],
    /// The running fiber's output is captured (D-1918): inherited output
    /// goes to temporary files under `temp_parent`.
    pub(super) capture: bool,
    pub(super) temp_parent: PathBuf,
    /// The key of the runtime that starts the child, for the open-children
    /// registry (D-1917, `process::owner_of`).
    pub(super) owner: u64,
}

impl Spec {
    /// The checks made before anything starts (module documentation).
    ///
    /// # Errors
    ///
    /// Kind 13.
    pub(super) fn check(&self) -> Result<(), ProcError> {
        let mut texts = std::iter::once(&self.program)
            .chain(&self.args)
            .chain(&self.dir)
            .chain(self.env.iter().map(|(name, _)| name))
            .chain(self.env.iter().filter_map(|(_, value)| value.as_ref()));
        if self.program.is_empty()
            || texts.any(|text| text.contains('\0'))
            || self
                .env
                .iter()
                .any(|(name, _)| name.is_empty() || name.contains('='))
        {
            return Err(ProcError::invalid_input());
        }
        Ok(())
    }

    /// The child's environment: the program's, or none, with the edits.
    fn environment(&self) -> Vec<(OsString, OsString)> {
        let mut vars: Vec<(OsString, OsString)> = if self.clear_env {
            Vec::new()
        } else {
            std::env::vars_os().collect()
        };
        for (name, value) in &self.env {
            vars.retain(|(known, _)| known.as_os_str() != OsStr::new(name));
            if let Some(value) = value {
                vars.push((OsString::from(name), OsString::from(value)));
            }
        }
        vars
    }
}

/// A started child as the job hands it to the fiber: the process (whose
/// drop kills it and hands it to the reaper, so a start that a
/// cancellation abandoned leaves no child, D-1916), its exit descriptor,
/// the program's ends of its pipes and the temporary files of its captured
/// output.
#[derive(Debug)]
pub(super) struct Spawned {
    pub(super) process: Process,
    /// `None` when the child exited before the descriptor was made.
    pub(super) exit: Option<OwnedFd>,
    /// The program's ends of the pipes of standard input, output and error.
    pub(super) pipes: [Option<OwnedFd>; 3],
    /// The temporary files of captured standard output and error.
    pub(super) captured: [Option<fs::File>; 2],
}

/// What one of the child's descriptors 0 to 2 is.
#[derive(Debug)]
enum ChildFd {
    /// That same descriptor of the program, inherited.
    Same,
    /// A descriptor of the runtime, made the child's.
    Own(OwnedFd),
}

/// The start's job (module documentation).
///
/// # Errors
///
/// The start's failure (D-1916).
pub(super) fn spawn(spec: &Spec) -> Result<Spawned, ProcError> {
    registry::default_ignored_sigchld();
    let env = spec.environment();
    let path = resolve(&spec.program, &env).map_err(|error| ProcError::of_start(&error))?;
    let mut captured: [Option<fs::File>; 2] = [None, None];
    let mut pipes: [Option<OwnedFd>; 3] = [None, None, None];
    let [stdin, stdout, stderr] = &spec.slots;
    let other = |error: io::Error| ProcError::of_other(&error);
    let child_stdin = match stdin {
        Slot::Inherit | Slot::SameAsStdout => ChildFd::Same,
        Slot::Discard => ChildFd::Own(null_device(false).map_err(other)?),
        Slot::Piped => {
            let (read, write) = pipe().map_err(other)?;
            set_nonblocking(&write).map_err(other)?;
            pipes[0] = Some(write);
            ChildFd::Own(read)
        }
        Slot::File(file) => file_fd(file, 0, spec, &mut captured)?,
    };
    let child_stdout = output_fd(stdout, 1, spec, &mut captured, &mut pipes)?;
    let child_stderr = match stderr {
        Slot::SameAsStdout => match &child_stdout {
            ChildFd::Same => ChildFd::Own(duplicate(1).map_err(other)?),
            ChildFd::Own(fd) => ChildFd::Own(fd.try_clone().map_err(other)?),
        },
        slot @ (Slot::Inherit | Slot::Discard | Slot::Piped | Slot::File(_)) => {
            output_fd(slot, 2, spec, &mut captured, &mut pipes)?
        }
    };
    if matches!(child_stdin, ChildFd::Same) {
        // D-1921: the child first gets the bytes the program's reader of
        // standard input read ahead, where the descriptor can seek.
        crate::io::give_back_stdin();
    }
    let Some(starting) = registry::begin_start(spec.owner) else {
        return Err(ProcError::other("the program is ending"));
    };
    let pid = start(
        &path,
        spec,
        &env,
        [&child_stdin, &child_stdout, &child_stderr],
    )
    .map_err(|error| ProcError::of_start(&error))?;
    let process = Process::new(pid);
    if !registry::opened(pid, spec.owner) {
        // D-1917: the run ended while this start was on the blocking pool:
        // the child is killed at once and left to the reaper.
        process.abandon();
        return Err(ProcError::other("the program is ending"));
    }
    drop(starting);
    // The child's ends close here, in the program.
    drop((child_stdin, child_stdout, child_stderr));
    let exit = exit_descriptor(&process)?;
    Ok(Spawned {
        process,
        exit,
        pipes,
        captured,
    })
}

/// The child's descriptor `index` (1 or 2) for an output `slot`.
fn output_fd(
    slot: &Slot,
    index: usize,
    spec: &Spec,
    captured: &mut [Option<fs::File>; 2],
    pipes: &mut [Option<OwnedFd>; 3],
) -> Result<ChildFd, ProcError> {
    let other = |error: io::Error| ProcError::of_other(&error);
    match slot {
        Slot::Inherit | Slot::SameAsStdout => inherited(index, index, spec, captured),
        Slot::Discard => Ok(ChildFd::Own(null_device(true).map_err(other)?)),
        Slot::Piped => {
            let (read, write) = pipe().map_err(other)?;
            set_nonblocking(&read).map_err(other)?;
            if let Some(end) = pipes.get_mut(index) {
                *end = Some(read);
            }
            Ok(ChildFd::Own(write))
        }
        Slot::File(file) => file_fd(file, index, spec, captured),
    }
}

/// The program's own stream `stream` (0 to 2) as the child's descriptor
/// `index`: for captured output (D-1918), that stream's temporary file;
/// else the same descriptor when the numbers agree, or a copy, so that no
/// descriptor of the child is set from one that an earlier action changed.
fn inherited(
    stream: usize,
    index: usize,
    spec: &Spec,
    captured: &mut [Option<fs::File>; 2],
) -> Result<ChildFd, ProcError> {
    let other = |error: io::Error| ProcError::of_other(&error);
    if spec.capture
        && let Some(slot) = stream.checked_sub(1).and_then(|at| captured.get_mut(at))
    {
        let file = match slot {
            Some(file) => file,
            None => slot.insert(capture_file(&spec.temp_parent).map_err(other)?),
        };
        return Ok(ChildFd::Own(OwnedFd::from(
            file.try_clone().map_err(other)?,
        )));
    }
    if stream == index {
        return Ok(ChildFd::Same);
    }
    let raw = libc::c_int::try_from(stream).map_err(|_| ProcError::invalid_input())?;
    Ok(ChildFd::Own(duplicate(raw).map_err(other)?))
}

/// The child's descriptor `index` for a `File` (D-1909): a standard
/// stream's handle is that stream of the program, as `inherited()` is
/// (D-1924). A handle that reads, standard input's included, given as
/// standard output or error, or one that writes given as standard input,
/// is kind 13 (D-1924).
fn file_fd(
    file: &ChildStream,
    index: usize,
    spec: &Spec,
    captured: &mut [Option<fs::File>; 2],
) -> Result<ChildFd, ProcError> {
    match file {
        ChildStream::Std(stream) => {
            let stream = usize::try_from(*stream).map_err(|_| ProcError::invalid_input())?;
            if (stream == 0) != (index == 0) {
                return Err(ProcError::invalid_input());
            }
            inherited(stream, index, spec, captured)
        }
        ChildStream::Disk(disk) => disk_fd(disk, index == 0),
    }
}

/// A copy of a file's descriptor (`Disk::child_descriptor`).
fn disk_fd(disk: &Arc<Disk>, input: bool) -> Result<ChildFd, ProcError> {
    disk.child_descriptor(input)
        .map(|file| ChildFd::Own(OwnedFd::from(file)))
        .map_err(ProcError::from)
}

/// The program to run: a path the program's own working directory does
/// not change (module documentation).
///
/// # Errors
///
/// `NotFound` or `PermissionDenied` from the search, or the working
/// directory's error.
pub(super) fn resolve(program: &str, env: &[(OsString, OsString)]) -> io::Result<PathBuf> {
    if program.starts_with('/') {
        return Ok(PathBuf::from(program));
    }
    let cwd = std::env::current_dir()?;
    if program.contains('/') {
        return Ok(cwd.join(program));
    }
    let search = env
        .iter()
        .find(|(name, _)| name.as_os_str() == OsStr::new("PATH"))
        .map_or_else(|| OsString::from(DEFAULT_PATH), |(_, value)| value.clone());
    let mut denied = false;
    for dir in search.as_bytes().split(|&byte| byte == b':') {
        let dir = if dir.is_empty() {
            Path::new(".")
        } else {
            Path::new(OsStr::from_bytes(dir))
        };
        let candidate = cwd.join(dir).join(program);
        if fs::metadata(&candidate).is_ok_and(|found| found.is_file()) {
            if executable(&candidate) {
                return Ok(candidate);
            }
            denied = true;
        }
    }
    Err(io::Error::from(if denied {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::NotFound
    }))
}

/// Whether the program may execute `path`.
fn executable(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

/// A new pipe, both ends close-on-exec and blocking: `(read, write)`.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: `fds` holds the two descriptors the call writes.
    let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as above.
    let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    let [read, write] = fds;
    // SAFETY: the call made both descriptors, owned by nothing else.
    let ends = unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // Not atomic: only this platform's `posix_spawn` with
        // `POSIX_SPAWN_CLOEXEC_DEFAULT` starts children (module
        // documentation).
        set_cloexec(&ends.0)?;
        set_cloexec(&ends.1)?;
    }
    Ok(ends)
}

/// Sets `FD_CLOEXEC` on `fd`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) fn set_cloexec(fd: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: `fcntl` on a live descriptor, with no pointer.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets `O_NONBLOCK` on `fd`, the program's end of a pipe.
fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `fcntl` on a live descriptor, with no pointer.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    // SAFETY: as above.
    if flags == -1 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A close-on-exec copy of the program's descriptor `raw`.
fn duplicate(raw: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: `fcntl` with no pointer; a bad descriptor fails.
    let copy = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
    if copy == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call made `copy`, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// The null device, opened for writing or for reading.
fn null_device(write: bool) -> io::Result<OwnedFd> {
    let file = fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .open("/dev/null")?;
    Ok(OwnedFd::from(file))
}

/// A new temporary file for a child's captured output (D-1918), already
/// unlinked: `cheby-output-<16 hex digits>` under `parent`, mode 0600,
/// named as scratch directories are (`file::new_random_name`).
///
/// # Errors
///
/// The creation's or the unlink's error.
pub(super) fn capture_file(parent: &Path) -> io::Result<fs::File> {
    let (path, file) = crate::file::new_random_name(parent, "cheby-output-", |path| {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    })?;
    fs::remove_file(&path)?;
    Ok(file)
}

/// The arguments and the environment as C strings: `argv` starts with the
/// program as given.
#[cfg(target_vendor = "apple")]
fn c_strings(
    spec: &Spec,
    env: &[(OsString, OsString)],
) -> io::Result<(Vec<CString>, Vec<CString>)> {
    let invalid = |_| io::Error::from(io::ErrorKind::InvalidInput);
    let argv = std::iter::once(&spec.program)
        .chain(&spec.args)
        .map(|arg| CString::new(arg.as_bytes()).map_err(invalid))
        .collect::<io::Result<Vec<_>>>()?;
    let envp = env
        .iter()
        .map(|(name, value)| {
            let mut entry = name.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).map_err(invalid)
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok((argv, envp))
}

/// A null-terminated array of pointers to `strings`, valid while they
/// live.
#[cfg(target_vendor = "apple")]
fn pointers(strings: &[CString]) -> Vec<*mut libc::c_char> {
    strings
        .iter()
        .map(|text| text.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect()
}

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    /// Apple's file action that keeps a descriptor in a child started with
    /// `POSIX_SPAWN_CLOEXEC_DEFAULT` (`<spawn.h>`, macOS 10.7).
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

/// The result of a `posix_spawn` family call: 0 or the error number.
#[cfg(target_vendor = "apple")]
fn spawn_call(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// The file actions and attributes of one `posix_spawn`, destroyed on
/// drop.
#[cfg(target_vendor = "apple")]
struct SpawnSetup {
    actions: libc::posix_spawn_file_actions_t,
    attr: libc::posix_spawnattr_t,
}

#[cfg(target_vendor = "apple")]
impl Drop for SpawnSetup {
    fn drop(&mut self) {
        // SAFETY: both were initialized by `SpawnSetup::new` and are
        // destroyed once, here.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.actions);
            libc::posix_spawnattr_destroy(&raw mut self.attr);
        }
    }
}

#[cfg(target_vendor = "apple")]
impl SpawnSetup {
    /// Initialized actions and attributes.
    fn new() -> io::Result<Self> {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        // SAFETY: `actions` is a live slot the call initializes.
        spawn_call(unsafe { libc::posix_spawn_file_actions_init(&raw mut actions) })?;
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        // SAFETY: `attr` is a live slot the call initializes.
        if let Err(error) = spawn_call(unsafe { libc::posix_spawnattr_init(&raw mut attr) }) {
            // SAFETY: initialized above, destroyed once.
            unsafe { libc::posix_spawn_file_actions_destroy(&raw mut actions) };
            return Err(error);
        }
        Ok(Self { actions, attr })
    }
}

/// Starts the child (module documentation): its process id.
#[cfg(target_vendor = "apple")]
fn start(
    path: &Path,
    spec: &Spec,
    env: &[(OsString, OsString)],
    fds: [&ChildFd; 3],
) -> io::Result<libc::pid_t> {
    let (argv, envp) = c_strings(spec, env)?;
    let invalid = |_| io::Error::from(io::ErrorKind::InvalidInput);
    let path = CString::new(path.as_os_str().as_bytes()).map_err(invalid)?;
    let dir = spec
        .dir
        .as_ref()
        .map(|dir| CString::new(dir.as_bytes()).map_err(invalid))
        .transpose()?;
    let mut setup = SpawnSetup::new()?;
    let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
        | libc::POSIX_SPAWN_SETSIGMASK
        | libc::POSIX_SPAWN_SETSIGDEF;
    let flags = libc::c_short::try_from(flags).map_err(|_| io::Error::other("spawn flags"))?;
    // SAFETY: the attributes and actions are initialized and live; the
    // signal sets are live locals the calls read; every descriptor named is
    // live until the spawn returns; every string outlives the spawn.
    unsafe {
        spawn_call(libc::posix_spawnattr_setflags(&raw mut setup.attr, flags))?;
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut mask);
        spawn_call(libc::posix_spawnattr_setsigmask(
            &raw mut setup.attr,
            &raw const mask,
        ))?;
        let mut defaults: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut defaults);
        libc::sigaddset(&raw mut defaults, libc::SIGPIPE);
        spawn_call(libc::posix_spawnattr_setsigdefault(
            &raw mut setup.attr,
            &raw const defaults,
        ))?;
        for (target, fd) in (0..).zip(fds) {
            spawn_call(match fd {
                ChildFd::Same => {
                    posix_spawn_file_actions_addinherit_np(&raw mut setup.actions, target)
                }
                ChildFd::Own(fd) => libc::posix_spawn_file_actions_adddup2(
                    &raw mut setup.actions,
                    fd.as_raw_fd(),
                    target,
                ),
            })?;
        }
        if let Some(dir) = &dir {
            spawn_call(libc::posix_spawn_file_actions_addchdir_np(
                &raw mut setup.actions,
                dir.as_ptr(),
            ))?;
        }
        let arg_ptrs = pointers(&argv);
        let envp = pointers(&envp);
        let mut pid: libc::pid_t = 0;
        let started = spawn_call(libc::posix_spawn(
            &raw mut pid,
            path.as_ptr(),
            &raw const setup.actions,
            &raw const setup.attr,
            arg_ptrs.as_ptr(),
            envp.as_ptr(),
        ));
        match started {
            // D-1926: a file the system refuses to execute, such as a
            // script without `#!`, runs through `/bin/sh` with its path and
            // arguments, as glibc's `execvp` does: `/bin/sh path args...`.
            Err(error) if error.raw_os_error() == Some(libc::ENOEXEC) => {
                let shell = c"/bin/sh";
                let script: Vec<CString> = [shell.to_owned(), path]
                    .into_iter()
                    .chain(argv.iter().skip(1).cloned())
                    .collect();
                let script = pointers(&script);
                spawn_call(libc::posix_spawn(
                    &raw mut pid,
                    shell.as_ptr(),
                    &raw const setup.actions,
                    &raw const setup.attr,
                    script.as_ptr(),
                    envp.as_ptr(),
                ))?;
            }
            other => other?,
        }
        Ok(pid)
    }
}

/// The child's step before `exec` on Linux (D-1923): marks every
/// descriptor from 3 up close-on-exec with `close_range`, so descriptors
/// that foreign code opened without the flag stay out of the child. Any
/// failure, `ENOSYS` and `EINVAL` before Linux 5.11 among them, is that
/// fallback: only the runtime's descriptors, all close-on-exec, are kept
/// out, and the start goes on. Async-signal-safe: one system call.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature `CommandExt::pre_exec` takes"
)]
fn close_on_exec_from_3() -> io::Result<()> {
    let first: libc::c_uint = 3;
    // SAFETY: `close_range` takes three integers and touches no memory of
    // the caller.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    Ok(())
}

/// Starts the child (module documentation): its process id.
#[cfg(not(target_vendor = "apple"))]
fn start(
    path: &Path,
    spec: &Spec,
    env: &[(OsString, OsString)],
    fds: [&ChildFd; 3],
) -> io::Result<libc::pid_t> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let to_stdio = |fd: &ChildFd| -> io::Result<Stdio> {
        Ok(match fd {
            ChildFd::Same => Stdio::inherit(),
            ChildFd::Own(fd) => Stdio::from(fd.try_clone()?),
        })
    };
    let [stdin, stdout, stderr] = fds;
    let mut command = Command::new(path);
    command
        .arg0(&spec.program)
        .args(&spec.args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(to_stdio(stdin)?)
        .stdout(to_stdio(stdout)?)
        .stderr(to_stdio(stderr)?);
    if let Some(dir) = &spec.dir {
        command.current_dir(dir);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: the hook runs in the child between `fork` and `exec` and
    // makes one raw system call, which is async-signal-safe, and reads
    // `errno`; it allocates nothing and takes no lock.
    unsafe {
        command.pre_exec(close_on_exec_from_3);
    }
    let child = command.spawn()?;
    libc::pid_t::try_from(child.id()).map_err(|_| io::Error::other("the child's id"))
}
