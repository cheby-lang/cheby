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
//!
//! Standard input has one process-wide buffered reader, Rust's `Stdin`
//! (D-474). `io::read_line` (`cheby_io_sys_read_line`) and the REPL
//! ([`read_input_line`], D-548) both read through it, so neither takes
//! lines meant for the other. Each read flushes standard output first
//! (D-346).
//!
//! The `Result` kernels of `std::io` (`sys_*`) never end the program on an
//! IO failure: they return `Err((kind, message))` (D-484), with kind 1 not
//! found, 2 permission denied, 3 invalid UTF-8 and 0 every other failure,
//! and the operating system's message without Rust's ` (os error N)`
//! suffix (GAP(io-error-pair)).

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::abi::{self, Word, sym};
use crate::panic::fatal;
use crate::{string, values};

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
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["io"];

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

// ---------------------------------------------------------------------
// The `Result` kernels of `std::io` (D-484)

/// The kind code of a failure with no predicate (D-484: any code but 1, 2
/// and 3).
const KIND_OTHER: i64 = 0;
/// The kind code of `io::is_not_found`.
const KIND_NOT_FOUND: i64 = 1;
/// The kind code of `io::is_permission_denied`.
const KIND_PERMISSION_DENIED: i64 = 2;
/// The kind code of `io::is_invalid_data` (D-311).
const KIND_INVALID_DATA: i64 = 3;

/// The message of an invalid-UTF-8 failure. `std::io` drops the message of
/// a kind with a predicate (D-345), so it never reaches the program.
const INVALID_UTF8: &str = "invalid UTF-8";

/// The kind code of an IO error (D-484).
fn kind_code(error: &io::Error) -> i64 {
    match error.kind() {
        io::ErrorKind::NotFound => KIND_NOT_FOUND,
        io::ErrorKind::PermissionDenied => KIND_PERMISSION_DENIED,
        io::ErrorKind::InvalidData => KIND_INVALID_DATA,
        _ => KIND_OTHER,
    }
}

/// The operating system's message of an IO error, without the
/// ` (os error N)` that Rust appends to it (GAP(io-error-pair)).
fn os_message(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .map_or_else(|| text.clone(), str::to_owned),
        None => text,
    }
}

/// `Err((kind, message))`, the failure of a `std::io` kernel (D-484).
fn err_pair(kind: i64, message: &str) -> Word {
    values::err(values::tuple(&[
        values::int_word(kind),
        values::string(message),
    ]))
}

/// The `Err` pair of an IO error.
fn io_err(error: &io::Error) -> Word {
    err_pair(kind_code(error), &os_message(error))
}

/// `Ok(value)` for a successful operation, the `Err` pair otherwise.
fn lift(outcome: io::Result<Word>) -> Word {
    match outcome {
        Ok(value) => values::ok(value),
        Err(error) => io_err(&error),
    }
}

/// `Ok(text)`, or the invalid-data `Err` pair when `data` is not UTF-8.
fn text_result(data: &[u8], wrap: impl FnOnce(Word) -> Word) -> Word {
    match std::str::from_utf8(data) {
        Ok(text) => values::ok(wrap(values::string(text))),
        Err(_) => err_pair(KIND_INVALID_DATA, INVALID_UTF8),
    }
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

/// `io::write_stdout`'s kernel: writes `text` through standard output's
/// buffer, then flushes it, so the result covers everything written so
/// far (D-365). A failure is returned, never fatal (D-306).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_stdout(text: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let bytes = unsafe { str_arg(text) }.as_bytes();
    let mut out = io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => {
            // Everything buffered so far is written: no reported text is
            // left over.
            STDOUT_FAILURE_REPORTED.store(false, Ordering::Relaxed);
            values::ok_nil()
        }
        Err(error) => {
            STDOUT_FAILURE_REPORTED.store(true, Ordering::Relaxed);
            io_err(&error)
        }
    }
}

/// `io::write_stderr`'s kernel: writes `text` to standard error, which is
/// not buffered (D-346). A failure is returned (D-306).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_stderr(text: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let bytes = unsafe { str_arg(text) }.as_bytes();
    lift(io::stderr().lock().write_all(bytes).map(|()| values::nil()))
}

