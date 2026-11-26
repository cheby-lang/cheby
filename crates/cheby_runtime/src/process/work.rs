//! The Unix bodies of the `std::process` kernels of children (D-1907 to
//! D-1919), which `process.rs`'s kernels call inside their guards: the
//! arguments read on the fiber, the work, and the result made on it.

use std::sync::Arc;

use super::StartArgs;
use super::child::ChildState;
use super::error::ProcError;
use super::pipes::{ReaderHandle, ReaderState, Work, WriterHandle, WriterState};
use super::registry;
use super::spawn::{self, Slot, Spec};
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::Cancelled;
use crate::fiber_ops::{needs_scheduler, running};
use crate::file::FileState;
use crate::handles::{self, HandleKind};
use crate::panic::fatal;
use crate::sched::Shared;
use crate::sched::blocking::run_blocking;
use crate::unwind::raise::raise_cancel;
use crate::{capture, io, list, string, values};

/// The running fiber of a kernel's `ctx` and its runtime, or the fatal
/// error that names `operation` when the entry runs no scheduler (D-983,
/// D-714, D-1919).
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block.
pub(super) unsafe fn scheduled<'a>(
    ctx: *mut ExecCtx,
    operation: &str,
) -> (&'a ExecCtx, &'a Arc<Shared>) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    match me.sched.runtime() {
        Some(runtime) => (me, runtime),
        None => needs_scheduler(operation),
    }
}

/// The text of a borrowed `String` argument.
///
/// # Safety
///
/// `text` is the uniform word of a live `String`, borrowed.
unsafe fn str_arg(text: Word) -> String {
    // SAFETY: forwarded from the caller.
    unsafe { string::as_str_unchecked(text) }
        .unwrap_or_else(|| fatal(format_args!("{text:#x} is not a String")))
        .to_owned()
}

/// The payload of a borrowed `Child`.
///
/// # Safety
///
/// `child` is a live `Child` handle, borrowed for `'a`.
unsafe fn child_of<'a>(child: Word) -> &'a ChildState {
    // SAFETY: forwarded from the caller.
    unsafe { handles::payload::<ChildState>(child, HandleKind::Child) }
}

/// The state of a borrowed `Reader`.
///
/// # Safety
///
/// `reader` is a live `Reader` handle, borrowed for `'a`.
unsafe fn reader_of<'a>(reader: Word) -> &'a ReaderState {
    // SAFETY: forwarded from the caller.
    &unsafe { handles::payload::<ReaderHandle>(reader, HandleKind::Reader) }.0
}

