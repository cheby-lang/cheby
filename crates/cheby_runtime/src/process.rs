//! The `std::process` kernels (D-985, D-1022, D-1023, D-1907 to D-1919):
//! `exit` ends the program as a termination does (D-709), and children
//! start, are waited for and signalled, and talk through pipes on the
//! poller.
//!
//! `cheby_process_sys_exit` flushes standard output, then terminates with
//! the status: one from 0 to 255 as given, any other 255, so a failure
//! never wraps to success (D-1022); a flush that fails gives 1, as for any
//! failed flush (D-306), unless an `io::write_stdout` already reported that
//! failure (D-574, `io::flush_stdout`). The termination is the runtime's
//! (`sched::terminate`): the calling fiber freezes where it is, every other
//! fiber stops without unwinding, no drop function and no user code runs,
//! and the host receives the status (D-709). It never calls libc's `exit`.
//! Under `cheby test` it ends only the calling test, with
//! `process::exit called with status N` as its failure (D-1023,
//! [`test_exit_report`]); the REPL ends the session with the status, as
//! after any termination (D-828).
//!
//! D-1293: the kernel is `@std_context` (`process.md`'s
//! kernel paragraph) and its C result is the uniform word of `Never`, which
//! it never returns.
//!
//! # Children (D-1907 to D-1919)
//!
//! - **Handles** (D-1919, D-703): a `Child` is runtime kind 49, a `Reader`
//!   50 and a `Writer` 51 (`abi::layout`), handle objects with a payload.
//!   A `Child`'s payload (`child.rs`) owns the process, its exit
//!   descriptor and its pipes' states, which its `Reader` and `Writer`
//!   handles share (`pipes.rs`). Its finalizer kills a child not yet
//!   reaped and hands it to the reaper; a pipe's descriptor closes with its
//!   last owner.
//! - **Starting** (D-1908, D-1909, D-1916, `spawn.rs`): the checks on the
//!   fiber, the flush of standard output, then one blocking-pool job that
//!   starts the child (`sched::blocking::run_blocking`); a cancellation
//!   meanwhile waits for the job and drops its result, which kills the
//!   child and hands it to the reaper.
//! - **Waiting** (D-1913, D-1916): on the poller, through the child's exit
//!   descriptor, so a waiting fiber counts as pending IO (D-984); a
//!   cancelled wait takes nothing.
//! - **The program's end** (D-1917, `registry.rs`): every runtime-controlled
//!   end of a run sends SIGKILL to every child that the run's runtime
//!   started and that is still open, and waits for none
//!   ([`kill_children_of`]): a termination (`process::exit` and every
//!   other, `sched::terminate`), the end of `run_main`, whatever ended
//!   `main`, a deadlock included, and the end of a REPL session
//!   (`cheby_driver`). A fatal runtime error (`panic::fatal`) ends the
//!   whole process and kills every child of it ([`kill_open_children`]).
//!   `cheby test`'s per-test exits unwind instead (D-1294).
//! - **Under `cheby test`** (D-1918): a child's inherited standard output
//!   and error are unlinked temporary files, joined to the test's capture
//!   when the child is reaped.
//! - **Without a scheduler** (D-983, D-714, D-1919): every kernel of
//!   children ends the program with a fatal runtime error that names its
//!   operation, so on Windows they are fatal until M6.

use std::sync::atomic::{AtomicI32, Ordering};

use crate::abi::{Word, status};
use crate::ctx::ExecCtx;
use crate::entry::EntryMode;
use crate::io;
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;

#[cfg(unix)]
mod child;
#[cfg(unix)]
mod error;
#[cfg(unix)]
mod pipes;
#[cfg(unix)]
mod registry;
#[cfg(unix)]
mod spawn;
#[cfg(unix)]
mod work;
#[cfg(not(unix))]
#[path = "process/unsupported.rs"]
mod work;

#[cfg(test)]
#[cfg(unix)]
#[path = "process/tests.rs"]
mod tests;

/// Appends every `std::process` kernel symbol and its address to `out`,
/// and registers the kinds 49 to 51 (D-1919).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    crate::handles::register();
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// Sends SIGKILL to every child of the process that started and was not
/// reaped, waiting for none (D-1917): where the whole process ends, at a
/// fatal runtime error. Nothing on Windows, where no child starts before
/// M6.
pub fn kill_open_children() {
    #[cfg(unix)]
    registry::kill_all();
}

/// Sends SIGKILL to every child that `runtime`'s fibers started and that
/// was not reaped, waiting for none (D-1917): at the end of the run or the
/// REPL session that `runtime` serves, so the children of other runtimes
/// of the process live on.
pub fn kill_children_of(runtime: &crate::Runtime) {
    kill_children_of_shared(&runtime.shared_handle());
}

/// [`kill_children_of`] by the runtime's shared part: at a termination
/// (`sched::terminate`).
pub(crate) fn kill_children_of_shared(shared: &crate::sched::Shared) {
    #[cfg(unix)]
    registry::kill_owned(owner_of(shared));
    #[cfg(not(unix))]
    let _ = shared;
}