/// `io::read_line`'s kernel: flushes standard output, then reads one line
/// through the process-wide reader (D-346, D-474): `Ok(Some(line))`
/// without its ending (D-305), `Ok(None)` at the end of input, the
/// invalid-data pair for a line that is not UTF-8, which is consumed
/// (D-311), and the error pair when standard input cannot be read.
#[unsafe(no_mangle)]
pub extern "C" fn cheby_io_sys_read_line() -> Word {
    match read_raw_line() {
        Ok(Some(line)) => text_result(&line, values::some),
        Ok(None) => values::ok(values::none()),
        Err(error) => io_err(&error),
    }
}

/// `io::read_file`'s kernel: the whole file as UTF-8 text, kept exactly,
/// including a leading byte-order mark (D-311, D-366).
///
/// # Safety
///
/// `path` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_read_file(path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    match std::fs::read(unsafe { str_arg(path) }) {
        Ok(data) => text_result(&data, |text| text),
        Err(error) => io_err(&error),
    }
}

/// `io::read_file_bytes`'s kernel: the whole file as `Bytes` (D-311).
///
/// # Safety
///
/// `path` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_read_file_bytes(path: Word) -> Word {
    // SAFETY: forwarded from the caller.
    lift(std::fs::read(unsafe { str_arg(path) }).map(|data| values::bytes(&data)))
}

/// `io::write_file`'s kernel: creates or replaces the file with `text`
/// as UTF-8, with no byte-order mark (D-328).
///
/// # Safety
///
/// `path` and `text` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_file(path: Word, text: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (path, text) = unsafe { (str_arg(path), str_arg(text)) };
    lift(std::fs::write(path, text).map(|()| values::nil()))
}

/// `io::write_file_bytes`'s kernel: creates or replaces the file with
/// `data` (D-328).
///
/// # Safety
///
/// `path` is a live `String` and `data` a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_file_bytes(path: Word, data: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (path, data) = unsafe { (str_arg(path), values::bytes_of(data)) };
    lift(std::fs::write(path, data).map(|()| values::nil()))
}

/// One kernel: symbol, address, C parameter types and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The `Result` kernels of `std::io`.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 7] {
    const W: &str = "Word";
    [
        (
            "cheby_io_sys_write_stdout",
            cheby_io_sys_write_stdout as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_io_sys_write_stderr",
            cheby_io_sys_write_stderr as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_io_sys_read_line",
            cheby_io_sys_read_line as *const u8,
            &[],
            W,
        ),
        (
            "cheby_io_sys_read_file",
            cheby_io_sys_read_file as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_io_sys_read_file_bytes",
            cheby_io_sys_read_file_bytes as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_io_sys_write_file",
            cheby_io_sys_write_file as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_io_sys_write_file_bytes",
            cheby_io_sys_write_file_bytes as *const u8,
            &[W, W],
            W,
        ),
    ]
}

/// The C signatures of every `std::io` kernel, for the cross-check
/// against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    let print: &'static [&'static str] = &["Word"];
    let mut out = vec![
        (sym::IO_PRINT, print, "()"),
        (sym::IO_PRINTLN, print, "()"),
        (sym::IO_EPRINT, print, "()"),
        (sym::IO_EPRINTLN, print, "()"),
    ];
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.2, kernel.3)));
    out
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
        Stream::Stdout => {
            let mut out = io::stdout().lock();
            // This text has no `Result`: if it is lost, a later failing
            // flush ends the program again (D-306, D-346). The excuse of
            // GAP(stdout-reported-failure) covers only text already
            // reported.
            STDOUT_FAILURE_REPORTED.store(false, Ordering::Relaxed);
            write_text(&mut out, bytes, newline)
        }
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

/// Set when `io::write_stdout` has returned an error to the program, and
/// cleared when any later text enters standard output's buffer.
static STDOUT_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

/// Flushes standard output.
///
/// Right after `io::write_stdout` has reported a failure to the program, a
/// failing flush is not an error: the text left in the buffer is the text
/// whose failure that `Err` already reported (D-365), so a program that
/// handles it and returns normally keeps its exit status
/// (GAP(stdout-reported-failure)). Once `print` or `println` adds text,
/// the excuse ends, and a failing flush is an error again (D-306).
///
/// # Errors
///
/// The flush's IO error, for example `EPIPE` when the reader has gone.
pub fn flush_stdout() -> io::Result<()> {
    match io::stdout().lock().flush() {
        Err(_) if STDOUT_FAILURE_REPORTED.load(Ordering::Relaxed) => Ok(()),
        other => other,
    }
}

