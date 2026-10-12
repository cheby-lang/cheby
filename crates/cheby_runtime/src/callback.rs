//! Calling Cheby closures from kernels (D-526, kernel-reentry).
//!
//! Kernels take no `ctx` (D-453). A kernel that must call a closure, such
//! as `map::upsert` or a caller of `@std_hooks` (D-484), calls [`call`],
//! which reaches the closure through the running entry's trampoline of the
//! right arity (`cb$tramp$<n>`, C ABI: `u64 (ctx*, u64 closure, u64 a0,
//! …)`) with the entry's context.
//!
//! A panic inside the closure: with the default backend, it abandons the
//! entry like any other panic (D-457), and no Rust frame of the kernel
//! ever runs again. With an unwinding backend, the trampoline is a catch
//! point (D-699) that returns while the fiber unwinds; [`call`] then
//! rethrows the `ChebyUnwind` marker, so the kernel's own frames unwind
//! with their `Drop`s to the kernel's guard, which hands the unwinding
//! back to generated code (D-697). A kernel must not hold an owned word
//! across the call without a drop guard (`map::upsert` has one).
//!
//! [`call`] also saves the fiber's `try_call` exit record (FCB offsets 16
//! and 24) around the trampoline and restores it before it returns or
//! rethrows, so a throw from the kernel's guard starts from the kernel's
//! caller, not from the closure's finished frames (plan §4.2).
//!
//! While the closure runs, the fiber is pinned to its worker (D-687): the
//! kernel's frames below it are tier-1 code written for one thread, so if
//! the closure suspends, the fiber resumes on the same thread.

use crate::abi::Word;
use crate::entry;
use crate::panic::fatal;
use crate::unwind::raise::{SavedExit, rethrow_if_unwinding};

/// The most arguments a closure called from a kernel may take (D-526).
pub const MAX_ARITY: usize = 4;

