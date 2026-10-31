//! Tests of the `std::process` kernel (D-1022, D-1023, D-709).

use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, run_entry};
use crate::fiber_ops::detached_tests::{config, runtime};
use crate::sched::testing;

use super::process::{cheby_process_sys_exit, exit_status, take_test_exit, test_exit_report};

/// The running fiber's context pointer, as kernels take it.
fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// D-1022: a status from 0 to 255 is used as given; any other, negative or
/// larger, is 255, so a failure never wraps to success.
#[test]
fn process_exit_status_is_clamped_to_255() {
    for status in [0, 1, 2, 42, 101, 255] {
        assert_eq!(i64::from(exit_status(status)), status);
    }
    for status in [-1, -255, 256, 257, 1 << 32, i64::MIN, i64::MAX] {
        assert_eq!(exit_status(status), 255, "{status}");
    }
}

/// D-1023: the failure text of a test that called `exit`.
#[test]
fn process_exit_test_report_names_the_status() {
    assert_eq!(test_exit_report(4), "process::exit called with status 4\n");
}

/// A thunk that calls `exit(300)`.
unsafe extern "C" fn exit_300(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's own context.
    unsafe { cheby_process_sys_exit(ctx, 300) }
}

/// A thunk that calls `exit(4)`.
unsafe extern "C" fn exit_4(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the entry's own context.
    unsafe { cheby_process_sys_exit(ctx, 4) }
}

/// D-1022, D-709: on an isolated `main` entry (the thread backend) `exit`
/// is a termination: the host receives `Exited` with the clamped status,
/// and no test exit is recorded.
#[test]
fn process_exit_on_an_isolated_run_entry_terminates() {
    let config = EntryConfig {
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::Run)
    };
    // SAFETY: a test thunk with the entry ABI.
    let outcome = unsafe { run_entry(exit_300, &config) };
    assert_eq!(outcome.unwrap(), EntryOutcome::Exited(255));
}

/// D-1023, D-714: on an isolated test entry (the thread backend) `exit`
/// records its status for the test host, once, and ends the entry.
#[test]
fn process_exit_on_an_isolated_test_entry_records_the_status() {
    let config = EntryConfig {
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: a test thunk with the entry ABI.
    let outcome = unsafe { run_entry(exit_4, &config) };
    assert_eq!(outcome.unwrap(), EntryOutcome::Exited(4));
    assert_eq!(take_test_exit(), Some(4));
    assert_eq!(take_test_exit(), None);
}

/// D-1022, D-709: on a root of a runtime under `cheby run`, `exit` fixes
/// the status, halts the runtime and sends `Exited` to the host; the
/// calling fiber stops where it is.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn process_exit_on_a_runtime_root_halts_with_its_status() {
    for threads in [1, 4] {
        let runtime = runtime(threads);
        let shared = runtime.shared_handle();
        let domain = Domain::new();
        let run = EntryConfig {
            mode: EntryMode::Run,
            ..config(&domain)
        };
        let outcome = testing::start_root_with(&runtime, &run, |me| {
            // SAFETY: the root's own context.
            unsafe { cheby_process_sys_exit(ctx_of(me), 9) }
        })
        .recv_timeout(testing::hang())
        .unwrap();
        assert_eq!(outcome, EntryOutcome::Exited(9));
        assert_eq!(shared.exit_status(), Some(9));
        assert!(shared.terminated());
        runtime.halt();
    }
}