/// Flushes standard output before standard input is read (D-346), ending
/// the program with status 1 when it cannot (D-306).
fn flush_before_read() {
    if flush_stdout().is_err() {
        exit_after_write_failure();
    }
}

/// Reads the next line of standard input through the process-wide reader,
/// after flushing standard output (D-346), without its ending: a final
/// `\n` or `\r\n` is removed (D-305). `Ok(None)` at the end of input.
fn read_raw_line() -> io::Result<Option<Vec<u8>>> {
    flush_before_read();
    let mut line = Vec::new();
    if io::stdin().lock().read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(strip_line_ending(line)))
}

/// Removes a final `\n` or `\r\n` (D-305). A lone `\r` stays.
fn strip_line_ending(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    line
}

/// Reads the next line of standard input for the REPL, through the same
/// buffered reader as `io::read_line` (D-548), after flushing standard
/// output (ending the program with status 1 when that fails, D-306). The
/// line has no ending (D-305), invalid UTF-8 is replaced with U+FFFD, and
/// the result is `None` at the end of input or when standard input cannot
/// be read (GAP(repl-input-line)).
#[must_use]
pub fn read_input_line() -> Option<String> {
    read_raw_line()
        .ok()
        .flatten()
        .map(|line| String::from_utf8_lossy(&line).into_owned())
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

    use crate::alloc::live;
    use crate::{rc, values};

    /// A scratch directory for this test, empty.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cheby-io-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a kernel's result holds: `Ok` with the value read by `read`,
    /// or `Err((kind, message))`. The result is dropped.
    fn outcome<T>(result: Word, read: impl FnOnce(Word) -> T) -> Result<T, (i64, String)> {
        // SAFETY: a live `Result` owned by the test.
        let out = match unsafe { values::read_result(result) } {
            Ok(value) => Ok(read(value)),
            Err(pair) => {
                // SAFETY: a live `(Int, String)`.
                let fields = unsafe { values::read_tuple(pair) };
                match fields.as_slice() {
                    // SAFETY: as above.
                    [kind, message] => Err(unsafe {
                        (
                            values::word_to_i64(*kind),
                            values::str_of(*message).to_owned(),
                        )
                    }),
                    _ => Err((-1, "not a pair".to_owned())),
                }
            }
        };
        // SAFETY: owned by the test.
        unsafe { rc::drop(result) };
        out
    }

    /// Calls a kernel on new strings, borrowed, then drops them.
    fn with_strings(args: &[&str], kernel: impl FnOnce(&[Word]) -> Word) -> Word {
        let words: Vec<Word> = args.iter().map(|a| values::string(a)).collect();
        let result = kernel(&words);
        for word in words {
            // SAFETY: owned by the test.
            unsafe { rc::drop(word) };
        }
        result
    }

    fn text(word: Word) -> String {
        // SAFETY: a live string.
        unsafe { values::str_of(word) }.to_owned()
    }

    fn bytes(word: Word) -> Vec<u8> {
        // SAFETY: a live `Bytes`.
        unsafe { values::bytes_of(word) }.to_vec()
    }

    fn is_nil(word: Word) -> bool {
        word == values::nil()
    }

    fn read_file(path: &str) -> Result<String, (i64, String)> {
        // SAFETY: live string arguments.
        outcome(
            with_strings(&[path], |w| unsafe { cheby_io_sys_read_file(w[0]) }),
            text,
        )
    }

    fn read_file_bytes(path: &str) -> Result<Vec<u8>, (i64, String)> {
        // SAFETY: live string arguments.
        outcome(
            with_strings(&[path], |w| unsafe { cheby_io_sys_read_file_bytes(w[0]) }),
            bytes,
        )
    }

    fn write_file(path: &str, contents: &str) -> Result<bool, (i64, String)> {
        // SAFETY: live string arguments.
        outcome(
            with_strings(&[path, contents], |w| unsafe {
                cheby_io_sys_write_file(w[0], w[1])
            }),
            is_nil,
        )
    }

    fn write_file_bytes(path: &str, data: &[u8]) -> Result<bool, (i64, String)> {
        let data = values::bytes(data);
        // SAFETY: live arguments.
        let result = with_strings(&[path], |w| unsafe {
            cheby_io_sys_write_file_bytes(w[0], data)
        });
        // SAFETY: owned by the test.
        unsafe { rc::drop(data) };
        outcome(result, is_nil)
    }

    #[test]
    fn files_round_trip() {
        let before = live();
        let dir = scratch("round-trip");
        let path = dir.join("out.txt");
        let path = path.to_str().unwrap();
        assert_eq!(write_file(path, "héllo\r\n\u{FEFF}"), Ok(true));
        assert_eq!(read_file(path).as_deref(), Ok("héllo\r\n\u{FEFF}"));
        assert_eq!(write_file(path, "replaced"), Ok(true));
        assert_eq!(read_file(path).as_deref(), Ok("replaced"));
        assert_eq!(write_file_bytes(path, b"\xEF\xBB\xBFa\x00"), Ok(true));
        assert_eq!(read_file_bytes(path), Ok(b"\xEF\xBB\xBFa\x00".to_vec()));
        // A leading byte-order mark is kept (D-366).
        assert_eq!(read_file(path).as_deref(), Ok("\u{FEFF}a\0"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(live(), before);
    }

    #[test]
    fn failures_are_error_pairs() {
        let before = live();
        let dir = scratch("failures");
        let missing = "no/such/directory/file.txt";
        assert_eq!(read_file(missing).map_err(|e| e.0), Err(KIND_NOT_FOUND));
        assert_eq!(
            read_file_bytes(missing).map_err(|e| e.0),
            Err(KIND_NOT_FOUND)
        );
        assert_eq!(
            write_file(missing, "x").map_err(|e| e.0),
            Err(KIND_NOT_FOUND)
        );
        assert_eq!(
            write_file_bytes(missing, b"x").map_err(|e| e.0),
            Err(KIND_NOT_FOUND)
        );
        // Invalid UTF-8 is kind 3 (D-311); the bytes read fine.
        let bad = dir.join("bad.txt");
        std::fs::write(&bad, b"f\xFF").unwrap();
        let bad = bad.to_str().unwrap();
        assert_eq!(read_file(bad).map_err(|e| e.0), Err(KIND_INVALID_DATA));
        assert_eq!(read_file_bytes(bad), Ok(b"f\xFF".to_vec()));
        // A directory is another kind, with the OS's message and no
        // ` (os error N)`.
        let (kind, message) = read_file(dir.to_str().unwrap()).unwrap_err();
        assert_eq!(kind, KIND_OTHER);
        assert!(
            !message.is_empty() && !message.contains("os error"),
            "{message}"
        );
        // A path with U+0000 cannot name a file: another kind.
        assert_eq!(read_file("a\0b").map_err(|e| e.0), Err(KIND_OTHER));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.join("locked.txt");
            std::fs::write(&locked, "secret").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads anything, so only check when the read fails.
            if std::fs::read(&locked).is_err() {
                assert_eq!(
                    read_file(locked.to_str().unwrap()).map_err(|e| e.0),
                    Err(KIND_PERMISSION_DENIED)
                );
            }
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(live(), before);
    }

    #[test]
    fn os_messages_lose_the_error_number() {
        let error = io::Error::from_raw_os_error(2);
        let message = os_message(&error);
        assert!(!message.contains("os error"), "{message}");
        assert_ne!(message, "");
        let custom = io::Error::other("custom text");
        assert_eq!(os_message(&custom), "custom text");
        assert_eq!(kind_code(&error), KIND_NOT_FOUND);
        assert_eq!(kind_code(&custom), KIND_OTHER);
    }

    #[test]
    fn line_endings_are_removed() {
        let strip = |line: &[u8]| strip_line_ending(line.to_vec());
        assert_eq!(strip(b"abc\n"), b"abc");
        assert_eq!(strip(b"abc\r\n"), b"abc");
        assert_eq!(strip(b"abc"), b"abc");
        assert_eq!(strip(b"abc\r"), b"abc\r");
        assert_eq!(strip(b"\n"), b"");
        assert_eq!(strip(b"a\r\r\n"), b"a\r");
    }

    #[test]
    fn write_stderr_reports_success() {
        let before = live();
        // SAFETY: a live string argument.
        let result = with_strings(&[""], |w| unsafe { cheby_io_sys_write_stderr(w[0]) });
        assert_eq!(outcome(result, is_nil), Ok(true));
        // SAFETY: as above.
        let result = with_strings(&[""], |w| unsafe { cheby_io_sys_write_stdout(w[0]) });
        assert_eq!(outcome(result, is_nil), Ok(true));
        assert_eq!(live(), before);
    }
}
