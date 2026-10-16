//! Standard output and standard error, and the `std::io` print kernels
//! (D-346, D-306).
//!
//! Standard output goes through Rust's `stdout()`, which is line-buffered;
//! standard error through `stderr()`, which is not buffered. When a write
//! or a flush fails, the program ends at once with status 1 and prints
//! nothing (D-306): a termination (D-709, `sched::terminate`), which the
//! running entry's host receives as `EntryOutcome::Exited(1)` while the
//! runtime halts; the runtime never ends the process itself. Rust ignores
//! `SIGPIPE`, so a closed pipe arrives here as an `EPIPE` error.
//!
//! Standard input and file IO run on the blocking pool (D-683): the five
//! suspending kernels (`sys_read_line`, `sys_read_file`,
//! `sys_read_file_bytes`, `sys_write_file`, `sys_write_file_bytes`) are
//! `@std_context` kernels (D-692) that copy their arguments to Rust data,
//! park their fiber while a pool thread makes the system call
//! (`sched::blocking::run_blocking`), and build the Cheby result on the
//! fiber once it runs again, charged to its domain. The fiber stays marked
//! as blocked while it waits, so a teardown and a halt skip it and count
//! it (D-786, GAP(pool-in-the-survey)). Without a runtime the call runs
//! inline, marked (D-692). The flush before a read stays on the fiber, and
//! a pool thread never flushes, terminates or makes a Cheby value.
//!
//! Every write to a standard stream goes through one funnel,
//! `write_stream`, which offers it to the running fiber's capture first
//! (`capture::route`, D-723) and otherwise writes the whole text under the
//! stream's lock (D-684), which is never held across a switch; D-574's
//! state changes only under the stdout lock, and only for text that
//! reached the real stream. A stalled stdout pipe still stalls the fibers
//! that print (D-684).
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
//! Standard input has one process-wide buffered reader (D-474), this
//! module's `STDIN`, which reads through Rust's `Stdin`.
//! `io::read_line` (`cheby_io_sys_read_line`) and the REPL
//! ([`read_input_line`], D-548) both read through it, so neither takes
//! lines meant for the other, and they take lines in the order they ask
//! for them. Each read flushes standard output first (D-346). A line the
//! reader already holds is served to a fiber at once, without the
//! blocking pool (GAP(stdin-fast-path)); only a read that must wait for
//! input goes to the pool.
//!
//! The `Result` kernels of `std::io` (`sys_*`) never end the program on an
//! IO failure: they return `Err((kind, message))` (D-484), with kind 1 not
//! found, 2 permission denied, 3 invalid UTF-8 and 0 every other failure,
//! and the operating system's message without Rust's ` (os error N)`
//! suffix (D-573). A path that names a directory is kind 0 on every
//! platform, although Windows refuses to open one with "access denied"
//! (`docs/stdlib/io.md`).

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::abi::{self, Word, sym};
use crate::capture::{self, Stream};
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::panic::fatal;
use crate::sched::blocking::run_blocking;
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
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
/// ` (os error N)` that Rust appends to it (D-573).
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

/// A failed IO operation as Rust data: its kind code (D-484) and the
/// operating system's message (D-573). A blocking-pool job makes it, so
/// that the job returns Rust data only; the fiber turns it into the `Err`
/// pair ([`Failure::word`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    kind: i64,
    message: String,
}

impl Failure {
    /// The failure of an IO error.
    fn of(error: &io::Error) -> Self {
        Self {
            kind: kind_code(error),
            message: os_message(error),
        }
    }

    /// The failure of an operation on the file at `path`. Windows refuses
    /// to open a directory as a file with "access denied", but naming a
    /// directory is another failure, not `io::is_permission_denied`, on
    /// every platform (`docs/stdlib/io.md`). The directory test is a
    /// system call, so it runs in the job, with the operation.
    fn of_file(path: &str, error: &io::Error) -> Self {
        let kind = match kind_code(error) {
            KIND_PERMISSION_DENIED if Path::new(path).is_dir() => KIND_OTHER,
            kind => kind,
        };
        Self {
            kind,
            message: os_message(error),
        }
    }

    /// The `Err` pair, made on the fiber.
    fn word(&self) -> Word {
        err_pair(self.kind, &self.message)
    }
}

/// Runs `job`, a blocking system call over Rust data, through the blocking
/// pool while the fiber of `ctx` is parked (D-683), or inline when `ctx`
/// is null (a Rust caller with no fiber, as the unit tests of this file
/// are). A cancellation pending before the job, or delivered during it,
/// raises once the job has returned, and its result is released first
/// (D-670): nothing of it reaches the program.
///
/// # Safety
///
/// `ctx` is null or the running fiber's context.
unsafe fn blocking_call<R: Send + 'static>(
    ctx: *mut ExecCtx,
    job: impl FnOnce() -> R + Send + 'static,
) -> R {
    // SAFETY: forwarded from the caller.
    let Some(me) = (unsafe { ctx.as_ref() }) else {
        return job();
    };
    match run_blocking(me, job) {
        Ok(result) => result,
        Err(Cancelled) => raise_cancel(Some(ctx)),
    }
}