/// The key of the runtime `shared` in the open-children registry
/// (`Shared::child_owner`), which no other runtime of the process has.
#[cfg(unix)]
fn owner_of(shared: &crate::sched::Shared) -> u64 {
    shared.child_owner
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["process"];

/// The exit status of `process::exit(status)` (D-1022): `status` itself
/// from 0 to 255, else 255.
#[must_use]
pub(crate) fn exit_status(status: i64) -> i32 {
    u8::try_from(status).map_or(255, i32::from)
}

/// The failure text of a test that called `process::exit` (D-1023): its
/// failure block's body under `cheby test`.
#[must_use]
pub fn test_exit_report(status: i32) -> String {
    format!("process::exit called with status {status}\n")
}

/// The status of the last `process::exit` that a test on the thread
/// backend called, or -1 (D-1023, D-714): that backend runs one test at a
/// time, each on an isolated entry, so its host reads this right after the
/// entry reports `Exited` ([`take_test_exit`]).
static ISOLATED_TEST_EXIT: AtomicI32 = AtomicI32::new(-1);

/// The status of the `process::exit` that ended the isolated test entry
/// that just reported `Exited`, taken once; `None` when that termination
/// was another one, such as a failed write (D-306), which ends the run.
#[must_use]
pub fn take_test_exit() -> Option<i32> {
    let status = ISOLATED_TEST_EXIT.swap(-1, Ordering::SeqCst);
    (status >= 0).then_some(status)
}

/// `process::exit`'s kernel (D-1022): flushes standard output and ends the
/// program with [`exit_status`]'s status, or 1 when the flush fails
/// (D-306, D-574); under `cheby test`, ends only the calling test
/// (D-1023). Never returns.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_process_sys_exit(ctx: *mut ExecCtx, status: i64) -> Word {
    let status = exit_status(status);
    // SAFETY: `ctx` is the running fiber's context (the caller's
    // contract), and only a shared reference is made.
    let me = unsafe { &*ctx };
    if matches!(me.entry_mode(), EntryMode::Test) && end_test(me, status) {
        return guarded(Some(ctx), || -> Word { raise_cancel(Some(ctx)) });
    }
    let status = if io::flush_stdout().is_ok() {
        status
    } else {
        status::ERR
    };
    crate::sched::terminate(status)
}

/// D-1023: `process::exit` under `cheby test` fails the calling test with
/// [`test_exit_report`] and the run continues; returns whether the calling
/// fiber must now unwind.
///
/// - On a runtime, the test's root is ended as the deadlock action ends a
///   test root (D-859, `sched::end_test_root`): the report is stored on
///   the root, its base scope is cancelled, so the root and the fibers of
///   its scopes unwind and the host receives the report as the test's
///   failure, then tears the test's group down as at every test's end
///   (D-723). The first report of a root wins.
/// - On the thread backend (D-714) the test is an isolated entry: the
///   status is recorded for its host ([`take_test_exit`]), and the
///   termination parks the entry's thread for good, as any termination of
///   an isolated entry does. Its output is not captured there and is
///   written as it goes, so nothing is flushed.
///
/// False when the running fiber's domain has no test root: the exit is then
/// a termination, as under `cheby run`.
fn end_test(me: &ExecCtx, status: i32) -> bool {
    if me.sched.runtime().is_none() {
        ISOLATED_TEST_EXIT.store(status, Ordering::SeqCst);
        crate::sched::terminate(status)
    }
    crate::sched::end_test_root(me, test_exit_report(status))
}

/// A kernel: its symbol, its address, its C parameter types after the
/// context pointer, and its C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The words of `sys_start`'s arguments, borrowed: the program, the
/// arguments, the directory, whether the environment starts empty, its
/// edits, and each stream's code with its `Option<file::File>`.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "children start on Unix only until M6 (D-1919)")
)]
pub(crate) struct StartArgs {
    pub(crate) program: Word,
    pub(crate) args: Word,
    pub(crate) dir: Word,
    pub(crate) clear_env: bool,
    pub(crate) env: Word,
    pub(crate) streams: [(i64, Word); 3],
}

