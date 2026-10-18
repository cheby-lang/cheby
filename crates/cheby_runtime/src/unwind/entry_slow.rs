//! The entry check's releasing slow paths (D-775, refining D-698 and
//! D-743).
//!
//! Every Cheby function starts with the stack check of D-124. Its slow
//! path may start an unwinding (a stack overflow, a cancellation, the
//! constant-evaluation deadline), and the unwinding must then release the
//! function's counted parameters, which the MIR's entry pad drops (D-698,
//! D-752). For a function with at most four counted parameters, code
//! generation does not translate that entry-only pad chain: the slow block
//! calls one of the helpers here with the parameters instead, and branches
//! on its result to the function's shared `resume` (a nonzero result) or
//! to the body (zero). The MIR keeps its entry pad; a function with more
//! counted parameters keeps the translated chain.
//!
//! The helpers are not guarded themselves. Each calls the guarded slow
//! path ([`cheby_rt_stack_slow`] or [`cheby_rt_stack_slow_frame`]), which
//! runs every interrupt step as before (a cancellation is delivered there,
//! D-688) and, when it starts an unwinding, sets the fiber's unwind flag
//! and returns (D-700, D-733). That makes the helpers the one Rust caller
//! of a guarded entry point that goes on after the call: guard.rs's note
//! forbids that unless the caller tests the flag right after the call, and
//! the helpers do exactly that with [`super::flag::get`]. They release the
//! words only through [`rc::drop`], which never raises (a panic in a drop
//! function aborts, D-171), so nothing here can unwind, and they are on
//! the guard test's list of entry points that never unwind.

use crate::abi::{ENTRY_UNWIND_WORDS as MAX_WORDS, Word, unwind_flag};
use crate::ctx::ExecCtx;
use crate::panic::{cheby_rt_stack_slow, cheby_rt_stack_slow_frame, fatal};
use crate::rc;

/// The entry check's slow path for a function with at most four counted
/// parameters (D-775). Calls [`cheby_rt_stack_slow`]; if
/// that started an unwinding (the flag is set: overflow, cancellation,
/// deadline), releases `p0..p(count-1)` and returns 1; else returns 0 and
/// leaves them untouched. Never raises itself.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow`]; `count` is at most
/// [`ENTRY_UNWIND_WORDS`](crate::abi::ENTRY_UNWIND_WORDS), and
/// `p0..p(count-1)` are each 0, a tagged integer or a reference the caller
/// owns. The words from `count` on are ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) -> u32 {
    // SAFETY: forwarded from the caller.
    unsafe { cheby_rt_stack_slow(ctx, location) };
    // SAFETY: forwarded from the caller.
    unsafe { release_if_unwinding(ctx, count, [p0, p1, p2, p3]) }
}

/// [`cheby_rt_stack_slow_unwind`] for the outer function of a large frame
/// of `frame_size` bytes, through [`cheby_rt_stack_slow_frame`] (D-743,
/// D-775).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_stack_slow_frame_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) -> u32 {
    // SAFETY: forwarded from the caller.
    unsafe { cheby_rt_stack_slow_frame(ctx, location, frame_size) };
    // SAFETY: forwarded from the caller.
    unsafe { release_if_unwinding(ctx, count, [p0, p1, p2, p3]) }
}

/// After a slow path returned: when the flag of `ctx` shows an unwinding,
/// releases the first `count` of `words` and returns 1; otherwise returns
/// 0 and releases nothing.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`]; the slow path has run on `ctx`.
unsafe fn release_if_unwinding(ctx: *mut ExecCtx, count: u32, words: [Word; 4]) -> u32 {
    // SAFETY: forwarded from the caller; the slow path found the context,
    // so it is the running fiber's live control block.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a stack check ran without an execution context"
        ));
    };
    if super::flag::get(fiber) == unwind_flag::NONE {
        return 0;
    }
    if count > MAX_WORDS {
        fatal(format_args!(
            "the entry check's slow path was given {count} counted words, more than {MAX_WORDS}"
        ));
    }
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    for &word in words.iter().take(count) {
        // SAFETY: each of the first `count` words is 0, a tagged integer
        // or a reference the caller owns and gives up here (the caller's
        // contract); `rc::drop` skips 0 and tagged integers, and never
        // raises.
        unsafe { rc::drop(word) };
    }
    1
}

#[cfg(test)]
mod tests {
    //! The releasing slow paths on a root fiber of a runtime, with a fresh
    //! domain whose live count must end at zero: they release 0, 1 and 4
    //! counted words exactly when the slow path starts an unwinding (a stack
    //! overflow; a cancellation delivered at the entry check), and leave
    //! them untouched otherwise. Fibers never assert: they write what they
    //! saw into a list of problems that the test checks afterwards.