/// The context a guard of a suspending kernel gets: `None` for a null
/// `ctx`, when the current fiber is used.
fn guard_ctx(ctx: *mut ExecCtx) -> Option<*mut ExecCtx> {
    (!ctx.is_null()).then_some(ctx)
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
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { str_arg(text) }.as_bytes();
        // D-574's state is stored under the stdout lock (D-684), which is
        // released before the result is allocated.
        match write_routed(Stream::Stdout, &[bytes], true) {
            Ok(()) => values::ok_nil(),
            Err(error) => io_err(&error),
        }
    })
}

/// `io::write_stderr`'s kernel: writes `text` to standard error, which is
/// not buffered (D-346). A failure is returned (D-306).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_stderr(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { str_arg(text) }.as_bytes();
        let written = write_stream(Stream::Stderr, &[bytes]);
        lift(written.map(|()| values::nil()))
    })
}

/// `io::read_line`'s kernel: flushes standard output, then reads one line
/// through the process-wide reader (D-346, D-474): `Ok(Some(line))`
/// without its ending (D-305), `Ok(None)` at the end of input, the
/// invalid-data pair for a line that is not UTF-8, which is consumed
/// (D-311), and the error pair when standard input cannot be read. A line
/// the reader already holds is taken on the fiber (GAP(stdin-fast-path));
/// otherwise the read runs on the blocking pool (D-683), and a
/// cancellation that arrives meanwhile is delivered once the line has been
/// read, and the line is released (§10.7).
///
/// # Safety
///
/// `ctx` is the running fiber's context, or null for a Rust caller with no
/// fiber.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_read_line(ctx: *mut ExecCtx) -> Word {
    guarded(guard_ctx(ctx), || {
        flush_before_read();
        // SAFETY: forwarded from the caller.
        let buffered = unsafe { ctx.as_ref() }.and_then(buffered_line);
        let line = buffered.map_or_else(
            || {
                // Counted from before the job is submitted, so that no
                // fast path takes a line before this read (D-548).
                let waiter = Waiter::new();
                // SAFETY: forwarded from the caller.
                unsafe {
                    blocking_call(ctx, move || {
                        read_line_waiting(waiter).map_err(|error| Failure::of(&error))
                    })
                }
            },
            |line| Ok(Some(line)),
        );
        match line {
            Ok(Some(line)) => text_result(&line, values::some),
            Ok(None) => values::ok(values::none()),
            Err(failure) => failure.word(),
        }
    })
}

/// `io::read_file`'s kernel: the whole file as UTF-8 text, kept exactly,
/// including a leading byte-order mark (D-311, D-366). The read runs on
/// the blocking pool (D-683).
///
/// # Safety
///
/// `ctx` is the running fiber's context, or null for a Rust caller with no
/// fiber; `path` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_read_file(ctx: *mut ExecCtx, path: Word) -> Word {
    guarded(guard_ctx(ctx), || {
        // SAFETY: forwarded from the caller.
        let path = unsafe { str_arg(path) }.to_owned();
        // SAFETY: forwarded from the caller.
        match unsafe { blocking_call(ctx, move || read_file(&path)) } {
            Ok(data) => text_result(&data, |text| text),
            Err(failure) => failure.word(),
        }
    })
}

/// `io::read_file_bytes`'s kernel: the whole file as `Bytes` (D-311). The
/// read runs on the blocking pool (D-683).
///
/// # Safety
///
/// `ctx` is the running fiber's context, or null for a Rust caller with no
/// fiber; `path` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_read_file_bytes(ctx: *mut ExecCtx, path: Word) -> Word {
    guarded(guard_ctx(ctx), || {
        // SAFETY: forwarded from the caller.
        let path = unsafe { str_arg(path) }.to_owned();
        // SAFETY: forwarded from the caller.
        match unsafe { blocking_call(ctx, move || read_file(&path)) } {
            Ok(data) => values::ok(values::bytes(&data)),
            Err(failure) => failure.word(),
        }
    })
}

/// `io::write_file`'s kernel: creates or replaces the file with `text`
/// as UTF-8, with no byte-order mark (D-328). The write runs on the
/// blocking pool (D-683).
///
/// # Safety
///
/// `ctx` is the running fiber's context, or null for a Rust caller with no
/// fiber; `path` and `text` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_file(
    ctx: *mut ExecCtx,
    path: Word,
    text: Word,
) -> Word {
    guarded(guard_ctx(ctx), || {
        // SAFETY: forwarded from the caller.
        let (path, text) = unsafe { (str_arg(path).to_owned(), str_arg(text).to_owned()) };
        // SAFETY: forwarded from the caller.
        match unsafe { blocking_call(ctx, move || write_file(&path, text.as_bytes())) } {
            Ok(()) => values::ok(values::nil()),
            Err(failure) => failure.word(),
        }
    })
}