/// Calls `closure` with `args` and returns its result: the closure is
/// borrowed, the arguments are owned (given to the closure, also when it
/// panics), the result is owned. Ends the process if no entry runs on this
/// thread, if the entry has no trampoline of this arity, or if there are
/// more than four arguments. When the closure panics under an unwinding
/// backend, it does not return: it rethrows the unwinding through the
/// caller's Rust frames to the kernel's guard (module documentation).
///
/// # Safety
///
/// Called on an entry's thread whose trampolines are valid; `closure` is a
/// live closure of exactly `args.len()` parameters, and each argument is a
/// uniform word of its parameter's type.
#[must_use]
pub unsafe fn call(closure: Word, args: &[Word]) -> Word {
    // One current-fiber lookup: the trampolines come from its control
    // block (D-686, GATE1-1).
    let Some(ctx) = entry::current_ctx() else {
        fatal(format_args!(
            "a kernel called a closure outside of any entry"
        ));
    };
    let missing = || -> ! {
        fatal(format_args!(
            "the running entry has no trampoline for a closure of {} arguments",
            args.len()
        ))
    };
    // D-687: the fiber is pinned to its worker while the kernel's frames
    // are on its stack, so they resume on the thread they started on.
    // SAFETY: `ctx` is the running fiber's control block, live while it
    // runs; the reference ends with this call.
    let fiber = unsafe { &*ctx };
    let callbacks = fiber.callbacks();
    fiber.sched.enter_callback();
    let exit = SavedExit::save(fiber);
    // SAFETY: the trampolines come from the entry's configuration, whose
    // ABI the host guarantees (`run_entry`'s contract); `ctx` is the running
    // entry's; the closure and arguments are as the caller guarantees.
    let result = unsafe {
        match *args {
            [] => callbacks.t0.unwrap_or_else(|| missing())(ctx, closure),
            [a0] => callbacks.t1.unwrap_or_else(|| missing())(ctx, closure, a0),
            [a0, a1] => callbacks.t2.unwrap_or_else(|| missing())(ctx, closure, a0, a1),
            [a0, a1, a2] => callbacks.t3.unwrap_or_else(|| missing())(ctx, closure, a0, a1, a2),
            [a0, a1, a2, a3] => {
                callbacks.t4.unwrap_or_else(|| missing())(ctx, closure, a0, a1, a2, a3)
            }
            _ => fatal(format_args!(
                "a kernel called a closure with {} arguments; at most {MAX_ARITY} are supported",
                args.len()
            )),
        }
    };
    exit.restore(fiber);
    fiber.sched.leave_callback();
    // The trampoline caught an unwinding (D-699) and returned 0: the
    // kernel's frames unwind to its guard (D-697).
    rethrow_if_unwinding(fiber);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ExecCtx;
    use crate::entry::{Callbacks, EntryConfig, EntryMode, EntryOutcome, run_entry};
    use crate::generic::tests::closure;
    use crate::rc;
    use crate::values;

    /// The sum of a closure's captures (tagged ints) and its arguments,
    /// read the way a trampoline would enter generated code.
    fn sum(closure: Word, args: &[Word]) -> u64 {
        // SAFETY: a live closure built by the test.
        let captures = unsafe { crate::generic::tests::captures(closure) };
        let total = captures
            .iter()
            .chain(args)
            .map(|word| abi_value(*word))
            .fold(0_i64, i64::wrapping_add);
        for arg in args {
            // SAFETY: the trampoline owns its arguments.
            unsafe { rc::drop(*arg) };
        }
        values::int_word(total)
    }

    fn abi_value(word: Word) -> i64 {
        // SAFETY: tagged or boxed ints only.
        unsafe { values::word_to_i64(word) }
    }

    unsafe extern "C" fn t0(_: *mut ExecCtx, c: u64) -> u64 {
        sum(c, &[])
    }
    unsafe extern "C" fn t1(_: *mut ExecCtx, c: u64, a0: u64) -> u64 {
        sum(c, &[a0])
    }
    unsafe extern "C" fn t2(_: *mut ExecCtx, c: u64, a0: u64, a1: u64) -> u64 {
        sum(c, &[a0, a1])
    }
    unsafe extern "C" fn t3(_: *mut ExecCtx, c: u64, a0: u64, a1: u64, a2: u64) -> u64 {
        sum(c, &[a0, a1, a2])
    }
    unsafe extern "C" fn t4(_: *mut ExecCtx, c: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
        sum(c, &[a0, a1, a2, a3])
    }

    /// A "kernel" that calls closures of every arity 0-4.
    unsafe extern "C" fn calls_every_arity(_: *mut ExecCtx) -> u64 {
        let base = closure(&[values::int_word(100)]);
        let mut total = 0_i64;
        for arity in 0..=4_i64 {
            let args: Vec<Word> = (1..=arity).map(values::int_word).collect();
            // SAFETY: on an entry's thread with every trampoline; the
            // closure is live and the arguments are given away.
            let result = unsafe { call(base, &args) };
            total = total.wrapping_add(abi_value(result));
        }
        // SAFETY: the test's reference.
        unsafe { rc::drop(base) };
        // 100 * 5 + (0 + 1 + 3 + 6 + 10)
        u64::try_from(total).unwrap_or(0)
    }

    #[test]
    fn kernels_call_closures_through_the_entrys_trampolines() {
        let config = EntryConfig {
            callbacks: Callbacks {
                t0: Some(t0),
                t1: Some(t1),
                t2: Some(t2),
                t3: Some(t3),
                t4: Some(t4),
            },
            ..EntryConfig::new(EntryMode::Test)
        };
        // SAFETY: a test thunk with the `EntryFn` ABI and static code, and
        // trampolines with their declared ABI.
        let outcome = unsafe { run_entry(calls_every_arity, &config) }.unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(520));
        assert_eq!(config.domain.live_objects(), 0);
    }
}