    use std::ptr;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use crate::abi::PanicKind;
    use crate::alloc::Domain;
    use crate::ctx::{reason, request};
    use crate::entry::{EntryConfig, EntryMode, EntryOutcome};
    use crate::fiber_ops::scope::{
        cheby_fiber_take_failure, cheby_rt_scope_close, cheby_rt_scope_open,
    };
    use crate::fiber_ops::testing::cancel_running_fiber_at_next_entry;
    use crate::panic::stack_position;
    use crate::sched::testing;
    use crate::{list, object, values};

    /// The numbers of counted words the tests give a helper.
    const COUNTS: [usize; 3] = [0, 1, 4];

    /// Which helper a test calls.
    #[derive(Clone, Copy, Debug)]
    enum Helper {
        /// [`cheby_rt_stack_slow_unwind`].
        Plain,
        /// [`cheby_rt_stack_slow_frame_unwind`] with a frame size.
        Frame(u64),
    }

    /// What fibers saw that a test rejects.
    type Problems = Arc<Mutex<Vec<String>>>;

    fn note(problems: &Problems, problem: String) {
        problems
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(problem);
    }

    /// Calls `helper` on the running fiber `ctx` with `words`.
    fn call(helper: Helper, ctx: &ExecCtx, words: &[Word]) -> u32 {
        let mut p = [0; 4];
        p[..words.len()].copy_from_slice(words);
        let count = u32::try_from(words.len()).unwrap();
        let ctx = ptr::from_ref(ctx).cast_mut();
        match helper {
            // SAFETY: the running fiber's context, no location, and words
            // the fiber owns.
            Helper::Plain => unsafe {
                cheby_rt_stack_slow_unwind(ctx, 0, count, p[0], p[1], p[2], p[3])
            },
            // SAFETY: as above.
            Helper::Frame(size) => unsafe {
                cheby_rt_stack_slow_frame_unwind(ctx, 0, size, count, p[0], p[1], p[2], p[3])
            },
        }
    }

    /// `n` new one-element lists, each with a second reference that the
    /// helper may release.
    fn shared_objects(n: usize) -> Vec<Word> {
        (0..n)
            .map(|i| {
                let word = list::from_iter([values::int_word(i64::try_from(i).unwrap())]);
                // SAFETY: a live object, owned.
                unsafe { rc::dup(word) };
                word
            })
            .collect()
    }

    fn count_of(word: Word) -> i32 {
        // SAFETY: tests pass live objects.
        unsafe { object::load_rc(object::word_to_ptr(word)) }
    }

    /// Checks that each of `words` has the count `expected`, then drops the
    /// fiber's last reference to each.
    fn check_and_release(problems: &Problems, context: &str, words: &[Word], expected: i32) {
        for &word in words {
            let count = count_of(word);
            if count != expected {
                note(
                    problems,
                    format!("{context}: a word has count {count}, expected {expected}"),
                );
            }
            // SAFETY: the fiber's own reference.
            unsafe { rc::drop(word) };
            if count == 2 {
                // SAFETY: the reference the helper did not release.
                unsafe { rc::drop(word) };
            }
        }
    }