/// `io::write_file_bytes`'s kernel: creates or replaces the file with
/// `data` (D-328). The write runs on the blocking pool (D-683).
///
/// # Safety
///
/// `ctx` is the running fiber's context, or null for a Rust caller with no
/// fiber; `path` is a live `String` and `data` a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_io_sys_write_file_bytes(
    ctx: *mut ExecCtx,
    path: Word,
    data: Word,
) -> Word {
    guarded(guard_ctx(ctx), || {
        // SAFETY: forwarded from the caller.
        let (path, data) = unsafe { (str_arg(path).to_owned(), values::bytes_of(data).to_vec()) };
        // SAFETY: forwarded from the caller.
        match unsafe { blocking_call(ctx, move || write_file(&path, &data)) } {
            Ok(()) => values::ok(values::nil()),
            Err(failure) => failure.word(),
        }
    })
}

/// A blocking-pool job: the whole file at `path`.
fn read_file(path: &str) -> Result<Vec<u8>, Failure> {
    std::fs::read(path).map_err(|error| Failure::of_file(path, &error))
}

/// A blocking-pool job: creates or replaces the file at `path` with
/// `data`.
fn write_file(path: &str, data: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, data).map_err(|error| Failure::of_file(path, &error))
}

/// The `std::io` kernels that take the running fiber's context
/// (`@std_context`, D-692): those that run on the blocking pool.
#[cfg(test)]
pub(crate) const CONTEXT_KERNELS: [&str; 5] = [
    "cheby_io_sys_read_line",
    "cheby_io_sys_read_file",
    "cheby_io_sys_read_file_bytes",
    "cheby_io_sys_write_file",
    "cheby_io_sys_write_file_bytes",
];

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

/// Writes a string object to a stream, then `\n` if `newline` (on every
/// platform), ending the program on failure. Never raises (D-767).
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
    let parts: &[&[u8]] = if newline { &[bytes, b"\n"] } else { &[bytes] };
    // The stream's lock is released when the funnel returns.
    if write_stream(stream, parts).is_err() {
        terminate(abi::status::ERR);
    }
}

/// Every write to a standard stream that the runtime makes, offered to
/// `capture::route` first: a write of a fiber whose group captures its
/// output is taken there, never fails, and leaves D-574's state alone,
/// since it never enters standard output's buffer. Any other write (a
/// host thread's, an isolated entry's, an uncaptured fiber's) writes its
/// parts in order under the stream's lock, as one call (D-684). Text that
/// enters standard output's buffer this way ends D-574's excuse, under
/// the lock: it has no `Result`, so if it is lost a later failing flush
/// ends the program again (D-306, D-346).
///
/// # Errors
///
/// The real stream's write error.
pub(crate) fn write_stream(stream: Stream, parts: &[&[u8]]) -> io::Result<()> {
    write_routed(stream, parts, false)
}

/// [`write_stream`]; with `flush`, standard output is also flushed under
/// the same lock, and D-574's state then records whether everything
/// buffered so far was written (`io::write_stdout`, D-365, D-574).
fn write_routed(stream: Stream, parts: &[&[u8]], flush: bool) -> io::Result<()> {
    if let Some(taken) = capture::route(stream, parts) {
        // A captured `io::write_stdout` records its flush, which the
        // host's replay makes (D-723).
        if flush {
            let _ = capture::flush();
        }
        return taken;
    }
    match stream {
        Stream::Stdout => {
            let mut out = io::stdout().lock();
            if flush {
                write_and_flush(out, parts, &STDOUT_FAILURE_REPORTED)
            } else {
                STDOUT_FAILURE_REPORTED.store(false, Ordering::Relaxed);
                write_parts(&mut out, parts)
            }
        }
        Stream::Stderr => write_parts(&mut io::stderr().lock(), parts),
    }
}

/// `io::write_stdout`'s real write: writes `parts` through `out`, the
/// locked stream, flushes it, and stores in `reported` (D-574's state)
/// whether that failed, all before `out`, and with it the lock, is
/// dropped (D-684): a failure is reported to the program, and a success
/// leaves no reported text over (D-365).
fn write_and_flush(mut out: impl Write, parts: &[&[u8]], reported: &AtomicBool) -> io::Result<()> {
    let written = write_parts(&mut out, parts).and_then(|()| out.flush());
    reported.store(written.is_err(), Ordering::Relaxed);
    drop(out);
    written
}

