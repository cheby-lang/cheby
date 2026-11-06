//! Tests of `cheby_rt_main` (D-925, D-926, D-927): synthetic entry
//! thunks written in Rust stand for `main`, and the tests that need a
//! process of their own re-execute this test binary.

use std::ffi::{CString, c_char, c_int};
use std::process::Command;

use crate::ExecCtx;
use crate::abi::{self, PanicKind, status};
use crate::alloc::tests::serial;
use crate::entry::{Callbacks, DEFAULT_ROOT_STACK_SIZE, DEFAULT_STACK_SIZE, EntryFn};
use crate::entry::{program_args, program_name};
use crate::image::{ImageParts, ProgramImage, Table};
use crate::panic::cheby_rt_panic;
use crate::rt_main::cheby_rt_main;
use crate::string::tests::StaticImage;

/// Selects what [`rt_main_child`] does in a child process.
const CHILD: &str = "CHEBY_RT_TEST_RT_MAIN_CHILD";

unsafe extern "C" fn returns_0(_: *mut ExecCtx) -> u64 {
    0
}

/// `main` returning `Err(e)` after printing it.
unsafe extern "C" fn returns_1(_: *mut ExecCtx) -> u64 {
    1
}

/// 0 when the program's arguments and name are what
/// [`rt_main_passes_arguments_and_the_program_name`] passes, else 1.
unsafe extern "C" fn reads_its_program(_: *mut ExecCtx) -> u64 {
    u64::from(program_args() != ["a", "b\u{fffd}c"] || program_name() != "./prog")
}

unsafe extern "C" fn panics(ctx: *mut ExecCtx) -> u64 {
    // Leaked, so no thread-local is needed (D-686).
    let message = Box::leak(Box::new(StaticImage::new("rt_main test panic"))).word();
    let location = Box::leak(Box::new(StaticImage::new("src/main.cheby:1:1"))).word();
    // SAFETY: `ctx` is the running entry's; the strings are static
    // objects that are never freed, and the report copies them.
    unsafe { cheby_rt_panic(ctx, PanicKind::Explicit.code(), message, location) };
    // The panic is recorded and the flag set (D-733): return at once.
    0
}

/// An image with no module whose `main` is `entry`.
fn image(entry: EntryFn) -> ProgramImage {
    // SAFETY: no module records, and `entry` is one of this module's
    // thunks, which have the `EntryFn` ABI and static code.
    unsafe {
        ProgramImage::new(ImageParts {
            version: abi::IMAGE_VERSION,
            unwind_edges: true,
            modules: Table::empty(),
            entry,
            callbacks: Callbacks::default(),
            main_stack: u64::try_from(DEFAULT_ROOT_STACK_SIZE).unwrap(),
            fiber_stack: u64::try_from(DEFAULT_STACK_SIZE).unwrap(),
            constants: Table::empty(),
            functions: Table::empty(),
        })
    }
}

/// `cheby_rt_main` with `argv` built from `args` (bytes, so a test can
/// pass invalid UTF-8) and an image whose `main` is `entry`.
fn rt_main(entry: EntryFn, words: &[&[u8]]) -> c_int {
    let owned: Vec<CString> = words.iter().map(|w| CString::new(*w).unwrap()).collect();
    let mut pointers: Vec<*const c_char> = owned.iter().map(|w| w.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let image = image(entry);
    let count = c_int::try_from(owned.len()).unwrap();
    // SAFETY: `pointers` holds `count` NUL-terminated strings followed by
    // a null pointer, all alive for the call, and `image` is a valid image.
    unsafe { cheby_rt_main(count, pointers.as_ptr(), &raw const image) }
}

/// D-926: `main`'s statuses reach the caller as `cheby run` gives them.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn rt_main_returns_the_status_of_main() {
    // `run_main` sets the program's arguments, as other tests do.
    let _guard = serial();
    assert_eq!(rt_main(returns_0, &[b"prog"]), status::SUCCESS);
    assert_eq!(rt_main(returns_1, &[b"prog"]), status::ERR);
    // Prints the panic report to the test's standard error.
    assert_eq!(rt_main(panics, &[b"prog"]), status::PANIC);
    assert_eq!(status::PANIC, 101);
}

/// `env::args()` is `argv[1..]` and `env::program_name()` is `argv[0]`,
/// each with invalid UTF-8 replaced (D-347, D-369, D-311).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn rt_main_passes_arguments_and_the_program_name() {
    let _guard = serial();
    assert_eq!(
        rt_main(reads_its_program, &[b"./prog", b"a", b"b\xffc"]),
        status::SUCCESS
    );
}

/// What a child process runs: nothing unless [`CHILD`] is set. With
/// `closed` it closes descriptors 0 and 1, runs `cheby_rt_main`, and exits
/// with 0 only if both were reopened on `/dev/null` (D-474); with
/// `status` it exits with `cheby_rt_main`'s status.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
#[expect(
    clippy::exit,
    reason = "the child process reports `cheby_rt_main`'s status as its own"
)]
fn rt_main_child() {
    let Some(mode) = std::env::var_os(CHILD) else {
        return;
    };
    if mode == "closed" {
        for fd in [0, 1] {
            // SAFETY: closing a standard descriptor of this child process,
            // which runs only this test (`--exact`, one thread).
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
        let status = rt_main(returns_0, &[b"prog"]);
        let reopened = [0, 1].into_iter().all(is_dev_null);
        // A write to the reopened standard output succeeds.
        // SAFETY: writes one byte from a live buffer to descriptor 1.
        let written = unsafe { libc::write(1, b"x".as_ptr().cast(), 1) };
        std::process::exit(if status == 0 && reopened && written == 1 {
            0
        } else {
            2
        });
    }
    std::process::exit(rt_main(returns_0, &[b"prog"]));
}

/// Whether `fd` is open on `/dev/null`.
fn is_dev_null(fd: c_int) -> bool {
    let mut open = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut null = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` and `stat` write a whole `stat` into the buffers
    // when they return 0, and the path is a NUL-terminated literal.
    unsafe {
        if libc::fstat(fd, open.as_mut_ptr()) != 0
            || libc::stat(c"/dev/null".as_ptr(), null.as_mut_ptr()) != 0
        {
            return false;
        }
        let (open, null) = (open.assume_init(), null.assume_init());
        open.st_dev == null.st_dev && open.st_ino == null.st_ino
    }
}

/// This test binary, running only [`rt_main_child`] in `mode`.
fn child(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "rt_main_tests::rt_main_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, mode)
        .env_remove("CHEBY_THREADS");
    command
}

/// D-474: standard descriptors that are closed when the executable starts
/// are reopened on `/dev/null`.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn rt_main_reopens_closed_standard_descriptors() {
    let output = child("closed").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// D-658, D-927: a bad `CHEBY_THREADS` stops the program before `main`
/// with status 1 and the decision's message.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn rt_main_rejects_a_bad_cheby_threads() {
    let output = child("status")
        .env("CHEBY_THREADS", "zero")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr
            .contains("error: CHEBY_THREADS must be a whole number of at least 1, not \"zero\"\n"),
        "{stderr}"
    );
    // And a good one runs `main`.
    let output = child("status").env("CHEBY_THREADS", "2").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