/// The state of a borrowed `Writer`.
///
/// # Safety
///
/// `writer` is a live `Writer` handle, borrowed for `'a`.
unsafe fn writer_of<'a>(writer: Word) -> &'a WriterState {
    // SAFETY: forwarded from the caller.
    &unsafe { handles::payload::<WriterHandle>(writer, HandleKind::Writer) }.0
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

/// The description of `args` (D-1908, D-1909), checked: kind 13 first for
/// every text and code, then kind 12 for a closed file.
///
/// # Safety
///
/// Every word of `args` is live and of the type `sys_start` declares.
unsafe fn read_spec(args: &StartArgs) -> Result<Spec, ProcError> {
    // SAFETY: forwarded from the caller.
    let (program, arguments, dir, env) = unsafe {
        let program = str_arg(args.program);
        let arguments: Vec<String> = list::iter(args.args).map(|arg| str_arg(arg)).collect();
        let dir = values::read_option(args.dir).map(|dir| str_arg(dir));
        let env: Vec<(String, Option<String>)> = list::iter(args.env)
            .map(|edit| match values::read_tuple(edit).as_slice() {
                &[name, value] => (
                    str_arg(name),
                    values::read_option(value).map(|value| str_arg(value)),
                ),
                _ => fatal(format_args!("{edit:#x} is not an environment edit")),
            })
            .collect();
        (program, arguments, dir, env)
    };
    let mut slots: [Slot; 3] = [Slot::Inherit, Slot::Inherit, Slot::Inherit];
    for (index, (slot, &(code, file))) in slots.iter_mut().zip(&args.streams).enumerate() {
        // SAFETY: forwarded from the caller.
        let file = unsafe { values::read_option(file) };
        *slot = match (code, file) {
            // The file's place, set below once every code is checked.
            (3, Some(_)) => Slot::Inherit,
            (3, None) => return Err(ProcError::invalid_input()),
            (code, _) => Slot::of(code, None, index)?,
        };
    }
    let mut spec = Spec {
        program,
        args: arguments,
        dir,
        clear_env: args.clear_env,
        env,
        slots,
        capture: false,
        temp_parent: crate::file::temp_parent(),
        owner: 0,
    };
    spec.check()?;
    for (slot, &(code, file)) in spec.slots.iter_mut().zip(&args.streams) {
        if code != 3 {
            continue;
        }
        // SAFETY: forwarded from the caller: a `Some` of a live `File`.
        let Some(file) = (unsafe { values::read_option(file) }) else {
            continue;
        };
        // SAFETY: as above.
        let state = unsafe { handles::payload::<FileState>(file, HandleKind::File) };
        *slot = Slot::File(state.child_stream().map_err(ProcError::from)?);
    }
    Ok(spec)
}

/// `sys_start`'s body (D-1916): the checks, the flush of standard output,
/// the start on the blocking pool, and the `Child` handle.
///
/// # Safety
///
/// `ctx` is the running fiber's context; the words of `args` are live and
/// borrowed.
pub(crate) unsafe fn start(ctx: *mut ExecCtx, args: &StartArgs) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, runtime) = unsafe { scheduled(ctx, "process::start") };
    // SAFETY: forwarded from the caller.
    let mut spec = match unsafe { read_spec(args) } {
        Ok(spec) => spec,
        Err(error) => return error.word(),
    };
    io::flush_before_read();
    registry::start_reaper();
    let capture = capture::current();
    spec.capture = capture.is_some();
    spec.owner = super::owner_of(runtime);
    if registry::is_ended(spec.owner) {
        // D-1917: the run ended; a start now would outlive it.
        return ProcError::other("the program is ending").word();
    }
    let spawned = match run_blocking(me, move || spawn::spawn(&spec)) {
        Ok(spawned) => spawned,
        // The job's result was dropped: its child was killed and handed
        // to the reaper (D-1916).
        Err(Cancelled) => raise_cancel(Some(ctx)),
    };
    match spawned.and_then(|spawned| ChildState::new(runtime, spawned, capture)) {
        Ok(state) => values::ok(
            // SAFETY: the running fiber's context.
            unsafe { handles::new_handle(ctx, HandleKind::Child, Box::new(state)) },
        ),
        Err(error) => error.word(),
    }
}

/// `sys_wait`'s body: `Ok((0, code))` or `Ok((1, signal))`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `child` a live `Child`,
/// borrowed.
pub(crate) unsafe fn wait(ctx: *mut ExecCtx, child: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, "process::wait") };
    // SAFETY: forwarded from the caller.
    let child = unsafe { child_of(child) };
    outcome(ctx, child.wait(me), |status| {
        let (how, value) = status.pair();
        values::tuple(&[values::int_word(how), values::int_word(value)])
    })
}

/// `sys_signal`'s body: 0 SIGINT, 1 SIGTERM, 2 SIGKILL (D-1913).
///
/// # Safety
///
/// As for [`wait`].
pub(crate) unsafe fn signal(ctx: *mut ExecCtx, child: Word, which: i64) -> Word {
    // SAFETY: forwarded from the caller.
    let _ = unsafe { scheduled(ctx, "process::signal") };
    // SAFETY: forwarded from the caller.
    let child = unsafe { child_of(child) };
    let signal = match which {
        0 => libc::SIGINT,
        1 => libc::SIGTERM,
        2 => libc::SIGKILL,
        _ => return ProcError::invalid_input().word(),
    };
    outcome(ctx, Ok(child.signal(signal)), |()| values::nil())
}

/// `sys_stdin`'s body: a `Writer` sharing the child's standard input.
///
/// # Safety
///
/// As for [`wait`].
pub(crate) unsafe fn stdin(ctx: *mut ExecCtx, child: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let _ = unsafe { scheduled(ctx, "process::stdin") };
    // SAFETY: forwarded from the caller.
    let child = unsafe { child_of(child) };
    let payload = WriterHandle(Arc::clone(&child.stdin));
    // SAFETY: the running fiber's context.
    unsafe { handles::new_handle(ctx, HandleKind::Writer, Box::new(payload)) }
}