/// Writes `parts` in order.
fn write_parts(out: &mut impl Write, parts: &[&[u8]]) -> io::Result<()> {
    for part in parts {
        out.write_all(part)?;
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
/// handles it and returns normally keeps its exit status (D-574). Once
/// `print` or `println` adds text, the excuse ends, and a failing flush is
/// an error again (D-306).
///
/// A fiber whose output is captured has nothing of its own in standard
/// output's buffer: its flush is recorded in its capture, for the host's
/// replay, and succeeds (D-723).
///
/// # Errors
///
/// The flush's IO error, for example `EPIPE` when the reader has gone.
pub fn flush_stdout() -> io::Result<()> {
    if capture::flush() {
        return Ok(());
    }
    // The load runs under the lock of the `match`'s scrutinee (D-684).
    match io::stdout().lock().flush() {
        Err(_) if STDOUT_FAILURE_REPORTED.load(Ordering::Relaxed) => Ok(()),
        other => other,
    }
}

/// Flushes standard output before standard input is read (D-346), ending
/// the program with status 1 when it cannot (D-306, D-709).
fn flush_before_read() {
    if flush_stdout().is_err() {
        terminate(abi::status::ERR);
    }
}

/// Standard input's one buffered reader (D-474): `io::read_line` and the
/// REPL's [`read_input_line`] both take their lines from it, so neither
/// takes lines meant for the other (D-548). It reads through
/// `std::io::stdin()`, which keeps D-474's closed-stream layer. Its lock
/// is a leaf (plan §0 'Locks'): a blocking-pool thread or the REPL's host
/// thread holds it while it waits for input, and a fiber only tries it
/// ([`serve_buffered`]), so no worker waits on it and no guard lives
/// across a switch.
static STDIN: Mutex<LineReader> = Mutex::new(LineReader::new());

/// The reads of standard input that wait for [`STDIN`] to give them a
/// line: a blocking-pool job submitted and not yet done, or the REPL's
/// host thread in its read. While one waits, the fast path serves no
/// line, so lines go out in the order the reads asked for them (D-548).
static STDIN_WAITERS: AtomicUsize = AtomicUsize::new(0);

/// How many bytes one read of standard input asks for: more than Rust's
/// `Stdin` buffers (8 KiB), so the read goes straight to the descriptor
/// when that buffer is empty, and a pipe's whole buffer fits.
const STDIN_CHUNK: usize = 64 * 1024;

/// Bytes read from standard input and not yet returned as lines.
#[derive(Debug)]
struct LineReader {
    /// The bytes read; those before `start` were returned.
    data: Vec<u8>,
    /// Where the next line starts in `data`.
    start: usize,
    /// How many bytes from `start` on are known to hold no `\n`: a line
    /// longer than one read is searched once, never again from its start
    /// with each read, so reading it takes time linear in its length, as
    /// `BufRead::read_until` did (gate 3's ROB-W3-1).
    scanned: usize,
    /// The bytes searched for `\n` so far (tests only).
    #[cfg(test)]
    searched: usize,
}

impl LineReader {
    /// An empty reader.
    const fn new() -> Self {
        Self {
            data: Vec::new(),
            start: 0,
            scanned: 0,
            #[cfg(test)]
            searched: 0,
        }
    }

    /// The next line without its ending (D-305), if a whole line is
    /// buffered; it never reads. Only the bytes after those already known
    /// to hold no `\n` are searched.
    fn buffered_line(&mut self) -> Option<Vec<u8>> {
        let rest = self.data.get(self.start..)?;
        let fresh = rest.get(self.scanned..)?;
        #[cfg(test)]
        {
            self.searched = self.searched.saturating_add(fresh.len());
        }
        let Some(found) = fresh.iter().position(|&byte| byte == b'\n') else {
            self.scanned = rest.len();
            return None;
        };
        let end = self.scanned.saturating_add(found);
        let line = strip_line_ending(rest.get(..=end)?.to_vec());
        self.start = self.start.saturating_add(end).saturating_add(1);
        self.scanned = 0;
        if self.start >= self.data.len() {
            self.data.clear();
            self.start = 0;
        }
        Some(line)
    }

    /// The next line without its ending (D-305), reading `input` until a
    /// whole line is buffered or the input ends: at the end, the unfinished
    /// line, or `Ok(None)` when nothing is buffered. As
    /// `BufRead::read_until`: an interrupted read is retried, and a failure
    /// consumes the bytes of the unfinished line.
    fn read_line(&mut self, input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(line) = self.buffered_line() {
                return Ok(Some(line));
            }
            self.data.drain(..self.start.min(self.data.len()));
            self.start = 0;
            let filled = self.data.len();
            self.data.resize(filled.saturating_add(STDIN_CHUNK), 0);
            let read = match self.data.get_mut(filled..) {
                Some(free) => input.read(free),
                None => Ok(0),
            };
            match read {
                Ok(0) => {
                    self.data.truncate(filled);
                    self.scanned = 0;
                    if self.data.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(strip_line_ending(std::mem::take(&mut self.data))));
                }
                Ok(count) => self.data.truncate(filled.saturating_add(count)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    self.data.truncate(filled);
                }
                Err(error) => {
                    self.data.clear();
                    self.scanned = 0;
                    return Err(error);
                }
            }
        }
    }
}