    /// Runs `body` as the root fiber of a one-thread runtime with a fresh
    /// domain, and returns its outcome once nothing of the domain is left
    /// alive.
    fn run(body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) -> EntryOutcome {
        let runtime = testing::runtime(1);
        let domain = Domain::new();
        let config = EntryConfig {
            stack_size: 1 << 20,
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        let outcome = testing::start_root_with(&runtime, &config, body)
            .recv_timeout(testing::hang())
            .expect("the root fiber hung");
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
        outcome
    }

    fn assert_no_problems(problems: &Problems) {
        let problems = problems.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// Without an unwinding, a helper runs the slow path (a pending yield
    /// is taken and handled) and returns 0 with every word untouched.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn entry_slow_leaves_the_words_when_nothing_unwinds() {
        for helper in [Helper::Plain, Helper::Frame(1 << 10)] {
            for n in COUNTS {
                let problems = Problems::default();
                let seen = Arc::clone(&problems);
                let outcome = run(move |ctx| {
                    let words = shared_objects(n);
                    request(ctx, reason::YIELD);
                    let before = ctx.interrupts();
                    let result = call(helper, ctx, &words);
                    let context = format!("{helper:?}, {n} words");
                    if result != 0 || crate::unwind::flag::get(ctx) != unwind_flag::NONE {
                        note(&seen, format!("{context}: the helper unwound"));
                    }
                    if ctx.interrupts() == before {
                        note(&seen, format!("{context}: the yield was not handled"));
                    }
                    check_and_release(&seen, &context, &words, 2);
                    7
                });
                assert_eq!(outcome, EntryOutcome::Returned(7), "{helper:?}, {n} words");
                assert_no_problems(&problems);
            }
        }
    }

    /// Recurses with a frame of about 1 KiB, making the entry check at
    /// each level through `helper` with `words`, until the check starts an
    /// unwinding; notes a level whose check returned 0 but released a
    /// word. Returns the helper's last result.
    #[inline(never)]
    fn recurse(ctx: &ExecCtx, words: &[Word], problems: &Problems, depth: u64) -> u32 {
        let frame = std::hint::black_box([depth; 128]);
        if stack_position() < ctx.stack_limit.load(Ordering::Relaxed) {
            let result = call(Helper::Plain, ctx, words);
            if result != 0 {
                return result;
            }
            if words.iter().any(|&word| count_of(word) != 2) {
                note(problems, format!("a word was released at depth {depth}"));
            }
        }
        if depth > 1 << 20 {
            note(problems, "the recursion never overflowed".to_owned());
            return 0;
        }
        let below = recurse(ctx, words, problems, depth.wrapping_add(1));
        std::hint::black_box(frame);
        below
    }

    /// A stack overflow found by the slow path: the helper releases its
    /// words and returns 1, the flag shows a panic, and the entry ends with
    /// a stack-overflow panic. The plain helper is reached by a real
    /// recursion, the frame variant by a frame that cannot fit.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    #[cfg_attr(miri, ignore = "compares real stack addresses")]
    fn entry_slow_releases_the_words_on_stack_overflow() {
        for frame in [false, true] {
            for n in COUNTS {
                let problems = Problems::default();
                let seen = Arc::clone(&problems);
                let outcome = run(move |ctx| {
                    let words = shared_objects(n);
                    let result = if frame {
                        call(Helper::Frame(u64::MAX), ctx, &words)
                    } else {
                        recurse(ctx, &words, &seen, 0)
                    };
                    let context = format!("frame {frame}, {n} words");
                    if result != 1 {
                        note(&seen, format!("{context}: the helper returned {result}"));
                    }
                    let flag = crate::unwind::flag::get(ctx);
                    if flag != unwind_flag::PANIC {
                        note(&seen, format!("{context}: the flag is {flag}"));
                    }
                    check_and_release(&seen, &context, &words, 1);
                    0
                });
                let EntryOutcome::Panicked(report) = outcome else {
                    panic!("frame {frame}, {n} words: expected a panic, got {outcome:?}");
                };
                assert_eq!(report.kind, PanicKind::StackOverflow);
                assert_no_problems(&problems);
            }
        }
    }

    /// A cancellation delivered at the entry check: inside a scope that is
    /// the fiber's target, the helper releases its words and returns 1 with
    /// the flag showing a cancellation; the scope's close, as `scope_run`'s
    /// pad makes it, catches it and fails.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn entry_slow_releases_the_words_on_cancellation() {
        for helper in [Helper::Plain, Helper::Frame(1 << 10)] {
            for n in COUNTS {
                let problems = Problems::default();
                let seen = Arc::clone(&problems);
                let outcome = run(move |ctx| {
                    let me = ptr::from_ref(ctx).cast_mut();
                    // SAFETY: the running fiber's context.
                    let scope = unsafe { cheby_rt_scope_open(me, false, 0, 0) };
                    let words = shared_objects(n);
                    cancel_running_fiber_at_next_entry(ctx);
                    let result = call(helper, ctx, &words);
                    let context = format!("{helper:?}, {n} words");
                    if result != 1 {
                        note(&seen, format!("{context}: the helper returned {result}"));
                    }
                    let flag = crate::unwind::flag::get(ctx);
                    if flag != unwind_flag::CANCEL {
                        note(&seen, format!("{context}: the flag is {flag}"));
                    }
                    check_and_release(&seen, &context, &words, 1);
                    crate::unwind::flag::clear(ctx);
                    // SAFETY: the running fiber's context and its scope's
                    // handle, consumed.
                    if unsafe { cheby_rt_scope_close(me, scope, true) } {
                        note(&seen, format!("{context}: the scope succeeded"));
                    }
                    // SAFETY: the running fiber's context, after a failed
                    // close; the failure is released.
                    unsafe { rc::drop(cheby_fiber_take_failure(me)) };
                    7
                });
                assert_eq!(outcome, EntryOutcome::Returned(7), "{helper:?}, {n} words");
                assert_no_problems(&problems);
            }
        }
    }
}
