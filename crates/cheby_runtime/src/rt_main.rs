//! `cheby_rt_main`, where an executable enters the runtime (D-925): the
//! program object's C `main` returns `cheby_rt_main(argc, argv,
//! &cb$image)`. Unix only: executables on Windows are M6's (D-935).
//!
//! It does what Cheby relies on from Rust's own startup, which a C `main`
//! skips: closed standard descriptors are reopened on `/dev/null` (D-474)
//! and `SIGPIPE` is ignored, so a write to a closed pipe fails with
//! `EPIPE` and ends the program with status 1 as under `cheby run`
//! (D-306). Then it installs the image and runs `main` with `cheby run`'s
//! exit policy, and returns the status (D-926): it never calls `exit`
//! (D-709), so libc's `exit` runs after the runtime has stopped.

use std::ffi::{CStr, c_char, c_int};
use std::io::Write as _;

use crate::LEAK_CHECK_ENABLED;
use crate::abi::status;
use crate::entry::{AtExit, RunConfig, run_main};
use crate::image::{self, ProgramImage};

/// The entry of an executable (D-925): reopens closed standard
/// descriptors, ignores `SIGPIPE`, checks the image's version (debug
/// builds only, D-930's runtime ID already ties the archive to the
/// compiler), installs the image, then runs `main` as `cheby run` does
/// and returns its status: 0, 1, 101, 102 (debug builds), 103 or a
/// termination's (D-926). A runtime error that stops the program before
/// `main` runs, such as a bad `CHEBY_THREADS` (D-658), is printed as
/// `error: …` and gives 1.
///
/// The runtime reads `CHEBY_THREADS`, `CHEBY_BACKTRACE` (D-1038, through
/// `Runtime::new`, so a bad value stops the program before `main` as a bad
/// `CHEBY_THREADS` does) and `CHEBY_RT_STATS` (D-927);
/// `env::args()` is `argv[1..]` and `env::program_name()` is `argv[0]`,
/// with invalid UTF-8 replaced (D-347, D-369, D-311). The stacks and
/// `unwind_edges` come from the image (D-660, D-742), and so do the
/// trampolines (D-526).
///
/// # Safety
///
/// `argv` holds `argc` pointers to NUL-terminated strings, as C's `main`
/// receives them, and `image` points to a program image that satisfies
/// [`ProgramImage::new`]'s contract for the life of the process. An
/// executable calls it once, from its main thread; tests call it more
/// often, which only registers the image's call sites again.
#[unsafe(no_mangle)]
#[expect(clippy::similar_names, reason = "C's names for `main`'s parameters")]
pub unsafe extern "C" fn cheby_rt_main(
    argc: c_int,
    argv: *const *const c_char,
    image: *const ProgramImage,
) -> c_int {
    reopen_standard_descriptors();
    ignore_sigpipe();
    // SAFETY: the caller passes `&cb$image`, valid for the process's life.
    let Some(image) = (unsafe { image.as_ref() }) else {
        report(&"the executable has no program image");
        return status::ERR;
    };
    #[cfg(debug_assertions)]
    if let Err(error) = image::check_version(image) {
        report(&error);
        return status::ERR;
    }
    if let Err(error) = image::install(image) {
        report(&error);
        return status::ERR;
    }
    // SAFETY: forwarded from the caller.
    let mut args = unsafe { arguments(argc, argv) }.into_iter();
    let program_name = args.next().unwrap_or_default();
    let config = RunConfig {
        args: args.collect(),
        program_name,
        root_stack_size: usize::try_from(image.main_stack()).unwrap_or(usize::MAX),
        fiber_stack: usize::try_from(image.fiber_stack()).unwrap_or(usize::MAX),
        // `cheby run`'s policy (D-797, D-926): the process exits next, so a
        // build without the leak check freezes the fibers left, and a
        // debug build tears them down for the leak check.
        at_exit: if LEAK_CHECK_ENABLED {
            AtExit::Teardown
        } else {
            AtExit::Freeze
        },
        callbacks: image.callbacks(),
        // `CHEBY_THREADS` decides (D-658).
        threads: None,
        unwind_edges: image.unwind_edges(),
        // The executable's code lives as long as the process.
        code: None,
        ..RunConfig::default()
    };
    // SAFETY: the image's entry is `main`'s entry thunk, generated with the
    // `EntryFn` ABI (the caller's contract), and its code is the
    // executable's own text, valid until the process ends.
    match unsafe { run_main(image.entry(), config) } {
        Ok(code) => code,
        Err(error) => {
            report(&error);
            status::ERR
        }
    }
}

/// Prints `error: <message>` to standard error. A failed write is
/// ignored: there is nowhere else to report it.
fn report(message: &dyn std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "error: {message}");
}

/// The process's arguments, each with invalid UTF-8 replaced (D-311).
///
/// # Safety
///
/// `argv` is null or holds `argc` pointers to NUL-terminated strings.
#[expect(clippy::similar_names, reason = "C's names for `main`'s parameters")]
unsafe fn arguments(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    let count = usize::try_from(argc).unwrap_or(0);
    if argv.is_null() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: `argv` holds `argc` pointers (the caller's contract), and
        // `index < argc`.
        let arg = unsafe { argv.add(index).read() };
        if arg.is_null() {
            break;
        }
        // SAFETY: each pointer of `argv` is a NUL-terminated string that
        // lives as long as the process.
        let text = unsafe { CStr::from_ptr(arg) };
        out.push(String::from_utf8_lossy(text.to_bytes()).into_owned());
    }
    out
}

/// Reopens each of descriptors 0 to 2 that is closed on `/dev/null`
/// (D-474), as Rust's startup does: writes to it then succeed and reads
/// see the end of input. A descriptor that cannot be reopened stays
/// closed, which Rust's standard streams also treat as a null device.
fn reopen_standard_descriptors() {
    for fd in 0..=2 {
        // SAFETY: `F_GETFD` only reads the descriptor's flags; a closed
        // descriptor fails with `EBADF`.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            continue;
        }
        // SAFETY: the path is a NUL-terminated C string literal; `open`
        // has no other precondition.
        let opened = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
        if opened < 0 || opened == fd {
            continue;
        }
        // The lower descriptors are open, so `open` returns `fd` unless
        // another thread of foreign code took it: move the new one there.
        // SAFETY: both are descriptors of this process; `dup2` makes `fd`
        // a copy of `opened`, then the extra one is closed.
        unsafe {
            libc::dup2(opened, fd);
            libc::close(opened);
        }
    }
}

/// Sets `SIGPIPE` to ignored, so a write to a closed pipe or socket
/// fails with `EPIPE` instead of killing the process (D-925).
fn ignore_sigpipe() {
    // SAFETY: `SIG_IGN` installs no handler code; changing a disposition
    // is async-signal-safe and touches no Rust state.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
