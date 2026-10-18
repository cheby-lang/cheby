//! The kernels of `std::runtime` (D-657, D-658): the scheduler's thread
//! count, read and set, and the number of CPU cores the program may use.
//!
//! `thread_count` and `set_thread_count` are `@std_context` kernels
//! (architecture §5.1, §5.5): they read the running fiber's runtime from
//! its control block. An isolated entry (the REPL, the thread backend on
//! Windows) runs no scheduler: there `thread_count` is 1 and
//! `set_thread_count` does nothing, since both complete without waiting
//! (D-692). `cpu_count` is ordinary: it reads the machine, not a fiber.
//!
//! None of them can fail or raise; each runs its body in `guarded` all the
//! same (D-697), as every kernel does.

use std::sync::Arc;

use crate::ctx::ExecCtx;
use crate::fiber_ops::running;
use crate::sched::{MAX_THREADS, Shared};
use crate::unwind::guard::guarded;

/// Appends every `std::runtime` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["runtime"];

/// `runtime::thread_count`: the number of OS threads the scheduler runs
/// fibers on now, from 1 to 1024 (D-657); 1 without a runtime
/// (architecture §5.1).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_runtime_thread_count(ctx: *mut ExecCtx) -> i64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        let count = me.sched.runtime().map_or(1, |shared| thread_count(shared));
        i64::try_from(count.clamp(1, MAX_THREADS)).unwrap_or(1)
    })
}

/// `runtime::set_thread_count`: sets the number of OS threads the scheduler
/// runs fibers on, `count` clamped to 1 to 1024 first (D-298, D-657);
/// nothing without a runtime (architecture §5.1).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_runtime_set_thread_count(ctx: *mut ExecCtx, count: i64) {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let me = unsafe { running(ctx) };
        if let Some(shared) = me.sched.runtime() {
            set_thread_count(shared, clamp_count(count));
        }
    });
}

/// `runtime::cpu_count`: the number of CPU cores the program may use, as
/// `std::thread::available_parallelism` reports it, and 1 when the system
/// reports nothing (D-657). It is not clamped to 1024: the page calls it
/// the default thread count "before the clamp".
#[unsafe(no_mangle)]
pub extern "C" fn cheby_runtime_cpu_count() -> i64 {
    guarded(None, || {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        i64::try_from(cpus).unwrap_or(i64::MAX)
    })
}

/// `count` clamped to 1 to [`MAX_THREADS`] (D-298, D-657).
fn clamp_count(count: i64) -> usize {
    let most = i64::try_from(MAX_THREADS).unwrap_or(i64::MAX);
    usize::try_from(count.clamp(1, most)).unwrap_or(1)
}

/// The runtime's thread count: its target (the scheduler's C4 seam), which
/// is the worker count until a program sets it, so a count just set is the
/// count read next (runtime.md's example).
fn thread_count(shared: &Shared) -> usize {
    shared.thread_count()
}

/// Sets the runtime's thread count (the scheduler's C4 seam), already
/// clamped: new workers start at once, and removed ones retire at their
/// next scheduling point (D-657, D-689); it waits for no fiber.
fn set_thread_count(shared: &Arc<Shared>, count: usize) {
    shared.set_thread_count(count);
}

/// One kernel: symbol, address, C parameter types after the context
/// pointer for a `@std_context` kernel, and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::runtime` kernel: the two `@std_context` ones first, then
/// `cpu_count`.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 3] {
    [
        (
            "cheby_runtime_thread_count",
            cheby_runtime_thread_count as *const u8,
            &[],
            "i64",
        ),
        (
            "cheby_runtime_set_thread_count",
            cheby_runtime_set_thread_count as *const u8,
            &["i64"],
            "()",
        ),
        (
            "cheby_runtime_cpu_count",
            cheby_runtime_cpu_count as *const u8,
            &[],
            "i64",
        ),
    ]
}

/// The kernels that take the fiber's context first (`@std_context`).
#[cfg(test)]
pub(crate) const CONTEXT_KERNELS: &[&str] = &[
    "cheby_runtime_thread_count",
    "cheby_runtime_set_thread_count",
];

/// The C signatures of the kernels, without the context pointer of the
/// `@std_context` ones, for the cross-check against `std/` in
/// `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::entry::{EntryConfig, EntryMode, EntryOutcome, run_entry};
    use crate::sched::testing;

    /// The context argument a kernel gets from the running fiber.
    fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
        std::ptr::from_ref(me).cast_mut()
    }

    /// `thread_count()` on the running fiber.
    fn count(me: &ExecCtx) -> i64 {
        // SAFETY: the running fiber's context.
        unsafe { cheby_runtime_thread_count(ctx_of(me)) }
    }

    /// `set_thread_count(n)` on the running fiber.
    fn set(me: &ExecCtx, n: i64) {
        // SAFETY: the running fiber's context.
        unsafe { cheby_runtime_set_thread_count(ctx_of(me), n) };
    }

    /// With a runtime, `thread_count` is the worker count until a program
    /// sets one, and then the count just set, clamped to 1 to 1024
    /// (D-298, D-657). Each runtime has its own count.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn runtime_info_thread_count_reads_and_sets_the_runtimes_target() {
        for threads in [1_i64, 2, 4] {
            let runtime = testing::runtime(usize::try_from(threads).unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&seen);
            testing::root(&runtime, move |me| {
                let mut seen = shared.lock().unwrap();
                seen.push(count(me));
                for n in [3, 0, -2, 1024, 5000, i64::MIN, 2] {
                    set(me, n);
                    seen.push(count(me));
                }
                0
            });
            runtime.shutdown();
            assert_eq!(
                *seen.lock().unwrap(),
                [threads, 3, 1, 1, 1024, 1024, 1, 2],
                "at {threads} threads"
            );
        }
    }

    /// What the isolated entry saw: `thread_count` before and after
    /// `set_thread_count(8)`.
    static ISOLATED: [AtomicI64; 2] = [AtomicI64::new(0), AtomicI64::new(0)];

    /// An isolated entry: counts, sets, counts again.
    unsafe extern "C" fn isolated(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        ISOLATED[0].store(count(me), Ordering::SeqCst);
        set(me, 8);
        ISOLATED[1].store(count(me), Ordering::SeqCst);
        7
    }

    /// Without a runtime (architecture §5.1, D-692), `thread_count` is 1 and
    /// `set_thread_count` does nothing: both complete without waiting.
    #[test]
    fn runtime_info_without_a_runtime_the_count_is_one_and_setting_does_nothing() {
        // SAFETY: a test thunk with the entry ABI.
        let outcome = unsafe { run_entry(isolated, &EntryConfig::new(EntryMode::Test)) };
        assert_eq!(outcome.ok(), Some(EntryOutcome::Returned(7)));
        assert_eq!(ISOLATED[0].load(Ordering::SeqCst), 1);
        assert_eq!(ISOLATED[1].load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_info_clamps_counts_to_one_through_1024() {
        assert_eq!(clamp_count(i64::MIN), 1);
        assert_eq!(clamp_count(-2), 1);
        assert_eq!(clamp_count(0), 1);
        assert_eq!(clamp_count(1), 1);
        assert_eq!(clamp_count(7), 7);
        assert_eq!(clamp_count(1024), 1024);
        assert_eq!(clamp_count(1025), 1024);
        assert_eq!(clamp_count(i64::MAX), 1024);
    }

    #[test]
    fn runtime_info_cpu_count_is_the_available_parallelism() {
        let expected = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let count = cheby_runtime_cpu_count();
        assert!(count >= 1);
        assert_eq!(usize::try_from(count).ok(), Some(expected));
    }
}