/// One read of standard input waiting for [`STDIN`], counted in
/// [`STDIN_WAITERS`] from its creation until it is dropped: when the read
/// has its line, or with a job that never ran.
#[derive(Debug)]
struct Waiter(&'static AtomicUsize);

impl Waiter {
    /// Counts a read that waits for [`STDIN`].
    fn new() -> Self {
        Self::on(&STDIN_WAITERS)
    }

    /// Counts a read in `waiters`.
    fn on(waiters: &'static AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::SeqCst);
        Self(waiters)
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `io::read_line`'s fast path (GAP(stdin-fast-path), a D-731-kind
/// optimization): a line [`STDIN`] already holds is taken on the fiber
/// `me`, without a blocking-pool round trip. `None` sends the read to the
/// pool: a cancellation is pending, which raises there before any line is
/// taken (GAP(pool-check-before-submit)), or [`serve_buffered`] has no
/// line to give. With a round trip per line, 100k piped lines took 23.21x
/// checkpoint 3's time (MA's quiet reading); with this path, about 0.85x
/// (I1t's busy reading, 64 KiB per pool read). A long line takes time
/// linear in its length, since [`LineReader`] searches each byte once: a
/// 64 MiB line read in 0.05 s (`cp4-w1` 0.03 s; 9.26 s while it was
/// searched again with each read, gate 3's ROB-W3-1).
fn buffered_line(me: &ExecCtx) -> Option<Vec<u8>> {
    if cancel::check(me).is_err() {
        return None;
    }
    serve_buffered(&STDIN, &STDIN_WAITERS)
}

/// A line `reader` already holds, if no read waits in `waiters` (it asked
/// first, D-548) and no thread holds `reader` (it waits for input, and the
/// caller must not). Never blocks.
fn serve_buffered(reader: &Mutex<LineReader>, waiters: &AtomicUsize) -> Option<Vec<u8>> {
    let mut reader = reader.try_lock()?;
    if waiters.load(Ordering::SeqCst) != 0 {
        return None;
    }
    reader.buffered_line()
}

/// Reads the next line of standard input through [`STDIN`] (D-474), once
/// standard output has been flushed (D-346), without its ending: a final
/// `\n` or `\r\n` is removed (D-305). `Ok(None)` at the end of input. It
/// blocks the calling thread (a blocking-pool job, or the REPL's host
/// thread) and ends `waiter`, its count in [`STDIN_WAITERS`], once it has
/// its line, before it releases the reader.
fn read_line_waiting(waiter: Waiter) -> io::Result<Option<Vec<u8>>> {
    let mut reader = STDIN.lock();
    let line = reader.read_line(&mut io::stdin());
    drop(waiter);
    drop(reader);
    line
}

/// Runs `call`, a system call that blocks the calling thread inline, with
/// the running fiber, if any, marked as blocked in it for the call's
/// duration (D-786): the REPL's prompt read, which runs on its host
/// thread (D-548, D-575), never on the blocking pool.
fn inline_marked<R>(call: impl FnOnce() -> R) -> R {
    /// Clears the mark, also if `call` unwinds.
    struct Mark<'a>(&'a crate::ctx::ExecCtx);
    impl Drop for Mark<'_> {
        fn drop(&mut self) {
            self.0.sched.set_blocking(false);
        }
    }
    let mark = crate::entry::current_fiber().map(|fiber| {
        fiber.sched.set_blocking(true);
        crate::sched::hand_off_before_blocking(fiber);
        Mark(fiber)
    });
    let result = call();
    drop(mark);
    result
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
/// output. The line has no ending (D-305), invalid UTF-8 is replaced with
/// U+FFFD, and the result is `Ok(None)` at the end of input or when
/// standard input cannot be read (D-575).
///
/// # Errors
///
/// The flush's error when standard output cannot be flushed: the REPL
/// then ends at once with status 1, printing nothing (D-306, spec §13.5).
/// The host's thread runs no entry, so there is no fiber to stop and no
/// host to tell: the failure goes back to its caller
/// (D-709, D-801).
pub fn read_input_line() -> io::Result<Option<String>> {
    flush_stdout()?;
    let waiter = Waiter::new();
    Ok(inline_marked(|| read_line_waiting(waiter))
        .ok()
        .flatten()
        .map(|line| String::from_utf8_lossy(&line).into_owned()))
}

/// Writes `text` to standard error at once, ending the program on failure
/// (D-306, D-709: a termination). For the runtime's own reports on an
/// entry, such as panics: through [`write_stream`], so a captured fiber's
/// report goes to its capture.
pub(crate) fn write_stderr(text: &str) {
    if try_write_stderr(text).is_err() {
        terminate(abi::status::ERR);
    }
}

/// Writes `text` to standard error at once, through [`write_stream`]. For
/// a host's thread, which runs no entry and ends with status 1 itself on a
/// failure (D-306), and for reports on an entry.
///
/// # Errors
///
/// The write's IO error.
pub(crate) fn try_write_stderr(text: &str) -> io::Result<()> {
    write_stream(Stream::Stderr, &[text.as_bytes()])
}

/// Writes `text` to the real standard error, never to a capture: the
/// runtime's last words before the process ends (a fatal runtime error,
/// D-770's edgeless unwinding), which no host would replay
/// (GAP(capture-skips-last-words)).
///
/// # Errors
///
/// The write's IO error.
pub(crate) fn write_last_words(text: &str) -> io::Result<()> {
    io::stderr().lock().write_all(text.as_bytes())
}

/// Ends the program with `status`, printing nothing more (D-306, D-709):
/// a termination, which the running entry's host receives as an outcome.
fn terminate(status: i32) -> ! {
    crate::sched::terminate(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn println_appends_a_newline() {
        let mut out = Vec::new();
        write_parts(&mut out, &[b"Hello, world!", b"\n"]).unwrap();
        write_parts(&mut out, &[b"a"]).unwrap();
        write_parts(&mut out, &[b"", b"\n"]).unwrap();
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
        assert!(write_parts(&mut Closed, &[b"x", b"\n"]).is_err());
    }

    /// D-684, D-574: `io::write_stdout` stores its failure state before it
    /// releases the stdout lock, so no `print` of another fiber runs
    /// between the failed write and the store. The stand-in for the locked
    /// stream records the state when it is dropped, which is the unlock.
    #[test]
    fn write_stdout_stores_its_flag_under_the_lock() {
        use std::sync::Arc;
        struct Locked {
            fail: bool,
            reported: Arc<AtomicBool>,
            at_unlock: Arc<std::sync::Mutex<Option<bool>>>,
        }
        impl Write for Locked {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.fail {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    Ok(())
                }
            }
        }
        impl Drop for Locked {
            fn drop(&mut self) {
                *self.at_unlock.lock().unwrap() = Some(self.reported.load(Ordering::Relaxed));
            }
        }
        let reported = Arc::new(AtomicBool::new(false));
        for fail in [true, false, true] {
            let at_unlock = Arc::new(std::sync::Mutex::new(None));
            let locked = Locked {
                fail,
                reported: Arc::clone(&reported),
                at_unlock: Arc::clone(&at_unlock),
            };
            let written = write_and_flush(locked, &[b"text"], &reported);
            assert_eq!(written.is_err(), fail);
            assert_eq!(
                *at_unlock.lock().unwrap(),
                Some(fail),
                "stored after the unlock"
            );
            assert_eq!(reported.load(Ordering::Relaxed), fail);
        }
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

    /// No fiber: the kernels run their jobs inline.
    const NO_FIBER: *mut ExecCtx = std::ptr::null_mut();

    fn read_file(path: &str) -> Result<String, (i64, String)> {
        // SAFETY: live string arguments, and no fiber.
        outcome(
            with_strings(&[path], |w| unsafe {
                cheby_io_sys_read_file(NO_FIBER, w[0])
            }),
            text,
        )
    }

    fn read_file_bytes(path: &str) -> Result<Vec<u8>, (i64, String)> {
        // SAFETY: live string arguments, and no fiber.
        outcome(
            with_strings(&[path], |w| unsafe {
                cheby_io_sys_read_file_bytes(NO_FIBER, w[0])
            }),
            bytes,
        )
    }

    fn write_file(path: &str, contents: &str) -> Result<bool, (i64, String)> {
        // SAFETY: live string arguments, and no fiber.
        outcome(
            with_strings(&[path, contents], |w| unsafe {
                cheby_io_sys_write_file(NO_FIBER, w[0], w[1])
            }),
            is_nil,
        )
    }

    fn write_file_bytes(path: &str, data: &[u8]) -> Result<bool, (i64, String)> {
        let data = values::bytes(data);
        // SAFETY: live arguments, and no fiber.
        let result = with_strings(&[path], |w| unsafe {
            cheby_io_sys_write_file_bytes(NO_FIBER, w[0], data)
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
        // A directory is another kind on every platform, although Windows
        // says "access denied", with the OS's message and no
        // ` (os error N)`.
        let dir_path = dir.to_str().unwrap();
        let (kind, message) = read_file(dir_path).unwrap_err();
        assert_eq!(kind, KIND_OTHER);
        assert!(
            !message.is_empty() && !message.contains("os error"),
            "{message}"
        );
        assert_eq!(read_file_bytes(dir_path).map_err(|e| e.0), Err(KIND_OTHER));
        assert_eq!(write_file(dir_path, "x").map_err(|e| e.0), Err(KIND_OTHER));
        assert_eq!(
            write_file_bytes(dir_path, b"x").map_err(|e| e.0),
            Err(KIND_OTHER)
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

    /// An input that gives its chunks one read at a time, each read
    /// interrupted once first, and then fails with `error` or ends.
    struct Chunks {
        parts: std::collections::VecDeque<&'static [u8]>,
        interrupt: bool,
        error: Option<io::ErrorKind>,
    }

    impl Chunks {
        fn new(chunks: &[&'static [u8]], error: Option<io::ErrorKind>) -> Self {
            Self {
                parts: chunks.iter().copied().collect(),
                interrupt: false,
                error,
            }
        }
    }

    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            match self.parts.pop_front() {
                Some(chunk) => {
                    let count = chunk.len().min(buf.len());
                    buf[..count].copy_from_slice(&chunk[..count]);
                    if count < chunk.len() {
                        self.parts.push_front(&chunk[count..]);
                    }
                    Ok(count)
                }
                None => match self.error {
                    Some(kind) => Err(io::Error::from(kind)),
                    None => Ok(0),
                },
            }
        }
    }

    /// Every line of `input` through one reader, as text.
    fn lines_of(input: &mut Chunks) -> io::Result<Vec<String>> {
        let mut reader = LineReader::new();
        let mut lines = Vec::new();
        while let Some(line) = reader.read_line(input)? {
            lines.push(String::from_utf8(line).unwrap());
        }
        Ok(lines)
    }

    /// GAP(stdin-fast-path): the reader splits the input into the lines
    /// `BufRead::read_until` gave, whatever the chunks: endings removed
    /// (D-305), a lone `\r` kept, the unfinished last line returned at
    /// the end, interrupted reads retried.
    #[test]
    fn stdin_fast_path_reader_splits_lines_across_chunks() {
        let mut input = Chunks::new(
            &[
                b"fir",
                b"st\n\r",
                b"\nsec",
                b"ond\r\r\n",
                b"\n",
                b"la",
                b"st",
            ],
            None,
        );
        assert_eq!(
            lines_of(&mut input).unwrap(),
            ["first", "", "second\r", "", "last"]
        );
        let mut empty = Chunks::new(&[], None);
        assert_eq!(lines_of(&mut empty).unwrap(), Vec::<String>::new());
        // Several lines in one chunk: the first read buffers them all, and
        // the rest are served without reading.
        let mut reader = LineReader::new();
        let mut one_chunk = Chunks::new(&[b"a\nb\r\nc\n"], None);
        assert_eq!(
            reader.read_line(&mut one_chunk).unwrap(),
            Some(b"a".to_vec())
        );
        assert_eq!(reader.buffered_line(), Some(b"b".to_vec()));
        assert_eq!(reader.buffered_line(), Some(b"c".to_vec()));
        assert_eq!(reader.buffered_line(), None);
        assert_eq!(reader.read_line(&mut one_chunk).unwrap(), None);
        // A line longer than one chunk.
        let long: &'static [u8] = Box::leak(vec![b'x'; STDIN_CHUNK * 2 + 3].into_boxed_slice());
        let mut long_input = Chunks::new(&[long, b"\nend\n"], None);
        let lines = lines_of(&mut long_input).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), STDIN_CHUNK * 2 + 3);
        assert_eq!(lines[1], "end");
    }

    /// GAP(stdin-fast-path), gate 3's ROB-W3-1: a line much longer than one
    /// read, arriving in small chunks, is searched for its end once: the
    /// bytes searched stay linear in the input (at most twice its length
    /// here, never once per chunk from the line's start), and lines after
    /// it are split as before.
    #[test]
    fn stdin_fast_path_reader_searches_a_long_line_once() {
        const LONG: usize = 4 << 20;
        const CHUNK: usize = 4 << 10;
        let long: &'static [u8] = Box::leak(vec![b'x'; LONG].into_boxed_slice());
        let mut parts: Vec<&'static [u8]> = long.chunks(CHUNK).collect();
        parts.push(b"\nshort\nend");
        let mut input = Chunks::new(&parts, None);
        let mut reader = LineReader::new();
        let first = reader.read_line(&mut input).unwrap().unwrap();
        assert_eq!(first.len(), LONG);
        assert_eq!(
            reader.read_line(&mut input).unwrap(),
            Some(b"short".to_vec())
        );
        assert_eq!(reader.read_line(&mut input).unwrap(), Some(b"end".to_vec()));
        assert_eq!(reader.read_line(&mut input).unwrap(), None);
        assert!(
            reader.searched <= 2 * (LONG + 10),
            "{} bytes searched for {} bytes of input",
            reader.searched,
            LONG + 10
        );
    }

    /// GAP(stdin-fast-path): a failing read consumes the unfinished line,
    /// as `read_until` did, and keeps nothing of it for the next read; the
    /// whole lines before it were already returned.
    #[test]
    fn stdin_fast_path_reader_failure_consumes_the_unfinished_line() {
        let mut reader = LineReader::new();
        let mut input = Chunks::new(&[b"whole\nhal"], Some(io::ErrorKind::BrokenPipe));
        assert_eq!(
            reader.read_line(&mut input).unwrap(),
            Some(b"whole".to_vec())
        );
        assert_eq!(
            reader.read_line(&mut input).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(reader.buffered_line(), None);
        let mut after = Chunks::new(&[b"next\n"], None);
        assert_eq!(
            reader.read_line(&mut after).unwrap(),
            Some(b"next".to_vec())
        );
    }

    /// GAP(stdin-fast-path), D-548: the fast path serves a buffered line
    /// only while no other read waits for the reader (it asked first) and
    /// no thread holds the reader (it waits for input); a waiter counts
    /// until it is dropped, also with a job that never ran.
    #[test]
    fn stdin_fast_path_serves_only_when_no_read_waits() {
        static READER: Mutex<LineReader> = Mutex::new(LineReader::new());
        static WAITERS: AtomicUsize = AtomicUsize::new(0);
        let mut input = Chunks::new(&[b"one\ntwo\nthree\nfour\n"], None);
        assert_eq!(
            READER.lock().read_line(&mut input).unwrap(),
            Some(b"one".to_vec())
        );
        assert_eq!(serve_buffered(&READER, &WAITERS), Some(b"two".to_vec()));
        let waiter = Waiter::on(&WAITERS);
        assert_eq!(serve_buffered(&READER, &WAITERS), None, "a read waits");
        let job = move || drop(waiter);
        drop(job);
        assert_eq!(
            WAITERS.load(Ordering::SeqCst),
            0,
            "a dropped job ends its count"
        );
        let held = READER.lock();
        assert_eq!(
            serve_buffered(&READER, &WAITERS),
            None,
            "the reader is held"
        );
        drop(held);
        assert_eq!(serve_buffered(&READER, &WAITERS), Some(b"three".to_vec()));
        assert_eq!(serve_buffered(&READER, &WAITERS), Some(b"four".to_vec()));
        assert_eq!(serve_buffered(&READER, &WAITERS), None, "nothing buffered");
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

    /// The child side of `stream_kernels_write_and_read_the_standard_streams`:
    /// does nothing unless `CHEBY_RT_TEST_IO` is set. It writes through the
    /// four print kernels and reads standard input to its end through
    /// `cheby_io_sys_read_line`, checking each line.
    #[test]
    fn stream_kernels_child() {
        if std::env::var_os("CHEBY_RT_TEST_IO").is_none() {
            return;
        }
        let before = live();
        let print = |kernel: unsafe extern "C" fn(Word), text: &str| {
            let word = values::string(text);
            // SAFETY: a live string, borrowed by the kernel.
            unsafe {
                kernel(word);
                rc::drop(word);
            }
        };
        print(cheby_io_print, "<<out:");
        print(cheby_io_println, "line>>");
        print(cheby_io_eprint, "<<err:");
        print(cheby_io_eprintln, "line>>");
        let mut lines = Vec::new();
        loop {
            // SAFETY: no fiber: the read runs inline.
            match outcome(unsafe { cheby_io_sys_read_line(NO_FIBER) }, |option| {
                // SAFETY: a live `Option<String>`.
                unsafe { values::read_option(option) }.map(text)
            }) {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => break,
                Err(error) => panic!("read_line failed: {error:?}"),
            }
        }
        assert_eq!(lines, ["first", "", "second\r", "last"]);
        assert_eq!(live(), before);
    }

    /// `io::print`, `println`, `eprint`, `eprintln` and `read_line` on real
    /// standard streams, in a child process (this test binary's
    /// `stream_kernels_child`).
    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn stream_kernels_write_and_read_the_standard_streams() {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "io::tests::stream_kernels_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHEBY_RT_TEST_IO", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"first\n\r\nsecond\r\r\nlast")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}{stderr}");
        assert!(stdout.contains("<<out:line>>\n"), "{stdout}");
        assert!(stderr.contains("<<err:line>>\n"), "{stderr}");
    }
}
