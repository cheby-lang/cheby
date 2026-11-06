//! The `std::process` kernel (D-985, D-1022, D-1023): `exit` ends the
//! program as a termination does (D-709).
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

use std::sync::atomic::{AtomicI32, Ordering};

use crate::abi::{Word, status};
use crate::ctx::ExecCtx;
use crate::entry::EntryMode;
use crate::io;
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;

/// Appends every `std::process` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
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

/// Every `std::process` kernel.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 1] {
    [(
        "cheby_process_sys_exit",
        cheby_process_sys_exit as *const u8,
        &["i64"],
        "Word",
    )]
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