/// `process::with_child`'s, `run`'s and `output`'s start (D-1908,
/// D-1909, D-1916): `Ok(Child)`, or the failure as a kind and a message.
/// Stream codes: 0 inherited, 1 discarded, 2 piped, 3 the file that follows
/// it, 4 (standard error only) wherever standard output goes.
///
/// Every kernel of children forwards to `work`, whose functions on
/// Windows end the program with a fatal runtime error that names their
/// operation until M6 (D-1919, `process/unsupported.rs`).
///
/// # Safety
///
/// `ctx` is the running fiber's context; every word is live, borrowed,
/// and of the type the declaration in `std/process.cheby` gives it.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_start(
    ctx: *mut ExecCtx,
    program: Word,
    args: Word,
    dir: Word,
    clear_env: bool,
    env: Word,
    stdin: i64,
    stdin_file: Word,
    stdout: i64,
    stdout_file: Word,
    stderr: i64,
    stderr_file: Word,
) -> Word {
    let args = StartArgs {
        program,
        args,
        dir,
        clear_env,
        env,
        streams: [
            (stdin, stdin_file),
            (stdout, stdout_file),
            (stderr, stderr_file),
        ],
    };
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::start(ctx, &args) })
}
/// `process::wait`'s kernel (D-1913): `Ok((0, code))` for an exit,
/// `Ok((1, signal))` for a signal.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `child` is a live `Child`,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_wait(ctx: *mut ExecCtx, child: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::wait(ctx, child) })
}
/// `interrupt`'s, `terminate`'s and `kill`'s kernel (D-1913): 0 SIGINT,
/// 1 SIGTERM, 2 SIGKILL.
///
/// # Safety
///
/// As for [`cheby_process_sys_wait`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_signal(
    ctx: *mut ExecCtx,
    child: Word,
    which: i64,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::signal(ctx, child, which) })
}
/// `process::stdin`'s kernel (D-1914): the child's standard input.
///
/// # Safety
///
/// As for [`cheby_process_sys_wait`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_stdin(ctx: *mut ExecCtx, child: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::stdin(ctx, child) })
}
/// `process::stdout`'s (1) and `stderr`'s (2) kernel (D-1914).
///
/// # Safety
///
/// As for [`cheby_process_sys_wait`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_output(
    ctx: *mut ExecCtx,
    child: Word,
    which: i64,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::output(ctx, child, which) })
}
/// `process::read_bytes`'s kernel (D-1914): `Some(Bytes)`, or `None` at
/// the end.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `reader` is a live `Reader`,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_read_bytes(
    ctx: *mut ExecCtx,
    reader: Word,
    limit: i64,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        work::read_bytes(ctx, reader, limit)
    })
}
/// `process::read_line`'s kernel (D-1914, D-305, D-311).
///
/// # Safety
///
/// As for [`cheby_process_sys_read_bytes`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_read_line(
    ctx: *mut ExecCtx,
    reader: Word,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::read_line(ctx, reader) })
}
/// `process::output`'s read of a stream to its end (D-1911).
///
/// # Safety
///
/// As for [`cheby_process_sys_read_bytes`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_read_all(
    ctx: *mut ExecCtx,
    reader: Word,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::read_all(ctx, reader) })
}
/// `process::write_bytes`'s kernel (D-1914): all of `data`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `writer` is a live `Writer` and
/// `data` a live `Bytes`, both borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_write_bytes(
    ctx: *mut ExecCtx,
    writer: Word,
    data: Word,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        work::write(ctx, writer, data, "process::write_bytes")
    })
}
/// `process::write`'s and `write_line`'s kernel (D-1914): `text` as UTF-8.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `writer` is a live `Writer` and
/// `text` a live `String`, both borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_write(
    ctx: *mut ExecCtx,
    writer: Word,
    text: Word,
) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        work::write(ctx, writer, text, "process::write")
    })
}
/// `process::close`'s kernel (D-1914): closes standard input now; never
/// parks.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `writer` is a live `Writer`,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_close(ctx: *mut ExecCtx, writer: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::close(ctx, writer) })
}
/// The end of `process::with_child`'s body (D-1912, D-1928): kills a
/// child still running, waits for it and reaps it, then closes its pipes.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `child` is a live `Child`,
/// borrowed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_process_sys_finish(ctx: *mut ExecCtx, child: Word) {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { work::finish(ctx, child) });
}

/// Every `std::process` kernel; each takes the running fiber's context
/// (`@std_context`, D-692).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 13] {
    const W: &str = "Word";
    const I: &str = "i64";
    const B: &str = "bool";
    [
        (
            "cheby_process_sys_exit",
            cheby_process_sys_exit as *const u8,
            &[I],
            W,
        ),
        (
            "cheby_process_sys_start",
            cheby_process_sys_start as *const u8,
            &[W, W, W, B, W, I, W, I, W, I, W],
            W,
        ),
        (
            "cheby_process_sys_wait",
            cheby_process_sys_wait as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_process_sys_signal",
            cheby_process_sys_signal as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_process_sys_stdin",
            cheby_process_sys_stdin as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_process_sys_output",
            cheby_process_sys_output as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_process_sys_read_bytes",
            cheby_process_sys_read_bytes as *const u8,
            &[W, I],
            W,
        ),
        (
            "cheby_process_sys_read_line",
            cheby_process_sys_read_line as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_process_sys_read_all",
            cheby_process_sys_read_all as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_process_sys_write_bytes",
            cheby_process_sys_write_bytes as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_process_sys_write",
            cheby_process_sys_write as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_process_sys_close",
            cheby_process_sys_close as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_process_sys_finish",
            cheby_process_sys_finish as *const u8,
            &[W],
            "()",
        ),
    ]
}

/// The C signatures of the kernels after the context pointer, for the
/// cross-check against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}