/// `sys_output`'s body: a `Reader` sharing the child's standard output
/// (1) or error (2).
///
/// # Safety
///
/// As for [`wait`].
pub(crate) unsafe fn output(ctx: *mut ExecCtx, child: Word, which: i64) -> Word {
    // SAFETY: forwarded from the caller.
    let _ = unsafe { scheduled(ctx, "process::output") };
    // SAFETY: forwarded from the caller.
    let child = unsafe { child_of(child) };
    let state = match which {
        1 => &child.stdout,
        2 => &child.stderr,
        other => fatal(format_args!("`process::sys_output` got the stream {other}")),
    };
    let payload = ReaderHandle(Arc::clone(state));
    // SAFETY: the running fiber's context.
    unsafe { handles::new_handle(ctx, HandleKind::Reader, Box::new(payload)) }
}

/// `Some(text)` for a line that is UTF-8, else the invalid-data pair; the
/// line was consumed either way (D-311).
fn line_word(ctx: *mut ExecCtx, result: Work<Option<Vec<u8>>>) -> Word {
    match result {
        Ok(Ok(Some(line))) => match std::str::from_utf8(&line) {
            Ok(text) => values::ok(values::some(values::string(text))),
            Err(_) => ProcError::fixed(super::error::KIND_INVALID_DATA).word(),
        },
        other => outcome(ctx, other, |_| values::none()),
    }
}

/// `sys_read_line`'s body.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `reader` a live `Reader`,
/// borrowed.
pub(crate) unsafe fn read_line(ctx: *mut ExecCtx, reader: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, "process::read_line") };
    // SAFETY: forwarded from the caller.
    let reader = unsafe { reader_of(reader) };
    line_word(ctx, reader.read_line(me))
}

/// `sys_read_bytes`'s body.
///
/// # Safety
///
/// As for [`read_line`].
pub(crate) unsafe fn read_bytes(ctx: *mut ExecCtx, reader: Word, limit: i64) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, "process::read_bytes") };
    // SAFETY: forwarded from the caller.
    let reader = unsafe { reader_of(reader) };
    outcome(ctx, reader.read_bytes(me, limit), |data| match data {
        Some(data) => values::some(values::bytes(&data)),
        None => values::none(),
    })
}

/// `sys_read_all`'s body.
///
/// # Safety
///
/// As for [`read_line`].
pub(crate) unsafe fn read_all(ctx: *mut ExecCtx, reader: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, "process::read_all") };
    // SAFETY: forwarded from the caller.
    let reader = unsafe { reader_of(reader) };
    outcome(ctx, reader.read_all(me), |data| values::bytes(&data))
}

/// `sys_write_bytes`'s and `sys_write`'s body: `data` is a `Bytes` or a
/// `String`, whose UTF-8 is written.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `writer` a live `Writer` and
/// `data` a live `Bytes` or `String`, both borrowed, unchanged for the
/// whole call.
pub(crate) unsafe fn write(ctx: *mut ExecCtx, writer: Word, data: Word, operation: &str) -> Word {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, operation) };
    // SAFETY: forwarded from the caller.
    let writer = unsafe { writer_of(writer) };
    // SAFETY: forwarded from the caller.
    let data = unsafe { values::bytes_of(data) };
    outcome(ctx, writer.write(me, data), |()| values::nil())
}

/// `sys_close`'s body: never parks.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `writer` a live `Writer`,
/// borrowed.
pub(crate) unsafe fn close(ctx: *mut ExecCtx, writer: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let _ = unsafe { scheduled(ctx, "process::close") };
    // SAFETY: forwarded from the caller.
    let writer = unsafe { writer_of(writer) };
    outcome(ctx, Ok(writer.close()), |()| values::nil())
}

/// `sys_finish`'s body (D-1912): a cancellation raises once the child is
/// finished.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `child` a live `Child`,
/// borrowed.
pub(crate) unsafe fn finish(ctx: *mut ExecCtx, child: Word) {
    // SAFETY: forwarded from the caller.
    let (me, _) = unsafe { scheduled(ctx, "process::finish") };
    // SAFETY: forwarded from the caller.
    let child = unsafe { child_of(child) };
    if child.finish(me).is_err() {
        raise_cancel(Some(ctx));
    }
}
