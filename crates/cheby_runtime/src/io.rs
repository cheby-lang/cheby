//! Standard output and standard error, and the `std::io` print kernels
//! (D-346, D-306).
//!
//! Standard output goes through Rust's `stdout()`, which is line-buffered;
//! standard error through `stderr()`, which is not buffered. When a write
//! or a flush fails, the program ends at once with status 1 and prints
//! nothing (D-306). Rust ignores `SIGPIPE`, so a closed pipe arrives here
//! as an `EPIPE` error.
//!
//! A standard stream that was closed when the program started behaves as a
//! null device: writes succeed and are discarded, and reads see end of
//! input. D-306 applies only to streams that were open at start. Two layers
//! give this on native, with no code here:
//!
//! - On Unix, Rust's startup code (`sanitize_standard_fds`) reopens closed
//!   descriptors 0-2 on `/dev/null` before `main`, so they are a real
//!   null device.
//! - Rust's `Stdout`, `Stderr` and `Stdin` turn `EBADF` (Unix) and
//!   `ERROR_INVALID_HANDLE` (Windows, where a missing standard handle is
//!   not reopened) into success: a write reports every byte written, a
//!   flush succeeds, and a read returns 0 bytes, which is end of input.
//!
//! A process entry that bypasses Rust's startup (AOT, M3) keeps the second
//! layer but must reopen closed descriptors 0-2 itself, or a file the
//! program opens could take descriptor 1. Stdin kernels must read through
//! `std::io::stdin()` to keep both layers.

use std::io::{self, Write};

use crate::abi::{self, Word, sym};
use crate::panic::fatal;
use crate::string;

/// Appends the `std::io` kernels this module implements, with their
/// addresses, to `out` (plan §4.3.3).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend([
        (sym::IO_PRINT, cheby_io_print as *const u8),
        (sym::IO_PRINTLN, cheby_io_println as *const u8),
        (sym::IO_EPRINT, cheby_io_eprint as *const u8),
        (sym::IO_EPRINTLN, cheby_io_eprintln as *const u8),
    ]);
}

/// The std modules whose every kernel is registered here: none yet
/// (`std::io`'s `sys_*` kernels come with W2-E).
pub(crate) const COMPLETE: &[&str] = &[];

/// `io::print(text)`: writes `text` to standard output.
///
/// # Safety
///
/// `text` points to a live `String` object, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_print(text: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { write_kernel(Stream::Stdout, text, false) };
}

/// `io::println(text)`: writes `text` and `\n` to standard output.
///
/// # Safety
///
/// `text` points to a live `String` object, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_println(text: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { write_kernel(Stream::Stdout, text, true) };
}

/// `io::eprint(text)`: writes `text` to standard error.
///
/// # Safety
///
/// `text` points to a live `String` object, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_eprint(text: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { write_kernel(Stream::Stderr, text, false) };
}

/// `io::eprintln(text)`: writes `text` and `\n` to standard error.
///
/// # Safety
///
/// `text` points to a live `String` object, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_eprintln(text: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { write_kernel(Stream::Stderr, text, true) };
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Writes a string object to a stream, ending the program on failure.
///
/// # Safety
///
/// `text` points to a live `String` object.
unsafe fn write_kernel(stream: Stream, text: Word, newline: bool) {
    // SAFETY: forwarded from the caller.
    let Some(bytes) = (unsafe { string::bytes(text) }) else {
        fatal(format_args!(
            "an output kernel received a value that is not a String: {text:#x}"
        ));
    };
    let written = match stream {
        Stream::Stdout => write_text(&mut io::stdout().lock(), bytes, newline),
        Stream::Stderr => write_text(&mut io::stderr().lock(), bytes, newline),
    };
    if written.is_err() {
        exit_after_write_failure();
    }
}

/// Writes `bytes`, then `\n` if `newline`. The line ending is `\n` on
/// every platform.
fn write_text(out: &mut impl Write, bytes: &[u8], newline: bool) -> io::Result<()> {
    out.write_all(bytes)?;
    if newline {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Flushes standard output.
///
/// # Errors
///
/// The flush's IO error, for example `EPIPE` when the reader has gone.
pub fn flush_stdout() -> io::Result<()> {
    io::stdout().lock().flush()
}

/// Writes `text` to standard error at once, ending the program on failure
/// (D-306). For the runtime's own reports, such as panics.
pub(crate) fn write_stderr(text: &str) {
    if io::stderr().lock().write_all(text.as_bytes()).is_err() {
        exit_after_write_failure();
    }
}

/// Ends the program with status 1, printing nothing (D-306).
#[expect(
    clippy::exit,
    reason = "D-306: a failed write ends the program at once with status 1"
)]
pub(crate) fn exit_after_write_failure() -> ! {
    std::process::exit(abi::status::ERR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn println_appends_a_newline() {
        let mut out = Vec::new();
        write_text(&mut out, b"Hello, world!", true).unwrap();
        write_text(&mut out, b"a", false).unwrap();
        write_text(&mut out, b"", true).unwrap();
        assert_eq!(out, b"Hello, world!\na\n");
    }

    #[test]
    fn write_errors_are_reported() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_text(&mut Closed, b"x", true).is_err());
    }
}
